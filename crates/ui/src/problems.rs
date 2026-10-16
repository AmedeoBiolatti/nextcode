//! Problems panel: diagnostics from every file, grouped by file.

use std::path::{Path, PathBuf};

use gpui::{
    Context, Entity, EventEmitter, Hsla, SharedString, Subscription, Window, div, prelude::*, px,
    uniform_list,
};
use language::Location;
use lsp::types::DiagnosticSeverity;

use crate::{
    lsp_store::{LspStore, LspStoreEvent, diagnostic_origin},
    selectable::selectable,
    theme,
};

const ROW_HEIGHT: f32 = 24.;

pub enum ProblemsEvent {
    Open(Location),
}

impl EventEmitter<ProblemsEvent> for ProblemsPanel {}

enum Row {
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
    root: PathBuf,
    store: Entity<LspStore>,
    rows: Vec<Row>,
    _subscription: Subscription,
}

impl ProblemsPanel {
    pub fn new(root: PathBuf, store: Entity<LspStore>, cx: &mut Context<Self>) -> Self {
        let subscription = cx.subscribe(&store, |this, _, event, cx| {
            if let LspStoreEvent::DiagnosticsUpdated(_) = event {
                this.rebuild(cx);
            }
        });
        let mut panel = Self {
            root,
            store,
            rows: Vec::new(),
            _subscription: subscription,
        };
        panel.rebuild(cx);
        panel
    }

    /// Files sorted by path; within a file, by severity then line. Hints
    /// (e.g. "unused") are left to the editor.
    fn rebuild(&mut self, cx: &mut Context<Self>) {
        let store = self.store.read(cx);
        let mut files: Vec<(&Path, Vec<&lsp::types::Diagnostic>)> = store
            .all_diagnostics()
            .map(|(path, diagnostics)| {
                let shown = diagnostics
                    .iter()
                    .filter(|d| d.severity != Some(DiagnosticSeverity::HINT))
                    .collect();
                (path, shown)
            })
            .filter(|(_, shown): &(_, Vec<_>)| !shown.is_empty())
            .collect();
        files.sort_by(|a, b| a.0.cmp(b.0));

        let mut rows = Vec::new();
        for (path, mut diagnostics) in files {
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
            rows.push(Row::File {
                path: path
                    .strip_prefix(&self.root)
                    .unwrap_or(path)
                    .display()
                    .to_string()
                    .into(),
                errors: count(Severity::Error),
                warnings: count(Severity::Warning),
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
        self.rows = rows;
        cx.notify();
    }
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
