//! One run of a workflow as a state machine, without I/O: the caller
//! performs the [`Effect`]s (open a project, run a check, start a command,
//! stop one, open a URL) and reports back what happened (project opened,
//! check result, output, exit, listening ports, clock ticks). Every report
//! carries the step's generation from the effect, so results of a step
//! that was restarted or stopped since are ignored.
//!
//! Per step: Pending → Waiting (for `needs`) → Starting (opening the
//! project, running the check) → Running (the command runs; not ready
//! yet) → Ready | Failed | Stopped.

use std::time::{Duration, Instant};

use crate::{
    file::{Ready, Workflow},
    output::OutputWatcher,
};

/// How long after a step got ready the URLs it prints still open.
pub const URL_GRACE: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// The workflow has not started it.
    Pending,
    /// Waiting for the steps it needs.
    Waiting,
    /// Opening its project or running its check.
    Starting,
    /// Its command runs (or it waits for its port, delay or Continue).
    Running,
    Ready,
    Failed(String),
    Stopped,
}

impl Status {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Waiting => "waiting",
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Ready => "ready",
            Self::Failed(_) => "failed",
            Self::Stopped => "stopped",
        }
    }

    /// Waiting, starting or running.
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Waiting | Self::Starting | Self::Running)
    }
}

/// What the caller must do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// Open the step's project (reuse an open one); answer with
    /// [`Run::project_opened`] or [`Run::project_failed`].
    OpenProject {
        step: usize,
        generation: u64,
    },
    /// Run the step's check; answer with [`Run::check_finished`].
    RunCheck {
        step: usize,
        generation: u64,
    },
    /// Start the step's command in its terminal (replacing what ran there);
    /// report [`Run::output`] and [`Run::exited`], or [`Run::start_failed`].
    StartCommand {
        step: usize,
        generation: u64,
    },
    /// Interrupt the step's command, and kill it if it does not exit soon.
    StopCommand {
        step: usize,
    },
    OpenUrl {
        step: usize,
        url: String,
    },
}

/// A step's status change, for listeners.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub step: usize,
    pub status: Status,
}

/// How the whole run stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Not started.
    Idle,
    Running,
    /// Every step is ready.
    Ready,
    /// A check without `onFail` failed: everything was stopped.
    Failed,
    /// Stop All.
    Stopped,
}

impl Outcome {
    pub fn name(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Running => "running",
            Self::Ready => "ready",
            Self::Failed => "failed",
            Self::Stopped => "stopped",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Idle,
    Project,
    Check,
    /// Readiness pending; `command` tells whether a command was started.
    Waiting,
}

#[derive(Debug)]
pub struct StepState {
    pub status: Status,
    /// What it waits for or does right now, for display.
    pub detail: String,
    /// When it left Waiting (elapsed time and timeouts count from here).
    pub started: Option<Instant>,
    /// When it became ready, failed or stopped.
    pub finished: Option<Instant>,
    /// The command's exit code once it exited (`Some(None)`: unknown).
    pub exit: Option<Option<i32>>,
    /// The check's last output line when it failed.
    pub check_output: Option<String>,
    phase: Phase,
    generation: u64,
    /// A command was started and has not exited (as far as reported).
    command_live: bool,
    ready_at: Option<Instant>,
    watcher: OutputWatcher,
}

impl StepState {
    fn new() -> Self {
        Self {
            status: Status::Pending,
            detail: String::new(),
            started: None,
            finished: None,
            exit: None,
            check_output: None,
            phase: Phase::Idle,
            generation: 0,
            command_live: false,
            ready_at: None,
            watcher: OutputWatcher::new(None),
        }
    }

    /// Time since it started, frozen when it finished.
    pub fn elapsed(&self, now: Instant) -> Option<Duration> {
        let started = self.started?;
        Some(
            self.finished
                .unwrap_or(now)
                .saturating_duration_since(started),
        )
    }

    /// The command's last output line.
    pub fn last_line(&self) -> &str {
        self.watcher.last_line()
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Whether its command is (as far as known) still running.
    pub fn command_live(&self) -> bool {
        self.command_live
    }
}

/// Answer of a check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckResult {
    /// It ran: exit code (`None`: killed) and its output.
    Exited { code: Option<i32>, output: String },
    /// It could not run (or timed out).
    Error(String),
}

pub struct Run {
    workflow: Workflow,
    steps: Vec<StepState>,
    effects: Vec<Effect>,
    changes: Vec<Change>,
    outcome: Outcome,
}

impl Run {
    pub fn new(workflow: Workflow) -> Self {
        let steps = workflow.steps.iter().map(|_| StepState::new()).collect();
        Self {
            workflow,
            steps,
            effects: Vec::new(),
            changes: Vec::new(),
            outcome: Outcome::Idle,
        }
    }

    pub fn workflow(&self) -> &Workflow {
        &self.workflow
    }

    pub fn steps(&self) -> &[StepState] {
        &self.steps
    }

    pub fn step(&self, id: &str) -> Option<usize> {
        self.workflow.steps.iter().position(|step| step.id == id)
    }

    pub fn outcome(&self) -> Outcome {
        self.outcome
    }

    /// Whether anything is waiting, starting or running.
    pub fn is_active(&self) -> bool {
        self.steps.iter().any(|step| step.status.is_active())
    }

    /// Effects to perform, in order.
    pub fn take_effects(&mut self) -> Vec<Effect> {
        std::mem::take(&mut self.effects)
    }

    /// Status changes since the last call, in order.
    pub fn take_changes(&mut self) -> Vec<Change> {
        std::mem::take(&mut self.changes)
    }

    /// Steps waiting for a port: `(step, generation, port)`.
    pub fn port_watches(&self) -> Vec<(usize, u64, u16)> {
        self.steps
            .iter()
            .enumerate()
            .filter_map(|(index, state)| match self.workflow.steps[index].ready {
                Ready::Port(port)
                    if state.status == Status::Running && state.phase == Phase::Waiting =>
                {
                    Some((index, state.generation, port))
                }
                _ => None,
            })
            .collect()
    }

    /// Run All: stops what runs, then starts every step afresh.
    pub fn start(&mut self, now: Instant) {
        self.stop_all(now);
        for index in 0..self.steps.len() {
            self.reset(index, now);
        }
        self.outcome = Outcome::Running;
        self.advance(now);
    }

    /// Stop All: interrupts every command the run started; unfinished
    /// steps (and ready ones whose command still runs) become Stopped.
    pub fn stop_all(&mut self, now: Instant) {
        let mut stopped = false;
        for index in 0..self.steps.len() {
            let state = &self.steps[index];
            if state.status.is_active() || (state.status == Status::Ready && state.command_live) {
                self.stop_step(index, now);
                stopped = true;
            }
        }
        if stopped || self.outcome == Outcome::Running || self.outcome == Outcome::Ready {
            self.outcome = Outcome::Stopped;
        }
    }

    fn stop_step(&mut self, index: usize, now: Instant) {
        let state = &mut self.steps[index];
        state.generation += 1;
        state.phase = Phase::Idle;
        state.ready_at = None;
        state.detail.clear();
        if state.command_live {
            state.command_live = false;
            self.effects.push(Effect::StopCommand { step: index });
        }
        self.set_status(index, Status::Stopped, now);
    }

    /// Restarts one step (whatever its state): it waits for its needs
    /// again, then starts; its command replaces the previous one.
    pub fn restart(&mut self, index: usize, now: Instant) {
        if index >= self.steps.len() {
            return;
        }
        self.reset(index, now);
        if self.outcome != Outcome::Running {
            self.outcome = Outcome::Running;
        }
        self.advance(now);
    }

    /// Retry: every failed step starts again.
    pub fn retry_failed(&mut self, now: Instant) {
        let failed: Vec<usize> = (0..self.steps.len())
            .filter(|&index| matches!(self.steps[index].status, Status::Failed(_)))
            .collect();
        for &index in &failed {
            self.reset(index, now);
        }
        if !failed.is_empty() {
            self.outcome = Outcome::Running;
            self.advance(now);
        }
    }

    /// Continue on a step waiting for the user; false if it was not.
    pub fn continue_step(&mut self, index: usize, now: Instant) -> bool {
        let waiting =
            self.steps.get(index).is_some_and(|state| {
                state.status == Status::Running && state.phase == Phase::Waiting
            }) && matches!(self.workflow.steps[index].ready, Ready::Manual(_));
        if waiting {
            self.mark_ready(index, now);
            self.advance(now);
        }
        waiting
    }

    /// Back to Waiting with a new generation; a live command is stopped
    /// (the next start replaces its terminal).
    fn reset(&mut self, index: usize, now: Instant) {
        let step = &self.workflow.steps[index];
        let pattern = match &step.ready {
            Ready::Output(pattern) => Some(pattern.as_str()),
            _ => None,
        };
        let state = &mut self.steps[index];
        state.generation += 1;
        state.phase = Phase::Idle;
        state.started = None;
        state.finished = None;
        state.exit = None;
        state.check_output = None;
        state.ready_at = None;
        state.detail.clear();
        state.watcher = OutputWatcher::new(pattern);
        if state.command_live {
            state.command_live = false;
            self.effects.push(Effect::StopCommand { step: index });
        }
        self.set_status(index, Status::Waiting, now);
    }

    fn set_status(&mut self, index: usize, status: Status, now: Instant) {
        let state = &mut self.steps[index];
        if state.status == status {
            return;
        }
        if matches!(status, Status::Ready | Status::Failed(_) | Status::Stopped) {
            state.finished = Some(now);
        }
        state.status = status.clone();
        self.changes.push(Change {
            step: index,
            status,
        });
    }

    /// Starts every waiting step whose needs are ready; notes what blocks
    /// the others.
    fn advance(&mut self, now: Instant) {
        loop {
            let mut started = false;
            for index in 0..self.steps.len() {
                if self.steps[index].status != Status::Waiting {
                    continue;
                }
                let blockers: Vec<(String, bool)> = self.workflow.steps[index]
                    .needs
                    .iter()
                    .filter_map(|need| {
                        let at = self.step(need)?;
                        match &self.steps[at].status {
                            Status::Ready => None,
                            Status::Failed(_) | Status::Stopped => Some((need.clone(), true)),
                            _ => Some((need.clone(), false)),
                        }
                    })
                    .collect();
                if blockers.is_empty() {
                    self.begin(index, now);
                    started = true;
                } else {
                    let names: Vec<&str> = blockers.iter().map(|(id, _)| id.as_str()).collect();
                    self.steps[index].detail = if blockers.iter().any(|(_, stuck)| *stuck) {
                        format!("blocked: {} did not get ready", names.join(", "))
                    } else {
                        format!("needs {}", names.join(", "))
                    };
                }
            }
            if !started {
                break;
            }
        }
        if self.outcome == Outcome::Running
            && self.steps.iter().all(|state| state.status == Status::Ready)
        {
            self.outcome = Outcome::Ready;
        }
    }

    fn begin(&mut self, index: usize, now: Instant) {
        let generation = self.steps[index].generation;
        self.steps[index].started = Some(now);
        self.set_status(index, Status::Starting, now);
        if self.workflow.steps[index].project.is_some() {
            self.steps[index].phase = Phase::Project;
            self.steps[index].detail = "opening the project".into();
            self.effects.push(Effect::OpenProject {
                step: index,
                generation,
            });
        } else {
            self.after_project(index, now);
        }
    }

    fn after_project(&mut self, index: usize, now: Instant) {
        let generation = self.steps[index].generation;
        if self.workflow.steps[index].check.is_some() {
            self.steps[index].phase = Phase::Check;
            self.steps[index].detail = "checking".into();
            self.effects.push(Effect::RunCheck {
                step: index,
                generation,
            });
        } else {
            self.launch(index, now);
        }
    }

    fn launch(&mut self, index: usize, now: Instant) {
        let step = &self.workflow.steps[index];
        let generation = self.steps[index].generation;
        let has_command = step.run.is_some();
        let state = &mut self.steps[index];
        state.phase = Phase::Waiting;
        state.detail = match &step.ready {
            Ready::Exit if has_command => "running".into(),
            Ready::Exit => String::new(),
            Ready::Output(pattern) => format!("waiting for output /{pattern}/"),
            Ready::Port(port) => format!("waiting for port {port}"),
            Ready::Manual(prompt) => prompt.clone(),
            Ready::Delay(ms) => format!("waiting {ms} ms"),
        };
        if let Ready::Delay(ms) = step.ready {
            state.ready_at = Some(now + Duration::from_millis(ms));
        }
        if has_command {
            state.command_live = true;
            self.effects.push(Effect::StartCommand {
                step: index,
                generation,
            });
        }
        let nothing_to_wait_for = !has_command && step.ready == Ready::Exit;
        self.set_status(index, Status::Running, now);
        if nothing_to_wait_for {
            self.mark_ready(index, now);
        }
    }

    fn mark_ready(&mut self, index: usize, now: Instant) {
        let state = &mut self.steps[index];
        state.phase = Phase::Idle;
        state.ready_at = None;
        state.detail.clear();
        self.set_status(index, Status::Ready, now);
    }

    fn fail(&mut self, index: usize, reason: String, now: Instant) {
        let state = &mut self.steps[index];
        state.phase = Phase::Idle;
        state.ready_at = None;
        state.detail.clear();
        state.generation += 1;
        if state.command_live {
            state.command_live = false;
            self.effects.push(Effect::StopCommand { step: index });
        }
        self.set_status(index, Status::Failed(reason), now);
    }

    fn current(&self, index: usize, generation: u64) -> bool {
        self.steps
            .get(index)
            .is_some_and(|state| state.generation == generation)
    }

    pub fn project_opened(&mut self, index: usize, generation: u64, now: Instant) {
        if self.current(index, generation) && self.steps[index].phase == Phase::Project {
            self.after_project(index, now);
            self.advance(now);
        }
    }

    pub fn project_failed(&mut self, index: usize, generation: u64, error: &str, now: Instant) {
        if self.current(index, generation) && self.steps[index].phase == Phase::Project {
            self.fail(index, format!("could not open the project: {error}"), now);
            self.advance(now);
        }
    }

    /// A check finished. A failure with `onFail` waits for Retry; without
    /// it the whole workflow fails and stops.
    pub fn check_finished(
        &mut self,
        index: usize,
        generation: u64,
        result: CheckResult,
        now: Instant,
    ) {
        if !self.current(index, generation) || self.steps[index].phase != Phase::Check {
            return;
        }
        let failure = match result {
            CheckResult::Exited {
                code: Some(0),
                output: _,
            } => None,
            CheckResult::Exited { code, output } => {
                let last = output
                    .lines()
                    .rev()
                    .map(str::trim)
                    .find(|line| !line.is_empty())
                    .unwrap_or_default()
                    .to_owned();
                let code = code.map_or("killed".to_owned(), |code| format!("exit code {code}"));
                Some((code, last))
            }
            CheckResult::Error(error) => Some((error, String::new())),
        };
        let Some((what, last)) = failure else {
            self.launch(index, now);
            self.advance(now);
            return;
        };
        self.steps[index].check_output = (!last.is_empty()).then(|| last.clone());
        match self.workflow.steps[index].on_fail.clone() {
            Some(message) => {
                self.fail(index, message, now);
                self.advance(now);
            }
            None => {
                let reason = if last.is_empty() {
                    format!("check failed ({what})")
                } else {
                    format!("check failed ({what}): {last}")
                };
                self.fail(index, reason, now);
                self.stop_all(now);
                self.outcome = Outcome::Failed;
            }
        }
    }

    /// The command could not start.
    pub fn start_failed(&mut self, index: usize, generation: u64, error: &str, now: Instant) {
        if self.current(index, generation) {
            self.steps[index].command_live = false;
            self.fail(index, format!("could not start: {error}"), now);
            self.advance(now);
        }
    }

    /// Output of the step's command.
    pub fn output(&mut self, index: usize, generation: u64, bytes: &[u8], now: Instant) {
        if !self.current(index, generation) {
            return;
        }
        let seen = self.steps[index].watcher.push(bytes);
        let step = &self.workflow.steps[index];
        let state = &self.steps[index];
        let waiting = state.status == Status::Running && state.phase == Phase::Waiting;
        // Until ready, plus a moment: servers often print their address
        // right after the line that made them ready.
        let open = match state.status {
            Status::Running => true,
            Status::Ready => state
                .finished
                .is_some_and(|at| now.saturating_duration_since(at) <= URL_GRACE),
            _ => false,
        };
        if step.open_urls && open {
            for url in seen.urls {
                self.effects.push(Effect::OpenUrl { step: index, url });
            }
        }
        if waiting && seen.matched && matches!(step.ready, Ready::Output(_)) {
            self.mark_ready(index, now);
            self.advance(now);
        }
    }

    /// The step's command exited.
    pub fn exited(&mut self, index: usize, generation: u64, code: Option<i32>, now: Instant) {
        if !self.current(index, generation) || !self.steps[index].command_live {
            return;
        }
        let state = &mut self.steps[index];
        state.command_live = false;
        state.exit = Some(code);
        let waiting = state.status == Status::Running && state.phase == Phase::Waiting;
        if !waiting {
            return;
        }
        let code_text = code.map_or("unknown exit code".to_owned(), |code| {
            format!("exit code {code}")
        });
        match (&self.workflow.steps[index].ready, code) {
            (Ready::Exit, Some(0)) => self.mark_ready(index, now),
            (Ready::Exit, _) => self.fail(index, format!("failed ({code_text})"), now),
            // A command that ends before its prompt is answered: the prompt
            // still decides.
            (Ready::Manual(_), _) => return,
            _ => self.fail(
                index,
                format!("exited before it was ready ({code_text})"),
                now,
            ),
        }
        self.advance(now);
    }

    /// Whether the step's port listens where it runs.
    pub fn port_state(&mut self, index: usize, generation: u64, listening: bool, now: Instant) {
        let waiting = self.current(index, generation)
            && self.steps[index].status == Status::Running
            && self.steps[index].phase == Phase::Waiting
            && matches!(self.workflow.steps[index].ready, Ready::Port(_));
        if waiting && listening {
            self.mark_ready(index, now);
            self.advance(now);
        }
    }

    /// Delays and timeouts up to `now`.
    pub fn tick(&mut self, now: Instant) {
        let mut changed = false;
        for index in 0..self.steps.len() {
            let state = &self.steps[index];
            if !state.status.is_active() || state.status == Status::Waiting {
                continue;
            }
            if state.phase == Phase::Waiting && state.ready_at.is_some_and(|at| at <= now) {
                self.mark_ready(index, now);
                changed = true;
                continue;
            }
            let timeout = self.workflow.steps[index]
                .timeout_s
                .map(Duration::from_secs);
            if let (Some(timeout), Some(started)) = (timeout, state.started)
                && now.saturating_duration_since(started) >= timeout
            {
                self.fail(
                    index,
                    format!("timed out after {} s", timeout.as_secs()),
                    now,
                );
                changed = true;
            }
        }
        if changed {
            self.advance(now);
        }
    }

    /// When `tick` next has something to do, if ever.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.steps
            .iter()
            .enumerate()
            .filter(|(_, state)| matches!(state.status, Status::Starting | Status::Running))
            .flat_map(|(index, state)| {
                let timeout = self.workflow.steps[index]
                    .timeout_s
                    .zip(state.started)
                    .map(|(seconds, started)| started + Duration::from_secs(seconds));
                [state.ready_at, timeout]
            })
            .flatten()
            .min()
    }
}

#[cfg(test)]
mod tests {
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

    const CHAIN: &str = r#"{"steps": [
        {"id": "a", "run": "a"},
        {"id": "b", "run": "b"},
        {"id": "c", "needs": ["a", "b"], "run": "c"},
        {"id": "d", "needs": ["c"], "run": "d"}
    ]}"#;

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
        let mut run = run(
            r#"{"steps": [{"id": "a", "project": {"local": "/p"}, "check": "c", "run": "r"}]}"#,
        );
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
            self::run(r#"{"steps": [{"id": "a", "project": {"local": "/p"}, "run": "r"}]}"#);
        failing.start(t);
        failing.project_failed(0, generation(&failing, 0), "no such folder", t);
        assert_eq!(
            failing.steps()[0].status,
            Status::Failed("could not open the project: no such folder".into())
        );
    }

    #[test]
    fn a_failed_check_with_on_fail_waits_for_retry() {
        let t = Instant::now();
        let mut run = run(r#"{"steps": [
                {"id": "vpn", "check": "ping", "onFail": "Connect the VPN"},
                {"id": "other", "run": "x", "ready": {"port": 80}},
                {"id": "app", "needs": ["vpn"], "run": "app"}
            ]}"#);
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
        assert_eq!(run.outcome(), Outcome::Running);
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
        let mut run = run(r#"{"steps": [
                {"id": "server", "run": "serve", "ready": {"output": "up"}},
                {"id": "docker", "check": "docker info"},
                {"id": "app", "needs": ["docker"], "run": "app"}
            ]}"#);
        run.start(t);
        run.take_effects();
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
        assert_eq!(statuses(&run), ["stopped", "failed", "stopped"]);
        assert_eq!(run.take_effects(), [Effect::StopCommand { step: 0 }]);
        assert_eq!(run.outcome(), Outcome::Failed);
        assert!(!run.is_active());
    }

    #[test]
    fn readiness_by_output_port_delay_manual_and_exit() {
        let t = Instant::now();
        let mut run = run(r#"{"steps": [
                {"id": "out", "run": "s", "ready": {"output": "Listening on (\\d+)"}},
                {"id": "port", "run": "s", "ready": {"port": 8000}},
                {"id": "delay", "run": "s", "ready": {"delay_ms": 500}},
                {"id": "gate", "ready": {"manual": "Log in"}},
                {"id": "build", "run": "make"}
            ]}"#);
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
        assert!(!run.continue_step(0, t), "not a manual step");
        assert!(run.continue_step(3, t));
        assert!(!run.continue_step(3, t), "already continued");
        assert_eq!(run.steps()[3].status, Status::Ready);

        run.exited(4, generation(&run, 4), Some(2), t);
        assert_eq!(
            run.steps()[4].status,
            Status::Failed("failed (exit code 2)".into())
        );
        assert_eq!(run.outcome(), Outcome::Running);
        // Exiting before readiness fails; exiting after it is only noted.
        run.exited(0, generation(&run, 0), Some(1), t);
        assert_eq!(run.steps()[0].status, Status::Ready);
        assert_eq!(run.steps()[0].exit, Some(Some(1)));
        run.restart(1, t);
        run.exited(1, generation(&run, 1), Some(0), t);
        assert_eq!(
            run.steps()[1].status,
            Status::Failed("exited before it was ready (exit code 0)".into())
        );
    }

    #[test]
    fn timeouts_fail_the_step_and_stop_its_command() {
        let t = Instant::now();
        let mut run = run(r#"{"steps": [
                {"id": "slow", "run": "s", "ready": {"output": "never"}, "timeout_s": 10},
                {"id": "after", "needs": ["slow"], "run": "x"}
            ]}"#);
        run.start(t);
        run.take_effects();
        assert_eq!(run.next_deadline(), Some(t + Duration::from_secs(10)));
        run.tick(t + Duration::from_secs(9));
        assert_eq!(statuses(&run), ["running", "waiting"]);
        run.tick(t + Duration::from_secs(10));
        assert_eq!(
            run.steps()[0].status,
            Status::Failed("timed out after 10 s".into())
        );
        assert_eq!(run.take_effects(), [Effect::StopCommand { step: 0 }]);
        assert_eq!(
            run.steps()[0].elapsed(t + Duration::from_secs(60)),
            Some(Duration::from_secs(10))
        );
        assert_eq!(run.steps()[1].detail, "blocked: slow did not get ready");
    }

    #[test]
    fn stop_interrupts_live_commands_and_ignores_late_reports() {
        let t = Instant::now();
        let mut run = run(r#"{"steps": [
                {"id": "server", "run": "s", "ready": {"port": 80}},
                {"id": "build", "run": "make"},
                {"id": "after", "needs": ["build"], "run": "x"}
            ]}"#);
        run.start(t);
        let old = generation(&run, 0);
        run.port_state(0, old, true, t);
        run.exited(1, generation(&run, 1), Some(0), t);
        run.take_effects();
        assert_eq!(statuses(&run), ["ready", "ready", "running"]);
        run.stop_all(t);
        // The ready server still ran: stopped too; the build had exited.
        assert_eq!(statuses(&run), ["stopped", "ready", "stopped"]);
        assert_eq!(
            run.take_effects(),
            [
                Effect::StopCommand { step: 0 },
                Effect::StopCommand { step: 2 }
            ]
        );
        assert_eq!(run.outcome(), Outcome::Stopped);
        assert!(!run.is_active());
        // Reports from the stopped commands change nothing.
        run.exited(0, old, Some(130), t);
        run.output(2, generation(&run, 2) - 1, b"late\n", t);
        assert_eq!(statuses(&run), ["stopped", "ready", "stopped"]);

        // Run All again starts everything afresh.
        run.start(t);
        assert_eq!(statuses(&run), ["running", "running", "waiting"]);
        assert_eq!(run.outcome(), Outcome::Running);
    }

    #[test]
    fn restart_replaces_the_command_and_drops_stale_results() {
        let t = Instant::now();
        let mut run = run(r#"{"steps": [{"id": "a", "run": "s", "ready": {"output": "up"}}]}"#);
        run.start(t);
        let first = generation(&run, 0);
        run.take_effects();
        run.restart(0, t);
        let second = generation(&run, 0);
        assert_ne!(first, second);
        assert_eq!(
            run.take_effects(),
            [
                Effect::StopCommand { step: 0 },
                Effect::StartCommand {
                    step: 0,
                    generation: second
                }
            ]
        );
        // The replaced command's output and exit are ignored.
        run.output(0, first, b"up\n", t);
        run.exited(0, first, Some(1), t);
        assert_eq!(run.steps()[0].status, Status::Running);
        run.output(0, second, b"up\n", t);
        assert_eq!(run.steps()[0].status, Status::Ready);
    }

    #[test]
    fn urls_open_once_until_the_step_is_ready() {
        let t = Instant::now();
        let mut run = run(r#"{"steps": [
                {"id": "web", "run": "s", "openUrls": true, "ready": {"output": "compiled"}},
                {"id": "quiet", "run": "s", "ready": {"output": "x"}}
            ]}"#);
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
}
