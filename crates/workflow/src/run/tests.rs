use super::*;
use crate::file::parse;

fn run(text: &str) -> Run {
    Run::new(parse(text, "test").unwrap())
}

fn statuses(run: &Run) -> Vec<&'static str> {
    run.steps()
        .iter()
        .map(|state| state.status.name())
        .collect()
}

fn generation(run: &Run, index: usize) -> u64 {
    run.steps()[index].generation()
}

fn secs(seconds: u64) -> Duration {
    Duration::from_secs(seconds)
}

/// The generation step `index`'s command was started with (from its
/// StartCommand effect).
fn started(effects: &[Effect], index: usize) -> u64 {
    effects
        .iter()
        .find_map(|effect| match effect {
            Effect::StartCommand { step, generation } if *step == index => Some(*generation),
            _ => None,
        })
        .expect("a StartCommand")
}

const CHAIN: &str = r#"
steps:
  a: {run: a}
  b: {run: b}
  c: {depends_on: [a, b], run: c}
  d: {depends_on: [c], run: d}
"#;

#[test]
fn independent_steps_start_together_and_needs_wait() {
    let t = Instant::now();
    let mut run = run(CHAIN);
    assert_eq!(run.outcome(), Outcome::Idle);
    assert_eq!(statuses(&run), ["pending"; 4]);
    run.start(t);
    assert_eq!(statuses(&run), ["running", "running", "waiting", "waiting"]);
    assert_eq!(
        run.take_effects(),
        [
            Effect::StartCommand {
                step: 0,
                generation: generation(&run, 0)
            },
            Effect::StartCommand {
                step: 1,
                generation: generation(&run, 1)
            },
        ]
    );
    assert_eq!(run.steps()[2].detail, "needs a, b");
    run.exited(0, generation(&run, 0), Some(0), t);
    assert_eq!(statuses(&run), ["ready", "running", "waiting", "waiting"]);
    assert_eq!(run.steps()[2].detail, "needs b");
    run.exited(1, generation(&run, 1), Some(0), t);
    assert_eq!(statuses(&run), ["ready", "ready", "running", "waiting"]);
    run.exited(2, generation(&run, 2), Some(0), t);
    run.exited(3, generation(&run, 3), Some(0), t);
    assert_eq!(statuses(&run), ["ready"; 4]);
    assert_eq!(run.outcome(), Outcome::Ready);
    let changes: Vec<(usize, &str)> = run
        .take_changes()
        .iter()
        .map(|change| (change.step, change.status.name()))
        .collect();
    assert_eq!(
        &changes[..4],
        [
            (0, "waiting"),
            (1, "waiting"),
            (2, "waiting"),
            (3, "waiting")
        ]
    );
    assert!(changes.contains(&(2, "running")));
    assert_eq!(changes.last(), Some(&(3, "ready")));
}

#[test]
fn projects_open_and_checks_run_before_the_command() {
    let t = Instant::now();
    let mut run =
        run(r#"
steps:
  a: {project: {local: /p}, check: c, run: r}
"#);
    run.start(t);
    let g = generation(&run, 0);
    assert_eq!(
        run.take_effects(),
        [Effect::OpenProject {
            step: 0,
            generation: g
        }]
    );
    assert_eq!(statuses(&run), ["starting"]);
    assert_eq!(run.steps()[0].detail, "opening the project");
    run.project_opened(0, g, t);
    assert_eq!(
        run.take_effects(),
        [Effect::RunCheck {
            step: 0,
            generation: g
        }]
    );
    assert_eq!(run.steps()[0].detail, "checking");
    run.check_finished(
        0,
        g,
        CheckResult::Exited {
            code: Some(0),
            output: String::new(),
        },
        t,
    );
    assert_eq!(
        run.take_effects(),
        [Effect::StartCommand {
            step: 0,
            generation: g
        }]
    );
    assert_eq!(statuses(&run), ["running"]);

    let mut failing =
        self::run(r#"
steps:
  a: {project: {local: /p}, run: r}
"#);
    failing.start(t);
    failing.project_failed(0, generation(&failing, 0), "no such folder", t);
    assert_eq!(
        failing.steps()[0].status,
        Status::Failed("could not open the project: no such folder".into())
    );
    assert_eq!(failing.outcome(), Outcome::Failed);
}

#[test]
fn a_failed_check_with_on_fail_waits_for_retry() {
    let t = Instant::now();
    let mut run = run(r#"
steps:
  vpn: {check: ping, on_fail: Connect the VPN}
  other: {run: x, ready: {port: 80}}
  app: {depends_on: [vpn], run: app}
"#);
    run.start(t);
    run.take_effects();
    run.check_finished(
        0,
        generation(&run, 0),
        CheckResult::Exited {
            code: Some(1),
            output: "Request timed out.\n".into(),
        },
        t,
    );
    assert_eq!(
        run.steps()[0].status,
        Status::Failed("Connect the VPN".into())
    );
    assert_eq!(
        run.steps()[0].check_output.as_deref(),
        Some("Request timed out.")
    );
    assert_eq!(statuses(&run), ["failed", "running", "waiting"]);
    assert_eq!(run.steps()[2].detail, "blocked: vpn did not get ready");
    assert!(run.steps()[2].is_blocked());
    assert_eq!(run.outcome(), Outcome::Running, "`other` still runs");
    assert!(run.take_effects().is_empty(), "nothing else is stopped");

    run.retry_failed(t);
    let g = generation(&run, 0);
    assert_eq!(
        run.take_effects(),
        [Effect::RunCheck {
            step: 0,
            generation: g
        }]
    );
    run.check_finished(
        0,
        g,
        CheckResult::Exited {
            code: Some(0),
            output: String::new(),
        },
        t,
    );
    assert_eq!(statuses(&run), ["ready", "running", "running"]);
}

#[test]
fn a_failed_check_without_on_fail_fails_the_workflow() {
    let t = Instant::now();
    let mut run = run(r#"
steps:
  server: {run: serve, ready: {output: up}}
  docker: {check: docker info}
  app: {depends_on: [docker], run: app}
"#);
    run.start(t);
    let server = started(&run.take_effects(), 0);
    run.check_finished(
        1,
        generation(&run, 1),
        CheckResult::Error("docker not found".into()),
        t,
    );
    assert_eq!(
        run.steps()[1].status,
        Status::Failed("check failed (docker not found)".into())
    );
    assert_eq!(statuses(&run), ["stopping", "failed", "stopped"]);
    assert_eq!(run.take_effects(), [Effect::InterruptCommand { step: 0 }]);
    assert_eq!(run.outcome(), Outcome::Failed);
    assert!(run.is_active(), "the server is still stopping");
    run.exited(0, server, Some(130), t + secs(1));
    assert_eq!(statuses(&run), ["stopped", "failed", "stopped"]);
    assert_eq!(run.outcome(), Outcome::Failed);
    assert!(!run.is_active());
}

#[test]
fn readiness_by_output_port_delay_manual_and_exit() {
    let t = Instant::now();
    let mut run = run(r#"
steps:
  out: {run: s, ready: {output: Listening on (\d+)}}
  port: {run: s, ready: {port: 8000}}
  delay: {run: s, ready: {delay: 500ms}}
  gate: {ready: {manual: Log in}}
  build: {run: make}
"#);
    run.start(t);
    run.output(0, generation(&run, 0), b"\x1b[32mListening", t);
    assert_eq!(run.steps()[0].status, Status::Running);
    run.output(0, generation(&run, 0), b" on\x1b[0m 8080\r\n", t);
    assert_eq!(run.steps()[0].status, Status::Ready);
    assert_eq!(run.steps()[0].last_line(), "Listening on 8080");

    assert_eq!(run.port_watches(), [(1, generation(&run, 1), 8000)]);
    run.port_state(1, generation(&run, 1), false, t);
    assert_eq!(run.steps()[1].status, Status::Running);
    run.port_state(1, generation(&run, 1), true, t);
    assert_eq!(run.steps()[1].status, Status::Ready);
    assert!(run.port_watches().is_empty());

    assert_eq!(run.next_deadline(), Some(t + Duration::from_millis(500)));
    run.tick(t + Duration::from_millis(499));
    assert_eq!(run.steps()[2].status, Status::Running);
    run.tick(t + Duration::from_millis(500));
    assert_eq!(run.steps()[2].status, Status::Ready);

    assert_eq!(run.steps()[3].detail, "Log in");
    assert!(run.awaits_user(3));
    assert!(!run.awaits_user(0) && !run.awaits_user(4));
    assert!(!run.continue_step(0, t), "not a manual step");
    assert!(run.continue_step(3, t));
    assert!(!run.awaits_user(3));
    assert!(!run.continue_step(3, t), "already continued");
    assert_eq!(run.steps()[3].status, Status::Ready);

    run.exited(4, generation(&run, 4), Some(2), t);
    assert_eq!(
        run.steps()[4].status,
        Status::Failed("failed (exit code 2)".into())
    );
    assert_eq!(run.outcome(), Outcome::Failed, "finished with a failure");
    // Exiting before readiness fails; exiting after it is only noted.
    run.exited(0, generation(&run, 0), Some(1), t);
    assert_eq!(run.steps()[0].status, Status::Ready);
    assert_eq!(run.steps()[0].exit, Some(Some(1)));
    // Restarting the port step: its server is stopped first.
    let old = generation(&run, 1);
    run.take_effects();
    run.restart(1, t);
    assert_eq!(run.outcome(), Outcome::Running);
    assert_eq!(run.take_effects(), [Effect::InterruptCommand { step: 1 }]);
    run.exited(1, old, Some(130), t);
    let new = started(&run.take_effects(), 1);
    run.exited(1, new, Some(0), t);
    assert_eq!(
        run.steps()[1].status,
        Status::Failed("exited before it was ready (exit code 0)".into())
    );
}

#[test]
fn timeouts_stop_the_command_then_fail_the_step() {
    let t = Instant::now();
    let mut run = run(r#"
steps:
  slow: {run: s, ready: {output: never}, timeout: 10s}
  next: {depends_on: [slow], run: x}
  deaf: {run: s, ready: {output: never}, timeout: 10s, stop_grace_period: 5s}
"#);
    run.start(t);
    let effects = run.take_effects();
    let (slow, deaf) = (started(&effects, 0), started(&effects, 2));
    assert_eq!(run.next_deadline(), Some(t + secs(10)));
    run.tick(t + secs(9));
    assert_eq!(statuses(&run), ["running", "waiting", "running"]);
    run.tick(t + secs(10));
    assert_eq!(statuses(&run), ["stopping", "waiting", "stopping"]);
    assert_eq!(
        run.steps()[0].detail,
        "timed out after 10s; stopping (3 s left)"
    );
    assert_eq!(
        run.take_effects(),
        [
            Effect::InterruptCommand { step: 0 },
            Effect::InterruptCommand { step: 2 }
        ]
    );
    assert_eq!(run.steps()[1].detail, "needs slow", "not blocked yet");
    // `slow` exits within its grace: failed with the timeout's reason.
    run.output(0, slow, b"saving...\n", t + secs(11));
    assert_eq!(
        run.steps()[0].last_line(),
        "saving...",
        "output while stopping"
    );
    run.exited(0, slow, Some(130), t + secs(11));
    assert_eq!(
        run.steps()[0].status,
        Status::Failed("timed out after 10s".into())
    );
    assert_eq!(run.steps()[0].elapsed(t + secs(60)), Some(secs(11)));
    assert_eq!(run.steps()[1].detail, "blocked: slow did not get ready");
    // `deaf` ignores Ctrl+C: killed after its 5 s.
    assert_eq!(run.next_deadline(), Some(t + secs(15)));
    run.tick(t + Duration::from_millis(12_500));
    assert_eq!(
        run.steps()[2].detail,
        "timed out after 10s; stopping (3 s left)"
    );
    run.tick(t + secs(15));
    assert_eq!(run.take_effects(), [Effect::KillCommand { step: 2 }]);
    assert_eq!(
        run.steps()[2].status,
        Status::Failed("timed out after 10s".into())
    );
    run.exited(2, deaf, None, t + secs(15));
    assert_eq!(
        run.steps()[2].exit,
        None,
        "the kill's exit is not waited for"
    );
    assert_eq!(run.outcome(), Outcome::Failed);
}

#[test]
fn stop_all_gives_commands_their_grace_and_a_second_stop_kills() {
    let t = Instant::now();
    let mut run = run(r#"
steps:
  server: {run: s, ready: {port: 80}}
  build: {run: make}
  eval: {depends_on: [build], run: x, stop_grace_period: 1m}
  deaf: {run: y, ready: {output: never}}
"#);
    run.start(t);
    let mut effects = run.take_effects();
    let server = started(&effects, 0);
    let deaf = started(&effects, 3);
    run.port_state(0, server, true, t);
    run.exited(1, generation(&run, 1), Some(0), t);
    effects = run.take_effects();
    let eval = started(&effects, 2);
    assert_eq!(statuses(&run), ["ready", "ready", "running", "running"]);
    run.stop_all(t);
    // The ready server still ran: stopped too; the build had exited.
    assert_eq!(
        statuses(&run),
        ["stopping", "ready", "stopping", "stopping"]
    );
    assert_eq!(
        run.take_effects(),
        [
            Effect::InterruptCommand { step: 0 },
            Effect::InterruptCommand { step: 2 },
            Effect::InterruptCommand { step: 3 }
        ]
    );
    assert_eq!(run.steps()[2].detail, "stopping (60 s left)");
    assert_eq!(run.steps()[2].stop_deadline(), Some(t + secs(60)));
    assert_eq!(run.outcome(), Outcome::Stopped);
    assert!(run.is_active());
    // Exits during the grace end it early; output still shows.
    run.exited(0, server, Some(130), t + secs(1));
    run.output(2, eval, b"checkpoint written\n", t + secs(2));
    run.exited(2, eval, Some(1), t + secs(2));
    assert_eq!(statuses(&run), ["stopped", "ready", "stopped", "stopping"]);
    assert_eq!(run.steps()[2].last_line(), "checkpoint written");
    assert_eq!(run.steps()[2].exit, Some(Some(1)));
    // A second Stop All kills what still stops.
    run.tick(t + secs(2));
    assert_eq!(run.steps()[3].detail, "stopping (1 s left)");
    run.stop_all(t + secs(2));
    assert_eq!(run.take_effects(), [Effect::KillCommand { step: 3 }]);
    assert_eq!(statuses(&run), ["stopped", "ready", "stopped", "stopped"]);
    assert!(!run.is_active());
    // Reports from the stopped commands change nothing.
    run.exited(3, deaf, Some(137), t + secs(3));
    run.output(2, eval, b"late\n", t + secs(3));
    assert_eq!(statuses(&run), ["stopped", "ready", "stopped", "stopped"]);
    assert_eq!(run.steps()[2].last_line(), "checkpoint written");
    assert_eq!(run.outcome(), Outcome::Stopped);

    // Run All again starts everything afresh.
    run.start(t);
    assert_eq!(statuses(&run), ["running", "running", "waiting", "running"]);
    assert_eq!(run.outcome(), Outcome::Running);
}

#[test]
fn restart_and_run_all_wait_for_the_old_command_to_end() {
    let t = Instant::now();
    let mut run = run(r#"
steps:
  a: {run: s, ready: {output: up}}
  b: {depends_on: [a], run: t, ready: {output: up}}
"#);
    run.start(t);
    let first = started(&run.take_effects(), 0);
    run.restart(0, t);
    assert_eq!(statuses(&run), ["stopping", "waiting"]);
    assert_eq!(run.steps()[0].detail, "restarting; stopping (3 s left)");
    assert_eq!(run.take_effects(), [Effect::InterruptCommand { step: 0 }]);
    // Its output while it ends does not make it ready.
    run.output(0, first, b"up\n", t);
    assert_eq!(statuses(&run), ["stopping", "waiting"]);
    // Gone: the replacement starts in the same terminal.
    run.exited(0, first, Some(1), t + secs(1));
    let second = started(&run.take_effects(), 0);
    assert_ne!(first, second);
    assert_eq!(statuses(&run), ["running", "waiting"]);
    run.output(0, second, b"up\n", t);
    assert_eq!(statuses(&run), ["ready", "running"]);
    let b = started(&run.take_effects(), 1);

    // Run All with both running: each is killed after its grace at the
    // latest, then starts again.
    run.start(t + secs(2));
    assert_eq!(statuses(&run), ["stopping", "stopping"]);
    run.exited(1, b, Some(0), t + secs(3));
    assert_eq!(statuses(&run), ["stopping", "waiting"]);
    assert_eq!(run.steps()[1].detail, "needs a");
    run.tick(t + secs(5));
    let effects = run.take_effects();
    assert_eq!(
        effects[..2],
        [
            Effect::InterruptCommand { step: 0 },
            Effect::InterruptCommand { step: 1 }
        ]
    );
    assert_eq!(effects[2], Effect::KillCommand { step: 0 });
    assert_eq!(started(&effects, 0), generation(&run, 0));
    assert_eq!(statuses(&run), ["running", "waiting"]);
    // Restart while stopping just restarts after.
    run.restart(0, t + secs(6));
    run.restart(0, t + secs(6));
    assert_eq!(run.take_effects(), [Effect::InterruptCommand { step: 0 }]);
}

#[test]
fn held_steps_wait_for_the_previous_runs_command() {
    let t = Instant::now();
    let mut run = run(r#"
steps:
  a: {run: s}
  b: {run: t}
"#);
    run.hold(0);
    run.start(t);
    assert_eq!(statuses(&run), ["waiting", "running"]);
    assert_eq!(
        run.steps()[0].detail,
        "waiting for its previous command to end"
    );
    assert!(run.is_held(0));
    run.release(0, t);
    assert_eq!(statuses(&run), ["running", "running"]);
    assert!(!run.is_held(0));
}

const EVAL: &str = r#"
steps:
  eval:
    run: python eval.py
    stop_grace_period: 30s
    artifacts: [{label: metrics, path: out/metrics.json}, 'https://board/${env:RUN}']

  report:
    depends_on: {eval: completed}
    run: report --code ${steps.eval.exit_code} --status ${steps.eval.status} --in ${steps.eval.artifacts.metrics}
    env: {BOARD: '${steps.eval.artifacts.1}'}

  publish: {depends_on: [eval], run: publish}
"#;

fn checked(paths: &[(&str, bool)]) -> Result<Vec<ArtifactState>, String> {
    Ok(paths
        .iter()
        .map(|&(resolved, exists)| ArtifactState {
            resolved: resolved.into(),
            exists,
        })
        .collect())
}

#[test]
fn after_runs_once_the_step_failed_with_its_results() {
    let t = Instant::now();
    let mut run = run(EVAL);
    run.start(t);
    let eval = started(&run.take_effects(), 0);
    assert_eq!(statuses(&run), ["running", "waiting", "waiting"]);
    assert_eq!(run.steps()[1].detail, "after eval");
    run.exited(0, eval, Some(1), t + secs(5));
    // Its artifacts are checked first: the steps waiting for it wait.
    let g = generation(&run, 0);
    assert_eq!(
        run.take_effects(),
        [Effect::CheckArtifacts {
            step: 0,
            generation: g
        }]
    );
    assert_eq!(statuses(&run), ["failed", "waiting", "waiting"]);
    assert_eq!(run.outcome(), Outcome::Running);
    run.artifacts_checked(
        0,
        g,
        checked(&[("/w/out/metrics.json", true), ("https://board/7", true)]),
        t + secs(5),
    );
    assert_eq!(statuses(&run), ["failed", "running", "waiting"]);
    assert_eq!(run.steps()[2].detail, "blocked: eval did not get ready");
    let report = run.definition(1);
    assert_eq!(
        report.run.as_deref(),
        Some("report --code 1 --status failed --in /w/out/metrics.json")
    );
    assert_eq!(report.env["BOARD"], "https://board/7");
    assert_eq!(
        run.workflow().steps[1].env["BOARD"],
        "${steps.eval.artifacts.1}",
        "the file's definition stays"
    );
    let report = started(&run.take_effects(), 1);
    run.exited(1, report, Some(0), t + secs(6));
    // Nothing else can run: finished with failures, the failure kept.
    assert_eq!(statuses(&run), ["failed", "ready", "waiting"]);
    assert_eq!(run.outcome(), Outcome::Failed);
    assert_eq!(
        run.steps()[0].status,
        Status::Failed("failed (exit code 1)".into())
    );
    // Restart goes back to running.
    run.restart(0, t + secs(7));
    assert_eq!(run.outcome(), Outcome::Running);
    assert_eq!(statuses(&run), ["running", "ready", "waiting"]);
}

#[test]
fn after_a_stopped_blocked_or_retry_waiting_step_is_blocked() {
    let t = Instant::now();
    let mut run = run(r#"
steps:
  eval: {run: e}
  report: {depends_on: {eval: completed}, run: r}
  gate: {check: c, on_fail: Fix it}
  late: {depends_on: [gate], run: l}
  tidy: {depends_on: {late: completed, gate: completed}, run: t}
"#);
    run.start(t);
    let eval = started(&run.take_effects(), 0);
    let fail_check = |run: &mut Run, code| {
        run.check_finished(
            2,
            generation(run, 2),
            CheckResult::Exited {
                code: Some(code),
                output: String::new(),
            },
            t,
        )
    };
    fail_check(&mut run, 1);
    assert_eq!(run.steps()[3].detail, "blocked: gate did not get ready");
    // Blocked through a blocked step, and by one waiting for Retry.
    assert_eq!(
        run.steps()[4].detail,
        "blocked: late can never run; gate waits for Retry"
    );
    assert_eq!(run.outcome(), Outcome::Running, "eval still runs");
    run.exited(0, eval, Some(0), t);
    let report = started(&run.take_effects(), 1);
    run.exited(1, report, Some(0), t);
    // Only the human can move it on: failed, with the reason shown.
    assert_eq!(
        statuses(&run),
        ["ready", "ready", "failed", "waiting", "waiting"]
    );
    assert_eq!(run.outcome(), Outcome::Failed);
    run.retry_failed(t);
    assert_eq!(run.outcome(), Outcome::Running);
    fail_check(&mut run, 0);
    assert_eq!(run.steps()[4].detail, "after late");

    // After a stopped step: blocked (here after Stop All and a Restart of
    // the waiting step).
    let mut single = self::run(
        r#"
steps:
  eval: {run: e}
  report: {depends_on: {eval: completed}, run: r}
"#,
    );
    single.start(t);
    let eval = started(&single.take_effects(), 0);
    single.stop_all(t);
    single.exited(0, eval, Some(130), t);
    assert_eq!(statuses(&single), ["stopped", "stopped"]);
    single.restart(1, t);
    assert_eq!(single.steps()[1].detail, "blocked: eval was stopped");
    assert!(single.steps()[1].is_blocked());
    assert_eq!(single.outcome(), Outcome::Failed);
}

#[test]
fn failed_step_variables_fail_the_step_that_reads_them() {
    let t = Instant::now();
    let mut run = run(EVAL);
    run.start(t);
    let eval = started(&run.take_effects(), 0);
    run.exited(0, eval, Some(0), t);
    run.artifacts_checked(
        0,
        generation(&run, 0),
        Err("could not read the environment".into()),
        t,
    );
    assert_eq!(
        run.steps()[1].status,
        Status::Failed(
            "`${steps.eval.artifacts.metrics}`: the artifacts of `eval` could not be resolved: \
             could not read the environment"
                .into()
        )
    );
    assert_eq!(run.steps()[2].status, Status::Running, "needs only");
    // On demand: checked again.
    assert!(run.check_artifacts(0));
    assert!(!run.check_artifacts(2), "no artifacts");
    assert!(matches!(
        run.take_effects().last(),
        Some(Effect::CheckArtifacts { step: 0, .. })
    ));
}

#[test]
fn a_step_that_never_got_where_it_runs_has_no_artifacts_to_check() {
    let t = Instant::now();
    let mut run = run(r#"
steps:
  eval: {project: {local: /p}, run: e, artifacts: [x]}
  report: {depends_on: {eval: completed}, run: 'r ${steps.eval.exit_code}'}
"#);
    run.start(t);
    run.take_effects();
    run.project_failed(0, generation(&run, 0), "gone", t);
    assert!(
        run.take_effects()
            .iter()
            .all(|effect| !matches!(effect, Effect::CheckArtifacts { .. }))
    );
    assert_eq!(run.definition(1).run.as_deref(), Some("r "), "no exit code");
    assert_eq!(statuses(&run), ["failed", "running"]);
}

#[test]
fn urls_open_once_until_the_step_is_ready() {
    let t = Instant::now();
    let mut run = run(r#"
steps:
  web: {run: s, open_urls: true, ready: {output: compiled}}
  quiet: {run: s, ready: {output: x}}
"#);
    run.start(t);
    run.take_effects();
    let g = generation(&run, 0);
    run.output(0, g, b"Local: http://localhost:5173/\r\n", t);
    run.output(
        0,
        g,
        b"Local: http://localhost:5173/\r\nLogin: https://auth.example/x\n",
        t,
    );
    run.output(1, generation(&run, 1), b"http://ignored/\n", t);
    assert_eq!(
        run.take_effects(),
        [
            Effect::OpenUrl {
                step: 0,
                url: "http://localhost:5173/".into()
            },
            Effect::OpenUrl {
                step: 0,
                url: "https://auth.example/x".into()
            },
        ]
    );
    // The line right after the one that made it ready still counts…
    run.output(0, g, b"compiled\nhttp://after.ready/\n", t);
    assert_eq!(run.steps()[0].status, Status::Ready);
    assert_eq!(
        run.take_effects(),
        [Effect::OpenUrl {
            step: 0,
            url: "http://after.ready/".into()
        }]
    );
    // …later output does not.
    run.output(
        0,
        g,
        b"http://later/\n",
        t + URL_GRACE + Duration::from_millis(1),
    );
    assert!(run.take_effects().is_empty());
}

#[test]
fn a_chain_with_a_failure_ends_failed_once_the_last_step_finished() {
    let t = Instant::now();
    let mut run = run(r#"
steps:
  prepare: {run: p}
  eval: {depends_on: [prepare], run: e, artifacts: [{label: m, path: m}]}
  report: {depends_on: {eval: completed}, run: 'r ${steps.eval.artifacts.m}', artifacts: [r]}
"#);
    run.start(t);
    let p = started(&run.take_effects(), 0);
    run.exited(0, p, Some(0), t);
    let e = started(&run.take_effects(), 1);
    run.exited(1, e, Some(1), t);
    run.take_effects();
    run.artifacts_checked(1, generation(&run, 1), checked(&[("/m", true)]), t);
    let r = started(&run.take_effects(), 2);
    run.exited(2, r, Some(0), t);
    assert_eq!(statuses(&run), ["ready", "failed", "ready"]);
    assert_eq!(
        run.definition(2).run.as_deref(),
        Some("r /m"),
        "the report read the evaluation's artifact"
    );
    assert_eq!(run.outcome(), Outcome::Failed);
}

const SERVICE: &str = r#"
steps:
  db: {run: d}

  svc:
    depends_on: [db]
    steps:
      build: {run: b}
      migrate: {depends_on: [build], run: m}
      lint: {run: l}

  web: {depends_on: [svc], run: w}
  report: {depends_on: {svc: completed}, run: 'r ${steps.svc.status}'}
"#;

/// Exits step `index`'s command (started by an effect already taken).
fn exit(run: &mut Run, index: usize, code: i32, t: Instant) {
    let generation = generation(run, index);
    run.exited(index, generation, Some(code), t);
}

#[test]
fn a_group_gates_its_steps_and_takes_its_state_from_them() {
    let t = Instant::now();
    let mut run = run(SERVICE);
    let paths: Vec<&str> = run.nodes().iter().map(|node| node.path.as_str()).collect();
    assert_eq!(
        paths,
        [
            "db",
            "svc",
            "svc/build",
            "svc/migrate",
            "svc/lint",
            "web",
            "report"
        ]
    );
    assert_eq!(run.step("svc/lint"), Some(4));
    assert_eq!(run.step("lint"), None);
    assert_eq!(statuses(&run), ["pending"; 7]);
    run.start(t);
    assert_eq!(
        statuses(&run),
        [
            "running", "waiting", "waiting", "waiting", "waiting", "waiting", "waiting"
        ]
    );
    assert_eq!(run.steps()[1].detail, "needs db");
    assert_eq!(
        run.steps()[2].detail,
        "needs db",
        "the group's needs gate it"
    );
    assert_eq!(run.steps()[3].detail, "needs build, db");
    assert_eq!(run.steps()[5].detail, "needs svc");
    assert_eq!(run.steps()[6].detail, "after svc");
    run.take_effects();
    exit(&mut run, 0, 0, t);
    assert_eq!(
        statuses(&run),
        [
            "ready", "running", "running", "waiting", "running", "waiting", "waiting"
        ]
    );
    assert_eq!(run.steps()[1].detail, "0 of 3 ready");
    assert!(run.steps()[1].started.is_some());
    exit(&mut run, 2, 0, t);
    exit(&mut run, 4, 1, t);
    assert_eq!(run.steps()[1].status, Status::Running, "migrate still runs");
    exit(&mut run, 3, 0, t);
    assert_eq!(
        run.steps()[1].status,
        Status::Failed("1 of 3 steps failed".into())
    );
    assert!(run.steps()[1].finished.is_some());
    // Outside steps wait for the group like for a step.
    assert_eq!(run.steps()[5].detail, "blocked: svc did not get ready");
    assert_eq!(run.steps()[6].status, Status::Running);
    assert_eq!(run.definition(6).run.as_deref(), Some("r failed"));
    exit(&mut run, 6, 0, t);
    assert_eq!(run.outcome(), Outcome::Failed);
    let changes: Vec<(usize, &str)> = run
        .take_changes()
        .iter()
        .map(|change| (change.step, change.status.name()))
        .filter(|(step, _)| *step == 1)
        .collect();
    assert_eq!(changes, [(1, "waiting"), (1, "running"), (1, "failed")]);

    // Retry on the group: its failed steps only.
    assert!(!run.retry(2, t), "build did not fail");
    assert!(run.retry(1, t));
    assert_eq!(
        statuses(&run),
        [
            "ready", "running", "ready", "ready", "running", "waiting", "ready"
        ]
    );
    assert_eq!(run.steps()[5].detail, "needs svc");
    assert_eq!(run.outcome(), Outcome::Running);
    exit(&mut run, 4, 0, t);
    assert_eq!(run.steps()[1].status, Status::Ready);
    assert_eq!(run.steps()[5].status, Status::Running);
    exit(&mut run, 5, 0, t);
    assert_eq!(run.outcome(), Outcome::Ready);

    // Restart on the group: every step in it.
    run.take_effects();
    run.restart(1, t);
    assert_eq!(
        statuses(&run),
        [
            "ready", "running", "running", "waiting", "running", "ready", "ready"
        ]
    );
    let effects = run.take_effects();
    assert_eq!(started(&effects, 2), generation(&run, 2));
    assert_eq!(started(&effects, 4), generation(&run, 4));
    assert_eq!(run.outcome(), Outcome::Running);
}

#[test]
fn groups_stop_and_block_like_steps() {
    let t = Instant::now();
    let mut run = run(r#"
steps:
  g:
    steps:
      a: {run: a}
      b: {depends_on: [a], run: b}

  z: {depends_on: {g: completed}, run: z}
"#);
    run.start(t);
    run.take_effects();
    assert_eq!(statuses(&run), ["running", "running", "waiting", "waiting"]);
    run.stop_all(t);
    assert_eq!(
        statuses(&run),
        ["stopping", "stopping", "stopped", "stopped"]
    );
    assert_eq!(run.take_effects(), [Effect::InterruptCommand { step: 1 }]);
    run.exited(1, generation(&run, 1) - 1, Some(130), t);
    assert_eq!(statuses(&run), ["stopped"; 4]);
    assert_eq!(run.outcome(), Outcome::Stopped);

    // A group whose need can never get ready: blocked, and so is what
    // runs after it.
    let mut run = run_blocked();
    run.start(t);
    let g = generation(&run, 0);
    run.check_finished(
        0,
        g,
        CheckResult::Exited {
            code: Some(1),
            output: String::new(),
        },
        t,
    );
    assert_eq!(statuses(&run), ["failed", "waiting", "waiting", "waiting"]);
    assert!(run.steps()[1].is_blocked());
    assert_eq!(run.steps()[1].detail, "blocked: vpn did not get ready");
    assert_eq!(run.steps()[3].detail, "blocked: g can never run");
    assert_eq!(run.outcome(), Outcome::Failed);
    run.retry_failed(t);
    assert_eq!(
        statuses(&run),
        ["starting", "waiting", "waiting", "waiting"]
    );
    assert!(!run.steps()[1].is_blocked());
    assert_eq!(run.steps()[1].detail, "needs vpn");
    assert_eq!(run.steps()[3].detail, "after g");
}

fn run_blocked() -> Run {
    run(r#"
steps:
  vpn: {check: c, on_fail: Connect}

  g:
    depends_on: [vpn]
    steps:
      a: {run: a}

  z: {depends_on: {g: completed}, run: z}
"#)
}

#[test]
fn steps_in_groups_inherit_and_read_what_their_groups_wait_for() {
    let t = Instant::now();
    let mut run = run(r#"
steps:
  eval: {run: e}

  g:
    depends_on: {eval: completed}
    project: {local: /p}
    env: {K: group, L: l}
    steps:
      inner:
        steps:
          use: {env: {K: own}, run: 'u ${steps.eval.exit_code} ${steps.eval.status}'}
"#);
    run.start(t);
    run.take_effects();
    assert_eq!(run.steps()[3].detail, "after eval");
    exit(&mut run, 0, 3, t);
    assert_eq!(
        run.take_effects(),
        [Effect::OpenProject {
            step: 3,
            generation: generation(&run, 3)
        }],
        "the group's project"
    );
    let use_step = run.definition(3);
    assert_eq!(use_step.run.as_deref(), Some("u 3 failed"));
    assert_eq!(use_step.env["K"], "own");
    assert_eq!(use_step.env["L"], "l");
    assert_eq!(
        statuses(&run),
        ["failed", "running", "running", "starting"],
        "groups run while a step in them starts"
    );
}
