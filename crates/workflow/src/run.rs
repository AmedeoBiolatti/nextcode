//! One run of a workflow as a state machine, without I/O: the caller
//! performs the [`Effect`]s (open a project, run a check, start a command,
//! interrupt or kill one, check artifacts, open a URL) and reports back
//! what happened (project opened, check result, output, exit, listening
//! ports, artifacts, clock ticks). Every report carries the step's
//! generation from the effect, so results of a step that was restarted or
//! stopped since are ignored.
//!
//! Per step: Pending → Waiting (for `needs` and `after`) → Starting
//! (opening the project, running the check) → Running (the command runs;
//! not ready yet) → Ready | Failed | Stopped. Stopping a live command
//! (Stop All, Restart, Run All, `timeout_s`, the workflow failing) goes
//! through Stopping: Ctrl+C, then a kill if it has not exited after the
//! step's `stop_grace_s`.

use std::time::{Duration, Instant};

use crate::{
    file::{Ready, Step, Workflow},
    output::OutputWatcher,
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
        }
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
    /// Ctrl+C to the step's command. It has the step's `stop_grace_s` to
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
    /// `onFail` failed and everything was stopped.
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
    Fail(String),
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
    /// Its check failed with `onFail`: it waits for Retry.
    awaiting_retry: bool,
    /// It got to where it runs (its project is open): artifacts can be
    /// located.
    placed: bool,
    /// It finished and its artifacts are being checked: steps waiting for
    /// it wait for that too (they may read `${steps.<id>.artifacts.…}`).
    artifacts_pending: bool,
    /// Its definition with `${steps.…}` replaced, since it started.
    resolved: Option<Step>,
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

    /// Step `index` as it runs: once it started, with its `${steps.…}`
    /// variables replaced.
    pub fn definition(&self, index: usize) -> &Step {
        self.steps[index]
            .resolved
            .as_ref()
            .unwrap_or(&self.workflow.steps[index])
    }

    pub fn outcome(&self) -> Outcome {
        self.outcome
    }

    /// Whether anything is waiting, starting, running or stopping.
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

    /// Run All: every step starts afresh; one whose command still runs
    /// is stopped first and starts once that command is gone.
    pub fn start(&mut self, now: Instant) {
        for index in 0..self.steps.len() {
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
        for index in 0..self.steps.len() {
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
            } else if state.status.is_active()
                || (state.status == Status::Ready && state.command.is_some())
            {
                self.stop_command(index, Then::Stopped, now);
                stopped = true;
            }
        }
        if stopped || matches!(self.outcome, Outcome::Running | Outcome::Ready) {
            self.outcome = Outcome::Stopped;
        }
    }

    /// Ctrl+C to the step's command: Stopping until it exits or its grace
    /// ends, then `then`. Without a live command, `then` at once.
    fn stop_command(&mut self, index: usize, then: Then, now: Instant) {
        let grace = self.workflow.steps[index].stop_grace();
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
            Then::Fail(reason) => self.set_status(index, Status::Failed(reason), now),
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
            Then::Fail(reason) => format!("{reason}; "),
            Then::Restart => "restarting; ".into(),
        };
        state.detail = format!("{before}stopping ({seconds} s left)");
    }

    /// Restarts one step (whatever its state): its command, if it still
    /// runs, is stopped first; then it waits for its needs again and starts
    /// in the same terminal.
    pub fn restart(&mut self, index: usize, now: Instant) {
        if index >= self.steps.len() {
            return;
        }
        if self.steps[index].command.is_some() {
            self.stop_command(index, Then::Restart, now);
        } else {
            self.reset(index, now);
        }
        self.outcome = Outcome::Running;
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
        let step = &self.workflow.steps[index];
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
        self.set_status(index, Status::Waiting, now);
    }

    fn set_status(&mut self, index: usize, status: Status, now: Instant) {
        let state = &mut self.steps[index];
        if state.status == status {
            return;
        }
        let finished = matches!(status, Status::Ready | Status::Failed(_) | Status::Stopped);
        if finished {
            state.finished = Some(now);
        }
        state.status = status.clone();
        self.changes.push(Change {
            step: index,
            status,
        });
        // What it produced: steps waiting for it wait for the check too.
        if finished && state.placed && !self.workflow.steps[index].artifacts.is_empty() {
            state.artifacts_pending = true;
            self.effects.push(Effect::CheckArtifacts {
                step: index,
                generation: state.generation,
            });
        }
    }

    /// What keeps waiting step `index` from starting: steps still on their
    /// way (`needs …; after …`), and steps that can never get there
    /// (`(id, why)`).
    fn blockers(&self, index: usize) -> (String, Vec<(&str, &'static str)>) {
        let step = &self.workflow.steps[index];
        let mut needs: Vec<&str> = Vec::new();
        let mut after: Vec<&str> = Vec::new();
        let mut stuck: Vec<(&str, &'static str)> = Vec::new();
        for need in &step.needs {
            let Some(state) = self.step(need).map(|at| &self.steps[at]) else {
                continue;
            };
            match &state.status {
                Status::Ready if !state.artifacts_pending => {}
                Status::Failed(_) | Status::Stopped => stuck.push((need, "did not get ready")),
                Status::Waiting if state.blocked => stuck.push((need, "can never run")),
                _ => needs.push(need),
            }
        }
        for id in &step.after {
            let Some(state) = self.step(id).map(|at| &self.steps[at]) else {
                continue;
            };
            match &state.status {
                Status::Failed(_) if state.awaiting_retry => stuck.push((id, "waits for Retry")),
                Status::Ready | Status::Failed(_) if !state.artifacts_pending => {}
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
    /// steps finished; notes what holds or blocks the others; then settles
    /// the outcome.
    fn advance(&mut self, now: Instant) {
        loop {
            let mut changed = false;
            for index in 0..self.steps.len() {
                if self.steps[index].status != Status::Waiting {
                    continue;
                }
                if self.steps[index].held {
                    let state = &mut self.steps[index];
                    state.detail = "waiting for its previous command to end".into();
                    state.blocked = false;
                    continue;
                }
                let (waiting, stuck) = self.blockers(index);
                if waiting.is_empty() && stuck.is_empty() {
                    self.begin(index, now);
                    changed = true;
                    continue;
                }
                let blocked = !stuck.is_empty();
                let detail = if blocked {
                    // `a, b did not get ready; c was stopped`.
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
                    format!("blocked: {}", groups.join("; "))
                } else {
                    waiting
                };
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

    /// Ready when every step is; failed (with failures) when nothing can
    /// progress any more.
    fn settle(&mut self) {
        if self.outcome != Outcome::Running {
            return;
        }
        if self.steps.iter().all(|state| state.status == Status::Ready) {
            self.outcome = Outcome::Ready;
            return;
        }
        let progressing = self.steps.iter().any(|state| match state.status {
            Status::Pending | Status::Starting | Status::Running | Status::Stopping => true,
            Status::Waiting => !state.blocked,
            Status::Ready | Status::Failed(_) | Status::Stopped => false,
        });
        if !progressing {
            self.outcome = Outcome::Failed;
        }
    }

    fn begin(&mut self, index: usize, now: Instant) {
        self.steps[index].started = Some(now);
        match self.resolve(index) {
            Ok(step) => self.steps[index].resolved = Some(step),
            Err(error) => return self.fail(index, error, now),
        }
        let generation = self.steps[index].generation;
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

    /// Step `index` with its `${steps.…}` variables replaced by what the
    /// steps it waits for left.
    fn resolve(&self, index: usize) -> Result<Step, String> {
        let step = &self.workflow.steps[index];
        if !step.texts().any(|text| text.contains("${steps.")) {
            return Ok(step.clone());
        }
        let ids: Vec<&str> = step.upstream().collect();
        let value = |found: &StepRef| -> Result<String, String> {
            let at = self
                .step(found.step)
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
                    let artifact = self.workflow.steps[at].artifact(by).ok_or_else(|| {
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
        let state = &mut self.steps[index];
        state.phase = Phase::Idle;
        state.ready_at = None;
        state.detail.clear();
        self.set_status(index, Status::Ready, now);
    }

    /// Fails the step; a command that still runs is stopped first (the
    /// step is Stopping until it is gone).
    fn fail(&mut self, index: usize, reason: String, now: Instant) {
        if self.steps[index].command.is_some() {
            return self.stop_command(index, Then::Fail(reason), now);
        }
        let state = &mut self.steps[index];
        state.phase = Phase::Idle;
        state.ready_at = None;
        state.detail.clear();
        state.generation += 1;
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
        let wanted = state.placed && !self.workflow.steps[index].artifacts.is_empty();
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
        for index in 0..self.steps.len() {
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
            .flat_map(|(index, state)| {
                let stop = state.stop_deadline();
                if !matches!(state.status, Status::Starting | Status::Running) {
                    return [stop, None, None];
                }
                let timeout = self.workflow.steps[index]
                    .timeout_s
                    .zip(state.started)
                    .map(|(seconds, started)| started + Duration::from_secs(seconds));
                [stop, state.ready_at, timeout]
            })
            .flatten()
            .min()
    }
}

#[cfg(test)]
mod tests;
