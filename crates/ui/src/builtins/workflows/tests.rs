//! A workflow driven through a real window: a check on this machine, a
//! project tab opened for a step whose command runs in that project's
//! Terminal panel, a manual gate, a command without a project, Restart
//! and Stop All; and the workflow tab: its graph following the editor,
//! problems over the last good graph, cards and the cursor, Run All saving
//! first, restore with the window; a file of the old JSON format
//! converted from its tab.

use std::{
    path::{Path, PathBuf},
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

/// A window on project `alpha` and its workflows, listing the test's user
/// folder.
fn open_window(root: &Path, cx: &mut TestAppContext) -> (WindowHandle<Shell>, Entity<Workflows>) {
    cx.update(|cx| crate::shell::open_window(Location::Local(root.join("alpha")), None, cx));
    cx.run_until_parked();
    let window = cx.windows()[0].downcast::<Shell>().unwrap();
    let workflows = window
        .update(cx, |_, window, cx| for_window(window, cx))
        .unwrap();
    workflows.update(cx, |workflows, _| {
        workflows.user_dir = Some(root.join("user"))
    });
    (window, workflows)
}

/// The tab of the user's workflow file `path`.
fn open_tab(
    window: WindowHandle<Shell>,
    path: &Path,
    cx: &mut TestAppContext,
) -> Entity<WorkflowView> {
    window
        .update(cx, |shell, window, cx| {
            super::window::open(shell, &Source::User, path, window, cx)
        })
        .unwrap()
        .unwrap()
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
        workflows.runs[0]
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
        r#"# Comments are fine.
name: Dev
steps:
  check: {{check: exit 0}}
  server:
    title: Server
    depends_on: [check]
    project: {{local: {beta}}}
    run: echo serv''ing; sleep 30
    ready: {{output: serving}}
  gate: {{depends_on: [server], ready: {{manual: Go on}}}}
  done: {{depends_on: [gate], run: echo finished}}
"#,
        beta = serde_json::to_string(&beta).unwrap(),
    );
    std::fs::write(root.join("user").join("dev.yaml"), file).unwrap();

    let (window, workflows) = open_window(&root, cx);
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
    let view = open_tab(window, &path, cx);
    view.update(cx, |view, cx| view.run_all(cx));
    wait_until(cx, "the server's output", |cx| {
        statuses(&workflows, cx) == ["ready", "ready", "running", "waiting"]
    });
    // The step's project opened in a background tab of this window, its
    // command in a terminal tab named after the step.
    let tabs = window
        .read_with(cx, |shell, cx| shell.projects(cx))
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

    // Stop All interrupts the server (stopping until it exits, killed
    // after its grace at the latest); the tabs stay.
    workflows.update(cx, |workflows, cx| workflows.stop(&id, cx));
    assert_eq!(statuses(&workflows, cx)[1], "stopping");
    wait_until(cx, "the stop", |cx| {
        statuses(&workflows, cx)[1] == "stopped"
    });
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
            .read_with(cx, |shell, cx| shell.projects(cx).len())
            .unwrap(),
        2
    );
    cleanup(root);
}

#[gpui::test]
fn a_failing_check_without_on_fail_fails_the_workflow(cx: &mut TestAppContext) {
    let root = root("fail");
    std::fs::write(
        root.join("user").join("broken.yaml"),
        r#"name: Broken
steps:
  vpn: {check: "echo 'no route'; exit 3"}
  after: {depends_on: [vpn], run: echo never}
"#,
    )
    .unwrap();
    std::fs::write(
        root.join("user").join("invalid.yml"),
        "steps: {a: {depends_on: [b], run: x}}",
    )
    .unwrap();
    let (_, workflows) = open_window(&root, cx);
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
            "`a` depends on unknown step `b`"
        );
        (workflows.discovered()[0].id.clone(), invalid.id.clone())
    });
    let refused = workflows.update(cx, |workflows, cx| {
        workflows.start(&invalid, &Default::default(), cx)
    });
    assert!(refused.unwrap_err().contains("cannot run"));

    workflows
        .update(cx, |workflows, cx| {
            workflows.start(&broken, &Default::default(), cx)
        })
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

const DIAMOND: &str = "name: Diamond
steps:
  check: {check: exit 0}
  api: {depends_on: [check], check: exit 0}
  web: {depends_on: [check], check: exit 0}
  join: {depends_on: [api, web], check: exit 0}
";

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
    let path = root.join("user").join("diamond.yaml");
    std::fs::write(&path, DIAMOND).unwrap();
    cx.update(crate::app_state::init);
    let (window, _) = open_window(&root, cx);
    let open = |cx: &mut TestAppContext| open_tab(window, &path, cx);
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

    // An unknown dependency: the problem, where it is; the last graph
    // stays.
    let broken = DIAMOND.replace("[api, web]", "[api, nope]");
    editor.update(cx, |editor, cx| editor.set_text(&broken, cx));
    cx.run_until_parked();
    assert!(
        view.read_with(cx, |view, _| view.problems()).is_empty(),
        "not before the pause"
    );
    settle(cx);
    assert_eq!(
        view.read_with(cx, |view, _| view.problems()),
        [(
            "`join` depends on unknown step `nope`".to_owned(),
            Some((6, 28))
        )]
    );
    assert_eq!(
        columns(&view, cx),
        [vec!["check"], vec!["api", "web"], vec!["join"]]
    );

    // Fixed differently: the graph follows (`join` needs only `api`).
    let fixed = DIAMOND.replace("[api, web]", "[api]");
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

    // A card click puts the cursor on its key; the cursor in a step
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
    assert_eq!(cursor_place, (5, 3));
    assert_eq!(cursor_step(&view, cx).as_deref(), Some("web"));
    let inside_api = fixed.find("api: {depends_on").unwrap() + 8;
    editor.update(cx, |editor, cx| editor.set_cursor(inside_api, cx));
    cx.run_until_parked();
    assert_eq!(cursor_step(&view, cx).as_deref(), Some("api"));

    // Run All saves the edits first, then runs what was saved.
    assert!(view.read_with(cx, |view, cx| view.is_dirty(cx)));
    view.update(cx, |view, cx| view.run_all(cx));
    // Saving is asynchronous: the run starts once the file is written.
    wait_until(cx, "the save", |_| {
        std::fs::read_to_string(&path).is_ok_and(|text| text == fixed)
    });
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
        crate::app_state::TabTarget::File {
            kind: "workflow".into(),
            path: path.clone(),
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

const GROUPED: &str = "name: Grouped
steps:
  check: {check: exit 0}
  svc:
    depends_on: [check]
    steps:
      build: {check: exit 0}
      inner:
        depends_on: [build]
        steps:
          x: {check: exit 0}
  report: {depends_on: {svc: completed}, check: exit 0}
";

#[gpui::test]
fn groups_expand_in_place_and_stay_expanded_across_edits(cx: &mut TestAppContext) {
    let root = root("groups");
    let path = root.join("user").join("grouped.yaml");
    std::fs::write(&path, GROUPED).unwrap();
    cx.update(crate::app_state::init);
    let (window, _) = open_window(&root, cx);
    let view = open_tab(window, &path, cx);
    cx.run_until_parked();
    let boxes = |cx: &mut TestAppContext| -> Vec<(String, (f32, f32))> {
        view.read_with(cx, |view, _| view.boxes())
    };
    let card = (200., 76.);
    let ids = |boxes: &[(String, (f32, f32))]| -> Vec<String> {
        boxes.iter().map(|(id, _)| id.clone()).collect()
    };
    // Collapsed: a group is a card like the others.
    let collapsed = boxes(cx);
    assert_eq!(ids(&collapsed), ["check", "svc", "report"]);
    assert!(collapsed.iter().all(|(_, size)| *size == card));
    let toggle = |path: &'static str, cx: &mut TestAppContext| {
        window
            .update(cx, |_, window, cx| {
                view.update(cx, |view, cx| view.toggle_group(path, window, cx))
            })
            .unwrap();
        cx.run_until_parked();
    };
    // Expanded: a bigger box around its steps, one level at a time.
    toggle("svc", cx);
    let open = boxes(cx);
    assert_eq!(
        ids(&open),
        ["check", "svc", "svc/build", "svc/inner", "report"]
    );
    let svc = open[1].1;
    assert!(svc.0 > card.0 && svc.1 > card.1, "{svc:?}");
    assert_eq!(open[3].1, card, "the inner group is still collapsed");
    toggle("svc/inner", cx);
    assert_eq!(
        ids(&boxes(cx)),
        [
            "check",
            "svc",
            "svc/build",
            "svc/inner",
            "svc/inner/x",
            "report"
        ]
    );
    // The click selected the group and put the cursor on its key.
    assert_eq!(cursor_step(&view, cx).as_deref(), Some("svc/inner"));
    let editor = view.read_with(cx, |view, _| view.editor());
    let inside_x = GROUPED.find("x: {check").unwrap() + 4;
    editor.update(cx, |editor, cx| editor.set_cursor(inside_x, cx));
    cx.run_until_parked();
    assert_eq!(cursor_step(&view, cx).as_deref(), Some("svc/inner/x"));

    // Edits keep the groups expanded.
    let edited = GROUPED.replace(
        "completed}, check: exit 0}",
        "completed}, check: 'exit 0 '}",
    );
    editor.update(cx, |editor, cx| editor.set_text(&edited, cx));
    settle(cx);
    assert_eq!(ids(&boxes(cx)).len(), 6);

    // Collapsing the outer group hides its steps again.
    toggle("svc", cx);
    assert_eq!(ids(&boxes(cx)), ["check", "svc", "report"]);

    // A run shows every step's state and each group's, by qualified id.
    view.update(cx, |view, cx| view.run_all(cx));
    wait_until(cx, "every check", |cx| {
        view.read_with(cx, |view, cx| view.outcome(cx)) == Outcome::Ready
    });
    assert_eq!(
        view.read_with(cx, |view, cx| view.cards(cx)),
        [
            "check",
            "svc",
            "svc/build",
            "svc/inner",
            "svc/inner/x",
            "report"
        ]
        .map(|id| (id.to_owned(), "ready".to_owned()))
    );
    window
        .update(cx, |_, window, _| window.remove_window())
        .unwrap();
    cx.run_until_parked();
    cleanup(root);
}

#[gpui::test]
fn run_all_asks_for_the_inputs_and_the_run_keeps_them(cx: &mut TestAppContext) {
    let root = root("inputs");
    let path = root.join("user").join("deploy.yaml");
    std::fs::write(
        &path,
        "name: Deploy
inputs:
  env: {type: choice, options: [dev, staging, prod]}
  branch: {required: true}
  workers: {type: number, default: 4, max: 64}
steps:
  show: {run: 'echo deploying-${inputs.env}-${inputs.workers}'}
",
    )
    .unwrap();
    let (window, workflows) = open_window(&root, cx);
    let view = open_tab(window, &path, cx);
    cx.run_until_parked();

    // Run All shows the form; nothing runs until it is valid and run.
    view.update(cx, |view, cx| view.run_all(cx));
    cx.run_until_parked();
    let form = view.read_with(cx, |view, _| view.form()).expect("the form");
    assert!(form.read_with(cx, |form, _| form.state().values().is_none()));
    form.update(cx, |form, cx| form.submit(cx));
    cx.run_until_parked();
    assert!(view.read_with(cx, |view, _| view.form().is_some()));
    assert!(workflows.read_with(cx, |workflows, _| workflows.runs.is_empty()));
    form.update(cx, |form, cx| {
        form.type_text(1, "feature", cx);
        form.type_text(2, "8", cx);
        form.choose(0, 2, cx);
    });
    cx.run_until_parked();
    // Run All with the form open is its Run.
    view.update(cx, |view, cx| view.run_all(cx));
    cx.run_until_parked();
    assert!(view.read_with(cx, |view, _| view.form().is_none()));
    let id = workflow_id(&Source::User, &path);
    let last_line = |cx: &mut TestAppContext| {
        workflows.read_with(cx, |workflows, _| {
            workflows.runs[0].run.steps()[0].last_line().to_owned()
        })
    };
    wait_until(cx, "the step's output", |cx| {
        statuses(&workflows, cx) == ["ready"] && last_line(cx) == "deploying-prod-8"
    });
    let summary = |cx: &mut TestAppContext| {
        workflows.read_with(cx, |workflows, _| workflows.runs[0].inputs.summary())
    };
    assert_eq!(summary(cx), "env=prod · branch=feature · workers=8");

    // Restart reuses them.
    let generation = |cx: &mut TestAppContext| {
        workflows.read_with(cx, |workflows, _| {
            workflows.runs[0].run.steps()[0].generation()
        })
    };
    let before = generation(cx);
    workflows.update(cx, |workflows, cx| workflows.restart_step(&id, 0, cx));
    wait_until(cx, "the restarted step", |cx| {
        generation(cx) > before
            && statuses(&workflows, cx) == ["ready"]
            && last_line(cx) == "deploying-prod-8"
    });
    assert_eq!(last_line(cx), "deploying-prod-8");
    assert_eq!(summary(cx), "env=prod · branch=feature · workers=8");

    // The next Run All starts with them; Cancel leaves the run alone.
    view.update(cx, |view, cx| view.run_all(cx));
    cx.run_until_parked();
    let form = view.read_with(cx, |view, _| view.form()).expect("the form");
    assert_eq!(
        form.read_with(cx, |form, _| form.state().values().unwrap().summary()),
        "env=prod · branch=feature · workers=8"
    );
    form.update(cx, |_, cx| cx.emit(super::form::InputFormEvent::Cancelled));
    cx.run_until_parked();
    assert!(view.read_with(cx, |view, _| view.form().is_none()));
    assert_eq!(summary(cx), "env=prod · branch=feature · workers=8");

    window
        .update(cx, |_, window, _| window.remove_window())
        .unwrap();
    cx.run_until_parked();
    cleanup(root);
}

#[gpui::test]
fn an_old_json_file_is_listed_with_a_problem_and_converts_from_its_tab(cx: &mut TestAppContext) {
    let root = root("legacy");
    let old = root.join("user").join("old.jsonc");
    std::fs::write(
        &old,
        r#"// Kept.
{
  "name": "Old",
  "steps": [
    { "id": "a", "check": "exit 0" },
    { "id": "b", "needs": "a", "check": "exit 0", "timeout_s": 90 }
  ]
}
"#,
    )
    .unwrap();
    cx.update(crate::app_state::init);
    let (window, workflows) = open_window(&root, cx);
    workflows
        .update(cx, |workflows, cx| workflows.refresh(cx))
        .detach();
    cx.run_until_parked();
    workflows.read_with(cx, |workflows, _| {
        let found = &workflows.discovered()[0];
        assert_eq!(found.name, "old");
        assert_eq!(found.parsed.as_ref().unwrap_err(), LEGACY_FORMAT);
    });

    // Its tab says so instead of showing a graph.
    let view = window
        .update(cx, |shell, window, cx| {
            super::window::open(shell, &Source::User, &old, window, cx)
        })
        .unwrap()
        .unwrap();
    cx.run_until_parked();
    assert_eq!(
        view.read_with(cx, |view, _| view.problems()),
        [(LEGACY_FORMAT.to_owned(), None)]
    );
    assert!(view.read_with(cx, |view, _| view.columns()).is_empty());

    // Convert to YAML: the new file beside it, the old one kept as
    // `.bak`, and the same tab shows the new file.
    view.update(cx, |view, cx| view.convert(cx));
    cx.run_until_parked();
    let yaml = root.join("user").join("old.yaml");
    let converted = std::fs::read_to_string(&yaml).unwrap();
    // The listing wrote the schema beside the user's workflows, so the
    // converted file points YAML editors at it.
    assert!(root.join("user").join(workflow::SCHEMA_FILE).exists());
    assert!(
        converted.starts_with(&format!(
            "{}\n# Kept.\nname: Old\n",
            workflow::SCHEMA_MODELINE
        )),
        "{converted}"
    );
    assert!(
        converted.contains("b: {depends_on: [a], check: exit 0, timeout: 1m30s}"),
        "{converted}"
    );
    assert!(!old.exists());
    assert!(root.join("user").join("old.jsonc.bak").exists());
    let tabs = window
        .read_with(cx, |shell, cx| shell.tab_summary(cx))
        .unwrap();
    assert_eq!(tabs, (vec!["alpha".into(), "◈ Old".into()], 1));
    let new_view = window
        .update(cx, |shell, window, cx| {
            super::window::open(shell, &Source::User, &yaml, window, cx)
        })
        .unwrap()
        .unwrap();
    assert_ne!(new_view, view);
    assert_eq!(
        window
            .read_with(cx, |shell, cx| shell.tab_summary(cx).0.len())
            .unwrap(),
        2,
        "the converted file's tab is the old one's"
    );
    assert!(new_view.read_with(cx, |view, _| view.problems()).is_empty());
    assert_eq!(
        new_view.read_with(cx, |view, _| view.columns()),
        [vec!["a"], vec!["b"]]
    );
    workflows.read_with(cx, |workflows, _| {
        let names: Vec<&str> = workflows
            .discovered()
            .iter()
            .map(|found| found.name.as_str())
            .collect();
        assert_eq!(names, ["Old"], "the `.bak` file is not listed");
    });
    // Another old file of that name is refused: `old.yaml` exists.
    let again = root.join("user").join("old.json");
    std::fs::write(&again, r#"{"steps": []}"#).unwrap();
    let error = convert_file(
        &Project::local(root.join("user")),
        &again,
        r#"{"steps": []}"#,
    )
    .unwrap_err();
    assert!(error.contains("already exists"), "{error}");
    assert!(again.exists(), "left as it was");
    window
        .update(cx, |_, window, _| window.remove_window())
        .unwrap();
    cx.run_until_parked();
    cleanup(root);
}

#[test]
fn new_workflows_come_with_the_schema_their_first_line_names() {
    let dir = std::env::temp_dir().join(format!("nc-workflows-schema-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    let path = create_template(&dir).unwrap();
    assert_eq!(path, dir.join("workflow.yaml"));
    let schema = dir.join(workflow::SCHEMA_FILE);
    assert_eq!(std::fs::read_to_string(&schema).unwrap(), workflow::SCHEMA);
    assert!(
        std::fs::read_to_string(&path)
            .unwrap()
            .starts_with(workflow::SCHEMA_MODELINE)
    );
    // An outdated copy is replaced; the schema is not a workflow.
    std::fs::write(&schema, "{}").unwrap();
    write_schema(&dir).unwrap();
    assert_eq!(std::fs::read_to_string(&schema).unwrap(), workflow::SCHEMA);
    assert!(!is_workflow_file(workflow::SCHEMA_FILE));
    assert!(is_workflow_file("old.json") && is_workflow_file("dev.yml"));
    std::fs::remove_dir_all(dir).ok();
}

#[gpui::test]
fn the_tab_s_editor_gets_problems_and_completions_from_the_workflow_server(
    cx: &mut TestAppContext,
) {
    let root = root("server");
    // Where the user's workflows are: the server serves that folder.
    let dir = root.join("user").join("next_code").join("workflows");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("dev.yaml");
    let text = "steps:\n  db: {run: d}\n  api: {depends_on: [db], run: a}\n  web:\n    run: w\n";
    std::fs::write(&path, text).unwrap();
    let (window, _) = open_window(&root, cx);
    let view = open_tab(window, &path, cx);
    let editor = view.read_with(cx, |view, _| view.editor());
    wait_until(cx, "the server", |cx| {
        editor.read_with(cx, |editor, _| editor.server_version().is_some())
    });

    // The server's problems show in the editor, where they are.
    let broken = text.replace("[db]", "[zz]");
    editor.update(cx, |editor, cx| editor.set_text(&broken, cx));
    wait_until(cx, "a problem", |cx| {
        !editor
            .read_with(cx, |editor, _| editor.diagnostic_ranges())
            .is_empty()
    });
    let problems = editor.read_with(cx, |editor, _| editor.diagnostic_ranges());
    assert_eq!(problems[0].1, "`api` depends on unknown step `zz`");
    assert_eq!(&broken[problems[0].0.clone()], "zz");

    // Completion of a dependency: the other steps.
    let typed = format!("{broken}    depends_on: [");
    editor.update(cx, |editor, cx| {
        editor.set_text(&typed, cx);
        editor.set_cursor(typed.len(), cx);
        editor.complete_at_cursor(cx);
    });
    wait_until(cx, "completions", |cx| {
        !editor
            .read_with(cx, |editor, _| editor.completion_labels())
            .is_empty()
    });
    assert_eq!(
        editor.read_with(cx, |editor, _| editor.completion_labels()),
        ["db", "api"]
    );
    editor.update(cx, |editor, cx| editor.accept_completion_at(0, cx));
    assert!(
        editor
            .read_with(cx, |editor, _| editor.text())
            .ends_with("depends_on: [db")
    );
    window
        .update(cx, |_, window, _| window.remove_window())
        .unwrap();
    cx.run_until_parked();
    cleanup(root);
}

#[gpui::test]
fn workflows_are_listed_from_extra_folders_and_opened_files(cx: &mut TestAppContext) {
    let root = root("folders");
    let team = root.join("team");
    let elsewhere = root.join("elsewhere");
    for dir in [&team, &elsewhere] {
        std::fs::create_dir_all(dir).unwrap();
    }
    std::fs::write(
        root.join("user").join("mine.yaml"),
        "name: Mine\nsteps: {}\n",
    )
    .unwrap();
    std::fs::write(
        team.join("deploy.yaml"),
        "name: Team deploy\nsteps: {a: {check: exit 0}}\n",
    )
    .unwrap();
    std::fs::write(team.join("notes.txt"), "not a workflow").unwrap();
    let single = elsewhere.join("one.yml");
    std::fs::write(&single, "name: One off\nsteps: {}\n").unwrap();
    let (window, workflows) = open_window(&root, cx);
    workflows.update(cx, |workflows, _| {
        // The user folder listed again as an extra one: once only.
        workflows.folders = Some(vec![team.clone(), root.join("user")]);
    });
    workflows
        .update(cx, |workflows, cx| workflows.refresh(cx))
        .detach();
    cx.run_until_parked();
    let listed = |cx: &mut TestAppContext| {
        workflows.read_with(cx, |workflows, _| {
            workflows
                .discovered()
                .iter()
                .map(|found| (found.name.clone(), found.source.label()))
                .collect::<Vec<_>>()
        })
    };
    assert_eq!(
        listed(cx),
        [
            ("Mine".to_owned(), "User".to_owned()),
            ("Team deploy".to_owned(), team.display().to_string()),
        ]
    );
    // A file opened from anywhere: its tab, and listed from then on.
    let view = window
        .update(cx, |shell, window, cx| {
            super::window::open(
                shell,
                &Source::Folder(elsewhere.clone()),
                &single,
                window,
                cx,
            )
        })
        .unwrap()
        .unwrap();
    cx.run_until_parked();
    assert_eq!(view.read_with(cx, |view, _| view.name()), "One off");
    assert_eq!(
        listed(cx)[2],
        ("One off".to_owned(), elsewhere.display().to_string())
    );
    // A team workflow runs from its folder.
    let id = workflows.read_with(cx, |workflows, _| {
        workflows.lookup("Team deploy").unwrap().id.clone()
    });
    workflows
        .update(cx, |workflows, cx| {
            workflows.start(&id, &Default::default(), cx)
        })
        .unwrap();
    wait_until(cx, "the team workflow", |cx| {
        statuses(&workflows, cx) == ["ready"]
    });
    window
        .update(cx, |_, window, _| window.remove_window())
        .unwrap();
    cx.run_until_parked();
    cleanup(root);
}

/// A host without projects or terminals, for steps that run nothing.
struct NoHost;

impl Host for NoHost {
    fn project(&mut self, location: &Location, _: &mut App) -> Result<Option<Project>, String> {
        Err(format!("no project {location}"))
    }

    fn projects(&mut self, _: &mut App) -> Vec<(Location, Project)> {
        Vec::new()
    }

    fn start(
        &mut self,
        _: &Location,
        _: &Project,
        _: &str,
        _: &str,
        _: &[String],
        _: OutputTap,
        _: &mut App,
    ) -> Result<Entity<TerminalView>, String> {
        Err("no terminals".into())
    }

    fn show(&mut self, _: &Location, _: &Entity<TerminalView>, _: &mut App) {}

    fn open_url(&mut self, _: &str, _: &mut App) {}

    fn open_file(&mut self, _: Option<&Location>, _: &Path, _: &mut App) {}
}

#[gpui::test]
fn ticks_notify_the_tab_only_when_something_it_shows_changed(cx: &mut TestAppContext) {
    let root = root("ticks");
    let dir = root.join("user");
    std::fs::write(
        dir.join("gate.yaml"),
        "name: Gate\nsteps:\n  gate: {ready: {manual: Go on}}\n",
    )
    .unwrap();
    let workflows = cx.new(|cx| Workflows::new(Box::new(NoHost), Some(dir), cx));
    let notifies = std::rc::Rc::new(std::cell::Cell::new(0usize));
    let counted = notifies.clone();
    let _watching =
        cx.update(|cx| cx.observe(&workflows, move |_, _| counted.set(counted.get() + 1)));
    workflows
        .update(cx, |workflows, cx| workflows.refresh(cx))
        .detach();
    cx.run_until_parked();
    let id = workflows.read_with(cx, |workflows, _| workflows.discovered()[0].id.clone());
    workflows.update(cx, |workflows, cx| {
        workflows.start(&id, &Default::default(), cx).unwrap();
    });
    cx.run_until_parked();
    assert_eq!(statuses(&workflows, cx), ["running"]);
    assert!(notifies.get() > 0, "starting notifies");

    // The gate waits for the user: ticks change nothing the tab shows,
    // except the elapsed time once a (real) second.
    notifies.set(0);
    let from = Instant::now();
    for _ in 0..8 {
        cx.executor().advance_clock(TICK);
        cx.run_until_parked();
    }
    let allowed = from.elapsed().as_secs() as usize + 1;
    assert!(
        notifies.get() <= allowed,
        "{} notifies for 8 idle ticks",
        notifies.get()
    );

    // Continue changes the step: that shows.
    notifies.set(0);
    workflows.update(cx, |workflows, cx| {
        assert!(workflows.continue_step(&id, 0, cx));
    });
    cx.run_until_parked();
    assert_eq!(statuses(&workflows, cx), ["ready"]);
    assert!(notifies.get() >= 1, "a state change notifies");
    std::fs::remove_dir_all(root).ok();
}

#[gpui::test]
fn turning_workflows_off_closes_their_tabs_and_a_launch_leaves_them_out(cx: &mut TestAppContext) {
    let root = root("builtin");
    let path = root.join("user").join("diamond.yaml");
    std::fs::write(&path, DIAMOND).unwrap();
    cx.update(crate::app_state::init);
    let store = cx.update(crate::extensions::shared);
    let set = |on: bool, cx: &mut TestAppContext| {
        store.update(cx, |store, cx| {
            store.set_builtin_unsaved("workflows", on, cx)
        });
        cx.run_until_parked();
    };
    set(true, cx);
    let (window, workflows) = open_window(&root, cx);
    let model = workflows.downgrade();
    drop(workflows);
    open_tab(window, &path, cx);
    cx.executor().advance_clock(Duration::from_millis(600));
    cx.run_until_parked();
    let state = crate::app_state::load().unwrap();
    let tabs = |window: WindowHandle<Shell>, cx: &mut TestAppContext| {
        window
            .read_with(cx, |shell, cx| shell.tab_summary(cx).0)
            .unwrap()
    };
    assert_eq!(tabs(window, cx), ["alpha", "◈ Diamond"]);

    // Its tab closes and its model (with any run's commands) goes.
    set(false, cx);
    assert_eq!(tabs(window, cx), ["alpha"]);
    assert!(model.upgrade().is_none());
    assert!(cx.update(|cx| crate::builtins::hidden("workflows::OpenWorkflow", cx)));

    // A launch leaves out the workflow tabs saved while it was on.
    window
        .update(cx, |_, window, _| window.remove_window())
        .unwrap();
    cx.run_until_parked();
    cx.update(|cx| crate::shell::restore(state, None, cx));
    cx.run_until_parked();
    let restored = cx.windows()[0].downcast::<Shell>().unwrap();
    assert_eq!(tabs(restored, cx), ["alpha"]);
    set(true, cx);
    assert!(
        restored
            .read_with(cx, |shell, _| shell.builtin_model::<Workflows>().is_some())
            .unwrap()
    );
    restored
        .update(cx, |_, window, _| window.remove_window())
        .unwrap();
    cx.run_until_parked();
    cleanup(root);
}
