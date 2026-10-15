//! The window: the menu bar (on Windows also the title bar, with drag
//! areas and caption buttons) with the command bar in it, project tabs,
//! and the active project. Every window's root is a [`Shell`]; a project
//! tab holds a `Workspace`, or `Connecting` while a remote project opens.

use std::{
    path::PathBuf,
    rc::Rc,
    sync::{Arc, LazyLock},
};

use gpui::{
    Action, AnyElement, AnyView, App, AppContext, Bounds, Context, Entity, EntityId, FocusHandle,
    Focusable, Image, ImageFormat, KeyBinding, KeyDownEvent, MouseButton, PathPromptOptions,
    PromptLevel, SharedString, Subscription, TitlebarOptions, Window, WindowBounds,
    WindowControlArea, WindowOptions, actions, anchored, deferred, div, img, prelude::*, px, size,
};

use crate::{
    command_palette::{Command, CommandPalette, CommandPaletteEvent},
    commands,
    connecting::{Connecting, ConnectingEvent},
    menus::{self, Item, List, MenuBar, SEPARATOR},
    project::{Location, Project},
    project_picker::{ProjectPicker, ProjectPickerEvent},
    recent,
    settings::Settings,
    theme::{self, Theme},
    theme_picker::{ThemePicker, ThemePickerEvent},
    workspace::{Preload, Workspace, WorkspaceEvent},
};

actions!(
    app,
    [
        OpenProject,
        OpenProjectInNewWindow,
        OpenRecent,
        CloseProject,
        NextProject,
        PreviousProject,
        ShowCommandPalette,
        CustomizeMenus,
        OpenSettings,
        SelectTheme,
        CustomizeTheme,
        Quit,
        About
    ]
);

/// A "File > Recent Projects" entry.
#[derive(Clone, PartialEq, Debug, gpui::Action)]
#[action(namespace = app, no_json)]
pub struct OpenRecentProject {
    pub location: Location,
}

const CONTEXT: &str = "Shell";
/// Windows gets no system title bar: the menu bar takes its place.
const CUSTOM_TITLE_BAR: bool = cfg!(windows);
const MENU_BAR_HEIGHT: f32 = 36.;

static APP_ICON: LazyLock<Arc<Image>> = LazyLock::new(|| {
    Arc::new(Image::from_bytes(
        ImageFormat::Png,
        include_bytes!("../assets/next_code-64.png").to_vec(),
    ))
});

const MENU_WIDTH: f32 = 280.;
const RECENT_IN_MENU: usize = 15;

pub fn bind_keys(cx: &mut App) {
    let ctx = Some(CONTEXT);
    cx.bind_keys([
        KeyBinding::new("secondary-o", OpenProject, ctx),
        KeyBinding::new("secondary-r", OpenRecent, ctx),
        // The last binding is the one shown.
        KeyBinding::new("f1", ShowCommandPalette, ctx),
        KeyBinding::new("secondary-shift-p", ShowCommandPalette, ctx),
        // JetBrains' next/previous project window.
        KeyBinding::new("ctrl-alt-]", NextProject, ctx),
        KeyBinding::new("ctrl-alt-[", PreviousProject, ctx),
    ]);
}

/// What a project tab shows.
enum Content {
    Connecting(Entity<Connecting>),
    Workspace(Entity<Workspace>),
}

impl Content {
    fn id(&self) -> EntityId {
        match self {
            Self::Connecting(view) => view.entity_id(),
            Self::Workspace(view) => view.entity_id(),
        }
    }

    fn view(&self) -> AnyView {
        match self {
            Self::Connecting(view) => view.clone().into(),
            Self::Workspace(view) => view.clone().into(),
        }
    }
}

struct ProjectTab {
    location: Location,
    content: Content,
    _subscription: Subscription,
}

impl ProjectTab {
    fn dirty_count(&self, cx: &App) -> usize {
        match &self.content {
            Content::Workspace(workspace) => workspace.read(cx).dirty_count(cx),
            Content::Connecting(_) => 0,
        }
    }
}

/// A menu row, resolved when its menu opens.
enum Row {
    Separator,
    Command {
        label: SharedString,
        /// `None`: unknown, or not available where focus is.
        action: Option<Box<dyn Action>>,
        keys: Option<SharedString>,
    },
    Submenu {
        label: SharedString,
        rows: Vec<Row>,
    },
}

struct OpenMenu {
    menu: usize,
    rows: Vec<Row>,
    /// Open submenus by row index, outermost first.
    path: Vec<usize>,
}

pub struct Shell {
    focus_handle: FocusHandle,
    tabs: Vec<ProjectTab>,
    active: usize,
    menus: Rc<MenuBar>,
    menu: Option<OpenMenu>,
    /// The palette, and the focus to run its command with.
    palette: Option<(Entity<CommandPalette>, Subscription, Option<FocusHandle>)>,
    picker: Option<(Entity<ProjectPicker>, Subscription)>,
    theme_picker: Option<(Entity<ThemePicker>, Subscription)>,
    notice: Option<SharedString>,
    /// The theme last drawn, and what is wrong with it (dismissable).
    theme: Arc<Theme>,
    theme_notice: Option<SharedString>,
    /// Set once the user confirmed discarding unsaved changes on close.
    close_confirmed: bool,
    /// Windows' icon font for the caption buttons (Segoe Fluent Icons on
    /// Windows 11, Segoe MDL2 Assets on 10; same code points).
    caption_font: SharedString,
    _activation: Subscription,
}

/// A short name for a project tab.
fn tab_title(location: &Location) -> String {
    /// The last component of a Linux path.
    fn linux_name(path: &str) -> &str {
        match path.trim_end_matches('/').rsplit('/').next() {
            Some(name) if !name.is_empty() => name,
            _ => "/",
        }
    }
    match location {
        Location::Local(root) => root
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| root.display().to_string()),
        Location::Wsl { distro, path } => format!("{} [{distro}]", linux_name(path)),
        Location::DevContainer { folder, .. } => match folder.as_ref() {
            Location::Wsl { distro, path } => {
                format!("{} [{distro}, Container]", linux_name(path))
            }
            folder => format!("{} [Container]", tab_title(folder)),
        },
    }
}

/// Opens `location` in a new window.
pub fn open_window(location: Location, preload: Option<Preload>, cx: &mut App) {
    let location = location.absolute();
    // 1280×800, but never past 90 % of the screen (the taskbar and the
    // window frame need room).
    let screen = cx
        .primary_display()
        .map(|display| display.bounds().size)
        .unwrap_or(size(px(1280.), px(800.)));
    let window_size = size(
        px(1280.).min(screen.width * 0.9),
        px(800.).min(screen.height * 0.88),
    );
    let bounds = Bounds::centered(None, window_size, cx);
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        titlebar: Some(TitlebarOptions {
            title: Some(window_title(Some(&location)).into()),
            appears_transparent: CUSTOM_TITLE_BAR,
            ..Default::default()
        }),
        ..Default::default()
    };
    cx.open_window(options, |window, cx| {
        cx.new(|cx| {
            let mut shell = Shell::new(window, cx);
            shell.add_tab(location, preload, window, cx);
            shell
        })
    })
    .expect("failed to open window");
}

fn window_title(location: Option<&Location>) -> String {
    match location {
        Some(location) => format!("{location} — next_code"),
        None => "next_code".into(),
    }
}

/// Brings forward the project tab showing `location` in a window other
/// than `except`; false when there is none.
fn activate_elsewhere(location: &Location, except: gpui::AnyWindowHandle, cx: &mut App) -> bool {
    for handle in cx.windows() {
        if handle == except {
            continue;
        }
        let Some(shell) = handle.downcast::<Shell>() else {
            continue;
        };
        let shown = shell
            .update(cx, |shell, window, cx| {
                let index = shell.position_of(location)?;
                shell.activate(index, window, cx);
                window.activate_window();
                Some(())
            })
            .ok()
            .flatten();
        if shown.is_some() {
            return true;
        }
    }
    false
}

impl Shell {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let shell = cx.weak_entity();
        window.on_window_should_close(cx, move |window, cx| {
            shell
                .update(cx, |this, cx| this.should_close(window, cx))
                .unwrap_or(true)
        });
        // Picks up a `menus.json`, `settings.json` or theme edited elsewhere
        // when the window returns.
        let activation = cx.observe_window_activation(window, |this, window, cx| {
            if window.is_window_active() {
                this.refresh_menus(cx);
                theme::refresh(cx);
            }
        });
        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle);
        theme::refresh(cx);
        let mut shell = Self {
            focus_handle,
            tabs: Vec::new(),
            active: 0,
            menus: Rc::new(MenuBar::default()),
            menu: None,
            palette: None,
            picker: None,
            theme_picker: None,
            notice: None,
            theme: theme::current(),
            theme_notice: theme::problem().map(Into::into),
            close_confirmed: false,
            caption_font: if window
                .text_system()
                .all_font_names()
                .iter()
                .any(|name| name == "Segoe Fluent Icons")
            {
                "Segoe Fluent Icons".into()
            } else {
                "Segoe MDL2 Assets".into()
            },
            _activation: activation,
        };
        shell.refresh_menus(cx);
        shell
    }

    fn refresh_menus(&mut self, cx: &mut Context<Self>) {
        let (menus, problem) = menus::current(cx);
        if !Rc::ptr_eq(&menus, &self.menus) {
            self.menus = menus;
            self.notice = problem.map(Into::into);
            cx.notify();
        }
    }

    fn position_of(&self, location: &Location) -> Option<usize> {
        self.tabs
            .iter()
            .position(|tab| tab.location.opens_same(location))
    }

    fn dirty_count(&self, cx: &App) -> usize {
        self.tabs.iter().map(|tab| tab.dirty_count(cx)).sum()
    }

    /// Shows `location`: the tab (in any window) that already has it, or a
    /// new tab here.
    fn open(&mut self, location: Location, window: &mut Window, cx: &mut Context<Self>) {
        let location = location.absolute();
        if let Location::Local(root) = &location
            && !root.is_dir()
        {
            self.notice = Some(format!("{} is not a folder", root.display()).into());
            return cx.notify();
        }
        if let Some(index) = self.position_of(&location) {
            return self.activate(index, window, cx);
        }
        if !activate_elsewhere(&location, window.window_handle(), cx) {
            self.add_tab(location, None, window, cx);
        }
    }

    fn add_tab(
        &mut self,
        location: Location,
        preload: Option<Preload>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let content = match location.clone() {
            Location::Local(root) => {
                let git = match preload {
                    Some(Preload::Local(git)) => Some(git),
                    _ => None,
                };
                Content::Workspace(
                    cx.new(|cx| Workspace::new(Project::local(root), git, window, cx)),
                )
            }
            remote => {
                let pending = match preload {
                    Some(Preload::Remote(pending)) => Some(pending),
                    _ => None,
                };
                Content::Connecting(cx.new(|cx| Connecting::new(remote, pending, window, cx)))
            }
        };
        let subscription = self.subscribe(&content, window, cx);
        self.tabs.push(ProjectTab {
            location,
            content,
            _subscription: subscription,
        });
        self.activate(self.tabs.len() - 1, window, cx);
    }

    fn subscribe(
        &mut self,
        content: &Content,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Subscription {
        match content {
            Content::Connecting(connecting) => cx.subscribe_in(
                connecting,
                window,
                |this, connecting, event: &ConnectingEvent, window, cx| match event {
                    ConnectingEvent::Connected(project) => {
                        this.connected(connecting.entity_id(), project.clone(), window, cx)
                    }
                },
            ),
            Content::Workspace(workspace) => cx.subscribe_in(
                workspace,
                window,
                |this, workspace, event: &WorkspaceEvent, window, cx| match event {
                    WorkspaceEvent::OpenProject(location) => {
                        this.open(location.clone(), window, cx)
                    }
                    WorkspaceEvent::ReplaceWith(location) => {
                        let id = workspace.entity_id();
                        let dirty = workspace.read(cx).dirty_count(cx);
                        this.open(location.clone(), window, cx);
                        if dirty == 0
                            && let Some(index) =
                                this.tabs.iter().position(|tab| tab.content.id() == id)
                        {
                            this.remove_tab(index, window, cx);
                        }
                    }
                },
            ),
        }
    }

    /// A remote project finished opening: its tab gets the workspace.
    fn connected(
        &mut self,
        id: EntityId,
        project: Project,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self.tabs.iter().position(|tab| tab.content.id() == id) else {
            return;
        };
        let location = project.location();
        let content = Content::Workspace(cx.new(|cx| Workspace::new(project, None, window, cx)));
        let subscription = self.subscribe(&content, window, cx);
        self.tabs[index] = ProjectTab {
            location,
            content,
            _subscription: subscription,
        };
        if index == self.active {
            self.activate(index, window, cx);
        }
        cx.notify();
    }

    fn activate(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.active = index;
        self.menu = None;
        let tab = &self.tabs[index];
        window.set_window_title(&window_title(Some(&tab.location)));
        match &tab.content {
            Content::Workspace(workspace) => {
                workspace.update(cx, |workspace, cx| workspace.focus_active(window, cx))
            }
            Content::Connecting(_) => window.focus(&self.focus_handle),
        }
        cx.notify();
    }

    /// Closes a project tab, asking first if it has unsaved changes.
    fn close_tab(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(index) else {
            return;
        };
        let dirty = tab.dirty_count(cx);
        if dirty == 0 {
            return self.remove_tab(index, window, cx);
        }
        let id = tab.content.id();
        let message = format!(
            "{} has {dirty} file(s) with unsaved changes. Close it anyway?",
            tab_title(&tab.location)
        );
        let answer = window.prompt(
            PromptLevel::Warning,
            &message,
            None,
            &["Discard Changes", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if answer.await != Ok(0) {
                return;
            }
            this.update_in(cx, |this, window, cx| {
                if let Some(index) = this.tabs.iter().position(|tab| tab.content.id() == id) {
                    this.remove_tab(index, window, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    fn remove_tab(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.tabs.remove(index);
        if self.tabs.is_empty() {
            self.active = 0;
            window.set_window_title(&window_title(None));
            window.focus(&self.focus_handle);
            return cx.notify();
        }
        let next = if self.active > index {
            self.active - 1
        } else {
            self.active.min(self.tabs.len() - 1)
        };
        self.activate(next, window, cx);
    }

    fn should_close(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let dirty = self.dirty_count(cx);
        if dirty == 0 || self.close_confirmed {
            return true;
        }
        let message = format!("{dirty} file(s) have unsaved changes. Close anyway?");
        let answer = window.prompt(
            PromptLevel::Warning,
            &message,
            None,
            &["Discard Changes", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if answer.await != Ok(0) {
                return;
            }
            this.update_in(cx, |this, window, _| {
                this.close_confirmed = true;
                window.remove_window();
            })
            .ok();
        })
        .detach();
        false
    }

    /// Asks for a folder; `\\wsl.localhost\...` folders open as WSL
    /// projects.
    fn prompt_for_project(
        &mut self,
        new_window: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Open Project".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else {
                return;
            };
            let Some(location) = paths
                .first()
                .and_then(|path| Location::parse(&path.to_string_lossy()))
            else {
                return;
            };
            this.update_in(cx, |this, window, cx| {
                if new_window {
                    let location = location.absolute();
                    if !activate_elsewhere(&location, window.window_handle(), cx) {
                        match this.position_of(&location) {
                            Some(index) => this.activate(index, window, cx),
                            None => open_window(location, None, cx),
                        }
                    }
                } else {
                    this.open(location, window, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    fn open_project(&mut self, _: &OpenProject, window: &mut Window, cx: &mut Context<Self>) {
        self.prompt_for_project(false, window, cx);
    }

    fn open_project_in_new_window(
        &mut self,
        _: &OpenProjectInNewWindow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.prompt_for_project(true, window, cx);
    }

    fn open_recent_project(
        &mut self,
        action: &OpenRecentProject,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open(action.location.clone(), window, cx);
    }

    /// `Ctrl+R`: recent projects, or a typed location.
    fn open_recent(&mut self, _: &OpenRecent, window: &mut Window, cx: &mut Context<Self>) {
        self.menu = None;
        let previous = window.focused(cx);
        let picker = cx.new(ProjectPicker::new);
        let subscription = cx.subscribe_in(&picker, window, move |this, _, event, window, cx| {
            this.picker = None;
            match &previous {
                Some(focus) => window.focus(focus),
                None => window.focus(&this.focus_handle),
            }
            if let ProjectPickerEvent::Open(location) = event {
                this.open(location.clone(), window, cx);
            }
            cx.notify();
        });
        window.focus(&picker.focus_handle(cx));
        self.picker = Some((picker, subscription));
        cx.notify();
    }

    fn close_project(&mut self, _: &CloseProject, window: &mut Window, cx: &mut Context<Self>) {
        self.close_tab(self.active, window, cx);
    }

    fn next_project(&mut self, _: &NextProject, window: &mut Window, cx: &mut Context<Self>) {
        if !self.tabs.is_empty() {
            self.activate((self.active + 1) % self.tabs.len(), window, cx);
        }
    }

    fn previous_project(
        &mut self,
        _: &PreviousProject,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.tabs.is_empty() {
            let count = self.tabs.len();
            self.activate((self.active + count - 1) % count, window, cx);
        }
    }

    /// The palette lists what can run where focus is now.
    fn show_command_palette(
        &mut self,
        _: &ShowCommandPalette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.menu = None;
        if self.palette.is_some() {
            return;
        }
        theme::refresh(cx);
        let focus = window.focused(cx);
        let commands: Vec<Command> = window
            .available_actions(cx)
            .into_iter()
            .filter(|action| commands::in_palette(action.name()))
            .map(|action| {
                let keys = commands::key_text(
                    window,
                    action.as_ref(),
                    Some(focus.as_ref().unwrap_or(&self.focus_handle)),
                );
                Command::new(action, keys)
            })
            .collect();
        let palette = cx.new(|cx| CommandPalette::new(commands, cx));
        let subscription = cx.subscribe_in(&palette, window, |this, _, event, window, cx| {
            let focus = this.palette.take().and_then(|(_, _, focus)| focus);
            match &focus {
                Some(focus) => window.focus(focus),
                None => window.focus(&this.focus_handle),
            }
            if let CommandPaletteEvent::Run(action) = event {
                window.dispatch_action(action.boxed_clone(), cx);
            }
            cx.notify();
        });
        window.focus(&palette.focus_handle(cx));
        self.palette = Some((palette, subscription, focus));
        cx.notify();
    }

    /// Opens a config file: in the active project when it is local, else
    /// with the system's editor.
    fn open_config(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ProjectTab {
            location: Location::Local(_),
            content: Content::Workspace(workspace),
            ..
        }) = self.tabs.get(self.active)
        {
            workspace.update(cx, |workspace, cx| workspace.open(&path, window, cx));
        } else {
            cx.open_with_system(&path);
        }
    }

    fn customize_menus(&mut self, _: &CustomizeMenus, window: &mut Window, cx: &mut Context<Self>) {
        match menus::ensure_file() {
            Ok(path) => self.open_config(path, window, cx),
            Err(err) => {
                self.notice = Some(format!("Could not create menus.json: {err}").into());
                cx.notify();
            }
        }
    }

    fn open_settings(&mut self, _: &OpenSettings, window: &mut Window, cx: &mut Context<Self>) {
        let created = Settings::path()
            .ok_or_else(|| std::io::Error::other("no config directory"))
            .and_then(|path| {
                if !path.exists() {
                    if let Some(dir) = path.parent() {
                        std::fs::create_dir_all(dir)?;
                    }
                    std::fs::write(&path, "{}\n")?;
                }
                Ok(path)
            });
        match created {
            Ok(path) => self.open_config(path, window, cx),
            Err(err) => {
                self.notice = Some(format!("Could not create settings.json: {err}").into());
                cx.notify();
            }
        }
    }

    fn select_theme(&mut self, _: &SelectTheme, window: &mut Window, cx: &mut Context<Self>) {
        self.menu = None;
        let previous = window.focused(cx);
        let picker = cx.new(ThemePicker::new);
        let subscription = cx.subscribe_in(&picker, window, move |this, _, event, window, cx| {
            this.theme_picker = None;
            match &previous {
                Some(focus) => window.focus(focus),
                None => window.focus(&this.focus_handle),
            }
            if let ThemePickerEvent::Select(name) = event
                && let Err(err) = theme::select(name, cx)
            {
                this.notice = Some(format!("Could not select the theme: {err}").into());
            }
            cx.notify();
        });
        window.focus(&picker.focus_handle(cx));
        self.theme_picker = Some((picker, subscription));
        cx.notify();
    }

    /// Opens the current theme's file, first creating one that extends a
    /// built-in theme.
    fn customize_theme(&mut self, _: &CustomizeTheme, window: &mut Window, cx: &mut Context<Self>) {
        match theme::customize(cx) {
            Ok(path) => self.open_config(path, window, cx),
            Err(err) => {
                self.notice = Some(format!("Could not create a theme file: {err}").into());
                cx.notify();
            }
        }
    }

    /// Exits after one confirmation covering every window's unsaved files.
    fn quit(&mut self, _: &Quit, window: &mut Window, cx: &mut Context<Self>) {
        let this_window = window.window_handle();
        let mut dirty = self.dirty_count(cx);
        for handle in cx.windows() {
            if handle == this_window {
                continue;
            }
            if let Some(shell) = handle.downcast::<Shell>() {
                dirty += shell
                    .read_with(cx, |shell, cx| shell.dirty_count(cx))
                    .unwrap_or(0);
            }
        }
        if dirty == 0 {
            return cx.quit();
        }
        let message = format!("{dirty} file(s) have unsaved changes. Exit anyway?");
        let answer = window.prompt(
            PromptLevel::Warning,
            &message,
            None,
            &["Discard Changes", "Cancel"],
            cx,
        );
        cx.spawn(async move |_, cx| {
            if answer.await == Ok(0) {
                cx.update(|cx| cx.quit()).ok();
            }
        })
        .detach();
    }

    fn about(&mut self, _: &About, window: &mut Window, cx: &mut Context<Self>) {
        let detail = format!(
            "Version {}\nConfiguration: {}",
            env!("CARGO_PKG_VERSION"),
            menus::path()
                .and_then(|path| path.parent().map(|dir| dir.display().to_string()))
                .unwrap_or_default()
        );
        let answer = window.prompt(PromptLevel::Info, "next_code", Some(&detail), &["OK"], cx);
        cx.background_spawn(async move { answer.await.ok() })
            .detach();
    }

    /// Opens menu `index`, resolving its rows for where focus is now.
    fn open_menu(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.refresh_menus(cx);
        theme::refresh(cx);
        let Some(menu) = self.menus.menus.get(index) else {
            self.menu = None;
            return cx.notify();
        };
        let items = menu.items.clone();
        let focus = window
            .focused(cx)
            .unwrap_or_else(|| self.focus_handle.clone());
        let rows = resolve(&items, &focus, window, cx);
        self.menu = Some(OpenMenu {
            menu: index,
            rows,
            path: Vec::new(),
        });
        cx.notify();
    }

    fn close_menu(&mut self, cx: &mut Context<Self>) {
        if self.menu.take().is_some() {
            cx.notify();
        }
    }

    /// Runs a menu command where focus is.
    fn run_menu_command(
        &mut self,
        action: Box<dyn Action>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_menu(cx);
        window.dispatch_action(action, cx);
    }

    fn render_menu_bar(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette_keys =
            commands::key_text(window, &ShowCommandPalette, Some(&self.focus_handle))
                .unwrap_or_default();
        // Empty title bar space moves the window (and maximizes it on
        // double click, as the system title bar would).
        let drag_area = |id: &'static str| {
            div()
                .id(id)
                .flex_1()
                .min_w(px(24.))
                .h_full()
                .when(CUSTOM_TITLE_BAR, |area| {
                    area.window_control_area(WindowControlArea::Drag)
                })
        };
        div()
            .flex()
            .flex_none()
            .items_center()
            .h(px(MENU_BAR_HEIGHT))
            .pl_2()
            .gap_0p5()
            .bg(theme::panel())
            .border_b_1()
            .border_color(theme::border())
            .text_sm()
            // Clicks on the bar keep focus where it is (the window would
            // otherwise take it): menu commands and the palette act there.
            // Occluding rather than handling the press leaves drag areas and
            // caption buttons to the system (gpui hands it unhandled presses).
            .id("menu-bar")
            .occlude()
            .child(img(APP_ICON.clone()).size(px(18.)).flex_none().mr_1p5())
            .children(
                self.menus
                    .menus
                    .iter()
                    .enumerate()
                    .map(|(index, menu)| self.render_menu_title(index, menu.label.clone(), cx)),
            )
            .child(drag_area("title-drag-left"))
            .child(
                div()
                    .id("command-bar")
                    .flex_shrink()
                    .w(px(440.))
                    .min_w(px(160.))
                    .h(px(24.))
                    .px_2()
                    .flex()
                    .items_center()
                    .gap_2()
                    .rounded_md()
                    .border_1()
                    .border_color(theme::border())
                    .bg(theme::background())
                    .text_color(theme::muted())
                    .cursor_pointer()
                    .hover(|style| style.border_color(theme::accent()))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, window, cx| {
                            cx.stop_propagation();
                            this.show_command_palette(&ShowCommandPalette, window, cx)
                        }),
                    )
                    .child(div().text_xs().child("⌕"))
                    .child(div().flex_1().truncate().child("Search commands"))
                    .child(div().text_xs().child(palette_keys)),
            )
            .child(drag_area("title-drag-right"))
            .when(CUSTOM_TITLE_BAR, |bar| {
                bar.child(self.render_caption_buttons(window))
            })
    }

    /// Minimize, maximize/restore and close, drawn like Windows 11's (the
    /// system acts on them through their window control areas).
    fn render_caption_buttons(&self, window: &Window) -> impl IntoElement {
        let button = |id: &'static str, area: WindowControlArea, glyph: &'static str| {
            div()
                .id(id)
                .w(px(46.))
                .h_full()
                .flex()
                .items_center()
                .justify_center()
                .font_family(self.caption_font.clone())
                .text_size(px(10.))
                .text_color(theme::text())
                .window_control_area(area)
                .child(glyph)
        };
        div()
            .flex()
            .h_full()
            .flex_none()
            .child(
                button("caption-min", WindowControlArea::Min, "\u{E921}")
                    .hover(|style| style.bg(theme::hover_row())),
            )
            .child(
                button(
                    "caption-max",
                    WindowControlArea::Max,
                    if window.is_maximized() {
                        "\u{E923}"
                    } else {
                        "\u{E922}"
                    },
                )
                .hover(|style| style.bg(theme::hover_row())),
            )
            .child(
                button("caption-close", WindowControlArea::Close, "\u{E8BB}")
                    .hover(|style| style.bg(theme::error()).text_color(theme::on_accent())),
            )
    }

    fn render_menu_title(
        &self,
        index: usize,
        label: String,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let open = self.menu.as_ref().filter(|menu| menu.menu == index);
        div()
            .id(("menu", index))
            .relative()
            .px_2()
            .py_0p5()
            .rounded_sm()
            .cursor_pointer()
            .when(open.is_some(), |title| title.bg(theme::active_row()))
            .when(open.is_none(), |title| {
                title.hover(|style| style.bg(theme::hover_row()))
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, window, cx| {
                    cx.stop_propagation();
                    if this.menu.as_ref().is_some_and(|menu| menu.menu == index) {
                        this.close_menu(cx);
                    } else {
                        this.open_menu(index, window, cx);
                    }
                }),
            )
            // Like native menu bars: with a menu open, pointing at another
            // title opens that one.
            .on_hover(cx.listener(move |this, hovered: &bool, window, cx| {
                if *hovered && this.menu.as_ref().is_some_and(|menu| menu.menu != index) {
                    this.open_menu(index, window, cx);
                }
            }))
            .child(label)
            .children(open.map(|menu| {
                div()
                    .absolute()
                    .top(px(MENU_BAR_HEIGHT - 4.))
                    .left_0()
                    .child(
                        deferred(anchored().child(self.render_rows(&menu.rows, 0, &menu.path, cx)))
                            .with_priority(1),
                    )
            }))
    }

    /// One menu panel: `rows` at submenu `depth`, with `path` naming the
    /// open submenus below it.
    fn render_rows(
        &self,
        rows: &[Row],
        depth: usize,
        path: &[usize],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let open_child = path.first().copied();
        div()
            .id(("menu-panel", depth))
            .occlude()
            .w(px(MENU_WIDTH))
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
            .children(rows.iter().enumerate().map(|(index, row)| {
                let hover = cx.listener(move |this, hovered: &bool, _, cx| {
                    if !*hovered {
                        return;
                    }
                    let Some(menu) = &mut this.menu else {
                        return;
                    };
                    menu.path.truncate(depth);
                    if let Some(Row::Submenu { .. }) = row_at(&menu.rows, &menu.path, index) {
                        menu.path.push(index);
                    }
                    cx.notify();
                });
                match row {
                    Row::Separator => div()
                        .my_1()
                        .h(px(1.))
                        .bg(theme::border())
                        .into_any_element(),
                    Row::Command {
                        label,
                        action,
                        keys,
                    } => {
                        let action = action.as_ref().map(|action| action.boxed_clone());
                        let enabled = action.is_some();
                        menu_row(("menu-row", depth * 1000 + index), label.clone(), enabled)
                            .on_hover(hover)
                            .children(
                                keys.clone().map(|keys| {
                                    div().text_xs().text_color(theme::muted()).child(keys)
                                }),
                            )
                            .when_some(action, |row, action| {
                                row.on_click(cx.listener(move |this, _, window, cx| {
                                    this.run_menu_command(action.boxed_clone(), window, cx)
                                }))
                            })
                            .into_any_element()
                    }
                    Row::Submenu { label, rows } => {
                        let open = open_child == Some(index);
                        menu_row(("menu-row", depth * 1000 + index), label.clone(), true)
                            .relative()
                            .when(open, |row| row.bg(theme::active_row()))
                            .on_hover(hover)
                            .child(div().text_color(theme::muted()).child("›"))
                            // Already inside the menu's deferred layer, which
                            // gpui does not nest; placed past the parent's
                            // rows so none paints over it.
                            .when(open, |row| {
                                row.child(
                                    div()
                                        .absolute()
                                        .top(px(-4.))
                                        .left(px(MENU_WIDTH - 8.))
                                        .child(anchored().child(self.render_rows(
                                            rows,
                                            depth + 1,
                                            &path[1..],
                                            cx,
                                        ))),
                                )
                            })
                            .into_any_element()
                    }
                }
            }))
            .into_any_element()
    }

    fn render_project_tabs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_none()
            .h(px(30.))
            .bg(theme::panel())
            .border_b_1()
            .border_color(theme::border())
            .text_sm()
            .children(self.tabs.iter().enumerate().map(|(index, tab)| {
                let active = index == self.active;
                let connecting = matches!(tab.content, Content::Connecting(_));
                div()
                    .id(("project-tab", index))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .border_r_1()
                    .border_color(theme::border())
                    .cursor_pointer()
                    .text_color(if active {
                        theme::text()
                    } else {
                        theme::muted()
                    })
                    .when(active, |tab| {
                        tab.bg(theme::background())
                            .border_b_2()
                            .border_color(theme::accent())
                    })
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.activate(index, window, cx)),
                    )
                    .child(if connecting {
                        format!("{} …", tab_title(&tab.location))
                    } else {
                        tab_title(&tab.location)
                    })
                    .child(
                        div()
                            .id(("project-close", index))
                            .w(px(14.))
                            .text_color(theme::muted())
                            .hover(|style| style.text_color(theme::text()))
                            .child("×")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.close_tab(index, window, cx);
                            })),
                    )
            }))
            .child(
                div()
                    .id("project-add")
                    .px_3()
                    .flex()
                    .items_center()
                    .cursor_pointer()
                    .text_color(theme::muted())
                    .hover(|style| style.text_color(theme::text()))
                    .child("+")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.prompt_for_project(false, window, cx)
                    })),
            )
    }

    /// The window without projects: how to open one.
    fn render_welcome(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let link = |id: &'static str, label: String| {
            div()
                .id(id)
                .px_2()
                .py_1()
                .rounded_sm()
                .cursor_pointer()
                .text_color(theme::accent_text())
                .hover(|style| style.bg(theme::hover_row()))
                .child(label)
        };
        let with_keys = |label: &str, action: &dyn Action| match commands::key_text(
            window,
            action,
            Some(&self.focus_handle),
        ) {
            Some(keys) => format!("{label}  ({keys})"),
            None => label.to_owned(),
        };
        let recents = recent::load();
        div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_2()
            .child(div().text_lg().child("No project open"))
            .child(
                link("welcome-open", with_keys("Open Project…", &OpenProject)).on_click(
                    cx.listener(|this, _, window, cx| this.prompt_for_project(false, window, cx)),
                ),
            )
            .child(
                link("welcome-recent", with_keys("Open Recent…", &OpenRecent)).on_click(
                    cx.listener(|this, _, window, cx| this.open_recent(&OpenRecent, window, cx)),
                ),
            )
            .when(!recents.is_empty(), |this| {
                this.child(
                    div()
                        .mt_4()
                        .text_sm()
                        .text_color(theme::muted())
                        .child("Recent projects"),
                )
            })
            .children(
                recents
                    .into_iter()
                    .take(10)
                    .enumerate()
                    .map(|(index, location)| {
                        let label = location.to_string();
                        div()
                            .id(("welcome-recent-project", index))
                            .px_2()
                            .rounded_sm()
                            .text_sm()
                            .cursor_pointer()
                            .hover(|style| style.bg(theme::hover_row()))
                            .child(label)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.open(location.clone(), window, cx)
                            }))
                    }),
            )
    }
}

/// The row a hover at `index` lands on, `path` deep.
fn row_at<'a>(rows: &'a [Row], path: &[usize], index: usize) -> Option<&'a Row> {
    let mut rows = rows;
    for &step in path {
        match rows.get(step)? {
            Row::Submenu { rows: inner, .. } => rows = inner,
            _ => return None,
        }
    }
    rows.get(index)
}

fn menu_row(
    id: (&'static str, usize),
    label: SharedString,
    enabled: bool,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .h(px(26.))
        .px_3()
        .flex()
        .items_center()
        .gap_2()
        .rounded_sm()
        .child(div().flex_1().truncate().child(label))
        .when(enabled, |row| {
            row.cursor_pointer()
                .hover(|style| style.bg(theme::active_row()))
        })
        .when(!enabled, |row| row.text_color(theme::muted()))
}

/// Menu items as rows: commands built and checked against where `focus`
/// is, lists filled in.
fn resolve(items: &[Item], focus: &FocusHandle, window: &mut Window, cx: &mut App) -> Vec<Row> {
    items
        .iter()
        .map(|item| match item {
            Item::Plain(command) if command == SEPARATOR => Row::Separator,
            Item::Plain(command) => command_row(command, None, focus, window, cx),
            Item::Command { command, label } => {
                command_row(command, Some(label), focus, window, cx)
            }
            Item::Submenu { label, items } => Row::Submenu {
                label: label.clone().into(),
                rows: resolve(items, focus, window, cx),
            },
            Item::List {
                label,
                list: List::RecentProjects,
            } => {
                let mut rows: Vec<Row> = recent::load()
                    .into_iter()
                    .take(RECENT_IN_MENU)
                    .map(|location| Row::Command {
                        // Paths share long prefixes: the name comes first.
                        label: format!("{}  —  {location}", tab_title(&location)).into(),
                        action: Some(Box::new(OpenRecentProject { location })),
                        keys: None,
                    })
                    .collect();
                if rows.is_empty() {
                    rows.push(Row::Command {
                        label: "No recent projects".into(),
                        action: None,
                        keys: None,
                    });
                }
                Row::Submenu {
                    label: label.clone().into(),
                    rows,
                }
            }
        })
        .collect()
}

fn command_row(
    command: &str,
    label: Option<&String>,
    focus: &FocusHandle,
    window: &mut Window,
    cx: &mut App,
) -> Row {
    let label = label.cloned().unwrap_or_else(|| commands::label(command));
    let action = cx.build_action(command, None).ok();
    let keys = action
        .as_ref()
        .and_then(|action| commands::key_text(window, action.as_ref(), Some(focus)));
    let action = action.filter(|action| window.is_action_available(action.as_ref(), cx));
    Row::Command {
        label: label.into(),
        action,
        keys: keys.map(Into::into),
    }
}

impl Focusable for Shell {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Shell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The theme changed (here or in another window): show its problem.
        let theme = theme::current();
        if !Arc::ptr_eq(&theme, &self.theme) {
            self.theme = theme;
            self.theme_notice = theme::problem().map(Into::into);
        }
        let content = match self.tabs.get(self.active) {
            Some(tab) => tab.content.view().into_any_element(),
            None => self.render_welcome(window, cx).into_any_element(),
        };
        let modal = |view: AnyView| {
            div()
                .absolute()
                .top(px(MENU_BAR_HEIGHT + 40.))
                .left_0()
                .right_0()
                .flex()
                .justify_center()
                .child(view)
        };
        div()
            .key_context(CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::open_project))
            .on_action(cx.listener(Self::open_project_in_new_window))
            .on_action(cx.listener(Self::open_recent))
            .on_action(cx.listener(Self::open_recent_project))
            .on_action(cx.listener(Self::close_project))
            .on_action(cx.listener(Self::next_project))
            .on_action(cx.listener(Self::previous_project))
            .on_action(cx.listener(Self::show_command_palette))
            .on_action(cx.listener(Self::customize_menus))
            .on_action(cx.listener(Self::open_settings))
            .on_action(cx.listener(Self::select_theme))
            .on_action(cx.listener(Self::customize_theme))
            .on_action(cx.listener(Self::quit))
            .on_action(cx.listener(Self::about))
            .capture_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                if this.menu.is_some() && event.keystroke.key == "escape" {
                    this.close_menu(cx);
                    cx.stop_propagation();
                }
            }))
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::background())
            .text_color(theme::text())
            .child(self.render_menu_bar(window, cx))
            .when(!self.tabs.is_empty(), |shell| {
                shell.child(self.render_project_tabs(cx))
            })
            .children(self.notice.clone().map(|notice| {
                div()
                    .id("shell-notice")
                    .px_3()
                    .py_1()
                    .text_sm()
                    .text_color(theme::error())
                    .cursor_pointer()
                    .child(notice)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.notice = None;
                        cx.notify();
                    }))
            }))
            .children(self.theme_notice.clone().map(|notice| {
                div()
                    .id("theme-notice")
                    .px_3()
                    .py_1()
                    .text_sm()
                    .text_color(theme::error())
                    .cursor_pointer()
                    .child(notice)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.theme_notice = None;
                        cx.notify();
                    }))
            }))
            // A flex container: in a block one, panels in the right dock
            // left the workspace's inputs (finder, go to line, rename)
            // laid out zero wide, so their text never showed.
            .child(div().flex_1().min_h_0().flex().child(content))
            // Clicking anywhere below the menu bar closes an open menu.
            .when(self.menu.is_some(), |shell| {
                shell.child(
                    deferred(
                        div()
                            .id("menu-dismiss")
                            .absolute()
                            .top(px(MENU_BAR_HEIGHT))
                            .left_0()
                            .size_full()
                            .occlude()
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _, _, cx| this.close_menu(cx)),
                            )
                            .on_mouse_down(
                                MouseButton::Right,
                                cx.listener(|this, _, _, cx| this.close_menu(cx)),
                            ),
                    )
                    .with_priority(0),
                )
            })
            .children(
                self.palette
                    .as_ref()
                    .map(|(palette, _, _)| modal(palette.clone().into())),
            )
            .children(
                self.picker
                    .as_ref()
                    .map(|(picker, _)| modal(picker.clone().into())),
            )
            .children(
                self.theme_picker
                    .as_ref()
                    .map(|(picker, _)| modal(picker.clone().into())),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Removes a test's folders, its recent-projects file and its project
    /// state (sessions saved when workspaces close).
    fn cleanup(root: PathBuf) {
        std::fs::remove_dir_all(root).ok();
        if let Some(recent) = recent::path() {
            std::fs::remove_file(recent).ok();
        }
        if let Some(state) = crate::layout::state_root() {
            std::fs::remove_dir_all(state).ok();
        }
    }

    /// Two local folders under a fresh temp dir.
    fn two_projects(name: &str) -> (PathBuf, Location, Location) {
        let root = std::env::temp_dir().join(format!("nc-shell-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for project in ["alpha", "beta"] {
            std::fs::create_dir_all(root.join(project)).unwrap();
        }
        std::fs::write(root.join("alpha").join("main.py"), "print(1)\n").unwrap();
        (
            root.clone(),
            Location::Local(root.join("alpha")),
            Location::Local(root.join("beta")),
        )
    }

    fn open_shell(location: Location, cx: &mut gpui::TestAppContext) -> gpui::WindowHandle<Shell> {
        cx.update(|cx| {
            crate::editor::bind_keys(cx);
            crate::workspace::bind_keys(cx);
            crate::project_picker::bind_keys(cx);
            crate::command_palette::bind_keys(cx);
            bind_keys(cx);
            open_window(location, None, cx);
        });
        cx.run_until_parked();
        cx.windows()[0].downcast::<Shell>().unwrap()
    }

    /// Tab titles and the active tab.
    fn tabs(
        window: gpui::WindowHandle<Shell>,
        cx: &mut gpui::TestAppContext,
    ) -> (Vec<String>, usize) {
        window
            .read_with(cx, |shell, _| {
                let titles = shell
                    .tabs
                    .iter()
                    .map(|tab| tab_title(&tab.location))
                    .collect();
                (titles, shell.active)
            })
            .unwrap()
    }

    #[gpui::test]
    fn projects_open_in_tabs_and_reopening_switches_to_them(cx: &mut gpui::TestAppContext) {
        let (root, alpha, beta) = two_projects("tabs");
        let window = open_shell(alpha.clone(), cx);
        assert_eq!(tabs(window, cx), (vec!["alpha".to_owned()], 0));

        cx.dispatch_action(window.into(), OpenRecentProject { location: beta });
        assert_eq!(tabs(window, cx), (vec!["alpha".into(), "beta".into()], 1));
        assert_eq!(cx.windows().len(), 1, "a tab, not a window");
        let title = |cx: &gpui::TestAppContext| {
            gpui::VisualTestContext::from_window(window.into(), cx)
                .window_title()
                .unwrap_or_default()
        };
        assert!(title(cx).contains("beta"), "{}", title(cx));

        cx.dispatch_action(window.into(), OpenRecentProject { location: alpha });
        assert_eq!(tabs(window, cx), (vec!["alpha".into(), "beta".into()], 0));

        cx.simulate_keystrokes(window.into(), "ctrl-alt-]");
        assert_eq!(tabs(window, cx).1, 1);
        cx.simulate_keystrokes(window.into(), "ctrl-alt-]");
        assert_eq!(tabs(window, cx).1, 0, "wraps around");

        cx.dispatch_action(window.into(), CloseProject);
        assert_eq!(tabs(window, cx), (vec!["beta".into()], 0));
        cx.dispatch_action(window.into(), CloseProject);
        assert_eq!(tabs(window, cx), (Vec::new(), 0));
        assert_eq!(title(cx), "next_code");
        cleanup(root);
    }

    #[gpui::test]
    fn workspace_inputs_get_the_width_of_their_box(cx: &mut gpui::TestAppContext) {
        // The default layout has the Outline in the right dock, which used
        // to leave inputs over the workspace (file finder) and over an
        // editor (go to line) zero wide: their text was invisible.
        let (root, alpha, _) = two_projects("input-width");
        let window = open_shell(alpha, cx);
        let workspace = window
            .read_with(cx, |shell, _| match &shell.tabs[0].content {
                Content::Workspace(workspace) => workspace.clone(),
                Content::Connecting(_) => panic!("a local project opens at once"),
            })
            .unwrap();
        cx.dispatch_action(window.into(), crate::workspace::ToggleFileFinder);
        cx.run_until_parked();
        let query = workspace
            .read_with(cx, |workspace, cx| workspace.file_finder_query(cx))
            .expect("Ctrl+P opens the file finder");
        let width = query.read_with(cx, |query, _| query.text_width()).unwrap();
        assert!(width > gpui::px(100.), "finder input {width:?} wide");

        cx.dispatch_action(window.into(), crate::file_finder::Dismiss);
        let main_py = root.join("alpha").join("main.py");
        window
            .update(cx, |_, window, cx| {
                workspace.update(cx, |workspace, cx| workspace.open(&main_py, window, cx))
            })
            .unwrap();
        cx.run_until_parked();
        let go_to_line_width = |cx: &mut gpui::TestAppContext| {
            cx.dispatch_action(window.into(), crate::editor::GoToLine);
            cx.run_until_parked();
            let input = workspace
                .read_with(cx, |workspace, cx| {
                    workspace.active_editor()?.read(cx).go_to_line_input()
                })
                .expect("Ctrl+G opens the go-to-line input");
            input.read_with(cx, |input, _| input.text_width()).unwrap()
        };
        let width = go_to_line_width(cx);
        assert!(width > gpui::px(100.), "go-to-line input {width:?} wide");
        // In the second pane of a split too.
        let split = cx.update(|cx| cx.build_action("workspace::SplitRight", None).unwrap());
        cx.dispatch_action(window.into(), crate::editor::CancelGoToLine);
        window
            .update(cx, |_, window, cx| window.dispatch_action(split, cx))
            .unwrap();
        cx.run_until_parked();
        let width = go_to_line_width(cx);
        assert!(
            width > gpui::px(100.),
            "split: go-to-line input {width:?} wide"
        );
        cleanup(root);
    }

    #[gpui::test]
    fn menus_and_the_palette_run_commands_where_focus_is(cx: &mut gpui::TestAppContext) {
        let (root, alpha, beta) = two_projects("menus");
        let window = open_shell(alpha, cx);
        cx.dispatch_action(window.into(), OpenRecentProject { location: beta });

        // File menu: project commands run anywhere; Save needs an editor.
        window
            .update(cx, |shell, window, cx| shell.open_menu(0, window, cx))
            .unwrap();
        let (open_enabled, save_enabled, recent) = window
            .read_with(cx, |shell, _| {
                let rows = &shell.menu.as_ref().unwrap().rows;
                let enabled = |wanted: &str| {
                    rows.iter().find_map(|row| match row {
                        Row::Command { label, action, .. } if label == wanted => {
                            Some(action.is_some())
                        }
                        _ => None,
                    })
                };
                let recent: Vec<String> = rows
                    .iter()
                    .find_map(|row| match row {
                        Row::Submenu { label, rows } if label == "Recent Projects" => Some(
                            rows.iter()
                                .filter_map(|row| match row {
                                    Row::Command { label, .. } => Some(label.to_string()),
                                    _ => None,
                                })
                                .collect(),
                        ),
                        _ => None,
                    })
                    .unwrap();
                (enabled("Open Project…"), enabled("Save"), recent)
            })
            .unwrap();
        assert_eq!(open_enabled, Some(true));
        assert_eq!(save_enabled, Some(false), "no file is open");
        assert!(
            recent.iter().any(|label| label.ends_with("alpha")),
            "{recent:?}"
        );

        // A menu command goes where focus is: the project tabs.
        window
            .update(cx, |shell, window, cx| {
                shell.run_menu_command(Box::new(PreviousProject), window, cx)
            })
            .unwrap();
        cx.run_until_parked();
        assert_eq!(tabs(window, cx).1, 0);
        assert!(
            window
                .read_with(cx, |shell, _| shell.menu.is_none())
                .unwrap()
        );

        // With a file open and edited, editor commands join in (with their
        // shortcuts) and run in that editor.
        let main_py = root.join("alpha").join("main.py");
        window
            .update(cx, |shell, window, cx| {
                let Content::Workspace(workspace) = &shell.tabs[0].content else {
                    panic!("alpha is local");
                };
                workspace.update(cx, |workspace, cx| workspace.open(&main_py, window, cx));
            })
            .unwrap();
        cx.run_until_parked();
        cx.simulate_input(window.into(), "x");
        let dirty = |cx: &mut gpui::TestAppContext| {
            window
                .read_with(cx, |shell, cx| shell.dirty_count(cx))
                .unwrap()
        };
        assert_eq!(dirty(cx), 1);

        cx.simulate_keystrokes(window.into(), "ctrl-shift-p");
        let (palette, commands) = window
            .read_with(cx, |shell, cx| {
                let palette = shell.palette.as_ref().unwrap().0.clone();
                let commands: Vec<(&'static str, Option<String>)> =
                    palette.read(cx).commands().collect();
                (palette, commands)
            })
            .unwrap();
        let keys = |name: &str| {
            commands
                .iter()
                .find(|(command, _)| *command == name)
                .map(|(_, keys)| keys.clone())
        };
        for expected in ["workspace::ToggleGit", "git::Fetch", "python::Sync"] {
            assert!(
                keys(expected).is_some(),
                "{expected} missing from {commands:?}"
            );
        }
        assert_eq!(keys("editor::Save"), Some(Some("Ctrl+S".into())));
        assert_eq!(keys("app::NextProject"), Some(Some("Ctrl+Alt+]".into())));
        assert_eq!(keys("editor::Left"), None, "typing commands stay out");
        let save = cx.update(|cx| cx.build_action("editor::Save", None).unwrap());
        palette.update(cx, |_, cx| cx.emit(CommandPaletteEvent::Run(save)));
        cx.run_until_parked();
        assert_eq!(dirty(cx), 0, "saved by the editor that had focus");
        assert_eq!(std::fs::read_to_string(&main_py).unwrap(), "xprint(1)\n");
        assert!(
            window
                .read_with(cx, |shell, _| shell.palette.is_none())
                .unwrap()
        );

        // Closing a project with unsaved edits asks first.
        cx.simulate_input(window.into(), "y");
        assert_eq!(dirty(cx), 1);
        cx.dispatch_action(window.into(), CloseProject);
        assert!(cx.has_pending_prompt());
        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();
        assert_eq!(tabs(window, cx).0.len(), 2, "cancel keeps the project");
        cx.dispatch_action(window.into(), CloseProject);
        cx.simulate_prompt_answer("Discard Changes");
        cx.run_until_parked();
        assert_eq!(tabs(window, cx), (vec!["beta".into()], 0));
        // Release the folder watchers so the temp dir can go.
        cx.dispatch_action(window.into(), CloseProject);
        cleanup(root);
    }

    #[test]
    fn project_tabs_are_named_after_the_folder_and_where_it_lives() {
        let local = Location::Local(PathBuf::from(if cfg!(windows) {
            r"C:\src\next_code"
        } else {
            "/src/next_code"
        }));
        assert_eq!(tab_title(&local), "next_code");
        let wsl = Location::Wsl {
            distro: "Ubuntu".into(),
            path: "/home/me/app/".into(),
        };
        assert_eq!(tab_title(&wsl), "app [Ubuntu]");
        let root = Location::Wsl {
            distro: "Ubuntu".into(),
            path: "/".into(),
        };
        assert_eq!(tab_title(&root), "/ [Ubuntu]");
        let container = |folder: Location| Location::DevContainer {
            folder: Box::new(folder),
            config: None,
        };
        assert_eq!(tab_title(&container(local)), "next_code [Container]");
        assert_eq!(tab_title(&container(wsl)), "app [Ubuntu, Container]");
    }
}
