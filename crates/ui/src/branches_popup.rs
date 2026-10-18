//! PyCharm's branches popup (from the title bar's branch widget): a search
//! box, repository actions, then Recent / Local / Remote branches; each
//! branch opens a submenu of what can be done with it. With several
//! repositories a row of repository buttons leads the popup and chooses
//! whose branches are listed and acted on. The workspace runs the chosen
//! [`PopupCommand`] in that repository.

use git::{BranchTracking, Ref, RefKind, Repository};
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, MouseButton, ScrollHandle,
    SharedString, Subscription, Task, Window, div, prelude::*, px,
};

use crate::{
    commit_panel::repository_name,
    editor::{self, Editor, EditorEvent},
    pinpoint::{Pin, kebab},
    project_picker,
    selectable::selectable,
    theme,
};

const ROW_HEIGHT: f32 = 24.;
/// The repository buttons' row (several repositories only).
const REPOSITORIES_HEIGHT: f32 = 30.;
const RECENT: usize = 5;

/// What the user picked; the workspace carries it out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PopupCommand {
    UpdateProject,
    Commit,
    /// Push this branch (the Push dialog).
    Push(String),
    /// New branch from `start` (default HEAD).
    NewBranch(Option<String>),
    CheckoutRevision,
    Checkout(Ref),
    /// Check out `branch` and rebase it onto the current branch.
    CheckoutAndRebase(String),
    /// Commits and files between the current branch and `branch`.
    Compare(String),
    /// `branch` against the working tree.
    ShowDiff(String),
    /// Rebase the current branch onto `branch`.
    RebaseOnto(String),
    /// Merge `branch` into the current branch.
    Merge(String),
    /// Update a tracked branch from its upstream.
    Update(String),
    Rename(String),
    Delete(Ref),
}

pub enum BranchesPopupEvent {
    Run {
        repository: Repository,
        command: PopupCommand,
    },
    Dismissed,
}

impl EventEmitter<BranchesPopupEvent> for BranchesPopup {}

#[derive(Debug, Clone, PartialEq)]
enum Item {
    Action(&'static str, PopupCommand),
    Header(&'static str),
    Branch {
        branch: Ref,
        tracking: Option<BranchTracking>,
    },
}

impl Item {
    fn selectable(&self) -> bool {
        !matches!(self, Self::Header(_))
    }
}

/// The submenu of a branch: `None` is a separator.
fn branch_commands(
    branch: &Ref,
    current: Option<&str>,
    tracking: Option<&BranchTracking>,
) -> Vec<Option<(String, PopupCommand)>> {
    let name = branch.name.clone();
    let mut items = Vec::new();
    let mut add = |label: String, command: PopupCommand| items.push(Some((label, command)));
    if branch.head {
        add(
            format!("New Branch from '{name}'…"),
            PopupCommand::NewBranch(Some(name.clone())),
        );
        if tracking.is_some_and(|t| t.upstream.is_some()) {
            add("Update".into(), PopupCommand::Update(name.clone()));
        }
        add("Push…".into(), PopupCommand::Push(name.clone()));
        items.push(None);
        items.push(Some(("Rename…".into(), PopupCommand::Rename(name))));
        return items;
    }
    add("Checkout".into(), PopupCommand::Checkout(branch.clone()));
    add(
        format!("New Branch from '{name}'…"),
        PopupCommand::NewBranch(Some(name.clone())),
    );
    if let Some(current) = current {
        add(
            format!("Checkout and Rebase onto '{current}'"),
            PopupCommand::CheckoutAndRebase(name.clone()),
        );
        items.push(None);
        items.push(Some((
            format!("Compare with '{current}'"),
            PopupCommand::Compare(name.clone()),
        )));
        items.push(Some((
            "Show Diff with Working Tree".into(),
            PopupCommand::ShowDiff(name.clone()),
        )));
        items.push(None);
        items.push(Some((
            format!("Rebase '{current}' onto '{name}'"),
            PopupCommand::RebaseOnto(name.clone()),
        )));
        items.push(Some((
            format!("Merge '{name}' into '{current}'"),
            PopupCommand::Merge(name.clone()),
        )));
    } else {
        items.push(None);
        items.push(Some((
            "Show Diff with Working Tree".into(),
            PopupCommand::ShowDiff(name.clone()),
        )));
    }
    items.push(None);
    if branch.kind == RefKind::Branch {
        if tracking.is_some_and(|t| t.upstream.is_some()) {
            items.push(Some(("Update".into(), PopupCommand::Update(name.clone()))));
        }
        items.push(Some(("Push…".into(), PopupCommand::Push(name.clone()))));
        items.push(Some(("Rename…".into(), PopupCommand::Rename(name))));
    }
    items.push(Some((
        "Delete".into(),
        PopupCommand::Delete(branch.clone()),
    )));
    items
}

/// Rows for `query`: actions (without a query), then branch groups.
fn build_items(
    refs: &[Ref],
    tracking: &[BranchTracking],
    recent: &[String],
    query: &str,
) -> Vec<Item> {
    let needle = query.trim().to_lowercase();
    let matches = |r: &&Ref| r.name.to_lowercase().contains(&needle);
    let tracked = |r: &Ref| {
        (r.kind == RefKind::Branch)
            .then(|| tracking.iter().find(|t| t.name == r.name).cloned())
            .flatten()
    };
    let mut items = Vec::new();
    if needle.is_empty() {
        items.extend([
            Item::Action("Update Project…", PopupCommand::UpdateProject),
            Item::Action("Commit…", PopupCommand::Commit),
            Item::Action("Push…", PopupCommand::Push(String::new())),
            Item::Action("New Branch…", PopupCommand::NewBranch(None)),
            Item::Action("Checkout Tag or Revision…", PopupCommand::CheckoutRevision),
        ]);
    }
    let local: Vec<&Ref> = refs
        .iter()
        .filter(|r| r.kind == RefKind::Branch)
        .filter(matches)
        .collect();
    let recent: Vec<&Ref> = recent
        .iter()
        .filter_map(|name| local.iter().find(|r| r.name == *name && !r.head).copied())
        .take(RECENT)
        .collect();
    let mut group = |title: &'static str, branches: Vec<&Ref>| {
        if branches.is_empty() {
            return;
        }
        items.push(Item::Header(title));
        items.extend(branches.into_iter().map(|branch| Item::Branch {
            branch: branch.clone(),
            tracking: tracked(branch),
        }));
    };
    group("Recent", recent);
    // The current branch first, like PyCharm.
    let mut local_sorted = local.clone();
    local_sorted.sort_by_key(|r| !r.head);
    group("Local", local_sorted);
    group(
        "Remote",
        refs.iter()
            .filter(|r| r.kind == RefKind::Remote)
            .filter(matches)
            .collect(),
    );
    items
}

pub struct BranchesPopup {
    /// Every repository of the project; `active` is the one listed.
    repositories: Vec<Repository>,
    active: usize,
    query: Entity<Editor>,
    refs: Vec<Ref>,
    tracking: Vec<BranchTracking>,
    recent: Vec<String>,
    items: Vec<Item>,
    /// Highlighted row (an action or branch).
    selected: usize,
    /// The branch row whose submenu is open, and its highlighted entry.
    submenu: Option<(usize, Option<usize>)>,
    error: Option<SharedString>,
    scroll: ScrollHandle,
    _load: Task<()>,
    _subscription: Subscription,
}

impl BranchesPopup {
    /// Lists `repositories[active]`'s branches (`repositories` must not be
    /// empty).
    pub fn new(
        repositories: Vec<Repository>,
        active: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let query = cx.new(Editor::single_line);
        window.focus(&query.focus_handle(cx));
        let subscription = cx.subscribe(&query, |this, _, event, cx| {
            if let EditorEvent::Edited = event {
                this.filter(cx);
            }
        });
        let active = active.min(repositories.len().saturating_sub(1));
        let mut popup = Self {
            repositories,
            active,
            query,
            refs: Vec::new(),
            tracking: Vec::new(),
            recent: Vec::new(),
            items: Vec::new(),
            selected: 0,
            submenu: None,
            error: None,
            scroll: ScrollHandle::new(),
            _load: Task::ready(()),
            _subscription: subscription,
        };
        popup.filter(cx);
        popup.load(cx);
        popup
    }

    /// Lists repository `index`'s branches.
    fn select_repository(&mut self, index: usize, cx: &mut Context<Self>) {
        if index == self.active || index >= self.repositories.len() {
            return;
        }
        self.active = index;
        self.refs.clear();
        self.tracking.clear();
        self.recent.clear();
        self.error = None;
        self.filter(cx);
        self.load(cx);
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let Some(repository) = self.repositories.get(self.active).cloned() else {
            return;
        };
        self._load = cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_spawn(async move {
                    Ok::<_, std::io::Error>((
                        repository.refs()?,
                        repository.branch_tracking()?,
                        repository.recent_branches(RECENT + 1)?,
                    ))
                })
                .await;
            this.update(cx, |this, cx| {
                match loaded {
                    Ok((refs, tracking, recent)) => {
                        this.refs = refs;
                        this.tracking = tracking;
                        this.recent = recent;
                    }
                    Err(err) => this.error = Some(err.to_string().into()),
                }
                this.filter(cx);
            })
            .ok();
        });
    }

    fn current(&self) -> Option<&str> {
        self.refs
            .iter()
            .find(|r| r.head && r.kind == RefKind::Branch)
            .map(|r| r.name.as_str())
    }

    fn filter(&mut self, cx: &mut Context<Self>) {
        let query = self.query.read(cx).text();
        self.items = build_items(&self.refs, &self.tracking, &self.recent, &query);
        self.selected = self
            .items
            .iter()
            .position(Item::selectable)
            .unwrap_or_default();
        self.submenu = None;
        cx.notify();
    }

    fn commands(&self, row: usize) -> Vec<Option<(String, PopupCommand)>> {
        match self.items.get(row) {
            Some(Item::Branch { branch, tracking }) => {
                branch_commands(branch, self.current(), tracking.as_ref())
            }
            _ => Vec::new(),
        }
    }

    fn run(&mut self, command: PopupCommand, cx: &mut Context<Self>) {
        if let Some(repository) = self.repositories.get(self.active).cloned() {
            cx.emit(BranchesPopupEvent::Run {
                repository,
                command,
            });
        }
    }

    fn activate(&mut self, row: usize, cx: &mut Context<Self>) {
        match self.items.get(row).cloned() {
            Some(Item::Action(_, command)) => self.run(command, cx),
            Some(Item::Branch { .. }) => {
                self.selected = row;
                self.submenu = Some((row, Some(0)));
                cx.notify();
            }
            _ => {}
        }
    }

    fn step(&mut self, forward: bool, cx: &mut Context<Self>) {
        if let Some((row, Some(entry))) = self.submenu {
            let commands = self.commands(row);
            let next = if forward {
                (entry + 1..commands.len()).find(|&i| commands[i].is_some())
            } else {
                (0..entry).rev().find(|&i| commands[i].is_some())
            }
            .unwrap_or(entry);
            self.submenu = Some((row, Some(next)));
            return cx.notify();
        }
        let mut next = self.selected;
        loop {
            next = if forward {
                if next + 1 >= self.items.len() {
                    return;
                }
                next + 1
            } else {
                if next == 0 {
                    return;
                }
                next - 1
            };
            if self.items[next].selectable() {
                break;
            }
        }
        self.selected = next;
        self.submenu = None;
        self.scroll.scroll_to_item(next);
        cx.notify();
    }

    fn select_prev(&mut self, _: &editor::Up, _: &mut Window, cx: &mut Context<Self>) {
        self.step(false, cx);
    }

    fn select_next(&mut self, _: &editor::Down, _: &mut Window, cx: &mut Context<Self>) {
        self.step(true, cx);
    }

    fn confirm(&mut self, _: &editor::Newline, _: &mut Window, cx: &mut Context<Self>) {
        if let Some((row, Some(entry))) = self.submenu {
            if let Some(Some((_, command))) = self.commands(row).get(entry).cloned() {
                self.run(command, cx);
            }
            return;
        }
        self.activate(self.selected, cx);
    }

    fn dismiss(&mut self, _: &project_picker::Dismiss, _: &mut Window, cx: &mut Context<Self>) {
        if self.submenu.take().is_some() {
            return cx.notify();
        }
        cx.emit(BranchesPopupEvent::Dismissed);
    }

    fn repositories_height(&self) -> f32 {
        if self.repositories.len() > 1 {
            REPOSITORIES_HEIGHT
        } else {
            0.
        }
    }

    /// Several repositories: one button each; the listed one is
    /// highlighted.
    fn render_repositories(&self, cx: &mut Context<Self>) -> Option<impl IntoElement + use<>> {
        if self.repositories.len() < 2 {
            return None;
        }
        Some(
            div()
                .h(px(REPOSITORIES_HEIGHT))
                .px_1()
                .flex()
                .items_center()
                .gap_0p5()
                .overflow_hidden()
                .border_b_1()
                .border_color(theme::border())
                .children(
                    self.repositories
                        .iter()
                        .enumerate()
                        .map(|(index, repository)| {
                            let active = index == self.active;
                            div()
                                .id(("branch-repository", index))
                                .h(px(22.))
                                .px_2()
                                .flex()
                                .flex_none()
                                .items_center()
                                .rounded_sm()
                                .cursor_pointer()
                                .text_color(if active {
                                    theme::text()
                                } else {
                                    theme::muted()
                                })
                                .when(active, |button| button.bg(theme::active_row()))
                                .hover(|style| style.bg(theme::hover_row()))
                                .child(repository_name(repository).to_owned())
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.select_repository(index, cx)
                                }))
                                .pin(format!("repository:{}", repository_name(repository)))
                        }),
                )
                .pin("repositories"),
        )
    }

    fn render_branch_row(
        &self,
        index: usize,
        branch: &Ref,
        tracking: Option<&BranchTracking>,
        kind: &str,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let selected = self.selected == index;
        let (ahead, behind) = tracking.map_or((0, 0), |t| (t.ahead, t.behind));
        let upstream = tracking.and_then(|t| t.upstream.clone());
        let label = match branch.kind {
            RefKind::Remote => branch.name.clone(),
            _ => branch.name.clone(),
        };
        div()
            .id(("branch-row", index))
            .h(px(ROW_HEIGHT))
            .px_2()
            .flex()
            .items_center()
            .gap_1p5()
            .rounded_sm()
            .cursor_pointer()
            .when(selected, |row| row.bg(theme::active_row()))
            .on_mouse_move(cx.listener(move |this, _, _, cx| {
                if this.submenu.is_none_or(|(row, _)| row != index) {
                    this.selected = index;
                    this.submenu = Some((index, None));
                    cx.notify();
                }
            }))
            .on_click(cx.listener(move |this, _, _, cx| this.activate(index, cx)))
            .child(
                div()
                    .w(px(12.))
                    .flex_none()
                    .text_xs()
                    .text_color(theme::accent_text())
                    .child(if branch.head { "★" } else { "" }),
            )
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_color(theme::text())
                    .when(branch.head, |name| {
                        name.font_weight(gpui::FontWeight::SEMIBOLD)
                    })
                    .child(selectable("name", label)),
            )
            .when(ahead > 0, |row| {
                row.child(
                    div()
                        .flex_none()
                        .text_xs()
                        .text_color(theme::git_added())
                        .child(selectable("ahead", format!("↑{ahead}"))),
                )
            })
            .when(behind > 0, |row| {
                row.child(
                    div()
                        .flex_none()
                        .text_xs()
                        .text_color(theme::accent_text())
                        .child(selectable("behind", format!("↓{behind}"))),
                )
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_right()
                    .text_xs()
                    .text_color(theme::muted())
                    .child(selectable("upstream", upstream.unwrap_or_default())),
            )
            .child(
                div()
                    .flex_none()
                    .text_xs()
                    .text_color(theme::muted())
                    .child("›"),
            )
            .pin(format!("{kind}:{}", branch.name))
            .label(branch.name.clone())
            .into_any_element()
    }

    fn render_submenu(&self, cx: &mut Context<Self>) -> Option<impl IntoElement + use<>> {
        let (row, highlighted) = self.submenu?;
        let commands = self.commands(row);
        // Line the submenu up with its row (header, repositories and
        // search box above).
        let scroll = self.scroll.offset().y;
        let top = px(42. + self.repositories_height() + ROW_HEIGHT * row as f32) + scroll;
        Some(
            div()
                .id("branch-submenu")
                .occlude()
                .mt(top.max(px(0.)))
                .ml_1()
                .min_w(px(260.))
                .p_1()
                .flex()
                .flex_col()
                .bg(theme::panel())
                .border_1()
                .border_color(theme::border())
                .rounded_md()
                .shadow_lg()
                .children(commands.into_iter().enumerate().map(|(entry, command)| {
                    let Some((label, command)) = command else {
                        return div()
                            .my_1()
                            .h(px(1.))
                            .bg(theme::border())
                            .into_any_element();
                    };
                    let id = format!("item:{}", kebab(&label));
                    div()
                        .id(("branch-command", entry))
                        .h(px(ROW_HEIGHT))
                        .px_3()
                        .flex()
                        .items_center()
                        .rounded_sm()
                        .cursor_pointer()
                        .when(highlighted == Some(entry), |item| {
                            item.bg(theme::active_row())
                        })
                        .hover(|style| style.bg(theme::hover_row()))
                        .child(label)
                        .on_click(cx.listener(move |this, _, _, cx| this.run(command.clone(), cx)))
                        .pin(id)
                        .into_any_element()
                }))
                .pin("submenu"),
        )
    }
}

impl Focusable for BranchesPopup {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.query.focus_handle(cx)
    }
}

impl Render for BranchesPopup {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut section = String::new();
        let rows: Vec<gpui::AnyElement> = self
            .items
            .clone()
            .iter()
            .enumerate()
            .map(|(index, item)| match item {
                Item::Header(title) => {
                    section = kebab(title);
                    div()
                        .h(px(ROW_HEIGHT))
                        .px_2()
                        .flex()
                        .items_end()
                        .pb_0p5()
                        .text_xs()
                        .text_color(theme::muted())
                        .child(selectable(("header", index), *title))
                        .into_any_element()
                }
                Item::Action(label, command) => {
                    let command = command.clone();
                    div()
                        .id(("branch-action", index))
                        .h(px(ROW_HEIGHT))
                        .px_2()
                        .pl(px(26.))
                        .flex()
                        .items_center()
                        .rounded_sm()
                        .cursor_pointer()
                        .when(self.selected == index, |row| row.bg(theme::active_row()))
                        .on_mouse_move(cx.listener(move |this, _, _, cx| {
                            if this.selected != index || this.submenu.is_some() {
                                this.selected = index;
                                this.submenu = None;
                                cx.notify();
                            }
                        }))
                        .child(*label)
                        .on_click(cx.listener(move |this, _, _, cx| this.run(command.clone(), cx)))
                        .pin(format!("action:{}", kebab(label)))
                        .label(*label)
                        .into_any_element()
                }
                Item::Branch { branch, tracking } => {
                    // A branch may be listed twice (recent and local).
                    let kind = if section.starts_with("recent") {
                        "recent"
                    } else {
                        "branch"
                    };
                    self.render_branch_row(index, branch, tracking.as_ref(), kind, cx)
                }
            })
            .collect();
        let main = div()
            .w(px(380.))
            .flex()
            .flex_col()
            .bg(theme::panel())
            .border_1()
            .border_color(theme::border())
            .rounded_md()
            .shadow_lg()
            .children(self.render_repositories(cx))
            .child(
                // A block container: inputs in flex rows can lay out zero
                // wide.
                div()
                    .relative()
                    .h(px(34.))
                    .pl(px(26.))
                    .pr_2()
                    .py_2()
                    .border_b_1()
                    .border_color(theme::border())
                    .child(
                        div()
                            .absolute()
                            .left(px(9.))
                            .top(px(8.))
                            .text_color(theme::muted())
                            .child("⌕"),
                    )
                    .child(self.query.clone().pin("input").label("Search branches")),
            )
            .child(
                div()
                    .id("branch-rows")
                    .max_h(px(520.))
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .p_1()
                    .flex()
                    .flex_col()
                    .children(rows)
                    .pin("rows"),
            )
            .children(self.error.clone().map(|error| {
                div()
                    .px_2()
                    .py_1()
                    .text_xs()
                    .text_color(theme::error())
                    .child(selectable("error", error))
            }));
        div()
            .key_context("ProjectPicker")
            .on_action(cx.listener(Self::select_prev))
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::dismiss))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                cx.emit(BranchesPopupEvent::Dismissed);
                this.submenu = None;
            }))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .flex()
            .items_start()
            .text_sm()
            .text_color(theme::text())
            .child(main)
            .children(self.render_submenu(cx))
            .pin("branches-popup")
            .label("Branches")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn branch(name: &str, kind: RefKind, head: bool) -> Ref {
        Ref {
            name: name.into(),
            kind,
            target: "abc1234".into(),
            head,
        }
    }

    fn labels(items: &[Item]) -> Vec<String> {
        items
            .iter()
            .map(|item| match item {
                Item::Action(label, _) => format!("action {label}"),
                Item::Header(title) => format!("# {title}"),
                Item::Branch { branch, .. } => branch.name.clone(),
            })
            .collect()
    }

    #[test]
    fn branches_are_grouped_recent_local_remote_and_filtered() {
        let refs = [
            branch("feature", RefKind::Branch, false),
            branch("main", RefKind::Branch, true),
            branch("old", RefKind::Branch, false),
            branch("origin/main", RefKind::Remote, false),
            branch("v1", RefKind::Tag, false),
        ];
        let recent = ["main".to_owned(), "old".to_owned(), "gone".to_owned()];
        let items = build_items(&refs, &[], &recent, "");
        assert_eq!(
            labels(&items),
            [
                "action Update Project…",
                "action Commit…",
                "action Push…",
                "action New Branch…",
                "action Checkout Tag or Revision…",
                "# Recent",
                "old",
                "# Local",
                "main",
                "feature",
                "old",
                "# Remote",
                "origin/main",
            ]
        );
        let items = build_items(&refs, &[], &recent, "MAI");
        assert_eq!(
            labels(&items),
            ["# Local", "main", "# Remote", "origin/main"]
        );
    }

    #[test]
    fn submenus_offer_pycharm_branch_actions() {
        let labels = |items: Vec<Option<(String, PopupCommand)>>| -> Vec<String> {
            items
                .into_iter()
                .map(|item| item.map_or("--".into(), |(label, _)| label))
                .collect()
        };
        let tracked = BranchTracking {
            name: "feature".into(),
            upstream: Some("origin/feature".into()),
            ahead: 1,
            behind: 2,
        };
        assert_eq!(
            labels(branch_commands(
                &branch("feature", RefKind::Branch, false),
                Some("main"),
                Some(&tracked)
            )),
            [
                "Checkout",
                "New Branch from 'feature'…",
                "Checkout and Rebase onto 'main'",
                "--",
                "Compare with 'main'",
                "Show Diff with Working Tree",
                "--",
                "Rebase 'main' onto 'feature'",
                "Merge 'feature' into 'main'",
                "--",
                "Update",
                "Push…",
                "Rename…",
                "Delete",
            ]
        );
        assert_eq!(
            labels(branch_commands(
                &branch("main", RefKind::Branch, true),
                Some("main"),
                None
            )),
            ["New Branch from 'main'…", "Push…", "--", "Rename…"]
        );
        let remote = labels(branch_commands(
            &branch("origin/x", RefKind::Remote, false),
            Some("main"),
            None,
        ));
        assert!(remote.contains(&"Merge 'origin/x' into 'main'".to_owned()));
        assert!(!remote.contains(&"Rename…".to_owned()));
        assert_eq!(remote.last().map(String::as_str), Some("Delete"));
    }

    #[gpui::test]
    fn the_repository_row_chooses_whose_branches_are_listed_and_acted_on(
        cx: &mut gpui::TestAppContext,
    ) {
        use std::{cell::RefCell, rc::Rc};

        use crate::commit_panel::test_support;
        let (dir, project) = test_support::three_folders("popup_repos");
        test_support::git(&dir.join("web"), &["branch", "feature"]);
        let repositories: Vec<Repository> = (0..2)
            .map(|root| project.repository_in(root).unwrap().unwrap())
            .collect();
        let window = cx.add_window(|window, cx| BranchesPopup::new(repositories, 1, window, cx));
        cx.run_until_parked();
        let popup = window.update(cx, |_, _, cx| cx.entity()).unwrap();
        let ran = Rc::new(RefCell::new(Vec::new()));
        cx.update(|cx| {
            let ran = ran.clone();
            cx.subscribe(&popup, move |_, event, _| {
                if let BranchesPopupEvent::Run {
                    repository,
                    command,
                } = event
                {
                    let name = repository_name(repository).to_owned();
                    ran.borrow_mut().push((name, command.clone()));
                }
            })
            .detach();
        });
        let branches = |popup: &BranchesPopup| -> Vec<String> {
            popup
                .refs
                .iter()
                .filter(|r| r.kind == RefKind::Branch)
                .map(|r| r.name.clone())
                .collect()
        };
        popup.update(cx, |popup, cx| {
            assert_eq!(branches(popup), ["dev", "feature"]);
            popup.select_repository(0, cx);
            assert!(
                popup.refs.is_empty(),
                "the other repository's branches are gone"
            );
        });
        cx.run_until_parked();
        popup.update(cx, |popup, cx| {
            assert_eq!(branches(popup), ["main"]);
            popup.run(PopupCommand::Commit, cx);
        });
        assert_eq!(*ran.borrow(), [("api".to_owned(), PopupCommand::Commit)]);
        std::fs::remove_dir_all(&dir).ok();
    }
}
