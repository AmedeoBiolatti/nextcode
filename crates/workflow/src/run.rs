//! One run of a workflow as a state machine, without I/O: the caller
//! performs the [`Effect`]s (open a project, run a check, start a command,
//! interrupt or kill one, check artifacts, open a URL) and reports back
//! what happened (project opened, check result, output, exit, listening
//! ports, artifacts, clock ticks). Every report carries the step's
//! generation from the effect, so results of a step that was restarted or
//! stopped since are ignored.
//!
//! Per step: Pending → Waiting (for its `depends_on`) → Starting
//! (opening the project, running the check) → Running (the command runs;
//! not ready yet) → Ready | Failed | Stopped, or Skipped when its `if` (or
//! a group's) is false or a step it needs ready was skipped. Stopping a
//! live command (Stop All, Restart, Run All, `timeout`, the workflow
//! failing) goes through Stopping: Ctrl+C, then a kill if it has not
//! exited after the step's `stop_grace_period`.
//!
//! A ready step whose command exits with a non-zero code crashed: it
//! fails. With a `restart` policy, a failed command (an exit code, a
//! crash, a timeout, an unhealthy `healthcheck`) waits for its backoff
//! and starts again (`always`: after a clean exit too), up to `max`
//! times; a `healthcheck` is probed every `interval` while the step is
//! ready.
//!
//! Steps are indexed as [`tree::flatten`] lists them, groups included. A
//! group's `depends_on` gates each of its steps; its state follows
//! from its steps' (see [`Run::steps`]), and steps outside wait for it by
//! that state like for any step's. The outcome counts the steps that do
//! something.

use std::time::{Duration, Instant};

use crate::{
    expr,
    file::{Ready, Step, Workflow, format_duration},
    inputs::InputValue,
    output::OutputWatcher,
    policy::{Probe, RestartPolicy},
    tree::{self, StepNode},
    vars::{self, StepField, StepRef},
};

/// How long after a step got ready the URLs it prints still open.
pub const URL_GRACE: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// The workflow has not started it.
    Pending,
    /// Waiting for the steps it needs or runs after.
    Waiting,
    /// Opening its project or running its check.
    Starting,
    /// Its command runs (or it waits for its port, delay or Continue).
    Running,
    /// Its command got Ctrl+C and has until its grace ends to exit.
    Stopping,
    Ready,
    Failed(String),
    Stopped,
    /// Its `if` (or a group's) was false, or a step it needs ready was
    /// skipped; the text says which.
    Skipped(String),
}

impl Status {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Waiting => "waiting",
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Stopping => "stopping",
            Self::Ready => "ready",
            Self::Failed(_) => "failed",
            Self::Stopped => "stopped",
            Self::Skipped(_) => "skipped",
        }
    }

    /// Ready, failed, stopped or skipped.
    pub fn is_finished(&self) -> bool {
        matches!(
            self,
            Self::Ready | Self::Failed(_) | Self::Stopped | Self::Skipped(_)
        )
    }

    /// Waiting, starting, running or stopping.
    pub fn is_active(&self) -> bool {
        matches!(
            self,
            Self::Waiting | Self::Starting | Self::Running | Self::Stopping
        )
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
    /// Run the step's check ([`Run::definition`]); answer with
    /// [`Run::check_finished`].
    RunCheck {
        step: usize,
        generation: u64,
    },
    /// Start the step's command ([`Run::definition`]) in its terminal
    /// (replacing what ran there); report [`Run::output`] and
    /// [`Run::exited`], or [`Run::start_failed`].
    StartCommand {
        step: usize,
        generation: u64,
    },
    /// Ctrl+C to the step's command. It has the step's `stop_grace_period` to
    /// exit (report [`Run::exited`] as usual) before a [`Effect::KillCommand`].
    InterruptCommand {
        step: usize,
    },
    /// Kill the step's command now (the run no longer waits for it).
    KillCommand {
        step: usize,
    },
    /// Resolve the step's artifacts where it runs and see which files
    /// exist; answer with [`Run::artifacts_checked`].
    CheckArtifacts {
        step: usize,
        generation: u64,
    },
    OpenUrl {
        step: usize,
        url: String,
    },
    /// Probe the step's `healthcheck` where it runs (its
    /// [`Run::definition`]); answer with [`Run::health_checked`].
    RunHealthcheck {
        step: usize,
        generation: u64,
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
    /// Finished with failures: nothing can progress and some step is not
    /// ready (failed, blocked or waiting for Retry); or a check without
    /// `on_fail` failed and everything was stopped.
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

/// An artifact as checked where its step runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactState {
    /// The absolute path as seen where the step runs, or the URL;
    /// variables expanded.
    pub resolved: String,
    /// Whether the file exists (URLs: true).
    pub exists: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Idle,
    Project,
    Check,
    /// Readiness pending.
    Waiting,
    /// The command was interrupted and may still run.
    Stopping,
}

/// What a stopping step becomes once its command is gone.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Then {
    Stopped,
    /// Fails with this reason; `true`: its `restart` policy may start it
    /// again.
    Fail(String, bool),
    /// Waits for what it waits for again, then starts afresh.
    Restart,
}

#[derive(Debug)]
struct Stop {
    /// When the command is killed if it still runs.
    deadline: Instant,
    then: Then,
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
    /// Its artifacts as last checked (`None`: not yet), in file order, or
    /// why they could not be checked.
    pub artifacts: Option<Result<Vec<ArtifactState>, String>>,
    phase: Phase,
    generation: u64,
    /// The generation its command started with, while it runs (as far as
    /// reported).
    command: Option<u64>,
    stop: Option<Stop>,
    ready_at: Option<Instant>,
    watcher: OutputWatcher,
    /// Waiting, but a step it waits for can never get there.
    blocked: bool,
    /// Waiting for a previous run's command in its terminal to end.
    held: bool,
    /// Its check failed with `on_fail`: it waits for Retry.
    awaiting_retry: bool,
    /// It got to where it runs (its project is open): artifacts can be
    /// located.
    placed: bool,
    /// It finished and its artifacts are being checked: steps waiting for
    /// it wait for that too (they may read `${steps.<id>.artifacts.…}`).
    artifacts_pending: bool,
    /// Its definition with `${steps.…}` replaced, since it started.
    resolved: Option<Step>,
    /// Times its `restart` policy started it again in a row (Run All,
    /// Restart and Retry count from 0 again).
    pub restarts: u32,
    /// Waiting for its restart backoff until then.
    restart_at: Option<Instant>,
    /// Health check failures in a row.
    pub health_failures: u32,
    /// When its health check is probed next (while it is ready).
    next_health: Option<Instant>,
    /// A health probe is out.
    health_busy: bool,
}

impl StepState {
    fn needs_ticks(&self) -> bool {
        self.status.is_active()
            || (self.status == Status::Ready && (self.health_busy || self.next_health.is_some()))
    }

    fn new() -> Self {
        Self {
            status: Status::Pending,
            detail: String::new(),
            started: None,
            finished: None,
            exit: None,
            check_output: None,
            artifacts: None,
            phase: Phase::Idle,
            generation: 0,
            command: None,
            stop: None,
            ready_at: None,
            watcher: OutputWatcher::new(None),
            blocked: false,
            held: false,
            awaiting_retry: false,
            placed: false,
            artifacts_pending: false,
            resolved: None,
            restarts: 0,
            restart_at: None,
            health_failures: 0,
            next_health: None,
            health_busy: false,
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

    /// Up to `count` last output lines, oldest first.
    pub fn last_lines(&self, count: usize) -> Vec<&str> {
        self.watcher.last_lines(count)
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Whether its command is (as far as known) still running.
    pub fn command_live(&self) -> bool {
        self.command.is_some()
    }

    /// Waiting for a step that can never get ready (or finish).
    pub fn is_blocked(&self) -> bool {
        self.blocked
    }

    /// When a stopping command gets killed.
    pub fn stop_deadline(&self) -> Option<Instant> {
        self.stop.as_ref().map(|stop| stop.deadline)
    }

    /// When its `restart` policy starts it again, while it waits for that.
    pub fn restart_at(&self) -> Option<Instant> {
        self.restart_at
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
    /// The steps, groups included, flattened.
    nodes: Vec<StepNode>,
    /// The steps that do something (not groups), by index.
    leaves: Vec<usize>,
    steps: Vec<StepState>,
    effects: Vec<Effect>,
    changes: Vec<Change>,
    outcome: Outcome,
}

/// A group's state as its steps make it.
struct Derived {
    status: Status,
    detail: String,
    blocked: bool,
    awaiting_retry: bool,
    artifacts_pending: bool,
    started: Option<Instant>,
}

/// What a waiting step's detail says: what it waits for, or (`blocked`)
/// `blocked: a, b did not get ready; c was stopped`.
fn waiting_detail(waiting: String, stuck: Vec<(&str, &'static str)>) -> (String, bool) {
    if stuck.is_empty() {
        return (waiting, false);
    }
    let mut groups: Vec<(&str, Vec<&str>)> = Vec::new();
    for (id, why) in stuck {
        match groups.iter_mut().find(|(reason, _)| *reason == why) {
            Some((_, ids)) => ids.push(id),
            None => groups.push((why, vec![id])),
        }
    }
    let groups: Vec<String> = groups
        .into_iter()
        .map(|(why, ids)| format!("{} {why}", ids.join(", ")))
        .collect();
    (format!("blocked: {}", groups.join("; ")), true)
}

impl Run {
    pub fn new(workflow: Workflow) -> Self {
        let nodes = tree::flatten(&workflow);
        let steps = nodes.iter().map(|_| StepState::new()).collect();
        let leaves = (0..nodes.len())
            .filter(|&at| !nodes[at].is_group())
            .collect();
        Self {
            workflow,
            nodes,
            leaves,
            steps,
            effects: Vec::new(),
            changes: Vec::new(),
            outcome: Outcome::Idle,
        }
    }

    pub fn workflow(&self) -> &Workflow {
        &self.workflow
    }

    /// The steps, groups included, in [`tree::flatten`]'s order: indices
    /// of [`Run::steps`], effects and changes.
    pub fn nodes(&self) -> &[StepNode] {
        &self.nodes
    }

    /// Each step's state, by node index. A group's: Pending before the
    /// run; Waiting for its own `needs`/`after` (blocked when they can
    /// never be met); Running while a step in it starts or runs
    /// (Stopping when its only active steps stop); Ready when all its
    /// steps are; Failed (`N of M steps failed`) when none can progress
    /// and some failed; Stopped when stopped.
    pub fn steps(&self) -> &[StepState] {
        &self.steps
    }

    /// The step with qualified id `id` (`group/step`; a top-level id as
    /// is).
    pub fn step(&self, id: &str) -> Option<usize> {
        tree::find(&self.nodes, id)
    }

    /// Step `index` as it runs: once it started, with its `${steps.…}`
    /// variables replaced; with what it inherits from its groups.
    pub fn definition(&self, index: usize) -> &Step {
        self.steps[index]
            .resolved
            .as_ref()
            .unwrap_or(&self.nodes[index].step)
    }

    pub fn outcome(&self) -> Outcome {
        self.outcome
    }

    /// Whether anything is working or monitoring a ready step's health.
    pub fn is_active(&self) -> bool {
        self.leaves
            .iter()
            .any(|&index| self.steps[index].needs_ticks())
    }

    /// Effects to perform, in order.
    pub fn take_effects(&mut self) -> Vec<Effect> {
        std::mem::take(&mut self.effects)
    }

    /// Status changes since the last call, in order.
    pub fn take_changes(&mut self) -> Vec<Change> {
        std::mem::take(&mut self.changes)
    }

    /// Steps waiting for a port or an HTTP answer to get ready:
    /// `(step, generation, probe)` (the URL as [`Run::definition`] has
    /// it); answer with [`Run::probe_result`].
    pub fn ready_probes(&self) -> Vec<(usize, u64, Probe)> {
        self.leaves
            .iter()
            .filter_map(|&index| {
                let state = &self.steps[index];
                if state.status != Status::Running || state.phase != Phase::Waiting {
                    return None;
                }
                let probe = match &self.definition(index).ready {
                    Ready::Port(port) => Probe::Port(*port),
                    Ready::Http(http) => Probe::Http(http.clone()),
                    _ => return None,
                };
                Some((index, state.generation, probe))
            })
            .collect()
    }

    /// Run All: every step starts afresh; one whose command still runs
    /// is stopped first and starts once that command is gone.
    pub fn start(&mut self, now: Instant) {
        for at in 0..self.leaves.len() {
            let index = self.leaves[at];
            self.steps[index].restarts = 0;
            if self.steps[index].command.is_some() {
                self.stop_command(index, Then::Restart, now);
            } else {
                self.reset(index, now);
            }
        }
        self.outcome = Outcome::Running;
        self.advance(now);
    }

    /// Stop All: Ctrl+C to every command the run started (unfinished steps
    /// and ready ones whose command still runs become Stopping, then
    /// Stopped); a second Stop All kills the commands still stopping.
    pub fn stop_all(&mut self, now: Instant) {
        self.stop_everything(true, now);
    }

    /// Stop All for a run being replaced by a new one: steps already
    /// stopping keep their grace.
    pub fn drain(&mut self, now: Instant) {
        self.stop_everything(false, now);
    }

    fn stop_everything(&mut self, kill_stopping: bool, now: Instant) {
        let mut stopped = false;
        for at in 0..self.leaves.len() {
            let index = self.leaves[at];
            let state = &mut self.steps[index];
            if state.status == Status::Stopping {
                if let Some(stop) = &mut state.stop
                    && stop.then == Then::Restart
                {
                    stop.then = Then::Stopped;
                }
                if kill_stopping {
                    self.kill(index, now);
                }
            } else if state.needs_ticks()
                || (state.status == Status::Ready && state.command.is_some())
            {
                self.stop_command(index, Then::Stopped, now);
                stopped = true;
            }
        }
        self.derive_groups(now);
        if stopped || matches!(self.outcome, Outcome::Running | Outcome::Ready) {
            self.outcome = Outcome::Stopped;
        }
    }

    /// Ctrl+C to the step's command: Stopping until it exits or its grace
    /// ends, then `then`. Without a live command, `then` at once.
    fn stop_command(&mut self, index: usize, then: Then, now: Instant) {
        let grace = self.nodes[index].step.stop_grace();
        let state = &mut self.steps[index];
        if state.status == Status::Stopping {
            if let Some(stop) = &mut state.stop {
                stop.then = then;
            }
            self.stopping_detail(index, now);
            return;
        }
        state.generation += 1;
        state.ready_at = None;
        if state.command.is_some() {
            state.phase = Phase::Stopping;
            state.stop = Some(Stop {
                deadline: now + grace,
                then,
            });
            self.effects.push(Effect::InterruptCommand { step: index });
            self.stopping_detail(index, now);
            self.set_status(index, Status::Stopping, now);
        } else {
            self.finish_stop(index, then, now);
        }
    }

    /// The stopped command is gone (or there was none).
    fn finish_stop(&mut self, index: usize, then: Then, now: Instant) {
        let state = &mut self.steps[index];
        state.stop = None;
        state.phase = Phase::Idle;
        state.detail.clear();
        match then {
            Then::Stopped => self.set_status(index, Status::Stopped, now),
            Then::Fail(reason, restartable) => self.settle_failure(index, reason, restartable, now),
            Then::Restart => self.reset(index, now),
        }
    }

    /// Kills a stopping step's command and finishes the stop.
    fn kill(&mut self, index: usize, now: Instant) {
        let state = &mut self.steps[index];
        let Some(stop) = state.stop.take() else {
            return;
        };
        state.command = None;
        self.effects.push(Effect::KillCommand { step: index });
        self.finish_stop(index, stop.then, now);
    }

    /// `stopping (N s left)`, after the reason when it will fail.
    fn stopping_detail(&mut self, index: usize, now: Instant) {
        let state = &mut self.steps[index];
        let Some(stop) = &state.stop else {
            return;
        };
        let left = stop.deadline.saturating_duration_since(now);
        let seconds = left.as_secs() + u64::from(left.subsec_nanos() > 0);
        let before = match &stop.then {
            Then::Stopped => String::new(),
            Then::Fail(reason, _) => format!("{reason}; "),
            Then::Restart => "restarting; ".into(),
        };
        state.detail = format!("{before}stopping ({seconds} s left)");
    }

    /// Restarts one step (whatever its state), or every step of a group:
    /// its command, if it still runs, is stopped first; then it waits for
    /// its needs again and starts in the same terminal.
    pub fn restart(&mut self, index: usize, now: Instant) {
        if index >= self.steps.len() {
            return;
        }
        for index in tree::subtree(&self.nodes, index) {
            if self.nodes[index].is_group() {
                continue;
            }
            self.steps[index].restarts = 0;
            if self.steps[index].command.is_some() {
                self.stop_command(index, Then::Restart, now);
            } else {
                self.reset(index, now);
            }
        }
        self.outcome = Outcome::Running;
        self.advance(now);
    }

    /// Retry: every failed step starts again.
    pub fn retry_failed(&mut self, now: Instant) {
        self.retry_in(0..self.steps.len(), now);
    }

    /// Retry for step `index`: it starts again if it failed; for a group,
    /// every failed step in it. False if none had failed.
    pub fn retry(&mut self, index: usize, now: Instant) -> bool {
        if index >= self.steps.len() {
            return false;
        }
        self.retry_in(tree::subtree(&self.nodes, index), now)
    }

    fn retry_in(&mut self, nodes: std::ops::Range<usize>, now: Instant) -> bool {
        let failed: Vec<usize> = nodes
            .filter(|&index| {
                !self.nodes[index].is_group()
                    && matches!(self.steps[index].status, Status::Failed(_))
            })
            .collect();
        for &index in &failed {
            self.steps[index].restarts = 0;
            self.reset(index, now);
        }
        if !failed.is_empty() {
            self.outcome = Outcome::Running;
            self.advance(now);
        }
        !failed.is_empty()
    }

    /// Whether step `index` is a manual gate waiting for the user's
    /// Continue (its command and check, if any, are done).
    pub fn awaits_user(&self, index: usize) -> bool {
        self.steps
            .get(index)
            .is_some_and(|state| state.status == Status::Running && state.phase == Phase::Waiting)
            && matches!(self.nodes[index].step.ready, Ready::Manual(_))
            && !self.nodes[index].is_group()
    }

    /// Continue on a step waiting for the user; false if it was not.
    pub fn continue_step(&mut self, index: usize, now: Instant) -> bool {
        let waiting = self.awaits_user(index);
        if waiting {
            self.mark_ready(index, now);
            self.advance(now);
        }
        waiting
    }

    /// Keeps step `index` waiting until [`Run::release`]: the command of a
    /// previous run still ends in its terminal. Call before [`Run::start`].
    pub fn hold(&mut self, index: usize) {
        if let Some(state) = self.steps.get_mut(index) {
            state.held = true;
        }
    }

    /// The previous command in step `index`'s terminal is gone.
    pub fn release(&mut self, index: usize, now: Instant) {
        if let Some(state) = self.steps.get_mut(index)
            && state.held
        {
            state.held = false;
            self.advance(now);
        }
    }

    /// Whether step `index` is held (see [`Run::hold`]).
    pub fn is_held(&self, index: usize) -> bool {
        self.steps.get(index).is_some_and(|state| state.held)
    }

    /// Back to Waiting with a new generation (its command is gone).
    fn reset(&mut self, index: usize, now: Instant) {
        let step = &self.nodes[index].step;
        let pattern = match &step.ready {
            Ready::Output(pattern) => Some(pattern.as_str()),
            _ => None,
        };
        let state = &mut self.steps[index];
        debug_assert!(state.command.is_none(), "reset with a live command");
        state.generation += 1;
        state.phase = Phase::Idle;
        state.started = None;
        state.finished = None;
        state.exit = None;
        state.check_output = None;
        state.artifacts = None;
        state.stop = None;
        state.ready_at = None;
        state.detail.clear();
        state.watcher = OutputWatcher::new(pattern);
        state.blocked = false;
        state.awaiting_retry = false;
        state.placed = false;
        state.artifacts_pending = false;
        state.resolved = None;
        state.restart_at = None;
        state.health_failures = 0;
        state.next_health = None;
        state.health_busy = false;
        self.set_status(index, Status::Waiting, now);
    }

    /// A failure of step `index` (its command gone): with a `restart`
    /// policy and `restartable`, it waits for its backoff and starts again
    /// (unless it gave up after `max` restarts); else Failed.
    fn settle_failure(&mut self, index: usize, reason: String, restartable: bool, now: Instant) {
        // A step of a run that was all ready fails or restarts: the run
        // goes on (and settles again).
        if self.outcome == Outcome::Ready {
            self.outcome = Outcome::Running;
        }
        let restart = &self.nodes[index].step.restart;
        if restartable && restart.policy != RestartPolicy::No {
            let used = self.steps[index].restarts;
            match restart.max {
                Some(max) if used >= max => {
                    let reason = format!("{reason} (gave up after {used} restarts)");
                    return self.set_status(index, Status::Failed(reason), now);
                }
                _ => return self.schedule_restart(index, &reason, now),
            }
        }
        self.set_status(index, Status::Failed(reason), now);
    }

    /// Waiting again, until its backoff ends: then it starts as usual
    /// (after what it waits for). Its command is gone.
    fn schedule_restart(&mut self, index: usize, reason: &str, now: Instant) {
        if self.outcome == Outcome::Ready {
            self.outcome = Outcome::Running;
        }
        let backoff = self.nodes[index].step.restart.backoff;
        let restarts = self.steps[index].restarts + 1;
        self.reset(index, now);
        let state = &mut self.steps[index];
        state.restarts = restarts;
        state.restart_at = Some(now + backoff);
        state.detail = format!(
            "restart {restarts} in {}: {reason}",
            format_duration(backoff)
        );
    }

    fn set_status(&mut self, index: usize, status: Status, now: Instant) {
        let state = &mut self.steps[index];
        if state.status == status {
            return;
        }
        let finished = status.is_finished();
        if finished {
            state.finished = Some(now);
        }
        state.status = status.clone();
        self.changes.push(Change {
            step: index,
            status,
        });
        // What it produced: steps waiting for it wait for the check too.
        if finished && state.placed && !self.nodes[index].step.artifacts.is_empty() {
            state.artifacts_pending = true;
            self.effects.push(Effect::CheckArtifacts {
                step: index,
                generation: state.generation,
            });
        }
    }

    /// What keeps waiting step `index` from starting: steps still on their
    /// way (`needs …; after …`), and steps that can never get there
    /// (`(id, why)`). A step waits for its own `needs` and `after` and its
    /// groups'.
    fn blockers(&self, index: usize) -> (String, Vec<(&str, &'static str)>) {
        let (gate_needs, gate_after) = tree::gates(&self.nodes, index);
        let mut needs: Vec<&str> = Vec::new();
        let mut after: Vec<&str> = Vec::new();
        let mut stuck: Vec<(&str, &'static str)> = Vec::new();
        for need in gate_needs {
            let (id, state) = (self.nodes[need].step.id.as_str(), &self.steps[need]);
            match &state.status {
                Status::Ready if !state.artifacts_pending => {}
                // Skips this step too (see `skipped_needs`).
                Status::Skipped(_) => {}
                Status::Failed(_) | Status::Stopped => stuck.push((id, "did not get ready")),
                Status::Waiting if state.blocked => stuck.push((id, "can never run")),
                _ => needs.push(id),
            }
        }
        for at in gate_after {
            let (id, state) = (self.nodes[at].step.id.as_str(), &self.steps[at]);
            match &state.status {
                Status::Failed(_) if state.awaiting_retry => stuck.push((id, "waits for Retry")),
                Status::Ready | Status::Failed(_) if !state.artifacts_pending => {}
                Status::Skipped(_) => {}
                Status::Stopped => stuck.push((id, "was stopped")),
                Status::Waiting if state.blocked => stuck.push((id, "can never run")),
                _ => after.push(id),
            }
        }
        let mut waiting = Vec::new();
        if !needs.is_empty() {
            waiting.push(format!("needs {}", needs.join(", ")));
        }
        if !after.is_empty() {
            waiting.push(format!("after {}", after.join(", ")));
        }
        (waiting.join("; "), stuck)
    }

    /// Starts every waiting step whose needs are ready and whose `after`
    /// steps finished; notes what holds or blocks the others; keeps the
    /// groups' states up with their steps'; then settles the outcome.
    fn advance(&mut self, now: Instant) {
        loop {
            let mut changed = self.derive_groups(now);
            for at in 0..self.leaves.len() {
                let index = self.leaves[at];
                if self.steps[index].status != Status::Waiting {
                    continue;
                }
                if self.steps[index].held {
                    let state = &mut self.steps[index];
                    state.detail = "waiting for its previous command to end".into();
                    state.blocked = false;
                    continue;
                }
                if self.steps[index].restart_at.is_some_and(|at| at > now) {
                    continue;
                }
                let (waiting, stuck) = self.blockers(index);
                if waiting.is_empty() && stuck.is_empty() {
                    match self.skip_reason(index) {
                        Some(reason) => self.set_status(index, Status::Skipped(reason), now),
                        None => self.begin(index, now),
                    }
                    changed = true;
                    continue;
                }
                let (detail, blocked) = waiting_detail(waiting, stuck);
                let state = &mut self.steps[index];
                state.detail = detail;
                if state.blocked != blocked {
                    state.blocked = blocked;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        self.settle();
    }

    /// Each group's state from its steps' (inner groups first); true if a
    /// status or whether one is blocked changed.
    fn derive_groups(&mut self, now: Instant) -> bool {
        let mut changed = false;
        for index in (0..self.nodes.len()).rev() {
            if !self.nodes[index].is_group() {
                continue;
            }
            let derived = self.derive(index);
            let state = &mut self.steps[index];
            changed |= state.blocked != derived.blocked;
            state.blocked = derived.blocked;
            state.awaiting_retry = derived.awaiting_retry;
            state.artifacts_pending = derived.artifacts_pending;
            state.detail = derived.detail;
            state.started = derived.started;
            if !derived.status.is_finished() {
                state.finished = None;
            }
            if state.status != derived.status {
                self.set_status(index, derived.status, now);
                changed = true;
            }
        }
        changed
    }

    /// Group `index`'s state as its steps (and its own `needs` and
    /// `after`) make it.
    fn derive(&self, index: usize) -> Derived {
        let children = &self.nodes[index].children;
        let states = || children.iter().map(|&child| &self.steps[child]);
        let total = children.len();
        let count = |wanted: fn(&Status) -> bool| states().filter(|s| wanted(&s.status)).count();
        let ready = count(|status| matches!(status, Status::Ready | Status::Skipped(_)));
        let skipped = count(|status| matches!(status, Status::Skipped(_)));
        let mut derived = Derived {
            status: Status::Pending,
            detail: String::new(),
            blocked: false,
            awaiting_retry: states()
                .any(|s| s.awaiting_retry && matches!(s.status, Status::Failed(_))),
            artifacts_pending: states().any(|s| s.artifacts_pending),
            started: states().filter_map(|s| s.started).min(),
        };
        let progressing = states().any(|s| match s.status {
            Status::Pending => true,
            Status::Waiting => !s.blocked,
            _ => false,
        });
        if count(|status| *status == Status::Pending) == total {
            derived.started = None;
        } else if skipped == total {
            derived.status = Status::Skipped(format!("its {total} steps were skipped"));
        } else if ready == total {
            derived.status = Status::Ready;
        } else if count(|status| matches!(status, Status::Starting | Status::Running)) > 0 {
            derived.status = Status::Running;
            derived.detail = format!("{ready} of {total} ready");
        } else if count(|status| *status == Status::Stopping) > 0 {
            derived.status = Status::Stopping;
            derived.detail = "stopping".into();
        } else if progressing {
            let (waiting, stuck) = self.blockers(index);
            if waiting.is_empty() && stuck.is_empty() {
                // Its steps wait for each other (or for their previous
                // commands to end).
                derived.status = Status::Running;
                derived.detail = format!("{ready} of {total} ready");
            } else {
                derived.status = Status::Waiting;
                (derived.detail, derived.blocked) = waiting_detail(waiting, stuck);
            }
        } else {
            let failed = count(|status| matches!(status, Status::Failed(_)));
            if failed > 0 {
                derived.status = Status::Failed(format!("{failed} of {total} steps failed"));
            } else if count(|status| *status == Status::Stopped) > 0 {
                derived.status = Status::Stopped;
            } else {
                // Its steps can never start: what it waits for cannot get
                // there.
                let (waiting, stuck) = self.blockers(index);
                derived.status = Status::Waiting;
                derived.blocked = true;
                derived.detail = if stuck.is_empty() {
                    "blocked".into()
                } else {
                    waiting_detail(waiting, stuck).0
                };
            }
        }
        derived
    }

    /// Ready when every step is; failed (with failures) when nothing can
    /// progress any more (groups follow their steps).
    fn settle(&mut self) {
        if self.outcome != Outcome::Running {
            return;
        }
        let leaves = || self.leaves.iter().map(|&index| &self.steps[index]);
        if leaves().all(|state| matches!(state.status, Status::Ready | Status::Skipped(_))) {
            self.outcome = Outcome::Ready;
            return;
        }
        let progressing = leaves().any(|state| match state.status {
            Status::Pending | Status::Starting | Status::Running | Status::Stopping => true,
            Status::Waiting => !state.blocked,
            Status::Ready | Status::Failed(_) | Status::Stopped | Status::Skipped(_) => false,
        });
        if !progressing {
            self.outcome = Outcome::Failed;
        }
    }

    /// Why step `index`, free to start, is skipped instead: a step it (or
    /// a group it is in) needs ready was skipped, or its `if` or a
    /// group's is false. `None`: it starts.
    fn skip_reason(&self, index: usize) -> Option<String> {
        let (gate_needs, _) = tree::gates(&self.nodes, index);
        let skipped: Vec<&str> = gate_needs
            .iter()
            .filter(|&&need| matches!(self.steps[need].status, Status::Skipped(_)))
            .map(|&need| self.nodes[need].step.id.as_str())
            .collect();
        if !skipped.is_empty() {
            return Some(format!("{} was skipped", skipped.join(", ")));
        }
        let mut node = Some(index);
        while let Some(at) = node {
            if let Some(text) = &self.nodes[at].step.condition
                && !self.condition_holds(at, text)
            {
                return Some(format!("if {text}"));
            }
            node = self.nodes[at].parent;
        }
        None
    }

    /// Whether node `at`'s `if` is true now: references read the run's
    /// inputs and the states of the steps in its scope; `success()` holds
    /// when every step it waits for is ready (or skipped), `failure()`
    /// when one failed.
    fn condition_holds(&self, at: usize, text: &str) -> bool {
        let Ok(parsed) = expr::parse(text) else {
            return false;
        };
        let scope = tree::scope(&self.nodes, at);
        let lookup = |path: &[String]| -> expr::Value {
            match path {
                [root, id] if root == "inputs" => match self.workflow.values.get(id) {
                    Some(InputValue::Text(text)) => expr::Value::String(text.clone()),
                    Some(InputValue::Number(Some(number))) => expr::Value::Number(*number),
                    Some(InputValue::Bool(flag)) => expr::Value::Bool(*flag),
                    Some(InputValue::Number(None)) | None => expr::Value::Null,
                },
                [root, id, field] if root == "steps" => {
                    let Some(&(_, target)) = scope.iter().find(|(scoped, _)| scoped == id) else {
                        return expr::Value::Null;
                    };
                    let state = &self.steps[target];
                    match field.as_str() {
                        "status" => expr::Value::String(state.status.name().to_owned()),
                        "exit_code" => state.exit.flatten().map_or(expr::Value::Null, |code| {
                            expr::Value::Number(f64::from(code))
                        }),
                        _ => expr::Value::Null,
                    }
                }
                _ => expr::Value::Null,
            }
        };
        let upstream: Vec<&StepState> = scope.iter().map(|&(_, at)| &self.steps[at]).collect();
        let call = |name: &str| match name {
            "success" => upstream
                .iter()
                .all(|state| matches!(state.status, Status::Ready | Status::Skipped(_))),
            "failure" => upstream
                .iter()
                .any(|state| matches!(state.status, Status::Failed(_))),
            _ => true,
        };
        parsed.eval(&lookup, &call).truthy()
    }

    fn begin(&mut self, index: usize, now: Instant) {
        self.steps[index].started = Some(now);
        match self.resolve(index) {
            Ok(step) => self.steps[index].resolved = Some(step),
            Err(error) => return self.fail(index, error, now),
        }
        let generation = self.steps[index].generation;
        self.set_status(index, Status::Starting, now);
        if self.nodes[index].step.project.is_some() {
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

    /// Step `index` with its `${steps.…}` variables replaced by what the
    /// steps it (or a group it is in) waits for left.
    fn resolve(&self, index: usize) -> Result<Step, String> {
        let step = &self.nodes[index].step;
        if !step.texts().any(|text| text.contains("${steps.")) {
            return Ok(step.clone());
        }
        let scope = tree::scope(&self.nodes, index);
        let ids: Vec<&str> = scope.iter().map(|&(id, _)| id).collect();
        let value = |found: &StepRef| -> Result<String, String> {
            let at = scope
                .iter()
                .find(|&&(id, _)| id == found.step)
                .map(|&(_, at)| at)
                .ok_or_else(|| format!("`{}`: no step `{}`", found.text, found.step))?;
            let state = &self.steps[at];
            match &found.field {
                StepField::Status => Ok(state.status.name().to_owned()),
                StepField::ExitCode => Ok(state
                    .exit
                    .flatten()
                    .map(|code| code.to_string())
                    .unwrap_or_default()),
                StepField::Artifact(by) => {
                    let artifact = self.nodes[at].step.artifact(by).ok_or_else(|| {
                        format!("`{}`: `{}` has no such artifact", found.text, found.step)
                    })?;
                    match &state.artifacts {
                        Some(Ok(checked)) => checked
                            .get(artifact)
                            .map(|checked| checked.resolved.clone())
                            .ok_or_else(|| format!("`{}`: not checked", found.text)),
                        Some(Err(error)) => Err(format!(
                            "`{}`: the artifacts of `{}` could not be resolved: {error}",
                            found.text, found.step
                        )),
                        None => Err(format!(
                            "`{}`: `{}` did not get to check its artifacts",
                            found.text, found.step
                        )),
                    }
                }
            }
        };
        let replace = |text: &str| vars::substitute(text, &ids, value);
        let mut resolved = step.clone();
        for text in [&mut resolved.run, &mut resolved.check, &mut resolved.cwd]
            .into_iter()
            .flatten()
        {
            *text = replace(text)?;
        }
        for text in resolved.env.values_mut() {
            *text = replace(text)?;
        }
        for artifact in &mut resolved.artifacts {
            match &mut artifact.target {
                crate::ArtifactTarget::Path(text) | crate::ArtifactTarget::Url(text) => {
                    *text = replace(text)?;
                }
            }
        }
        Ok(resolved)
    }

    fn after_project(&mut self, index: usize, now: Instant) {
        let generation = self.steps[index].generation;
        self.steps[index].placed = true;
        if self.nodes[index].step.check.is_some() {
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
        let step = &self.nodes[index].step;
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
            Ready::Delay(delay) => format!("waiting {}", format_duration(*delay)),
            Ready::Http(http) => format!("waiting for {} to answer", http.url),
        };
        if let Ready::Delay(delay) = step.ready {
            state.ready_at = Some(now + delay);
        }
        if has_command {
            state.command = Some(generation);
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
        let interval = self.nodes[index]
            .step
            .healthcheck
            .as_ref()
            .map(|check| check.interval);
        let state = &mut self.steps[index];
        state.phase = Phase::Idle;
        state.ready_at = None;
        state.detail.clear();
        state.health_failures = 0;
        state.next_health = interval.map(|interval| now + interval);
        self.set_status(index, Status::Ready, now);
    }

    /// Fails the step; a command that still runs is stopped first (the
    /// step is Stopping until it is gone).
    fn fail(&mut self, index: usize, reason: String, now: Instant) {
        self.fail_with(index, reason, false, now);
    }

    /// [`Run::fail`] for a failure of its command (an exit code, a
    /// timeout, unhealthy): its `restart` policy may start it again.
    fn fail_command(&mut self, index: usize, reason: String, now: Instant) {
        self.fail_with(index, reason, true, now);
    }

    fn fail_with(&mut self, index: usize, reason: String, restartable: bool, now: Instant) {
        if self.steps[index].command.is_some() {
            return self.stop_command(index, Then::Fail(reason, restartable), now);
        }
        let state = &mut self.steps[index];
        state.phase = Phase::Idle;
        state.ready_at = None;
        state.next_health = None;
        state.detail.clear();
        state.generation += 1;
        self.settle_failure(index, reason, restartable, now);
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

    /// A check finished. A failure with `on_fail` waits for Retry; without
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
        match self.nodes[index].step.on_fail.clone() {
            Some(message) => {
                self.steps[index].awaiting_retry = true;
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
                self.drain(now);
                self.outcome = Outcome::Failed;
            }
        }
    }

    /// The command could not start (or was not started: the step was
    /// stopped before).
    pub fn start_failed(&mut self, index: usize, generation: u64, error: &str, now: Instant) {
        let Some(state) = self.steps.get_mut(index) else {
            return;
        };
        if state.command == Some(generation) {
            state.command = None;
            if let Some(stop) = state.stop.take() {
                self.finish_stop(index, stop.then, now);
                self.advance(now);
                return;
            }
        }
        if self.current(index, generation) {
            self.fail(index, format!("could not start: {error}"), now);
            self.advance(now);
        }
    }

    /// Output of the step's command (kept while it stops, too).
    pub fn output(&mut self, index: usize, generation: u64, bytes: &[u8], now: Instant) {
        let Some(state) = self.steps.get_mut(index) else {
            return;
        };
        let current = state.generation == generation;
        if !current && state.command != Some(generation) {
            return;
        }
        let seen = state.watcher.push(bytes);
        if !current {
            return;
        }
        let step = &self.nodes[index].step;
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

    /// The step's command exited (a stopping one: the stop is done).
    pub fn exited(&mut self, index: usize, generation: u64, code: Option<i32>, now: Instant) {
        let Some(state) = self.steps.get_mut(index) else {
            return;
        };
        if state.command != Some(generation) {
            return;
        }
        state.command = None;
        state.exit = Some(code);
        if let Some(stop) = state.stop.take() {
            self.finish_stop(index, stop.then, now);
            self.advance(now);
            return;
        }
        let waiting = state.status == Status::Running && state.phase == Phase::Waiting;
        let code_text = code.map_or("unknown exit code".to_owned(), |code| {
            format!("exit code {code}")
        });
        let always = self.nodes[index].step.restart.policy == RestartPolicy::Always;
        if !waiting {
            // A ready command that exits: crashed (non-zero), or done.
            if self.steps[index].status == Status::Ready {
                if code != Some(0) {
                    let reason = format!("exited after it was ready ({code_text})");
                    self.fail_command(index, reason, now);
                } else if always {
                    self.schedule_restart(index, &format!("exited ({code_text})"), now);
                }
                self.advance(now);
            }
            return;
        }
        match (&self.nodes[index].step.ready, code) {
            (Ready::Exit, Some(0)) => {
                self.mark_ready(index, now);
                if always {
                    // Its dependents start first; then it runs again.
                    self.advance(now);
                    self.schedule_restart(index, &format!("exited ({code_text})"), now);
                }
            }
            (Ready::Exit, _) => self.fail_command(index, format!("failed ({code_text})"), now),
            // A command that ends before its prompt is answered: the prompt
            // still decides.
            (Ready::Manual(_), _) => return,
            _ => self.fail_command(
                index,
                format!("exited before it was ready ({code_text})"),
                now,
            ),
        }
        self.advance(now);
    }

    /// Whether the step's readiness probe ([`Run::ready_probes`]: its
    /// port listens, its URL answers) passed.
    pub fn probe_result(&mut self, index: usize, generation: u64, passed: bool, now: Instant) {
        let waiting = self.current(index, generation)
            && self.steps[index].status == Status::Running
            && self.steps[index].phase == Phase::Waiting
            && matches!(
                self.nodes[index].step.ready,
                Ready::Port(_) | Ready::Http(_)
            );
        if waiting && passed {
            self.mark_ready(index, now);
            self.advance(now);
        }
    }

    /// A health probe's answer: `Err` says why it failed. `retries`
    /// failures in a row (outside the `start_period` after it got ready)
    /// make the step unhealthy: it fails (and restarts with a policy).
    pub fn health_checked(
        &mut self,
        index: usize,
        generation: u64,
        result: Result<(), String>,
        now: Instant,
    ) {
        if !self.current(index, generation) {
            return;
        }
        let Some(check) = self.nodes[index].step.healthcheck.clone() else {
            return;
        };
        let state = &mut self.steps[index];
        state.health_busy = false;
        if state.status != Status::Ready {
            return;
        }
        state.next_health = Some(now + check.interval);
        let failure = match result {
            Ok(()) => {
                state.health_failures = 0;
                state.detail.clear();
                return;
            }
            Err(failure) => failure,
        };
        let starting = state
            .finished
            .is_some_and(|ready| now.saturating_duration_since(ready) < check.start_period);
        if starting {
            state.detail = format!("starting up; health check failed: {failure}");
            return;
        }
        state.health_failures += 1;
        if state.health_failures < check.retries {
            state.detail = format!(
                "health check failed ({} of {}): {failure}",
                state.health_failures, check.retries
            );
            return;
        }
        self.fail_command(index, format!("unhealthy: {failure}"), now);
        self.advance(now);
    }

    /// The step's artifacts, resolved and checked where it runs (in file
    /// order), or why they could not be.
    pub fn artifacts_checked(
        &mut self,
        index: usize,
        generation: u64,
        result: Result<Vec<ArtifactState>, String>,
        now: Instant,
    ) {
        if !self.current(index, generation) {
            return;
        }
        let state = &mut self.steps[index];
        state.artifacts = Some(result);
        state.artifacts_pending = false;
        self.advance(now);
    }

    /// Checks the step's artifacts again; false if it has none or did not
    /// get to where it runs.
    pub fn check_artifacts(&mut self, index: usize) -> bool {
        let Some(state) = self.steps.get(index) else {
            return false;
        };
        let wanted = state.placed && !self.nodes[index].step.artifacts.is_empty();
        if wanted {
            self.effects.push(Effect::CheckArtifacts {
                step: index,
                generation: state.generation,
            });
        }
        wanted
    }

    /// Delays, timeouts and stop graces up to `now`.
    pub fn tick(&mut self, now: Instant) {
        let mut changed = false;
        for at in 0..self.leaves.len() {
            let index = self.leaves[at];
            let state = &mut self.steps[index];
            if state.status == Status::Waiting && state.restart_at.is_some_and(|at| at <= now) {
                state.restart_at = None;
                changed = true;
                continue;
            }
            if state.status == Status::Ready
                && !state.health_busy
                && state.next_health.is_some_and(|at| at <= now)
            {
                state.health_busy = true;
                state.next_health = None;
                self.effects.push(Effect::RunHealthcheck {
                    step: index,
                    generation: state.generation,
                });
                continue;
            }
            let state = &self.steps[index];
            if state.status == Status::Stopping {
                if state.stop.as_ref().is_some_and(|stop| stop.deadline <= now) {
                    self.kill(index, now);
                    changed = true;
                } else {
                    self.stopping_detail(index, now);
                }
                continue;
            }
            if !matches!(state.status, Status::Starting | Status::Running) {
                continue;
            }
            if state.phase == Phase::Waiting && state.ready_at.is_some_and(|at| at <= now) {
                self.mark_ready(index, now);
                changed = true;
                continue;
            }
            let timeout = self.nodes[index].step.timeout;
            if let (Some(timeout), Some(started)) = (timeout, state.started)
                && now.saturating_duration_since(started) >= timeout
            {
                self.fail_command(
                    index,
                    format!("timed out after {}", format_duration(timeout)),
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
            .flat_map(|(index, state)| {
                let stop = state.stop_deadline();
                match state.status {
                    Status::Waiting => return [stop, state.restart_at, None],
                    Status::Ready if !state.health_busy => return [stop, state.next_health, None],
                    Status::Starting | Status::Running => {}
                    _ => return [stop, None, None],
                }
                let timeout = self.nodes[index]
                    .step
                    .timeout
                    .zip(state.started)
                    .map(|(timeout, started)| started + timeout);
                [stop, state.ready_at, timeout]
            })
            .flatten()
            .min()
    }
}

#[cfg(test)]
mod tests;
