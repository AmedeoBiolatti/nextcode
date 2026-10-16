//! Problems panel: diagnostics from every file, grouped by file (and by
//! folder in a multi-root project).

use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
};

use gpui::{
    Context, Entity, EventEmitter, Hsla, SharedString, Subscription, Window, div, prelude::*, px,
    uniform_list,
};
use language::Location;
use lsp::types::DiagnosticSeverity;

use crate::{
    editor::spell::SpellingProblem,
    lsp_store::{LspStore, LspStoreEvent, diagnostic_origin},
    project::Project,
    selectable::selectable,
    theme,
};

const ROW_HEIGHT: f32 = 24.;

pub enum ProblemsEvent {
    Open(Location),
}

impl EventEmitter<ProblemsEvent> for ProblemsPanel {}

enum Row {
    /// A folder of a multi-root project (or the files outside them all).
    Root {
        name: SharedString,
        errors: usize,
        warnings: usize,
    },
    File {
        path: SharedString,
        errors: usize,
        warnings: usize,
    },
    Problem {
        location: Location,
        severity: Severity,
        message: SharedString,
        position: SharedString,
        /// `source(code)`, as VS Code shows it ("rustc(E0308)").
        code: Option<SharedString>,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Severity {
    Error,
    Warning,
    Info,
}

impl Severity {
    fn of(severity: Option<DiagnosticSeverity>) -> Self {
        match severity {
            Some(DiagnosticSeverity::WARNING) => Self::Warning,
            Some(DiagnosticSeverity::INFORMATION | DiagnosticSeverity::HINT) => Self::Info,
            _ => Self::Error,
        }
    }

    fn badge(self) -> (&'static str, Hsla) {
        match self {
            Self::Error => ("✕", theme::error()),
            Self::Warning => ("⚠", theme::warning()),
            Self::Info => ("i", theme::accent_text()),
        }
    }
}

pub struct ProblemsPanel {
    project: Project,
    store: Entity<LspStore>,
    rows: Vec<Row>,
    /// Unknown words of open files (`spell.in_problems`), by path.
    spelling: HashMap<PathBuf, Vec<lsp::types::Diagnostic>>,
    _subscription: Subscription,
}

impl ProblemsPanel {
    pub fn new(project: Project, store: Entity<LspStore>, cx: &mut Context<Self>) -> Self {
        let subscription = cx.subscribe(&store, |this, _, event, cx| {
            if let LspStoreEvent::DiagnosticsUpdated(_) = event {
                this.rebuild(cx);
            }
        });
        let mut panel = Self {
            project,
            store,
            rows: Vec::new(),
            spelling: HashMap::new(),
            _subscription: subscription,
        };
        panel.rebuild(cx);
        panel
    }

    /// Replaces the unknown words listed for `path` (none removes it).
    pub fn set_spelling(
        &mut self,
        path: &Path,
        problems: &[SpellingProblem],
        cx: &mut Context<Self>,
    ) {
        let diagnostics: Vec<lsp::types::Diagnostic> = problems
            .iter()
            .map(|problem| lsp::types::Diagnostic {
                range: problem.range,
                severity: Some(DiagnosticSeverity::INFORMATION),
                source: Some("spelling".into()),
                message: format!("Unknown word \"{}\"", problem.word),
                ..Default::default()
            })
            .collect();
        let changed = match self.spelling.get(path) {
            Some(listed) => *listed != diagnostics,
            None => !diagnostics.is_empty(),
        };
        if !changed {
            return;
        }
        if diagnostics.is_empty() {
            self.spelling.remove(path);
        } else {
            self.spelling.insert(path.to_path_buf(), diagnostics);
        }
        self.rebuild(cx);
    }

    /// Rebuilds the rows (see [`rows`]). Hints (e.g. "unused") are left to
    /// the editor.
    fn rebuild(&mut self, cx: &mut Context<Self>) {
        let store = self.store.read(cx);
        let mut by_path: BTreeMap<&Path, Vec<&lsp::types::Diagnostic>> = BTreeMap::new();
        let spelling = self
            .spelling
            .iter()
            .map(|(path, diagnostics)| (path.as_path(), diagnostics.as_slice()));
        for (path, diagnostics) in store.all_diagnostics().chain(spelling) {
            let shown = diagnostics
                .iter()
                .filter(|d| d.severity != Some(DiagnosticSeverity::HINT));
            by_path.entry(path).or_default().extend(shown);
        }
        self.rows = rows(&self.project, by_path);
        cx.notify();
    }
}

/// One row per file with problems, sorted by path, followed by its
/// problems by severity then line. In a multi-root project files are
/// grouped under a header per folder (in folder order) and shown relative
/// to their folder; files outside every folder come last, under "Other
/// files", with their full path.
fn rows<'a>(
    project: &Project,
    files: impl IntoIterator<Item = (&'a Path, Vec<&'a lsp::types::Diagnostic>)>,
) -> Vec<Row> {
    let multi_root = project.is_multi_root();
    let mut files: Vec<(Option<usize>, &Path, Vec<&lsp::types::Diagnostic>)> = files
        .into_iter()
        .filter(|(_, shown)| !shown.is_empty())
        .map(|(path, shown)| {
            let root = match multi_root {
                true => project.root_index_of(path),
                false => Some(0),
            };
            (root, path, shown)
        })
        .collect();
    files.sort_by_key(|(root, path, _)| (root.unwrap_or(usize::MAX), *path));

    let mut rows = Vec::new();
    // The current folder's header: (its folder, its row).
    let mut header: Option<(Option<usize>, usize)> = None;
    for (root, path, mut diagnostics) in files {
        diagnostics.sort_by_key(|d| {
            (
                Severity::of(d.severity),
                d.range.start.line,
                d.range.start.character,
            )
        });
        let count = |severity| {
            diagnostics
                .iter()
                .filter(|d| Severity::of(d.severity) == severity)
                .count()
        };
        let (errors, warnings) = (count(Severity::Error), count(Severity::Warning));
        if multi_root {
            if header.is_none_or(|(folder, _)| folder != root) {
                header = Some((root, rows.len()));
                let name = root.map_or("Other files", |root| &project.roots()[root].name);
                rows.push(Row::Root {
                    name: name.to_owned().into(),
                    errors: 0,
                    warnings: 0,
                });
            }
            if let Some((_, index)) = header
                && let Row::Root {
                    errors: total_errors,
                    warnings: total_warnings,
                    ..
                } = &mut rows[index]
            {
                *total_errors += errors;
                *total_warnings += warnings;
            }
        }
        let shown = match root {
            Some(root) => path
                .strip_prefix(&project.roots()[root].path)
                .unwrap_or(path),
            None => path,
        };
        rows.push(Row::File {
            path: shown.display().to_string().into(),
            errors,
            warnings,
        });
        for diagnostic in diagnostics {
            let start = diagnostic.range.start;
            rows.push(Row::Problem {
                location: Location {
                    path: path.to_path_buf(),
                    range: diagnostic.range,
                },
                severity: Severity::of(diagnostic.severity),
                message: diagnostic
                    .message
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .to_owned()
                    .into(),
                position: format!("{}:{}", start.line + 1, start.character + 1).into(),
                code: diagnostic_origin(diagnostic).map(Into::into),
            });
        }
    }
    rows
}

impl Render for ProblemsPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.rows.is_empty() {
            return div()
                .size_full()
                .p_3()
                .text_sm()
                .text_color(theme::muted())
                .child(selectable(
                    "empty",
                    "No problems reported by language servers.",
                ))
                .into_any_element();
        }
        uniform_list(
            "problems",
            self.rows.len(),
            cx.processor(|this, range: std::ops::Range<usize>, _, cx| {
                range
                    .map(|index| match &this.rows[index] {
                        Row::Root {
                            name,
                            errors,
                            warnings,
                        } => div()
                            .id(index)
                            .h(px(ROW_HEIGHT))
                            .px_2()
                            .flex()
                            .items_center()
                            .gap_2()
                            .text_sm()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(theme::text())
                            .child(selectable("root", name.clone()))
                            .child(
                                div()
                                    .text_xs()
                                    .font_weight(gpui::FontWeight::NORMAL)
                                    .text_color(theme::muted())
                                    .child(selectable(
                                        "counts",
                                        format!("{errors} errors, {warnings} warnings"),
                                    )),
                            ),
                        Row::File {
                            path,
                            errors,
                            warnings,
                        } => div()
                            .id(index)
                            .h(px(ROW_HEIGHT))
                            .px_2()
                            .flex()
                            .items_center()
                            .gap_2()
                            .text_sm()
                            .text_color(theme::text())
                            .child(selectable("path", path.clone()))
                            .child(div().text_xs().text_color(theme::muted()).child(selectable(
                                "counts",
                                format!("{errors} errors, {warnings} warnings"),
                            ))),
                        Row::Problem {
                            location,
                            severity,
                            message,
                            position,
                            code,
                        } => {
                            let (badge, color) = severity.badge();
                            let location = location.clone();
                            div()
                                .id(index)
                                .h(px(ROW_HEIGHT))
                                .pl(px(28.))
                                .pr_2()
                                .flex()
                                .items_center()
                                .gap_2()
                                .text_sm()
                                .cursor_pointer()
                                .hover(|style| style.bg(theme::hover_row()))
                                .on_click(cx.listener(move |_, _, _, cx| {
                                    cx.emit(ProblemsEvent::Open(location.clone()))
                                }))
                                .child(div().w(px(14.)).flex_none().text_color(color).child(badge))
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .text_color(theme::text())
                                        .child(selectable("message", message.clone())),
                                )
                                .children(code.clone().map(|code| {
                                    div()
                                        .flex_none()
                                        .text_xs()
                                        .text_color(theme::muted())
                                        .child(selectable("code", code))
                                }))
                                .child(
                                    div()
                                        .flex_none()
                                        .text_xs()
                                        .text_color(theme::muted())
                                        .child(selectable("position", position.clone())),
                                )
                        }
                    })
                    .collect()
            }),
        )
        .size_full()
        .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use std::path::MAIN_SEPARATOR;

    use lsp::types::{Diagnostic, Position, Range};

    use super::*;

    fn diagnostic(line: u32, severity: DiagnosticSeverity, message: &str) -> Diagnostic {
        Diagnostic {
            range: Range::new(Position::new(line, 0), Position::new(line, 1)),
            severity: Some(severity),
            message: message.into(),
            ..Default::default()
        }
    }

    /// Each row as text: `# folder (errors/warnings)`, `file (e/w)`, `  message`.
    fn outline(project: &Project, files: &[(PathBuf, Vec<Diagnostic>)]) -> Vec<String> {
        let files = files
            .iter()
            .map(|(path, diagnostics)| (path.as_path(), diagnostics.iter().collect()));
        rows(project, files)
            .iter()
            .map(|row| match row {
                Row::Root {
                    name,
                    errors,
                    warnings,
                } => format!("# {name} ({errors}/{warnings})"),
                Row::File {
                    path,
                    errors,
                    warnings,
                } => format!("{path} ({errors}/{warnings})"),
                Row::Problem { message, .. } => format!("  {message}"),
            })
            .collect()
    }

    #[test]
    fn multi_root_problems_are_grouped_under_their_folder() {
        let base = std::env::temp_dir().join("next_code_problems_roots");
        let (api, web) = (base.join("api"), base.join("web"));
        let project = Project::local_folders(&[&web, &api]);
        let outside = std::env::temp_dir().join("next_code_problems_other.rs");
        let files = vec![
            (
                api.join("src").join("lib.rs"),
                vec![
                    diagnostic(9, DiagnosticSeverity::WARNING, "unused"),
                    diagnostic(2, DiagnosticSeverity::ERROR, "mismatched types"),
                ],
            ),
            (
                outside.clone(),
                vec![diagnostic(0, DiagnosticSeverity::ERROR, "lost")],
            ),
            (
                web.join("app.py"),
                vec![diagnostic(0, DiagnosticSeverity::WARNING, "shadowed")],
            ),
            (
                api.join("build.rs"),
                vec![diagnostic(0, DiagnosticSeverity::ERROR, "no main")],
            ),
            // Only hints: filtered before, so an empty list shows nothing.
            (web.join("empty.py"), vec![]),
        ];
        assert_eq!(
            outline(&project, &files),
            [
                // Folders in the workspace's order, not by path.
                "# web (0/1)".to_owned(),
                "app.py (0/1)".into(),
                "  shadowed".into(),
                "# api (2/1)".into(),
                "build.rs (1/0)".into(),
                "  no main".into(),
                format!("src{MAIN_SEPARATOR}lib.rs (1/1)"),
                "  mismatched types".into(),
                "  unused".into(),
                "# Other files (1/0)".into(),
                format!("{} (1/0)", outside.display()),
                "  lost".into(),
            ]
        );
    }

    #[test]
    fn single_root_problems_have_no_folder_headers() {
        let root = std::env::temp_dir().join("next_code_problems_single");
        let project = Project::local(root.clone());
        let outside = std::env::temp_dir().join("next_code_problems_other.rs");
        let files = vec![
            (
                root.join("src").join("main.rs"),
                vec![diagnostic(0, DiagnosticSeverity::ERROR, "e")],
            ),
            (
                outside.clone(),
                vec![diagnostic(0, DiagnosticSeverity::WARNING, "w")],
            ),
        ];
        // Sorted by full path (`…_other.rs` before `…_single/…`), with no
        // header, as before folders existed.
        assert_eq!(
            outline(&project, &files),
            [
                format!("{} (0/1)", outside.display()),
                "  w".into(),
                format!("src{MAIN_SEPARATOR}main.rs (1/0)"),
                "  e".into(),
            ]
        );
    }
}
