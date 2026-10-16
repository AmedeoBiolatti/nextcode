//! The Commit window's Shelf tab (PyCharm's): shelved change lists, newest
//! first, each expandable to its files. Unshelve (keeping or removing the
//! shelf), a file's diff (base ↔ shelved), Delete. Shelves are
//! `git::Shelf`s, stored under the repository's `.next_code/shelf/`.

use std::{collections::HashSet, io, ops::Range};

use git::{ApplyOutcome, Change, Repository, Shelf, WorktreeFiles};
use gpui::{
    App, ClickEvent, Context, EventEmitter, FocusHandle, Focusable, MouseButton, MouseDownEvent,
    Pixels, Point, PromptLevel, SharedString, Task, Window, anchored, deferred, div, prelude::*,
    px, uniform_list,
};

use crate::{
    change_tree::{self, Node},
    commit_panel::{INDENT, ROW_HEIGHT, change_color, row_text},
    debugger::panel::Tooltip,
    diff_view::DiffSource,
    git_log::relative_time,
    project::Project,
    selectable::selectable,
    theme,
};

/// Shelf files read and written through the project (locally, in WSL or
/// in a container): what both the UI and headless mode hand to the git
/// crate's shelf operations.
pub(crate) struct ProjectFiles<'a> {
    project: &'a Project,
    repository: &'a Repository,
}

impl<'a> ProjectFiles<'a> {
    pub fn new(project: &'a Project, repository: &'a Repository) -> Self {
        Self {
            project,
            repository,
        }
    }
}

impl WorktreeFiles for ProjectFiles<'_> {
    fn read(&self, path: &str) -> io::Result<Vec<u8>> {
        self.project
            .read_bytes(&self.project.repository_path(self.repository, path))
    }

    fn write(&self, path: &str, data: &[u8]) -> io::Result<()> {
        let file = self.project.repository_path(self.repository, path);
        if let Some(parent) = file.parent() {
            self.project.create_dir_all(parent)?;
        }
        self.project.write(&file, data.to_vec())
    }
}

/// "Unshelved 2 files" / conflicts, for the Commit window's report line.
pub(crate) fn unshelve_message(shelf: &str, outcome: &ApplyOutcome, removed: bool) -> String {
    match outcome {
        ApplyOutcome::Done { .. } if removed => format!("Unshelved '{shelf}' and removed it"),
        ApplyOutcome::Done { .. } => format!("Unshelved '{shelf}' (kept on the shelf)"),
        ApplyOutcome::Conflicts { paths } => format!(
            "Unshelving '{shelf}' left conflicts in {}: resolve them under Merge Conflicts (the \
             shelf is kept)",
            paths.join(", ")
        ),
    }
}

pub enum ShelfViewEvent {
    OpenDiff {
        repository: Repository,
        source: DiffSource,
    },
    /// An unshelve or delete finished; `conflicts`: files to resolve.
    Changed {
        message: String,
        conflicts: bool,
    },
    Failed(String),
}

impl EventEmitter<ShelfViewEvent> for ShelfView {}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ShelfRow {
    Shelf(usize),
    File {
        shelf: usize,
        file: usize,
        name: String,
        dir: String,
    },
}

impl ShelfRow {
    fn shelf(&self) -> usize {
        match self {
            Self::Shelf(shelf) | Self::File { shelf, .. } => *shelf,
        }
    }
}

pub struct ShelfView {
    project: Project,
    repository: Option<Repository>,
    shelves: Vec<Shelf>,
    loaded: bool,
    error: Option<SharedString>,
    /// Expanded shelves, by id.
    expanded: HashSet<String>,
    rows: Vec<ShelfRow>,
    selected: Option<usize>,
    menu: Option<(Point<Pixels>, usize)>,
    busy: bool,
    focus_handle: FocusHandle,
    _load: Task<()>,
    _operation: Task<()>,
}

impl ShelfView {
    pub fn new(project: Project, cx: &mut Context<Self>) -> Self {
        Self {
            project,
            repository: None,
            shelves: Vec::new(),
            loaded: false,
            error: None,
            expanded: HashSet::new(),
            rows: Vec::new(),
            selected: None,
            menu: None,
            busy: false,
            focus_handle: cx.focus_handle(),
            _load: Task::ready(()),
            _operation: Task::ready(()),
        }
    }

    /// Lists `repository`'s shelves from the next refresh; another
    /// repository's shelves are cleared at once.
    pub fn set_repository(&mut self, repository: Option<Repository>) {
        let root = |repository: &Option<Repository>| {
            repository
                .as_ref()
                .map(|r| crate::commit_panel::repository_key(r.root()))
        };
        if root(&self.repository) != root(&repository) {
            self.shelves.clear();
            self.rows.clear();
            self.selected = None;
            self.menu = None;
            self.loaded = false;
            self.error = None;
            // A read of the previous repository's shelves is stale.
            self._load = Task::ready(());
        }
        self.repository = repository;
    }

    /// Re-reads the shelves.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(repository) = self.repository.clone() else {
            return;
        };
        let project = self.project.clone();
        self._load = cx.spawn(async move |this, cx| {
            let shelves = cx
                .background_spawn(async move {
                    repository.shelves(&ProjectFiles::new(&project, &repository))
                })
                .await;
            this.update(cx, |this, cx| {
                let selected = this.selected_key();
                match shelves {
                    Ok(shelves) => {
                        this.shelves = shelves;
                        this.error = None;
                    }
                    Err(err) => this.error = Some(err.to_string().into()),
                }
                this.loaded = true;
                this.rebuild_rows();
                this.selected = selected.and_then(|key| this.find_row(&key));
                cx.notify();
            })
            .ok();
        });
    }

    fn rebuild_rows(&mut self) {
        let mut rows = Vec::new();
        for (index, shelf) in self.shelves.iter().enumerate() {
            rows.push(ShelfRow::Shelf(index));
            if !self.expanded.contains(&shelf.id) {
                continue;
            }
            let paths: Vec<&str> = shelf.files.iter().map(|file| file.path.as_str()).collect();
            rows.extend(
                change_tree::list(&paths)
                    .into_iter()
                    .filter_map(|node| match node {
                        Node::File {
                            item, name, dir, ..
                        } => Some(ShelfRow::File {
                            shelf: index,
                            file: item,
                            name,
                            dir,
                        }),
                        Node::Dir { .. } => None,
                    }),
            );
        }
        self.rows = rows;
        if self.selected.is_some_and(|row| row >= self.rows.len()) {
            self.selected = None;
        }
    }

    /// The selected row as (shelf id, file path), to find it again.
    fn selected_key(&self) -> Option<(String, Option<String>)> {
        let row = self.rows.get(self.selected?)?;
        let shelf = self.shelves.get(row.shelf())?;
        let file = match row {
            ShelfRow::Shelf(_) => None,
            ShelfRow::File { file, .. } => Some(shelf.files.get(*file)?.path.clone()),
        };
        Some((shelf.id.clone(), file))
    }

    fn find_row(&self, key: &(String, Option<String>)) -> Option<usize> {
        self.rows.iter().position(|row| {
            let Some(shelf) = self.shelves.get(row.shelf()) else {
                return false;
            };
            let file = match row {
                ShelfRow::Shelf(_) => None,
                ShelfRow::File { file, .. } => shelf.files.get(*file).map(|f| f.path.clone()),
            };
            shelf.id == key.0 && file == key.1
        })
    }

    fn toggle_expanded(&mut self, shelf: usize, cx: &mut Context<Self>) {
        let Some(id) = self.shelves.get(shelf).map(|shelf| shelf.id.clone()) else {
            return;
        };
        if !self.expanded.remove(&id) {
            self.expanded.insert(id);
        }
        let selected = self.selected_key();
        self.rebuild_rows();
        self.selected = selected.and_then(|key| self.find_row(&key));
        cx.notify();
    }

    fn selected_shelf(&self) -> Option<usize> {
        Some(self.rows.get(self.selected?)?.shelf())
    }

    fn show_diff(&mut self, row: usize, cx: &mut Context<Self>) {
        let (Some(ShelfRow::File { shelf, file, .. }), Some(repository)) =
            (self.rows.get(row), self.repository.clone())
        else {
            return;
        };
        let shelf = &self.shelves[*shelf];
        cx.emit(ShelfViewEvent::OpenDiff {
            repository,
            source: DiffSource::Shelved {
                shelf: shelf.id.clone(),
                name: shelf.message.clone(),
                change: shelf.files[*file].clone(),
            },
        });
    }

    fn click_row(
        &mut self,
        row: usize,
        event: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.menu = None;
        self.selected = Some(row);
        window.focus(&self.focus_handle);
        if event.click_count() >= 2 {
            match self.rows.get(row) {
                Some(ShelfRow::Shelf(shelf)) => self.toggle_expanded(*shelf, cx),
                Some(ShelfRow::File { .. }) => self.show_diff(row, cx),
                None => {}
            }
        }
        cx.notify();
    }

    fn open_menu(
        &mut self,
        row: usize,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.selected = Some(row);
        window.focus(&self.focus_handle);
        self.menu = Some((event.position, row));
        cx.notify();
    }

    /// Asks whether to keep the shelf, then applies it.
    pub fn unshelve(&mut self, shelf: usize, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(shelf), Some(repository)) = (self.shelves.get(shelf), self.repository.clone())
        else {
            return;
        };
        if self.busy {
            return;
        }
        let (id, name) = (shelf.id.clone(), shelf.message.clone());
        let answer = window.prompt(
            PromptLevel::Info,
            &format!("Unshelve '{name}'?"),
            Some(
                "Its changes are applied to the working tree. If they conflict with the files \
                 now, the conflicts are left to resolve and the shelf is kept.",
            ),
            &["Unshelve and Remove", "Unshelve and Keep", "Cancel"],
            cx,
        );
        let project = self.project.clone();
        self._operation = cx.spawn(async move |this, cx| {
            let remove = match answer.await {
                Ok(0) => true,
                Ok(1) => false,
                _ => return,
            };
            this.update(cx, |this, cx| {
                this.busy = true;
                cx.notify();
            })
            .ok();
            let result = cx
                .background_spawn(async move {
                    repository.unshelve(&ProjectFiles::new(&project, &repository), &id, remove)
                })
                .await;
            this.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(outcome) => {
                        let conflicts = matches!(outcome, ApplyOutcome::Conflicts { .. });
                        let message = unshelve_message(&name, &outcome, remove && !conflicts);
                        cx.emit(ShelfViewEvent::Changed { message, conflicts });
                    }
                    Err(err) => cx.emit(ShelfViewEvent::Failed(err.to_string())),
                }
                this.refresh(cx);
            })
            .ok();
        });
    }

    /// Asks, then deletes the shelf.
    pub fn delete(&mut self, shelf: usize, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(shelf), Some(repository)) = (self.shelves.get(shelf), self.repository.clone())
        else {
            return;
        };
        let (id, name) = (shelf.id.clone(), shelf.message.clone());
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("Delete shelf '{name}'?"),
            Some("Its changes are lost. This cannot be undone."),
            &["Delete", "Cancel"],
            cx,
        );
        self._operation = cx.spawn(async move |this, cx| {
            if answer.await != Ok(0) {
                return;
            }
            let result = cx
                .background_spawn(async move { repository.delete_shelf(&id) })
                .await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(()) => cx.emit(ShelfViewEvent::Changed {
                        message: format!("Deleted shelf '{name}'"),
                        conflicts: false,
                    }),
                    Err(err) => cx.emit(ShelfViewEvent::Failed(err.to_string())),
                }
                this.refresh(cx);
            })
            .ok();
        });
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let has_shelf = self.selected_shelf().is_some();
        let selected_file = matches!(
            self.selected.and_then(|row| self.rows.get(row)),
            Some(ShelfRow::File { .. })
        );
        let button = |id: &'static str, glyph: &'static str, title: &'static str, enabled: bool| {
            div()
                .id(id)
                .flex_none()
                .w(px(24.))
                .h(px(22.))
                .flex()
                .items_center()
                .justify_center()
                .rounded_sm()
                .text_sm()
                .text_color(if enabled {
                    theme::text()
                } else {
                    theme::muted()
                })
                .cursor_pointer()
                .hover(|style| style.bg(theme::hover_row()))
                .tooltip(move |window, cx| Tooltip::view(title, window, cx))
                .child(glyph)
        };
        div()
            .h(px(28.))
            .px_1()
            .flex()
            .flex_none()
            .items_center()
            .gap_0p5()
            .border_b_1()
            .border_color(theme::border())
            .child(
                button("shelf-refresh", "⟳", "Refresh", true)
                    .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
            )
            .child(
                button("shelf-unshelve", "⇪", "Unshelve…", has_shelf).on_click(cx.listener(
                    |this, _, window, cx| {
                        if let Some(shelf) = this.selected_shelf() {
                            this.unshelve(shelf, window, cx)
                        }
                    },
                )),
            )
            .child(
                button("shelf-show-diff", "⇄", "Show Diff", selected_file).on_click(cx.listener(
                    |this, _, _, cx| {
                        if let Some(row) = this.selected {
                            this.show_diff(row, cx)
                        }
                    },
                )),
            )
            .child(
                button("shelf-delete", "✕", "Delete…", has_shelf).on_click(cx.listener(
                    |this, _, window, cx| {
                        if let Some(shelf) = this.selected_shelf() {
                            this.delete(shelf, window, cx)
                        }
                    },
                )),
            )
    }

    fn render_row(&self, index: usize, cx: &mut Context<Self>) -> gpui::AnyElement {
        let row = &self.rows[index];
        let selected = self.selected == Some(index);
        let depth = match row {
            ShelfRow::Shelf(_) => 0,
            ShelfRow::File { .. } => 1,
        };
        let base = div()
            .id(("shelf-row", index))
            .w_full()
            .h(px(ROW_HEIGHT))
            .pl(px(6. + INDENT * depth as f32))
            .pr_2()
            .flex()
            .items_center()
            .gap_1p5()
            .text_sm()
            .when(selected, |row| row.bg(theme::active_row()))
            .when(!selected, |row| {
                row.hover(|style| style.bg(theme::hover_row()))
            })
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                this.click_row(index, event, window, cx)
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    this.open_menu(index, event, window, cx)
                }),
            );
        match row {
            ShelfRow::Shelf(shelf_index) => {
                let shelf = &self.shelves[*shelf_index];
                let expanded = self.expanded.contains(&shelf.id);
                let shelf_index = *shelf_index;
                let count = match shelf.files.len() {
                    1 => "1 file".to_owned(),
                    n => format!("{n} files"),
                };
                base.child(
                    div()
                        .id(("shelf-chevron", index))
                        .w(px(12.))
                        .flex_none()
                        .text_xs()
                        .text_color(theme::muted())
                        .child(if expanded { "▾" } else { "▸" })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.toggle_expanded(shelf_index, cx)
                        })),
                )
                .child(
                    div()
                        .flex_none()
                        .max_w(gpui::relative(0.7))
                        .truncate()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .child(row_text("name", shelf.message.clone())),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_xs()
                        .text_color(theme::muted())
                        .child(row_text(
                            "detail",
                            format!("{count} · {}", relative_time(shelf.date)),
                        )),
                )
                .into_any_element()
            }
            ShelfRow::File {
                shelf,
                file,
                name,
                dir,
            } => {
                let change = self.shelves[*shelf].files[*file].change;
                base.child(
                    div()
                        .flex_none()
                        .text_color(change_color(change))
                        .when(change == Change::Deleted, |name| name.line_through())
                        .child(row_text("name", name.clone())),
                )
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_xs()
                        .text_color(theme::muted())
                        .child(row_text("detail", dir.clone())),
                )
                .into_any_element()
            }
        }
    }

    fn render_menu(&self, cx: &mut Context<Self>) -> Option<impl IntoElement + use<>> {
        let (position, row) = self.menu?;
        let target = self.rows.get(row)?;
        let shelf = target.shelf();
        type Handler = Box<dyn Fn(&mut ShelfView, &mut Window, &mut Context<ShelfView>)>;
        let mut items: Vec<(&'static str, Handler)> = Vec::new();
        if matches!(target, ShelfRow::File { .. }) {
            items.push((
                "Show Diff",
                Box::new(move |this, _, cx| this.show_diff(row, cx)),
            ));
        }
        items.push((
            "Unshelve…",
            Box::new(move |this, window, cx| this.unshelve(shelf, window, cx)),
        ));
        items.push((
            "Delete…",
            Box::new(move |this, window, cx| this.delete(shelf, window, cx)),
        ));
        let rows = items
            .into_iter()
            .enumerate()
            .map(|(index, (label, handler))| {
                div()
                    .id(("shelf-menu-item", index))
                    .px_3()
                    .py_1()
                    .rounded_sm()
                    .cursor_pointer()
                    .hover(|style| style.bg(theme::active_row()))
                    .child(label)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.menu = None;
                        handler(this, window, cx);
                        cx.notify();
                    }))
            });
        Some(deferred(
            anchored().position(position).child(
                div()
                    .id("shelf-menu")
                    .occlude()
                    .min_w(px(200.))
                    .p_1()
                    .flex()
                    .flex_col()
                    .text_sm()
                    .text_color(theme::text())
                    .bg(theme::panel())
                    .border_1()
                    .border_color(theme::border())
                    .rounded_md()
                    .shadow_lg()
                    .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                        this.menu = None;
                        cx.notify();
                    }))
                    .children(rows),
            ),
        ))
    }
}

impl Focusable for ShelfView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ShelfView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = if let Some(error) = self.error.clone() {
            div()
                .p_3()
                .text_sm()
                .text_color(theme::error())
                .child(selectable("shelf-error", error))
                .into_any_element()
        } else if self.loaded && self.shelves.is_empty() {
            div()
                .flex_1()
                .flex()
                .flex_col()
                .items_center()
                .pt_6()
                .px_3()
                .text_center()
                .gap_1()
                .text_sm()
                .text_color(theme::muted())
                .child(selectable("shelf-empty", "No shelved changes"))
                .child(div().text_xs().child(selectable(
                    "shelf-empty-detail",
                    "Shelve changes from the Changes tab: right-click › Shelve Changes…",
                )))
                .into_any_element()
        } else {
            uniform_list(
                "shelf-rows",
                self.rows.len(),
                cx.processor(|this, range: Range<usize>, _, cx| {
                    range.map(|index| this.render_row(index, cx)).collect()
                }),
            )
            .flex_1()
            .w_full()
            .into_any_element()
        };
        div()
            .key_context("ShelfView")
            .track_focus(&self.focus_handle)
            .size_full()
            .flex()
            .flex_col()
            .child(self.render_toolbar(cx))
            .child(div().flex_1().min_h_0().flex().flex_col().child(body))
            .when(self.busy, |view| {
                view.child(
                    div()
                        .px_2()
                        .py_1()
                        .text_xs()
                        .text_color(theme::muted())
                        .child("Unshelving…"),
                )
            })
            .children(self.render_menu(cx))
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, path::Path, process::Command, rc::Rc};

    use gpui::TestAppContext;

    use super::*;

    fn git(dir: &Path, args: &[&str]) {
        let mut command = Command::new("git");
        remote::launch::no_window(&mut command);
        let output = command.arg("-C").arg(dir).args(args).output().unwrap();
        assert!(output.status.success(), "git {args:?}: {output:?}");
    }

    #[gpui::test]
    fn the_shelf_lists_shelves_and_shows_a_shelved_files_diff(cx: &mut TestAppContext) {
        let dir = std::env::temp_dir().join(format!("next_code_shelf_view_{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();
        git(&dir, &["init", "-q", "-b", "main"]);
        for (key, value) in [
            ("user.name", "Test"),
            ("user.email", "test@example.com"),
            ("commit.gpgsign", "false"),
            ("core.autocrlf", "false"),
        ] {
            git(&dir, &["config", key, value]);
        }
        std::fs::write(dir.join("a.txt"), "one\ntwo\n").unwrap();
        git(&dir, &["add", "-A"]);
        git(&dir, &["commit", "-q", "-m", "first"]);
        std::fs::write(dir.join("a.txt"), "one\nTWO\n").unwrap();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/new.rs"), "fn main() {}\n").unwrap();

        // Shelved through the project, as the Shelve dialog does.
        let project = Project::local(dir.clone());
        let repository = project.repository_in(0).unwrap().unwrap();
        let files = ProjectFiles::new(&project, &repository);
        let paths = ["a.txt".to_owned(), "src/new.rs".to_owned()];
        repository.shelve(&files, &paths, "Login fix").unwrap();
        assert!(repository.status().unwrap().entries.is_empty());

        let view = cx.new(|cx| ShelfView::new(project.clone(), cx));
        let opened = Rc::new(RefCell::new(Vec::new()));
        cx.update(|cx| {
            let opened = opened.clone();
            cx.subscribe(&view, move |_, event, _| {
                if let ShelfViewEvent::OpenDiff { source, .. } = event {
                    opened.borrow_mut().push(source.clone());
                }
            })
            .detach();
        });
        view.update(cx, |view, cx| {
            view.set_repository(Some(repository.clone()));
            view.refresh(cx);
        });
        cx.run_until_parked();
        view.update(cx, |view, cx| {
            assert_eq!(view.rows, [ShelfRow::Shelf(0)]);
            assert_eq!(view.shelves[0].message, "Login fix");
            view.toggle_expanded(0, cx);
            let files: Vec<(&str, &str)> = view
                .rows
                .iter()
                .filter_map(|row| match row {
                    ShelfRow::File { name, dir, .. } => Some((name.as_str(), dir.as_str())),
                    ShelfRow::Shelf(_) => None,
                })
                .collect();
            assert_eq!(files, [("a.txt", ""), ("new.rs", "src")]);
            view.show_diff(1, cx);
        });
        let source = opened.borrow_mut().pop().expect("a diff opened");
        assert_eq!(source.path(), "a.txt");
        let (diff, _) = source.compute(&project, &repository, 3).unwrap();
        assert_eq!((diff.added, diff.removed), (1, 1));
        std::fs::remove_dir_all(&dir).ok();
    }
}
