//! A workflow driven through a real window: a check on this machine, a
//! project tab opened for a step whose command runs in that project's
//! Terminal panel, a manual gate, a command without a project, Restart
//! and Stop All; and the workflow tab: its graph following the editor,
//! problems over the last good graph, cards and the cursor, Run All saving
//! first, restore with the window.

use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

use gpui::{Focusable, TestAppContext, WindowHandle};
use workflow::Status;

use super::*;
use crate::{shell::Shell, workspace::Workspace};

fn root(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("nc-workflows-{name}-{}", std::process::id()));
    std::fs::remove_dir_all(&root).ok();
    for dir in ["alpha", "beta", "user"] {
        std::fs::create_dir_all(root.join(dir)).unwrap();
    }
    root
}

fn cleanup(root: PathBuf) {
    std::fs::remove_dir_all(root).ok();
    if let Some(recent) = crate::recent::path() {
        std::fs::remove_file(recent).ok();
    }
    if let Some(state) = crate::layout::state_root() {
        std::fs::remove_dir_all(state).ok();
    }
}

/// Runs the app (real processes write from their own threads) until
/// `done` holds.
fn wait_until(
    cx: &mut TestAppContext,
    what: &str,
    mut done: impl FnMut(&mut TestAppContext) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        cx.run_until_parked();
        if done(cx) {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        cx.executor().advance_clock(TICK);
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn statuses(workflows: &Entity<Workflows>, cx: &mut TestAppContext) -> Vec<&'static str> {
    workflows.read_with(cx, |workflows, _| {
        workflows.runs()[0]
            .run
            .steps()
            .iter()
            .map(|step| step.status.name())
            .collect()
    })
}

fn workspace(
    window: WindowHandle<Shell>,
    tab: usize,
    cx: &mut TestAppContext,
) -> Entity<Workspace> {
    window
        .read_with(cx, |shell, _| shell.workspace_entities()[tab].clone())
        .unwrap()
}

#[gpui::test]
fn a_workflow_opens_projects_runs_steps_in_order_and_restart_reuses_the_terminal(
    cx: &mut TestAppContext,
) {
    let root = root("run");
    let beta = root.join("beta");
    // `echo`, `sleep` and `exit` mean the same in sh and PowerShell.
    let file = format!(
        r#"{{
          // Comments are fine.
          "name": "Dev",
          "steps": [
            {{ "id": "check", "check": "exit 0" }},
            {{
              "id": "server", "title": "Server", "needs": ["check"],
              "project": {{ "local": {beta} }},
              "run": "echo serv''ing; sleep 30",
              "ready": {{ "output": "serving" }}
            }},
            {{ "id": "gate", "needs": ["server"], "ready": {{ "manual": "Go on" }} }},
            {{ "id": "done", "needs": ["gate"], "run": "echo finished" }},
          ],
        }}"#,
        beta = serde_json::to_string(&beta).unwrap(),
    );
    std::fs::write(root.join("user").join("dev.jsonc"), file).unwrap();

    cx.update(|cx| crate::shell::open_window(Location::Local(root.join("alpha")), None, cx));
    cx.run_until_parked();
    let window = cx.windows()[0].downcast::<Shell>().unwrap();
    let workflows = window
        .update(cx, |_, window, cx| for_window(window, cx))
        .unwrap();
    workflows.update(cx, |workflows, _| {
        workflows.user_dir = Some(root.join("user"))
    });
    workflows
        .update(cx, |workflows, cx| workflows.refresh(cx))
        .detach();
    cx.run_until_parked();
    let (id, path) = workflows.read_with(cx, |workflows, _| {
        let found = &workflows.discovered()[0];
        assert_eq!(found.name, "Dev");
        assert_eq!(found.source, Source::User);
        (found.id.clone(), found.path.clone())
    });

    // Its tab runs it.
    let view = window
        .update(cx, |shell, window, cx| {
            shell.open_workflow(&Source::User, &path, window, cx)
        })
        .unwrap()
        .unwrap();
    view.update(cx, |view, cx| view.run_all(cx));
    wait_until(cx, "the server's output", |cx| {
        statuses(&workflows, cx) == ["ready", "ready", "running", "waiting"]
    });
    // The step's project opened in a background tab of this window, its
    // command in a terminal tab named after the step.
    let tabs = window
        .read_with(cx, |shell, cx| shell.workflow_projects(cx))
        .unwrap();
    assert_eq!(tabs.len(), 2);
    assert_eq!(tabs[1].0, Location::Local(beta.clone()).absolute());
    let terminal =
        workspace(window, 1, cx).read_with(cx, |workspace, _| workspace.terminal_panel());
    let first_view = terminal.read_with(cx, |terminal, _| terminal.views());
    assert_eq!(first_view.len(), 1);
    assert_eq!(
        first_view[0].read_with(cx, |view, _| view.run_name().map(str::to_owned)),
        Some("Server".to_owned())
    );
    // The tab's graph shows the steps and their states.
    assert_eq!(
        view.read_with(cx, |view, cx| view.cards(cx)),
        [
            ("check".to_owned(), "ready".to_owned()),
            ("server".to_owned(), "ready".to_owned()),
            ("gate".to_owned(), "running".to_owned()),
            ("done".to_owned(), "waiting".to_owned()),
        ]
    );

    // Continue on the gate; the last step runs here and exits 0.
    assert!(workflows.update(cx, |workflows, cx| workflows.continue_step(&id, 2, cx)));
    wait_until(cx, "the last step", |cx| {
        statuses(&workflows, cx) == ["ready"; 4]
    });
    workflows.read_with(cx, |workflows, _| {
        let active = workflows.run_of(&id).unwrap();
        assert_eq!(active.run.outcome(), Outcome::Ready);
        assert!(active.terminal(3).is_some(), "shown by the tab");
        assert!(active.location(3).is_none());
    });

    // Restart replaces the step's terminal tab instead of adding one.
    workflows.update(cx, |workflows, cx| workflows.restart_step(&id, 1, cx));
    wait_until(cx, "the restarted server", |cx| {
        statuses(&workflows, cx)[1] == "ready"
            && terminal.read_with(cx, |terminal, _| terminal.views()[0] != first_view[0])
    });
    assert_eq!(
        terminal.read_with(cx, |terminal, _| terminal.views().len()),
        1
    );

    // Stop All interrupts the server; the tabs stay.
    workflows.update(cx, |workflows, cx| workflows.stop(&id, cx));
    wait_until(cx, "the stop", |cx| {
        statuses(&workflows, cx)[1] == "stopped"
    });
    cx.executor().advance_clock(STOP_GRACE);
    wait_until(cx, "the server to end", |cx| {
        terminal.read_with(cx, |terminal, cx| {
            terminal.views()[0].read(cx).exit().is_some()
        })
    });
    workflows.read_with(cx, |workflows, _| {
        assert_eq!(
            workflows.run_of(&id).unwrap().run.outcome(),
            Outcome::Stopped
        );
    });
    assert_eq!(
        window
            .read_with(cx, |shell, cx| shell.workflow_projects(cx).len())
            .unwrap(),
        2
    );
    cleanup(root);
}

#[gpui::test]
fn a_failing_check_without_on_fail_fails_the_workflow(cx: &mut TestAppContext) {
    let root = root("fail");
    std::fs::write(
        root.join("user").join("broken.json"),
        r#"{"name": "Broken", "steps": [
            {"id": "vpn", "check": "echo 'no route'; exit 3"},
            {"id": "after", "needs": ["vpn"], "run": "echo never"}
        ]}"#,
    )
    .unwrap();
    std::fs::write(
        root.join("user").join("invalid.json"),
        r#"{"steps": [{"id": "a", "needs": ["b"], "run": "x"}]}"#,
    )
    .unwrap();
    cx.update(|cx| crate::shell::open_window(Location::Local(root.join("alpha")), None, cx));
    cx.run_until_parked();
    let window = cx.windows()[0].downcast::<Shell>().unwrap();
    let workflows = window
        .update(cx, |_, window, cx| for_window(window, cx))
        .unwrap();
    workflows.update(cx, |workflows, _| {
        workflows.user_dir = Some(root.join("user"))
    });
    workflows
        .update(cx, |workflows, cx| workflows.refresh(cx))
        .detach();
    cx.run_until_parked();
    let (broken, invalid) = workflows.read_with(cx, |workflows, _| {
        let names: Vec<&str> = workflows
            .discovered()
            .iter()
            .map(|found| found.name.as_str())
            .collect();
        assert_eq!(names, ["Broken", "invalid"]);
        let invalid = &workflows.discovered()[1];
        assert_eq!(
            invalid.parsed.as_ref().unwrap_err(),
            "`a` needs unknown step `b`"
        );
        (workflows.discovered()[0].id.clone(), invalid.id.clone())
    });
    let refused = workflows.update(cx, |workflows, cx| workflows.start(&invalid, cx));
    assert!(refused.unwrap_err().contains("cannot run"));

    workflows
        .update(cx, |workflows, cx| workflows.start(&broken, cx))
        .unwrap();
    wait_until(cx, "the check", |cx| {
        statuses(&workflows, cx) == ["failed", "stopped"]
    });
    workflows.read_with(cx, |workflows, _| {
        let active = workflows.run_of(&broken).unwrap();
        assert_eq!(active.run.outcome(), Outcome::Failed);
        let Status::Failed(reason) = &active.run.steps()[0].status else {
            panic!()
        };
        assert!(reason.starts_with("check failed ("), "{reason}");
        assert!(reason.contains("no route"), "{reason}");
    });
    cleanup(root);
}

const DIAMOND: &str = r#"{
  "name": "Diamond",
  "steps": [
    { "id": "check", "check": "exit 0" },
    { "id": "api", "needs": ["check"], "check": "exit 0" },
    { "id": "web", "needs": ["check"], "check": "exit 0" },
    { "id": "join", "needs": ["api", "web"], "check": "exit 0" }
  ]
}
"#;

fn columns(view: &Entity<WorkflowView>, cx: &mut TestAppContext) -> Vec<Vec<String>> {
    view.read_with(cx, |view, _| view.columns())
}

fn cursor_step(view: &Entity<WorkflowView>, cx: &mut TestAppContext) -> Option<String> {
    view.read_with(cx, |view, _| view.cursor_step().map(str::to_owned))
}

/// Lets the graph catch up with the editor.
fn settle(cx: &mut TestAppContext) {
    cx.executor().advance_clock(Duration::from_millis(250));
    cx.run_until_parked();
}

#[gpui::test]
fn a_workflow_tab_shows_the_file_and_a_graph_that_follows_edits(cx: &mut TestAppContext) {
    let root = root("tab");
    let path = root.join("user").join("diamond.jsonc");
    std::fs::write(&path, DIAMOND).unwrap();
    cx.update(crate::app_state::init);
    cx.update(|cx| crate::shell::open_window(Location::Local(root.join("alpha")), None, cx));
    cx.run_until_parked();
    let window = cx.windows()[0].downcast::<Shell>().unwrap();
    let workflows = window
        .update(cx, |_, window, cx| for_window(window, cx))
        .unwrap();
    workflows.update(cx, |workflows, _| {
        workflows.user_dir = Some(root.join("user"))
    });
    let open = |cx: &mut TestAppContext| {
        window
            .update(cx, |shell, window, cx| {
                shell.open_workflow(&Source::User, &path, window, cx)
            })
            .unwrap()
            .unwrap()
    };
    let view = open(cx);
    cx.run_until_parked();
    let tabs = |cx: &mut TestAppContext| {
        window
            .read_with(cx, |shell, cx| shell.tab_summary(cx))
            .unwrap()
    };
    assert_eq!(tabs(cx), (vec!["alpha".into(), "◈ Diamond".into()], 1));
    // Opening it again shows the same tab.
    cx.dispatch_action(window.into(), crate::shell::PreviousProject);
    assert_eq!(tabs(cx).1, 0);
    assert_eq!(open(cx), view);
    assert_eq!(tabs(cx), (vec!["alpha".into(), "◈ Diamond".into()], 1));

    // The editor holds the file, the graph its steps by depth.
    let editor = view.read_with(cx, |view, _| view.editor());
    assert_eq!(editor.read_with(cx, |editor, _| editor.text()), DIAMOND);
    assert_eq!(
        columns(&view, cx),
        [vec!["check"], vec!["api", "web"], vec!["join"]]
    );
    let focused = window
        .update(cx, |_, window, cx| {
            editor.focus_handle(cx).is_focused(window)
        })
        .unwrap();
    assert!(focused, "the tab's editor has focus");

    // An unknown `needs` id: the problem, where it is; the last graph stays.
    let broken = DIAMOND.replace(r#"["api", "web"]"#, r#"["api", "nope"]"#);
    editor.update(cx, |editor, cx| editor.set_text(&broken, cx));
    cx.run_until_parked();
    assert!(
        view.read_with(cx, |view, _| view.problems()).is_empty(),
        "not before the pause"
    );
    settle(cx);
    assert_eq!(
        view.read_with(cx, |view, _| view.problems()),
        [("`join` needs unknown step `nope`".to_owned(), Some((7, 38)))]
    );
    assert_eq!(
        columns(&view, cx),
        [vec!["check"], vec!["api", "web"], vec!["join"]]
    );

    // Fixed differently: the graph follows (`join` needs only `api`).
    let fixed = DIAMOND.replace(r#"["api", "web"]"#, r#"["api"]"#);
    editor.update(cx, |editor, cx| editor.set_text(&fixed, cx));
    settle(cx);
    assert!(view.read_with(cx, |view, _| view.problems()).is_empty());
    assert_eq!(
        view.read_with(cx, |view, _| view.edges()),
        [
            ("check".to_owned(), "api".to_owned()),
            ("check".to_owned(), "web".to_owned()),
            ("api".to_owned(), "join".to_owned()),
        ]
    );

    // A card click puts the cursor on its `"id"`; the cursor in a step
    // highlights its card.
    window
        .update(cx, |_, window, cx| {
            view.update(cx, |view, cx| view.select("web", window, cx))
        })
        .unwrap();
    cx.run_until_parked();
    let cursor_place = editor.read_with(cx, |editor, _| {
        workflow::source::line_column(&editor.text(), editor.cursor())
    });
    assert_eq!(cursor_place, (6, 7));
    assert_eq!(cursor_step(&view, cx).as_deref(), Some("web"));
    let inside_api = fixed.find("\"api\", \"needs\"").unwrap();
    editor.update(cx, |editor, cx| editor.set_cursor(inside_api, cx));
    cx.run_until_parked();
    assert_eq!(cursor_step(&view, cx).as_deref(), Some("api"));

    // Run All saves the edits first, then runs what was saved.
    assert!(view.read_with(cx, |view, cx| view.is_dirty(cx)));
    view.update(cx, |view, cx| view.run_all(cx));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), fixed);
    assert!(!view.read_with(cx, |view, cx| view.is_dirty(cx)));
    wait_until(cx, "every check", |cx| {
        view.read_with(cx, |view, cx| view.outcome(cx)) == Outcome::Ready
    });
    assert_eq!(
        view.read_with(cx, |view, cx| view.cards(cx)),
        [
            ("check".to_owned(), "ready".to_owned()),
            ("api".to_owned(), "ready".to_owned()),
            ("web".to_owned(), "ready".to_owned()),
            ("join".to_owned(), "ready".to_owned()),
        ]
    );

    // The tab is saved with the window and comes back with it.
    cx.executor().advance_clock(Duration::from_millis(600));
    cx.run_until_parked();
    let state = crate::app_state::load().unwrap();
    assert_eq!(
        state.windows[0].tabs[1],
        crate::app_state::TabTarget::Workflow {
            workflow: path.clone(),
            project: None,
        }
    );
    window
        .update(cx, |_, window, _| window.remove_window())
        .unwrap();
    cx.run_until_parked();
    cx.update(|cx| crate::shell::restore(state, None, cx));
    cx.run_until_parked();
    let restored = cx.windows()[0].downcast::<Shell>().unwrap();
    assert_eq!(
        restored
            .read_with(cx, |shell, cx| shell.tab_summary(cx))
            .unwrap(),
        (vec!["alpha".into(), "◈ Diamond".into()], 1)
    );
    restored
        .update(cx, |_, window, _| window.remove_window())
        .unwrap();
    cx.run_until_parked();
    cleanup(root);
}
