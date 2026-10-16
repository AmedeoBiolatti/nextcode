//! A workflow driven through a real window: a check on this machine, a
//! project tab opened for a step whose command runs in that project's
//! Terminal panel, a manual gate, a command without a project, Restart
//! and Stop All.

use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

use gpui::{TestAppContext, WindowHandle};
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
    let alpha = workspace(window, 0, cx);
    alpha.update(cx, |workspace, cx| workspace.show_workflows(cx));
    workflows
        .update(cx, |workflows, cx| workflows.refresh(cx))
        .detach();
    cx.run_until_parked();
    let id = workflows.read_with(cx, |workflows, _| {
        let found = &workflows.discovered()[0];
        assert_eq!(found.name, "Dev");
        assert_eq!(found.source, Source::User);
        found.id.clone()
    });

    workflows
        .update(cx, |workflows, cx| workflows.run_workflow(id.clone(), cx))
        .detach();
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
    // The panel shows the steps and their states.
    let panel = alpha.read_with(cx, |workspace, _| workspace.workflows_panel());
    assert_eq!(
        panel.read_with(cx, |panel, _| panel.shown.clone()),
        [
            ("check".to_owned(), "ready".to_owned()),
            ("Server".to_owned(), "ready".to_owned()),
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
        assert!(active.terminal(3).is_some(), "shown by the panel");
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
        .update(cx, |workflows, cx| {
            workflows.run_workflow(broken.clone(), cx)
        })
        .detach();
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
