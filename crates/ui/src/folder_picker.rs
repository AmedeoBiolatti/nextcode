//! Folder selection on a project's machine. All filesystem work stays off
//! the UI thread and runs through Project, never the local system dialog.

use std::path::Path;

use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, KeyBinding, MouseButton,
    ScrollStrategy, SharedString, Subscription, Task, UniformListScrollHandle, Window, actions,
    div, prelude::*, px, uniform_list,
};

use crate::{
    editor::{self, Editor, EditorEvent},
    pinpoint::Pin,
    project::{Location, Project},
    selectable::selectable,
    theme, workspace_file,
};

actions!(folder_picker, [Dismiss, OpenFolder, Parent, Home]);
const CONTEXT: &str = "FolderPicker";

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("escape", Dismiss, Some(CONTEXT)),
        KeyBinding::new("ctrl-enter", OpenFolder, Some(CONTEXT)),
        KeyBinding::new("cmd-enter", OpenFolder, Some(CONTEXT)),
        KeyBinding::new("alt-up", Parent, Some(CONTEXT)),
        KeyBinding::new("alt-home", Home, Some(CONTEXT)),
    ]);
}

pub enum FolderPickerEvent {
    Open(String),
    Local,
    Dismissed,
}
impl EventEmitter<FolderPickerEvent> for FolderPicker {}

pub struct FolderPicker {
    project: Project,
    query: Entity<Editor>,
    directory: String,
    requested: String,
    folders: Vec<String>,
    selected: Option<usize>,
    show_hidden: bool,
    loading: bool,
    error: Option<SharedString>,
    generation: u64,
    scroll: UniformListScrollHandle,
    _subscription: Subscription,
    _task: Task<()>,
}

impl FolderPicker {
    pub fn new(project: Project, cx: &mut Context<Self>) -> Self {
        let directory = project.root().to_string_lossy().into_owned();
        let query = cx.new(Editor::single_line);
        let subscription = cx.subscribe(&query, |this, _, event, cx| {
            if let EditorEvent::Edited = event
                && this.query.read(cx).text() != this.requested
            {
                this.generation += 1;
                this.loading = false;
                this.selected = None;
                this.folders.clear();
                this.error = None;
                this._task = Task::ready(());
                cx.notify();
            }
        });
        let mut picker = Self {
            project,
            query,
            directory: directory.clone(),
            requested: String::new(),
            folders: Vec::new(),
            selected: None,
            show_hidden: false,
            loading: false,
            error: None,
            generation: 0,
            scroll: UniformListScrollHandle::new(),
            _subscription: subscription,
            _task: Task::ready(()),
        };
        picker.load(directory, false, cx);
        picker
    }

    fn visible(&self) -> Vec<&String> {
        self.folders
            .iter()
            .filter(|name| self.show_hidden || !name.starts_with('.'))
            .collect()
    }

    fn input_path(&self, cx: &App) -> String {
        let input = self.query.read(cx).text();
        let input = input.trim();
        if input == "~"
            || input.starts_with("~/")
            || input.starts_with('/')
            || (!self.project.is_remote() && Path::new(input).is_absolute())
        {
            input.to_owned()
        } else if input.is_empty() {
            String::new()
        } else {
            self.project
                .join(Path::new(&self.directory), input)
                .to_string_lossy()
                .into_owned()
        }
    }

    fn load(&mut self, path: String, open: bool, cx: &mut Context<Self>) {
        if path.is_empty() {
            self.error = Some("Enter a folder path".into());
            return cx.notify();
        }
        self.generation += 1;
        let generation = self.generation;
        self.requested = path.clone();
        self.query.update(cx, |query, cx| query.set_text(&path, cx));
        self.loading = true;
        self.error = None;
        self.selected = None;
        let project = self.project.clone();
        self._task = cx.spawn(async move |this, cx| {
            let listing = cx
                .background_spawn(async move { project.browse_folders(&path) })
                .await;
            this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                this.loading = false;
                match listing {
                    Ok(listing) => {
                        this.directory = listing.path.clone();
                        this.requested = listing.path.clone();
                        this.query
                            .update(cx, |query, cx| query.set_text(&listing.path, cx));
                        this.folders = listing.folders;
                        this.scroll.scroll_to_item(0, ScrollStrategy::Top);
                        if open {
                            cx.emit(FolderPickerEvent::Open(listing.path));
                        }
                    }
                    Err(error) => {
                        this.folders.clear();
                        this.error = Some(error.to_string().into());
                    }
                }
                cx.notify();
            })
            .ok();
        });
        cx.notify();
    }

    fn enter(&mut self, _: &editor::Newline, _: &mut Window, cx: &mut Context<Self>) {
        if self.loading {
            return;
        }
        match self
            .selected
            .and_then(|index| self.visible().get(index).cloned())
            .cloned()
        {
            Some(name) => self.browse_child(&name, cx),
            None => self.load(self.input_path(cx), false, cx),
        }
    }

    fn browse_child(&mut self, name: &str, cx: &mut Context<Self>) {
        let path = self.project.join(Path::new(&self.directory), name);
        self.load(path.to_string_lossy().into_owned(), false, cx);
    }

    fn select_prev(&mut self, _: &editor::Up, _: &mut Window, cx: &mut Context<Self>) {
        self.select(self.selected.unwrap_or(0).saturating_sub(1), cx);
    }

    fn select_next(&mut self, _: &editor::Down, _: &mut Window, cx: &mut Context<Self>) {
        self.select(self.selected.map_or(0, |index| index + 1), cx);
    }

    fn select(&mut self, index: usize, cx: &mut Context<Self>) {
        if !self.loading && !self.visible().is_empty() {
            let index = index.min(self.visible().len() - 1);
            self.selected = Some(index);
            self.scroll.scroll_to_item(index, ScrollStrategy::Center);
            cx.notify();
        }
    }

    fn open_folder(&mut self, _: &OpenFolder, _: &mut Window, cx: &mut Context<Self>) {
        if !self.loading {
            self.load(self.input_path(cx), true, cx);
        }
    }

    fn parent(&mut self, _: &Parent, _: &mut Window, cx: &mut Context<Self>) {
        let path = if self.directory == "/" {
            "/".into()
        } else {
            workspace_file::parent(&self.directory, self.project.location().posix())
        };
        self.load(path, false, cx);
    }

    fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        self.load("~".into(), false, cx);
    }

    fn dismiss(&mut self, _: &Dismiss, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(FolderPickerEvent::Dismissed);
    }

    fn title(&self) -> String {
        match self.project.location() {
            Location::Remote { host, .. } => format!("Open Project on {host}"),
            Location::Wsl { distro, .. }
            | Location::Workspace {
                distro: Some(distro),
                ..
            } => format!("Open Project in {distro}"),
            Location::DevContainer { .. } => "Open Project in Container".into(),
            _ => "Open Project".into(),
        }
    }
}

impl Focusable for FolderPicker {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.query.focus_handle(cx)
    }
}

impl Render for FolderPicker {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let count = self.visible().len();
        div()
            .key_context(CONTEXT)
            .on_action(cx.listener(Self::enter))
            .on_action(cx.listener(Self::select_prev))
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::open_folder))
            .on_action(cx.listener(Self::parent))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::dismiss))
            .on_mouse_down_out(
                cx.listener(|this, _, window, cx| this.dismiss(&Dismiss, window, cx)),
            )
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .w(px(700.))
            .max_w_full()
            .flex()
            .flex_col()
            .bg(theme::panel())
            .text_color(theme::text())
            .border_1()
            .border_color(theme::border())
            .rounded_md()
            .shadow_lg()
            .text_sm()
            .child(
                div()
                    .px_3()
                    .py_2()
                    .child(selectable("title", self.title()))
                    .pin("title"),
            )
            .child(
                div()
                    .px_3()
                    .py_2()
                    .border_y_1()
                    .border_color(theme::border())
                    .child(self.query.clone())
                    .pin("path")
                    .label("Folder path on this machine"),
            )
            .child(
                div()
                    .px_3()
                    .py_2()
                    .flex()
                    .gap_4()
                    .child(
                        div()
                            .id("parent")
                            .cursor_pointer()
                            .child("Up")
                            .on_click(
                                cx.listener(|this, _, window, cx| this.parent(&Parent, window, cx)),
                            )
                            .pin("parent")
                            .label("Parent folder"),
                    )
                    .child(
                        div()
                            .id("home")
                            .cursor_pointer()
                            .child("Home")
                            .on_click(
                                cx.listener(|this, _, window, cx| this.home(&Home, window, cx)),
                            )
                            .pin("home")
                            .label("Home folder"),
                    )
                    .child(
                        div()
                            .id("hidden")
                            .cursor_pointer()
                            .child(if self.show_hidden {
                                "Hide hidden folders"
                            } else {
                                "Show hidden folders"
                            })
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.show_hidden = !this.show_hidden;
                                this.selected = None;
                                cx.notify();
                            }))
                            .pin("hidden")
                            .label("Toggle hidden folders"),
                    ),
            )
            .when(self.loading, |view| {
                view.child(div().p_3().child("Loading folders…"))
            })
            .children(self.error.clone().map(|error| {
                div()
                    .p_3()
                    .text_color(theme::error())
                    .child(selectable("error", error))
                    .pin("error")
            }))
            .when(
                !self.loading && self.error.is_none() && count == 0,
                |view| {
                    view.child(
                        div()
                            .p_3()
                            .text_color(theme::muted())
                            .child("No folders here"),
                    )
                },
            )
            .when(!self.loading && count > 0, |view| {
                view.child(
                    uniform_list(
                        "folders",
                        count,
                        cx.processor(|this, range: std::ops::Range<usize>, _, cx| {
                            let folders = this.visible();
                            range
                                .map(|index| {
                                    let name = folders[index].clone();
                                    let target = name.clone();
                                    let selected = this.selected == Some(index);
                                    div()
                                        .id(index)
                                        .h(px(28.))
                                        .px_3()
                                        .flex()
                                        .items_center()
                                        .cursor_pointer()
                                        .when(selected, |row| row.bg(theme::active_row()))
                                        .when(!selected, |row| {
                                            row.hover(|style| style.bg(theme::hover_row()))
                                        })
                                        .child(selectable("name", format!("{name}/")))
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.browse_child(&target, cx)
                                        }))
                                        .pin(format!("folder:{name}"))
                                        .label(name.clone())
                                })
                                .collect()
                        }),
                    )
                    .h(px(280.))
                    .track_scroll(self.scroll.clone()),
                )
            })
            .child(
                div()
                    .px_3()
                    .py_2()
                    .border_t_1()
                    .border_color(theme::border())
                    .flex()
                    .justify_between()
                    .items_center()
                    .child(
                        div()
                            .id("local")
                            .cursor_pointer()
                            .text_color(theme::muted())
                            .child("Local Computer…")
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(FolderPickerEvent::Local)))
                            .pin("local")
                            .label("Open a local project instead"),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_4()
                            .child(
                                div()
                                    .id("cancel")
                                    .cursor_pointer()
                                    .child("Cancel")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.dismiss(&Dismiss, window, cx)
                                    }))
                                    .pin("cancel"),
                            )
                            .child(
                                div()
                                    .id("open")
                                    .px_3()
                                    .py_1()
                                    .rounded_sm()
                                    .bg(theme::active_row())
                                    .when(!self.loading, |button| {
                                        button.cursor_pointer().on_click(cx.listener(
                                            |this, _, window, cx| {
                                                this.open_folder(&OpenFolder, window, cx)
                                            },
                                        ))
                                    })
                                    .when(self.loading, |button| button.opacity(0.5))
                                    .child("Open Folder")
                                    .pin("open")
                                    .label("Open this folder as a project"),
                            ),
                    ),
            )
            .child(
                div()
                    .px_3()
                    .pb_2()
                    .text_xs()
                    .text_color(theme::muted())
                    .child("Enter browses · Ctrl/Cmd+Enter opens this folder · Escape cancels"),
            )
            .pin("folder-picker")
            .label(self.title())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::{
        fs,
        time::{Duration, Instant},
    };

    fn settle(picker: &Entity<FolderPicker>, cx: &mut gpui::TestAppContext) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            cx.run_until_parked();
            if !picker.read_with(cx, |picker, _| picker.loading) {
                break;
            }
            assert!(Instant::now() < deadline, "folder listing timed out");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[gpui::test]
    fn remote_navigation_validates_paths_and_keeps_the_machine(cx: &mut gpui::TestAppContext) {
        let root = std::env::temp_dir().join(format!("nc-folder-picker-{}", std::process::id()));
        fs::create_dir_all(root.join("current")).unwrap();
        fs::create_dir_all(root.join("other project/.hidden")).unwrap();
        let root = root.canonicalize().unwrap();
        let project = Project::test_ssh(root.join("current"));
        let picker = cx.new(|cx| FolderPicker::new(project.clone(), cx));
        let opened = cx.new(|cx| {
            cx.subscribe(&picker, |opened: &mut Vec<String>, _, event, _| {
                if let FolderPickerEvent::Open(path) = event {
                    opened.push(path.clone());
                }
            })
            .detach();
            Vec::<String>::new()
        });
        settle(&picker, cx);
        picker.read_with(cx, |picker, _| assert!(picker.error.is_none()));
        picker.update(cx, |picker, cx| {
            picker.load(root.to_string_lossy().into_owned(), false, cx)
        });
        settle(&picker, cx);
        picker.read_with(cx, |picker, _| {
            assert_eq!(
                picker.visible(),
                [&"current".to_owned(), &"other project".to_owned()]
            )
        });
        picker.update(cx, |picker, cx| picker.browse_child("other project", cx));
        settle(&picker, cx);
        picker.read_with(cx, |picker, _| {
            assert!(picker.visible().is_empty());
            assert_eq!(
                picker.project.location_of(&picker.directory),
                Some(Location::Remote {
                    scheme: "ssh".into(),
                    host: "test-host".into(),
                    path: root.join("other project").to_string_lossy().into_owned(),
                })
            );
        });
        picker.update(cx, |picker, cx| {
            picker.show_hidden = true;
            assert_eq!(picker.visible(), [&".hidden".to_owned()]);
            picker.load(picker.directory.clone(), true, cx);
        });
        settle(&picker, cx);
        assert_eq!(
            opened.read_with(cx, |opened, _| opened.clone()),
            [root.join("other project").to_string_lossy().into_owned()]
        );
        picker.update(cx, |picker, cx| {
            picker.load(
                root.join("missing").to_string_lossy().into_owned(),
                true,
                cx,
            );
        });
        settle(&picker, cx);
        picker.read_with(cx, |picker, _| {
            assert!(picker.error.is_some());
            assert_eq!(
                picker.directory,
                root.join("other project").to_string_lossy()
            );
        });
        assert_eq!(
            opened.read_with(cx, |opened, _| opened.len()),
            1,
            "missing folders cannot open"
        );
        // An older in-flight listing cannot overwrite a subsequently typed path.
        picker.update(cx, |picker, cx| {
            picker.load(root.to_string_lossy().into_owned(), false, cx);
            picker
                .query
                .update(cx, |query, cx| query.set_text("/different/path", cx));
        });
        cx.run_until_parked();
        picker.read_with(cx, |picker, cx| {
            assert_eq!(picker.query.read(cx).text(), "/different/path")
        });
        drop(picker);
        fs::remove_dir_all(root).unwrap();
    }
}
