//! A tab comparing a base revision with another revision or the working
//! tree: the changed files (as a compact tree or a list), each opening its
//! diff.

use std::collections::HashSet;

use git::{FileChange, Repository, Side};
use gpui::{
    App, Context, EventEmitter, FocusHandle, Focusable, SharedString, Task, Window, div,
    prelude::*, px, uniform_list,
};

use crate::{
    change_tree::{self, Node},
    commit_panel::{change_color, short_revision},
    diff_view::DiffSource,
    pinpoint::Pin,
    selectable::selectable,
    theme,
};

const ROW_HEIGHT: f32 = 24.;

pub enum CompareEvent {
    OpenDiff {
        repository: Repository,
        source: DiffSource,
    },
    ChangeBase(Repository),
    ChangeTarget(Repository),
    TitleChanged,
}

impl EventEmitter<CompareEvent> for CompareView {}

pub struct CompareView {
    repository: Repository,
    base: String,
    target: Side,
    changes: Option<Vec<FileChange>>,
    error: Option<SharedString>,
    as_tree: bool,
    collapsed: HashSet<String>,
    nodes: Vec<Node>,
    focus_handle: FocusHandle,
    _load: Task<()>,
}

impl CompareView {
    pub fn new(repository: Repository, base: String, target: Side, cx: &mut Context<Self>) -> Self {
        let mut view = Self {
            repository,
            base,
            target,
            changes: None,
            error: None,
            as_tree: true,
            collapsed: HashSet::new(),
            nodes: Vec::new(),
            focus_handle: cx.focus_handle(),
            _load: Task::ready(()),
        };
        view.reload(cx);
        view
    }

    pub fn sides(&self) -> (&String, &Side) {
        (&self.base, &self.target)
    }

    fn target_label(&self) -> String {
        match &self.target {
            Side::Worktree => "working tree".into(),
            Side::Revision(revision) => short_revision(revision).into(),
            Side::Empty => "nothing".into(),
        }
    }

    pub fn title(&self) -> SharedString {
        format!(
            "Compare {} ↔ {}",
            short_revision(&self.base),
            self.target_label()
        )
        .into()
    }

    pub fn set_base(&mut self, base: String, cx: &mut Context<Self>) {
        self.base = base;
        self.reload(cx);
        cx.emit(CompareEvent::TitleChanged);
    }

    pub fn set_target(&mut self, target: Side, cx: &mut Context<Self>) {
        self.target = target;
        self.reload(cx);
        cx.emit(CompareEvent::TitleChanged);
    }

    fn swap(&mut self, cx: &mut Context<Self>) {
        if let Side::Revision(target) = &self.target {
            let target = target.clone();
            self.target = Side::Revision(std::mem::replace(&mut self.base, target));
            self.reload(cx);
            cx.emit(CompareEvent::TitleChanged);
        }
    }

    /// Re-reads the changed files.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        let (repository, base, target) = (
            self.repository.clone(),
            self.base.clone(),
            self.target.clone(),
        );
        self._load = cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { repository.changes(&base, &target) })
                .await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(mut changes) => {
                        changes.sort_by(|a, b| a.path.cmp(&b.path));
                        this.changes = Some(changes);
                        this.error = None;
                    }
                    Err(err) => this.error = Some(err.to_string().into()),
                }
                this.rebuild();
                cx.notify();
            })
            .ok();
        });
    }

    fn rebuild(&mut self) {
        let paths: Vec<&str> = self
            .changes
            .iter()
            .flatten()
            .map(|change| change.path.as_str())
            .collect();
        self.nodes = if self.as_tree {
            change_tree::tree(&paths, &self.collapsed)
        } else {
            change_tree::list(&paths)
        };
    }

    fn activate(&mut self, index: usize, cx: &mut Context<Self>) {
        match self.nodes.get(index) {
            Some(Node::Dir { path, .. }) => {
                if !self.collapsed.remove(path) {
                    self.collapsed.insert(path.clone());
                }
                self.rebuild();
                cx.notify();
            }
            Some(&Node::File { item, .. }) => {
                let Some(change) = self.changes.as_ref().and_then(|c| c.get(item)) else {
                    return;
                };
                cx.emit(CompareEvent::OpenDiff {
                    repository: self.repository.clone(),
                    source: DiffSource::Revisions {
                        old: Side::Revision(self.base.clone()),
                        new: self.target.clone(),
                        change: change.clone(),
                    },
                });
            }
            None => {}
        }
    }

    fn render_node(&self, index: usize, cx: &mut Context<Self>) -> gpui::AnyElement {
        let changes = self.changes.as_deref().unwrap_or_default();
        render_change_node(
            &self.nodes[index],
            changes,
            index,
            cx.listener(move |this, _, _, cx| this.activate(index, cx)),
        )
    }

    fn button(
        id: &'static str,
        label: impl Into<SharedString>,
        cx: &mut Context<Self>,
        on_click: impl Fn(&mut Self, &mut Context<Self>) + 'static,
    ) -> impl IntoElement {
        div()
            .id(id)
            .px_2()
            .py_px()
            .rounded_sm()
            .border_1()
            .border_color(theme::border())
            .cursor_pointer()
            .hover(|style| style.bg(theme::hover_row()))
            .child(label.into())
            .on_click(cx.listener(move |this, _, _, cx| on_click(this, cx)))
    }
}

/// A row of a changed-files tree or list: folders toggle, files open.
/// The names are selectable; a double-click on them reaches the row (the
/// git log's details pane opens a file's diff on double-click).
pub fn render_change_node(
    node: &Node,
    changes: &[FileChange],
    index: usize,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> gpui::AnyElement {
    let text = |id: &'static str, text: String| selectable(id, text).double_click_through();
    let row = div()
        .id(("change", index))
        .h(px(ROW_HEIGHT))
        .pr_2()
        .flex()
        .items_center()
        .gap_2()
        .text_sm()
        .cursor_pointer()
        .hover(|style| style.bg(theme::hover_row()))
        .on_click(on_click);
    match node {
        Node::Dir {
            path,
            label,
            depth,
            collapsed,
            ..
        } => row
            .pl(px(8. + 14. * *depth as f32))
            .text_color(theme::muted())
            .child(if *collapsed { "▸" } else { "▾" })
            .child(text("label", label.clone()))
            .pin(format!("dir:{path}"))
            .label(path.clone())
            .into_any_element(),
        Node::File {
            item,
            name,
            dir,
            depth,
        } => {
            let change = &changes[*item];
            let renamed = change.orig_path.as_deref().map(|orig| format!("← {orig}"));
            row.pl(px(22. + 14. * *depth as f32))
                .child(
                    div()
                        .flex_none()
                        .text_color(theme::text())
                        .child(text("name", name.clone())),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_xs()
                        .text_color(theme::muted())
                        .child(text("dir", renamed.unwrap_or_else(|| dir.clone()))),
                )
                .child(
                    div()
                        .w(px(12.))
                        .flex_none()
                        .text_color(change_color(change.change))
                        .child(change.change.letter().to_string()),
                )
                .pin(format!("file:{}", change.path))
                .label(change.path.clone())
                .into_any_element()
        }
    }
}

impl Focusable for CompareView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for CompareView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let count = self.changes.as_ref().map_or(0, Vec::len);
        let base = short_revision(&self.base).to_owned();
        let target = self.target_label();
        let can_swap = matches!(self.target, Side::Revision(_));
        let header = div()
            .px_3()
            .py_2()
            .flex()
            .items_center()
            .gap_2()
            .border_b_1()
            .border_color(theme::border())
            .text_sm()
            .child(
                Self::button("base", base, cx, |this, cx| {
                    cx.emit(CompareEvent::ChangeBase(this.repository.clone()))
                })
                .pin("base"),
            )
            .child(div().text_color(theme::muted()).child("↔"))
            .child(
                Self::button("target", target, cx, |this, cx| {
                    cx.emit(CompareEvent::ChangeTarget(this.repository.clone()))
                })
                .pin("target"),
            )
            .when(can_swap, |header| {
                header.child(
                    Self::button("swap", "⇄ Swap", cx, |this, cx| this.swap(cx))
                        .pin("swap")
                        .label("Swap"),
                )
            })
            .child(div().flex_1())
            .child(
                div()
                    .text_xs()
                    .text_color(theme::muted())
                    .child(selectable("count", format!("{count} files"))),
            )
            .child(
                Self::button(
                    "layout",
                    if self.as_tree { "List" } else { "Tree" },
                    cx,
                    |this, cx| {
                        this.as_tree = !this.as_tree;
                        this.rebuild();
                        cx.notify();
                    },
                )
                .pin("layout")
                .label(if self.as_tree { "List" } else { "Tree" }),
            );
        let note = matches!(self.target, Side::Worktree).then(|| {
            div()
                .px_3()
                .py_1()
                .text_xs()
                .text_color(theme::muted())
                .child(selectable(
                    "note",
                    "Includes staged and unstaged changes; untracked files are not listed.",
                ))
        });
        let body = match (&self.error, &self.changes) {
            (Some(error), _) => div()
                .p_3()
                .text_sm()
                .text_color(theme::error())
                .child(selectable("error", error.clone()))
                .into_any_element(),
            (None, Some(changes)) if changes.is_empty() => div()
                .p_3()
                .text_sm()
                .text_color(theme::muted())
                .child(selectable("empty", "No differences."))
                .into_any_element(),
            (None, Some(_)) => uniform_list(
                "compare-files",
                self.nodes.len(),
                cx.processor(|this, range: std::ops::Range<usize>, _, cx| {
                    range.map(|index| this.render_node(index, cx)).collect()
                }),
            )
            .flex_1()
            .pin("files")
            .into_any_element(),
            (None, None) => div()
                .p_3()
                .text_sm()
                .text_color(theme::muted())
                .child(selectable("loading", "Comparing…"))
                .into_any_element(),
        };
        div()
            .track_focus(&self.focus_handle)
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::background())
            .text_color(theme::text())
            .child(header.pin("toolbar"))
            .children(note)
            .child(body)
    }
}
