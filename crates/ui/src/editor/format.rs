//! Formatting: Format Document (Shift+Alt+F), Format Selection (Ctrl+K
//! Ctrl+F) and format on save, through the language server or an external
//! program (`"formatter"` in settings.json). The result is applied as the
//! minimal edits from the old text, as one undo step, so cursors stay put.

use std::path::PathBuf;

use futures::future::{Either, select};
use gpui::BackgroundExecutor;
use lsp::types::{
    DocumentFormattingParams, DocumentRangeFormattingParams, FormattingOptions, OneOf,
    TextDocumentIdentifier,
    request::{Formatting, RangeFormatting},
};

use super::*;
use crate::{
    lsp_store,
    settings::{Formatter, Settings},
    workspace_edit,
};

actions!(editor, [FormatDocument, FormatSelection]);

/// How long saving waits for the formatter before saving unformatted.
pub(crate) const FORMAT_ON_SAVE_TIMEOUT: Duration = Duration::from_secs(2);
/// External formatters are stopped after this long.
const FORMATTER_TIMEOUT: Duration = Duration::from_secs(20);

/// Who formats a file.
pub(crate) enum Plan {
    /// The language server's document or range formatting.
    Server(Arc<RunningServer>),
    /// A program reading the text on stdin; `fallback` formats instead when
    /// the program is not installed.
    External {
        command: String,
        args: Vec<String>,
        fallback: Option<Arc<RunningServer>>,
    },
    /// Waits `delay`, then returns `output`.
    #[cfg(test)]
    Fake { delay: Duration, output: String },
}

/// What to format.
pub(crate) struct Job {
    /// The file (for `{file}` in a formatter's arguments).
    pub path: PathBuf,
    /// The document as open on the server (server formatting needs it).
    pub uri: Option<lsp::types::Uri>,
    pub text: String,
    /// Format only this range (server range formatting).
    pub range: Option<lsp::types::Range>,
    pub options: FormattingOptions,
}

fn formats_documents(server: &RunningServer) -> bool {
    matches!(
        server.capabilities.document_formatting_provider,
        Some(OneOf::Left(true) | OneOf::Right(_))
    )
}

fn formats_ranges(server: &RunningServer) -> bool {
    matches!(
        server.capabilities.document_range_formatting_provider,
        Some(OneOf::Left(true) | OneOf::Right(_))
    )
}

/// The formatter for `language` files: the configured one, else the
/// language server (Python: `ruff format` when installed, else the server).
pub(crate) fn plan(
    settings: &Settings,
    language: Language,
    server: Option<Arc<RunningServer>>,
) -> Result<Plan, String> {
    let server = server.filter(|server| formats_documents(server));
    match settings.formatter(language) {
        Formatter::Server => server.map(Plan::Server).ok_or_else(|| {
            format!(
                "No formatter for {} files: the language server cannot format them \
                 (set \"formatter\" in settings.json)",
                language.name()
            )
        }),
        Formatter::External {
            command,
            args,
            fallback,
        } => Ok(Plan::External {
            command,
            args,
            fallback: server.filter(|_| fallback),
        }),
    }
}

/// The formatted text of `job` (the whole text, also for a range).
pub(crate) async fn run(
    plan: Plan,
    project: Option<Project>,
    job: Job,
    executor: BackgroundExecutor,
) -> Result<String, String> {
    match plan {
        Plan::Server(server) => server_format(&server, &job, &executor).await,
        Plan::External {
            command,
            args,
            fallback,
        } => {
            let project = project.ok_or("the file has no project")?;
            let lookup = (project.clone(), command.clone());
            let program = executor
                .spawn(async move { lookup.0.find_executable(&lookup.1) })
                .await
                .map_err(|err| format!("cannot look up {command}: {err}"))?;
            let Some(program) = program else {
                return match fallback {
                    Some(server) => server_format(&server, &job, &executor).await,
                    None => Err(format!("{command} not found")),
                };
            };
            if job.range.is_some() {
                return Err(format!("{command} cannot format a selection"));
            }
            let file = job.path.to_string_lossy().into_owned();
            let args: Vec<String> = args
                .iter()
                .map(|arg| arg.replace("{file}", &file))
                .collect();
            let program = program.to_string_lossy().into_owned();
            let name = command.clone();
            let output = executor
                .spawn(async move {
                    project.run_program(
                        &program,
                        &args,
                        &[],
                        Some(job.text.as_bytes()),
                        FORMATTER_TIMEOUT,
                    )
                })
                .await
                .map_err(|err| format!("{name}: {err}"))?;
            if output.code != Some(0) {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let message = stderr.lines().find(|line| !line.trim().is_empty());
                return Err(match (message, output.code) {
                    (Some(message), _) => format!("{command}: {}", message.trim()),
                    (None, Some(code)) => format!("{command} exited with code {code}"),
                    (None, None) => format!("{command} was stopped"),
                });
            }
            String::from_utf8(output.stdout).map_err(|_| format!("{command} wrote invalid UTF-8"))
        }
        #[cfg(test)]
        Plan::Fake { delay, output } => {
            executor.timer(delay).await;
            Ok(output)
        }
    }
}

async fn server_format(
    server: &RunningServer,
    job: &Job,
    executor: &BackgroundExecutor,
) -> Result<String, String> {
    let uri = job
        .uri
        .clone()
        .ok_or("the file is not open on the language server")?;
    let text_document = TextDocumentIdentifier::new(uri);
    let retry = Duration::from_millis(400);
    let edits = match job.range {
        Some(range) => {
            if !formats_ranges(server) {
                return Err("the language server cannot format a selection".into());
            }
            let params = DocumentRangeFormattingParams {
                text_document,
                range,
                options: job.options.clone(),
                work_done_progress_params: Default::default(),
            };
            lsp_store::request_retrying::<RangeFormatting>(server, params, executor, 1, retry).await
        }
        None => {
            let params = DocumentFormattingParams {
                text_document,
                options: job.options.clone(),
                work_done_progress_params: Default::default(),
            };
            lsp_store::request_retrying::<Formatting>(server, params, executor, 1, retry).await
        }
    };
    let edits = edits.map_err(|err| match err {
        lsp::Error::Response { message, .. } => message,
        err => err.to_string(),
    })?;
    workspace_edit::edit_text(&job.text, &edits.unwrap_or_default(), server.encoding)
}

#[derive(Default)]
pub(super) struct FormatState {
    _task: Option<Task<()>>,
    /// Formats with this (delay, output) instead.
    #[cfg(test)]
    pub fake: Option<(Duration, String)>,
    /// Format on save regardless of settings.json.
    #[cfg(test)]
    pub on_save: bool,
}

impl Editor {
    fn formatting_options(&self) -> FormattingOptions {
        FormattingOptions {
            tab_size: self.indent.width as u32,
            insert_spaces: !self.indent.tabs,
            ..FormattingOptions::default()
        }
    }

    /// The formatter and the job for this buffer (`range`: a selection).
    fn format_job(&self, range: Option<Range<usize>>) -> Result<(Plan, Job), String> {
        let (Some(language), Some(path)) = (self.language(), self.buffer.path()) else {
            return Err("No formatter for this file".into());
        };
        let encoding = self
            .lsp
            .as_ref()
            .map_or(Encoding::Utf16, |doc| doc.server.encoding);
        let job = Job {
            path: path.to_path_buf(),
            uri: self.lsp.as_ref().map(|doc| doc.uri.clone()),
            text: self.text(),
            range: range.map(|range| lsp::types::Range {
                start: language::to_position(&self.buffer, range.start, encoding),
                end: language::to_position(&self.buffer, range.end, encoding),
            }),
            options: self.formatting_options(),
        };
        #[cfg(test)]
        if let Some((delay, output)) = self.format.fake.clone() {
            return Ok((Plan::Fake { delay, output }, job));
        }
        let plan = plan(&crate::settings::current(), language, self.server())?;
        Ok((plan, job))
    }

    /// Starts formatting; the task yields the whole formatted text.
    fn format_task(
        &self,
        range: Option<Range<usize>>,
        cx: &mut Context<Self>,
    ) -> Result<Task<Result<String, String>>, String> {
        let (plan, job) = self.format_job(range)?;
        let executor = cx.background_executor().clone();
        let project = self.project.clone();
        Ok(cx.background_spawn(run(plan, project, job, executor)))
    }

    fn start_format(&mut self, range: Option<Range<usize>>, cx: &mut Context<Self>) {
        let version = self.version;
        let task = match self.format_task(range, cx) {
            Ok(task) => task,
            Err(message) => {
                self.notice = Some(message.into());
                return cx.notify();
            }
        };
        self.format._task = Some(cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                let applied = result.and_then(|text| this.apply_formatted(version, &text, cx));
                if let Err(message) = applied {
                    this.notice = Some(format!("Format failed: {message}").into());
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    pub(super) fn format_document(
        &mut self,
        _: &FormatDocument,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.start_format(None, cx);
    }

    /// The selections' span (the cursor's line when nothing is selected).
    pub(super) fn format_selection(
        &mut self,
        _: &FormatSelection,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let primary = self.primary();
        let range = if primary.is_empty() {
            let line = self.buffer.line_of(primary.head);
            self.buffer.line_start(line)..self.buffer.line_end(line)
        } else {
            primary.range()
        };
        self.start_format(Some(range), cx);
    }

    /// Replaces the text with `formatted` (computed from the text at
    /// `version`) through minimal edits, as one undo step.
    pub(super) fn apply_formatted(
        &mut self,
        version: u64,
        formatted: &str,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        if self.version != version {
            return Err("the file changed while it was being formatted".into());
        }
        let formatted = if self.buffer.line_ending() == "\r\n" {
            formatted.replace("\r\n", "\n").replace('\n', "\r\n")
        } else {
            formatted.to_owned()
        };
        let edits = workspace_edit::minimal_edits(&self.text(), &formatted);
        if !edits.is_empty() {
            self.apply_edits_keeping_selections(edits, cx);
        }
        Ok(())
    }

    fn format_on_save(&self) -> bool {
        #[cfg(test)]
        if self.format.on_save {
            return true;
        }
        self.project.is_some() && crate::settings::current().format_on_save(self.language())
    }

    /// Ctrl+S: formats first when `format_on_save` is on, giving the
    /// formatter [`FORMAT_ON_SAVE_TIMEOUT`] before saving unformatted.
    pub(super) fn save_with_format(&mut self, cx: &mut Context<Self>) {
        if !self.format_on_save() {
            self.save(cx);
            return;
        }
        let version = self.version;
        // Without a formatter there is nothing to wait for.
        let Ok(task) = self.format_task(None, cx) else {
            self.save(cx);
            return;
        };
        let timeout = cx.background_executor().timer(FORMAT_ON_SAVE_TIMEOUT);
        self.format._task = Some(cx.spawn(async move |this, cx| {
            let outcome = select(task, timeout).await;
            this.update(cx, |this, cx| {
                let problem = match outcome {
                    Either::Left((Ok(text), _)) => this
                        .apply_formatted(version, &text, cx)
                        .err()
                        .map(|err| format!("Not formatted: {err}")),
                    Either::Left((Err(err), _)) => Some(format!("Formatting failed: {err}")),
                    Either::Right(_) => Some(format!(
                        "Formatting took longer than {} s",
                        FORMAT_ON_SAVE_TIMEOUT.as_secs()
                    )),
                };
                if this.save(cx)
                    && let Some(problem) = problem
                {
                    this.notice = Some(format!("{problem}; saved unformatted").into());
                    cx.notify();
                }
            })
            .ok();
        }));
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use gpui::TestAppContext;

    use super::*;
    use crate::editor::tests::{marked, parse};

    /// An editor for `marked` saved as `name` in a temporary project.
    fn project_editor(
        name: &str,
        marked_text: &str,
        cx: &mut TestAppContext,
    ) -> (PathBuf, gpui::WindowHandle<Editor>) {
        let root = std::env::temp_dir().join(format!(
            "next_code_format_{}_{}",
            name.replace('.', "_"),
            std::process::id()
        ));
        fs::remove_dir_all(&root).ok();
        fs::create_dir_all(&root).unwrap();
        let (text, selections) = parse(marked_text);
        let path = root.join(name);
        fs::write(&path, &text).unwrap();
        let project = Project::local(root.clone());
        let window = cx.add_window(|_, cx| {
            let mut editor = Editor::for_file(Buffer::with_path(&text, path.clone()), project, cx);
            editor.selections = selections;
            editor.format.on_save = true;
            editor
        });
        (path, window)
    }

    #[gpui::test]
    fn formatting_keeps_the_cursor_through_minimal_edits(cx: &mut TestAppContext) {
        let (path, window) =
            project_editor("a.rs", "fn main() {\nlet x = va|lue;\n  call(x);\n}\n", cx);
        window
            .update(cx, |e, _, cx| {
                e.format.fake = Some((
                    Duration::from_millis(100),
                    "fn main() {\n    let x = value;\n    call(x);\n}\n".into(),
                ));
                e.save_with_format(cx);
            })
            .unwrap();
        cx.executor().advance_clock(Duration::from_millis(150));
        cx.run_until_parked();
        let (state, dirty, notice) = window
            .read_with(cx, |e, _| {
                (marked(e), e.buffer.is_dirty(), e.notice.clone())
            })
            .unwrap();
        assert_eq!(state, "fn main() {\n    let x = va|lue;\n    call(x);\n}\n");
        assert!(!dirty);
        assert_eq!(notice, None);
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "fn main() {\n    let x = value;\n    call(x);\n}\n"
        );
        // One undo step restores the unformatted text and the cursor.
        window.update(cx, |e, w, cx| e.undo(&Undo, w, cx)).unwrap();
        let state = window.read_with(cx, |e, _| marked(e)).unwrap();
        assert_eq!(state, "fn main() {\nlet x = va|lue;\n  call(x);\n}\n");
        fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[gpui::test]
    fn a_slow_formatter_does_not_hold_up_saving(cx: &mut TestAppContext) {
        let (path, window) = project_editor("b.rs", "fn f() {\nx|;\n}\n", cx);
        window
            .update(cx, |e, _, cx| {
                e.format.fake = Some((Duration::from_secs(30), "formatted\n".into()));
                e.type_text("y", cx);
                e.save_with_format(cx);
            })
            .unwrap();
        cx.run_until_parked();
        assert!(window.read_with(cx, |e, _| e.buffer.is_dirty()).unwrap());
        cx.executor().advance_clock(FORMAT_ON_SAVE_TIMEOUT);
        cx.run_until_parked();
        let (dirty, notice) = window
            .read_with(cx, |e, _| (e.buffer.is_dirty(), e.notice.clone()))
            .unwrap();
        assert!(!dirty, "saved after the time limit");
        assert_eq!(fs::read_to_string(&path).unwrap(), "fn f() {\nxy;\n}\n");
        assert_eq!(
            notice.as_ref().map(|notice| notice.as_ref()),
            Some("Formatting took longer than 2 s; saved unformatted")
        );
        fs::remove_dir_all(path.parent().unwrap()).ok();
    }
}
