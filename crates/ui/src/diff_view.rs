//! A tab showing one file's changes: staged or unstaged, or between two
//! revisions (or a revision and the working tree). Unified or side by side,
//! changed hunks only or the whole file.

use std::{ops::Range, path::PathBuf};

use git::{
    DIFF_CONTEXT, Entry, FULL_FILE, FileChange, FileDiff, HunkAction, LineKind, Patch, Repository,
    Row, Side, SideLine, SplitRow,
};
use gpui::{
    App, Context, EventEmitter, FocusHandle, Focusable, Global, PromptLevel, SharedString, Task,
    Window, div, prelude::*, px, relative, uniform_list,
};

use crate::{commit_panel::short_revision, project::Project, theme};

const ROW_HEIGHT: f32 = 20.;

/// What a diff compares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffSource {
    /// Staged (HEAD → index) or unstaged (index → working tree) changes.
    Index { entry: Entry, staged: bool },
    /// One file between two sides.
    Revisions {
        old: Side,
        new: Side,
        change: FileChange,
    },
}

impl DiffSource {
    /// Repository-relative path of the file (new name for renames).
    pub fn path(&self) -> &str {
        match self {
            Self::Index { entry, .. } => &entry.path,
            Self::Revisions { change, .. } => &change.path,
        }
    }

    fn orig_path(&self) -> Option<&str> {
        match self {
            Self::Index { entry, .. } => entry.orig_path.as_deref(),
            Self::Revisions { change, .. } => change.orig_path.as_deref(),
        }
    }

    /// Which versions are compared, e.g. `HEAD ↔ index` or `main ↔ working tree`.
    pub fn sides(&self) -> String {
        let side = |side: &Side| match side {
            Side::Empty => "nothing".to_owned(),
            Side::Revision(revision) => short_revision(revision).to_owned(),
            Side::Worktree => "working tree".to_owned(),
        };
        match self {
            Self::Index { staged: true, .. } => "HEAD ↔ index".into(),
            Self::Index { staged: false, .. } => "index ↔ working tree".into(),
            Self::Revisions { old, new, .. } => format!("{} ↔ {}", side(old), side(new)),
        }
    }

    /// Computes the diff with `context` lines, reading the working tree
    /// where the project lives. Index diffs come from `git diff` itself, with
    /// the patch kept for hunk actions; other files (untracked, binary) and
    /// revision comparisons are diffed here. Blocks.
    pub fn compute(
        &self,
        project: &Project,
        repository: &Repository,
        context: usize,
    ) -> std::io::Result<(FileDiff, Option<Patch>)> {
        let worktree = || project.worktree_file(repository, self.path());
        match self {
            Self::Index { entry, staged } => {
                if let Some(patch) = repository.index_patch(entry, *staged, context)? {
                    return Ok((patch.to_file_diff(), Some(patch)));
                }
                Ok((
                    repository.file_diff(entry, *staged, context, worktree)?,
                    None,
                ))
            }
            Self::Revisions { old, new, change } => Ok((
                repository.revision_diff(old, new, change, context, worktree)?,
                None,
            )),
        }
    }
}

/// How diffs are shown; the last choice applies to newly opened diffs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DiffOptions {
    pub side_by_side: bool,
    pub whole_file: bool,
}

impl Global for DiffOptions {}

pub enum DiffViewEvent {
    /// Open the file at a 0-based line (from clicking a line).
    OpenFile { path: PathBuf, line: usize },
    /// A hunk was staged, unstaged or discarded.
    RepositoryChanged,
}

impl EventEmitter<DiffViewEvent> for DiffView {}

pub struct DiffView {
    project: Project,
    repository: Repository,
    source: DiffSource,
    path: PathBuf,
    options: DiffOptions,
    diff: Option<FileDiff>,
    /// The `git diff` behind an index diff, for hunk actions.
    patch: Option<Patch>,
    split: Vec<SplitRow>,
    error: Option<SharedString>,
    focus_handle: FocusHandle,
    _load: Task<()>,
    _apply: Task<()>,
}

impl DiffView {
    pub fn new(
        project: Project,
        repository: Repository,
        source: DiffSource,
        cx: &mut Context<Self>,
    ) -> Self {
        let path = project.repository_path(&repository, source.path());
        let options = cx.try_global::<DiffOptions>().copied().unwrap_or_default();
        let mut view = Self {
            project,
            repository,
            source,
            path,
            options,
            diff: None,
            patch: None,
            split: Vec::new(),
            error: None,
            focus_handle: cx.focus_handle(),
            _load: Task::ready(()),
            _apply: Task::ready(()),
        };
        view.reload(cx);
        view
    }

    pub fn source(&self) -> &DiffSource {
        &self.source
    }

    pub fn title(&self) -> SharedString {
        let path = self.source.path();
        let name = path.rsplit('/').next().unwrap_or(path);
        let suffix = match &self.source {
            DiffSource::Index { staged: true, .. } => "staged".to_owned(),
            DiffSource::Index { staged: false, .. } => "changes".to_owned(),
            DiffSource::Revisions { .. } => self.source.sides(),
        };
        format!("{name} ({suffix})").into()
    }

    /// Recomputes the diff from the repository and working tree.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        let (project, repository, source) = (
            self.project.clone(),
            self.repository.clone(),
            self.source.clone(),
        );
        let context = if self.options.whole_file {
            FULL_FILE
        } else {
            DIFF_CONTEXT
        };
        self._load = cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { source.compute(&project, &repository, context) })
                .await;
            this.update(cx, |this, cx| {
                match result {
                    Ok((diff, patch)) => {
                        this.split = git::side_by_side(&diff.rows);
                        this.diff = Some(diff);
                        this.patch = patch;
                        this.error = None;
                    }
                    Err(err) => this.error = Some(err.to_string().into()),
                }
                cx.notify();
            })
            .ok();
        });
    }

    /// The hunk actions this diff offers.
    fn hunk_actions(&self) -> &'static [(HunkAction, &'static str)] {
        match (&self.source, &self.patch) {
            (DiffSource::Index { staged: false, .. }, Some(_)) => &[
                (HunkAction::Discard, "Discard"),
                (HunkAction::Stage, "Stage hunk"),
            ],
            (DiffSource::Index { staged: true, .. }, Some(_)) => {
                &[(HunkAction::Unstage, "Unstage hunk")]
            }
            _ => &[],
        }
    }

    fn apply_hunk(
        &mut self,
        hunk: usize,
        action: HunkAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(patch) = self.patch.clone() else {
            return;
        };
        let confirm = (action == HunkAction::Discard).then(|| {
            window.prompt(
                PromptLevel::Warning,
                "Discard this change?",
                Some("The working tree loses it. This cannot be undone."),
                &["Discard", "Cancel"],
                cx,
            )
        });
        let repository = self.repository.clone();
        self._apply = cx.spawn(async move |this, cx| {
            if let Some(confirm) = confirm
                && confirm.await != Ok(0)
            {
                return;
            }
            let result = cx
                .background_spawn(async move { repository.apply_hunk(&patch, hunk, action) })
                .await;
            this.update(cx, |this, cx| {
                if let Err(err) = result {
                    this.error = Some(err.to_string().into());
                }
                this.reload(cx);
                cx.emit(DiffViewEvent::RepositoryChanged);
            })
            .ok();
        });
    }

    /// A hunk's `@@` line with its actions.
    fn render_hunk(
        &self,
        row: usize,
        hunk: usize,
        header: &str,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        div()
            .w_full()
            .h(px(ROW_HEIGHT))
            .px_2()
            .flex()
            .items_center()
            .gap_2()
            .bg(theme::panel())
            .text_color(theme::accent_text())
            .child(div().flex_1().min_w_0().truncate().child(header.to_owned()))
            .children(self.hunk_actions().iter().map(|&(action, label)| {
                div()
                    .id((label, row))
                    .px_2()
                    .rounded_sm()
                    .font_family(theme::UI_FONT)
                    .text_xs()
                    .text_color(theme::text())
                    .bg(theme::border())
                    .cursor_pointer()
                    .hover(|style| style.bg(theme::active_row()))
                    .child(label)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.apply_hunk(hunk, action, window, cx)
                    }))
            }))
            .into_any_element()
    }

    fn set_options(&mut self, options: DiffOptions, cx: &mut Context<Self>) {
        let reload = options.whole_file != self.options.whole_file;
        self.options = options;
        cx.set_global(options);
        if reload {
            self.reload(cx);
        }
        cx.notify();
    }

    /// Line numbers, sign and text of one diff line.
    fn line_cells(
        kind: LineKind,
        numbers: [Option<usize>; 2],
        show_both: bool,
        text: &str,
    ) -> gpui::Div {
        let number = |line: Option<usize>| {
            div()
                .w(px(44.))
                .flex_none()
                .pr_2()
                .flex()
                .justify_end()
                .text_color(theme::muted())
                .child(line.map(|line| line.to_string()).unwrap_or_default())
        };
        let (sign, background) = match kind {
            LineKind::Context => (" ", None),
            LineKind::Added => ("+", Some(theme::diff_added())),
            LineKind::Removed => ("-", Some(theme::diff_removed())),
        };
        div()
            .h(px(ROW_HEIGHT))
            .flex()
            .items_center()
            .overflow_hidden()
            .when_some(background, |row, color| row.bg(color))
            .child(number(numbers[0]))
            .when(show_both, |row| row.child(number(numbers[1])))
            .child(div().w(px(16.)).flex_none().child(sign))
            .child(
                div()
                    .min_w_0()
                    .whitespace_nowrap()
                    .child(text.replace('\t', "    ")),
            )
    }

    fn skipped(lines: usize) -> gpui::AnyElement {
        div()
            .w_full()
            .h(px(ROW_HEIGHT))
            .px_2()
            .flex()
            .items_center()
            .bg(theme::panel())
            .text_color(theme::muted())
            .child(format!("⋯ {lines} unchanged lines"))
            .into_any_element()
    }

    /// Clicking a line opens the file there (the new side when it exists).
    fn open_line(&self, line: Option<usize>, cx: &mut Context<Self>) {
        if let Some(line) = line {
            cx.emit(DiffViewEvent::OpenFile {
                path: self.path.clone(),
                line: line.saturating_sub(1),
            });
        }
    }

    fn render_unified(&self, index: usize, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(diff) = &self.diff else {
            return div().into_any_element();
        };
        match &diff.rows[index] {
            Row::Skipped { lines } => Self::skipped(*lines),
            Row::Hunk {
                index: hunk,
                header,
            } => self.render_hunk(index, *hunk, header, cx),
            Row::Line {
                kind,
                old,
                new,
                text,
            } => {
                let target = new.or(*old);
                Self::line_cells(*kind, [*old, *new], true, text)
                    .w_full()
                    .id(index)
                    .hover(|style| style.bg(theme::hover_row()))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| this.open_line(target, cx)))
                    .into_any_element()
            }
        }
    }

    fn render_split(&self, index: usize, cx: &mut Context<Self>) -> gpui::AnyElement {
        match &self.split[index] {
            SplitRow::Skipped { lines } => Self::skipped(*lines),
            SplitRow::Hunk {
                index: hunk,
                header,
            } => self.render_hunk(index, *hunk, header, cx),
            SplitRow::Pair { left, right } => {
                let half = |side: &Option<SideLine>, id: &'static str| -> gpui::AnyElement {
                    match side {
                        Some(line) => {
                            let target = Some(line.line);
                            let clickable = line.kind != LineKind::Removed;
                            Self::line_cells(line.kind, [Some(line.line), None], false, &line.text)
                                .id((id, index))
                                .flex_1()
                                .flex_basis(relative(0.))
                                .min_w_0()
                                .when(clickable, |cell| {
                                    cell.cursor_pointer()
                                        .hover(|style| style.bg(theme::hover_row()))
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.open_line(target, cx)
                                        }))
                                })
                                .into_any_element()
                        }
                        None => div()
                            .flex_1()
                            .flex_basis(relative(0.))
                            .min_w_0()
                            .h(px(ROW_HEIGHT))
                            .bg(theme::panel())
                            .into_any_element(),
                    }
                };
                // Two equal halves whatever their text: the row spans the
                // pane and each side gets half of it.
                div()
                    .w_full()
                    .flex()
                    .child(half(left, "left"))
                    .child(
                        div()
                            .w(px(1.))
                            .h(px(ROW_HEIGHT))
                            .flex_none()
                            .bg(theme::border()),
                    )
                    .child(half(right, "right"))
                    .into_any_element()
            }
        }
    }

    fn render_toggle(
        &self,
        id: &'static str,
        label: &'static str,
        on: bool,
        options: DiffOptions,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        div()
            .id(id)
            .px_2()
            .rounded_sm()
            .cursor_pointer()
            .text_color(if on { theme::text() } else { theme::muted() })
            .when(on, |toggle| toggle.bg(theme::active_row()))
            .hover(|style| style.text_color(theme::text()))
            .child(label)
            .on_click(cx.listener(move |this, _, _, cx| this.set_options(options, cx)))
    }
}

impl Focusable for DiffView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for DiffView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let summary = match &self.diff {
            Some(diff) if diff.binary => "binary file".to_owned(),
            Some(diff) => format!("+{} −{}", diff.added, diff.removed),
            None => "loading…".to_owned(),
        };
        let mut name = self.source.path().to_owned();
        if let Some(orig) = self.source.orig_path() {
            name = format!("{orig} → {name}");
        }
        let header = format!("{name} · {} · {summary}", self.source.sides());
        let options = self.options;
        let toolbar = div()
            .flex()
            .gap_1()
            .text_xs()
            .child(self.render_toggle(
                "unified",
                "Unified",
                !options.side_by_side,
                DiffOptions {
                    side_by_side: false,
                    ..options
                },
                cx,
            ))
            .child(self.render_toggle(
                "split",
                "Side by side",
                options.side_by_side,
                DiffOptions {
                    side_by_side: true,
                    ..options
                },
                cx,
            ))
            .child(div().w(px(8.)))
            .child(self.render_toggle(
                "hunks",
                "Changes only",
                !options.whole_file,
                DiffOptions {
                    whole_file: false,
                    ..options
                },
                cx,
            ))
            .child(self.render_toggle(
                "whole",
                "Whole file",
                options.whole_file,
                DiffOptions {
                    whole_file: true,
                    ..options
                },
                cx,
            ));
        let body = match (&self.error, &self.diff) {
            (Some(error), _) => div()
                .p_3()
                .text_color(theme::error())
                .child(error.clone())
                .into_any_element(),
            (None, Some(diff)) if diff.rows.is_empty() => div()
                .p_3()
                .text_color(theme::muted())
                .child(if diff.binary {
                    "Binary files are not shown."
                } else {
                    "No differences in content."
                })
                .into_any_element(),
            (None, Some(_)) if options.side_by_side => uniform_list(
                "diff-split",
                self.split.len(),
                cx.processor(|this, range: Range<usize>, _, cx| {
                    range.map(|index| this.render_split(index, cx)).collect()
                }),
            )
            .flex_1()
            .w_full()
            .into_any_element(),
            (None, Some(diff)) => uniform_list(
                "diff-rows",
                diff.rows.len(),
                cx.processor(|this, range: Range<usize>, _, cx| {
                    range.map(|index| this.render_unified(index, cx)).collect()
                }),
            )
            .flex_1()
            .w_full()
            .into_any_element(),
            (None, None) => div().into_any_element(),
        };
        div()
            .track_focus(&self.focus_handle)
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::background())
            .font_family(theme::MONO_FONT)
            .text_size(px(theme::EDITOR_FONT_SIZE))
            .text_color(theme::text())
            .child(
                div()
                    .px_3()
                    .py_1()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .border_b_1()
                    .border_color(theme::border())
                    .font_family(theme::UI_FONT)
                    .text_sm()
                    .text_color(theme::muted())
                    .child(div().min_w_0().truncate().child(header))
                    .child(toolbar),
            )
            .child(body)
    }
}
