//! Built-in extensions: optional features compiled into the editor. Each
//! reaches the editor only through what it registers here (commands, items
//! in the built-in menus, palette entries, window tabs, a model per window
//! and per workspace, dock panels, editor-area tabs, tabs files open in,
//! hooks on open files' views, file changes, language-server locations
//! and offers) and the window's and workspace's
//! methods for them (`shell::builtins`, `workspace::builtins`), so neither
//! the shell nor the workspace names any of them. Manage Extensions turns
//! them on and off (`extensions::Extensions::builtin_enabled`); one that is
//! off has no commands, menu items, panels, offers or tabs (a window tab
//! with unsaved changes stays until closed).

use std::{path::Path, rc::Rc};

use gpui::{
    AnyEntity, AnyView, App, Context, Div, Entity, EntityId, FocusHandle, Focusable, Hsla, Render,
    Subscription, Window,
};
use serde_json::Value;
use watcher::Batch;

use crate::{
    command_palette::Command,
    editor::Editor,
    layout::DockPosition,
    pinpoint::context::Limits,
    project::{Location, Project},
    shell::Shell,
    workspace::Workspace,
};

pub mod assembly;
pub mod notebook;
pub mod notes;
pub mod python;
pub mod workflows;

/// Every built-in extension, as Manage Extensions lists them. Menu items
/// go right after their anchor, so of two at one anchor the later one's
/// come first (and may anchor on an earlier one's).
pub const ALL: &[&Builtin] = &[
    &assembly::BUILTIN,
    &workflows::BUILTIN,
    &notebook::BUILTIN,
    &python::BUILTIN,
    &notes::BUILTIN,
];

pub struct Builtin {
    /// Its id, which is also the namespace of its commands (`assembly::…`).
    pub id: &'static str,
    /// What Manage Extensions shows.
    pub name: &'static str,
    pub description: &'static str,
    /// Whether it is on until the user chooses.
    pub default_enabled: bool,
    /// What menus and the palette call its commands: (command, label).
    pub labels: &'static [(&'static str, &'static str)],
    /// Commands the palette leaves out (keys of its views, or listed
    /// through `palette`).
    pub unlisted: &'static [&'static str],
    /// Items it adds to the built-in menus.
    pub menu_items: &'static [MenuItem],

    // In each window:
    /// Installs its command handlers on the window's root element.
    pub window_actions: fn(Div, &mut Context<Shell>) -> Div,
    /// Its model for the window, kept while it is on (see
    /// `Shell::builtin_model`).
    pub window_model: Option<fn(&mut Window, &mut Context<Shell>) -> AnyEntity>,
    /// Palette entries besides its commands.
    pub palette: fn(&mut Shell, &mut Context<Shell>) -> Vec<Command>,
    /// A project tab of the window finished opening.
    pub project_opened: fn(&mut Shell, &mut Context<Shell>),
    pub window_tabs: &'static [WindowTabKind],

    // In each workspace:
    /// Installs its command handlers on the workspace's root element.
    pub workspace_actions: fn(Div, &mut Context<Workspace>) -> Div,
    /// Its model for the workspace, kept while it is on (see
    /// `Workspace::builtin_model`).
    pub workspace_model: Option<fn(&mut Window, &mut Context<Workspace>) -> AnyEntity>,
    pub panels: &'static [Panel],
    /// Kinds of tabs files open in instead of a text editor.
    pub file_tabs: &'static [FileTabKind],
    /// A file's view: each one opened while the extension is on, and every
    /// open one when it is turned on.
    pub attach: fn(&mut Workspace, &FileView, &mut Context<Workspace>),
    /// Every open file's view when it is turned off.
    pub detach: fn(&mut Workspace, &FileView, &mut Context<Workspace>),
    /// Another tab became the active one.
    pub tab_activated: fn(&mut Workspace, &mut Context<Workspace>),
    /// After external changes to the project's files.
    pub files_changed: fn(&mut Workspace, &Batch, &mut Window, &mut Context<Workspace>),
    /// Shows a language-server location in a document of its own (a
    /// notebook's virtual document); false when it is not.
    pub navigate:
        fn(&mut Workspace, &language::Location, &mut Window, &mut Context<Workspace>) -> bool,
    /// The button of one of its offers (`OfferKind::Builtin`) was pressed;
    /// gets the offer's key.
    pub accept_offer: fn(&mut Workspace, &str, &mut Window, &mut Context<Workspace>),
}

impl Builtin {
    /// No contributions: the rest of a declaration (`..Builtin::NONE`).
    pub const NONE: Self = Self {
        id: "",
        name: "",
        description: "",
        default_enabled: false,
        labels: &[],
        unlisted: &[],
        menu_items: &[],
        window_actions: |element, _| element,
        window_model: None,
        palette: |_, _| Vec::new(),
        project_opened: |_, _| {},
        window_tabs: &[],
        workspace_actions: |element, _| element,
        workspace_model: None,
        panels: &[],
        file_tabs: &[],
        attach: |_, _, _| {},
        detach: |_, _, _| {},
        tab_activated: |_, _| {},
        files_changed: |_, _, _, _| {},
        navigate: |_, _, _, _| false,
        accept_offer: |_, _, _, _| {},
    };
}

/// An open file's view: its text editor, or a built-in extension's file
/// tab (a notebook).
pub enum FileView {
    Editor(Entity<Editor>),
    Builtin(BuiltinTab),
}

/// A kind of tab files open in instead of a text editor (a notebook).
pub struct FileTabKind {
    /// Names its tabs in `next_code context` and their content's pinpoint
    /// key (`<kind>:<file>`).
    pub kind: &'static str,
    /// Whether `path` (read as `text`, when read; else it has a tab of
    /// this kind) opens in it.
    pub opens: fn(&Workspace, &Path, Option<&str>, &App) -> bool,
    /// Shows `path` in the active pane (`text`: what it read as, when
    /// read): its tab there, else a new one (another view of what a tab
    /// of it in another pane shows).
    pub open: OpenFile,
}

/// `FileTabKind::open`.
pub type OpenFile = fn(
    &mut Workspace,
    &Path,
    Option<&str>,
    &mut Window,
    &mut Context<Workspace>,
) -> Result<(), String>;

/// The view in a `FileTabKind` tab: marked dirty, saved and closed as a
/// text editor's file is.
pub trait FileTabView: Render + Focusable {
    fn is_dirty(&self, cx: &App) -> bool;
    /// Writes its unsaved changes; whether that succeeded.
    fn save(&mut self, cx: &mut Context<Self>) -> bool;
    /// No tab shows its file any more: its last one closed, or the project
    /// closes, or the app quits.
    fn release(&mut self, cx: &mut Context<Self>);
    /// Shows part `index` of the file (a notebook's cell, for pinpoint);
    /// false when there is none.
    fn reveal_part(&mut self, index: usize, cx: &mut Context<Self>) -> bool;
    /// The members of the `next_code context` reply while it is the
    /// active tab.
    fn context(&self, project: &Project, limits: Limits, cx: &App) -> Value;
}

/// `FileTabView` without its type.
trait AnyFileTab {
    fn is_dirty(&self, cx: &App) -> bool;
    fn save(&self, cx: &mut App) -> bool;
    fn release(&self, cx: &mut App);
    fn reveal_part(&self, index: usize, cx: &mut App) -> bool;
    fn context(&self, project: &Project, limits: Limits, cx: &App) -> Value;
}

impl<V: FileTabView> AnyFileTab for Entity<V> {
    fn is_dirty(&self, cx: &App) -> bool {
        self.read(cx).is_dirty(cx)
    }

    fn save(&self, cx: &mut App) -> bool {
        self.update(cx, |view, cx| view.save(cx))
    }

    fn release(&self, cx: &mut App) {
        self.update(cx, |view, cx| view.release(cx))
    }

    fn reveal_part(&self, index: usize, cx: &mut App) -> bool {
        self.update(cx, |view, cx| view.reveal_part(index, cx))
    }

    fn context(&self, project: &Project, limits: Limits, cx: &App) -> Value {
        self.read(cx).context(project, limits, cx)
    }
}

/// The file tab kind `kind`, and its extension.
pub fn file_tab(kind: &str) -> Option<(&'static Builtin, &'static FileTabKind)> {
    ALL.iter().find_map(|builtin| {
        let found = builtin.file_tabs.iter().find(|own| own.kind == kind)?;
        Some((*builtin, found))
    })
}

/// `items` in the built-in menu `menu`, after the item that runs `after`
/// (last when the menu has no such item).
pub struct MenuItem {
    pub menu: &'static str,
    pub after: &'static str,
    pub items: &'static [Entry],
}

pub enum Entry {
    /// A command under its label (see `Builtin::labels`).
    Command(&'static str),
    /// A command under another label: (command, label).
    Labeled(&'static str, &'static str),
    Separator,
}

#[cfg(test)]
impl Entry {
    fn command(&self) -> Option<&'static str> {
        match self {
            Self::Command(command) | Self::Labeled(command, _) => Some(command),
            Self::Separator => None,
        }
    }
}

/// A kind of window tab beside the project tabs: a file of a project, or
/// of this machine.
pub struct WindowTabKind {
    /// Names it in saved windows (`{"<key>": file, "project": …}`), the
    /// window title (`<title> (<key>)`) and pinpoint keys (`<key>:<file
    /// name>` for its content).
    pub key: &'static str,
    /// Marks its tabs in the tab strip.
    pub icon: &'static str,
    /// What a tab shows while its project opens.
    pub waiting: &'static str,
    /// The view of `file` (`project`: the project holding it; `None`: on
    /// this machine). `Ok(None)` while that project still opens (the tab
    /// waits for it); an error is shown as a notice.
    pub open: OpenWindowTab,
}

/// `WindowTabKind::open`.
pub type OpenWindowTab = fn(
    &mut Shell,
    &Path,
    Option<&Location>,
    &mut Window,
    &mut Context<Shell>,
) -> Result<Option<WindowTab>, String>;

/// A view shown in a window tab (`WindowTabKind`).
pub trait WindowTabView: Render + Focusable {
    /// What the tab strip shows.
    fn title(&self, cx: &App) -> String;
    fn is_dirty(&self, cx: &App) -> bool;
    /// The colour of its kind's icon.
    fn icon_color(&self, cx: &App) -> Hsla;
    /// The members of the `next_code context` reply while it is shown
    /// (`focus` and its own).
    fn context(&self, limits: Limits, cx: &App) -> Value;
}

/// A window tab's view, with what connects it to the window.
pub struct WindowTab {
    view: Rc<dyn AnyWindowTab>,
    _subscriptions: Vec<Subscription>,
}

impl WindowTab {
    pub fn new<V: WindowTabView>(
        view: Entity<V>,
        mut subscriptions: Vec<Subscription>,
        cx: &mut Context<Shell>,
    ) -> Self {
        // The tab strip shows its title and state.
        subscriptions.push(cx.observe(&view, |_, _, cx| cx.notify()));
        Self {
            view: Rc::new(view),
            _subscriptions: subscriptions,
        }
    }

    pub fn view(&self) -> AnyView {
        self.view.view()
    }

    pub fn entity_id(&self) -> EntityId {
        self.view().entity_id()
    }

    pub fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.view.focus_handle(cx)
    }

    pub fn title(&self, cx: &App) -> String {
        self.view.title(cx)
    }

    pub fn is_dirty(&self, cx: &App) -> bool {
        self.view.is_dirty(cx)
    }

    pub fn icon_color(&self, cx: &App) -> Hsla {
        self.view.icon_color(cx)
    }

    pub fn context(&self, limits: Limits, cx: &App) -> Value {
        self.view.context(limits, cx)
    }
}

/// `WindowTabView` without its type.
trait AnyWindowTab {
    fn view(&self) -> AnyView;
    fn focus_handle(&self, cx: &App) -> FocusHandle;
    fn title(&self, cx: &App) -> String;
    fn is_dirty(&self, cx: &App) -> bool;
    fn icon_color(&self, cx: &App) -> Hsla;
    fn context(&self, limits: Limits, cx: &App) -> Value;
}

impl<V: WindowTabView> AnyWindowTab for Entity<V> {
    fn view(&self) -> AnyView {
        self.clone().into()
    }

    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.read(cx).focus_handle(cx)
    }

    fn title(&self, cx: &App) -> String {
        self.read(cx).title(cx)
    }

    fn is_dirty(&self, cx: &App) -> bool {
        self.read(cx).is_dirty(cx)
    }

    fn icon_color(&self, cx: &App) -> Hsla {
        self.read(cx).icon_color(cx)
    }

    fn context(&self, limits: Limits, cx: &App) -> Value {
        self.read(cx).context(limits, cx)
    }
}

/// The window tab kind keyed `key`, and its extension.
pub fn window_tab(key: &str) -> Option<(&'static Builtin, &'static WindowTabKind)> {
    ALL.iter().find_map(|builtin| {
        let kind = builtin.window_tabs.iter().find(|kind| kind.key == key)?;
        Some((*builtin, kind))
    })
}

/// A built-in extension's dock panel.
pub struct Panel {
    /// Its name in saved layouts and its pinpoint key (`panel:<key>`).
    pub key: &'static str,
    pub title: &'static str,
    /// Where new layouts put it: this dock, after the panel keyed `after`
    /// (last when there is none).
    pub position: DockPosition,
    pub after: &'static str,
    /// Its view in a workspace, made when the workspace opens or the
    /// extension is turned on.
    pub build: fn(&mut Workspace, &mut Window, &mut Context<Workspace>) -> PanelView,
    /// A count beside its title (things waiting for the person).
    pub badge: Option<fn(&Workspace, &App) -> usize>,
}

/// Every built-in extension's panels.
pub fn panels() -> impl Iterator<Item = &'static Panel> {
    ALL.iter().flat_map(|builtin| builtin.panels)
}

pub fn panel(key: &str) -> Option<&'static Panel> {
    panels().find(|panel| panel.key == key)
}

/// The label a built-in extension gives one of its commands.
pub fn label(name: &str) -> Option<&'static str> {
    let (namespace, _) = name.split_once("::")?;
    let builtin = ALL.iter().find(|builtin| builtin.id == namespace)?;
    builtin
        .labels
        .iter()
        .find(|(command, _)| *command == name)
        .map(|(_, label)| *label)
}

/// Whether `builtin` is on.
pub fn enabled(builtin: &Builtin, cx: &mut App) -> bool {
    crate::extensions::shared(cx)
        .read(cx)
        .builtin_enabled(builtin)
}

/// Whether menus leave out `command`: it is a built-in extension's that
/// is off.
pub fn hidden(command: &str, cx: &mut App) -> bool {
    let namespace = command.split_once("::").map(|(namespace, _)| namespace);
    ALL.iter()
        .any(|builtin| Some(builtin.id) == namespace && !enabled(builtin, cx))
}

/// Whether the palette leaves out `command` (`Builtin::unlisted`).
pub fn unlisted(command: &str) -> bool {
    ALL.iter()
        .any(|builtin| builtin.unlisted.contains(&command))
}

/// The built-in extensions that are on.
pub fn on(cx: &mut App) -> Vec<&'static Builtin> {
    ALL.iter()
        .copied()
        .filter(|builtin| enabled(builtin, cx))
        .collect()
}

/// The window's root element with the commands of the built-in
/// extensions that are on.
pub fn window_actions(element: Div, cx: &mut Context<Shell>) -> Div {
    on(cx).into_iter().fold(element, |element, builtin| {
        (builtin.window_actions)(element, cx)
    })
}

/// The workspace's root element with the commands of the built-in
/// extensions that are on.
pub fn workspace_actions(element: Div, cx: &mut Context<Workspace>) -> Div {
    on(cx).into_iter().fold(element, |element, builtin| {
        (builtin.workspace_actions)(element, cx)
    })
}

impl Builtin {
    /// A tab of this extension showing `view`, which closes when the
    /// extension is turned off. `kind` says what the view shows
    /// (`assembly`): the tab's kind in `next_code context` and the start
    /// of its pinpoint key.
    pub fn tab<V: Render + Focusable>(&self, view: Entity<V>, kind: &'static str) -> BuiltinTab {
        BuiltinTab {
            view: View::new(view),
            extension: self.id,
            kind,
            file: None,
        }
    }

    /// A tab of this extension showing `view` of a file, in a tab of
    /// `kind` (one of its `file_tabs`).
    pub fn file_tab<V: FileTabView>(&self, view: Entity<V>, kind: &'static str) -> BuiltinTab {
        BuiltinTab {
            view: View::new(view.clone()),
            extension: self.id,
            kind,
            file: Some(Rc::new(view)),
        }
    }
}

/// A view's focus handle.
type Focus = Rc<dyn Fn(&App) -> FocusHandle>;

/// Any focusable view.
#[derive(Clone)]
struct View {
    view: AnyView,
    focus: Focus,
}

impl View {
    fn new<V: Render + Focusable>(view: Entity<V>) -> Self {
        Self {
            view: view.clone().into(),
            focus: Rc::new(move |cx| view.focus_handle(cx)),
        }
    }
}

/// A built-in extension's panel in one workspace, with what connects it
/// there (dropped with it when the extension is turned off).
pub struct PanelView {
    view: AnyView,
    /// `None` for a panel that is never focused (`next_code context` then
    /// never names it).
    focus: Option<Focus>,
    _subscriptions: Vec<Subscription>,
}

impl PanelView {
    pub fn new<V: Render + Focusable>(view: Entity<V>, subscriptions: Vec<Subscription>) -> Self {
        let View { view, focus } = View::new(view);
        Self {
            view,
            focus: Some(focus),
            _subscriptions: subscriptions,
        }
    }

    pub fn without_focus<V: Render>(view: Entity<V>, subscriptions: Vec<Subscription>) -> Self {
        Self {
            view: view.into(),
            focus: None,
            _subscriptions: subscriptions,
        }
    }

    pub fn view(&self) -> AnyView {
        self.view.clone()
    }

    pub fn focus_handle(&self, cx: &App) -> Option<FocusHandle> {
        self.focus.as_ref().map(|focus| focus(cx))
    }

    /// Its view, when it is a `V`.
    pub fn downcast<V: 'static>(&self) -> Option<Entity<V>> {
        self.view.clone().downcast().ok()
    }
}

/// A built-in extension's tab in the editor area: any focusable view, or
/// a file's (`Builtin::file_tab`).
#[derive(Clone)]
pub struct BuiltinTab {
    view: View,
    extension: &'static str,
    kind: &'static str,
    file: Option<Rc<dyn AnyFileTab>>,
}

impl BuiltinTab {
    pub fn view(&self) -> AnyView {
        self.view.view.clone()
    }

    pub fn entity_id(&self) -> EntityId {
        self.view.view.entity_id()
    }

    pub fn focus_handle(&self, cx: &App) -> FocusHandle {
        (self.view.focus)(cx)
    }

    pub fn kind(&self) -> &'static str {
        self.kind
    }

    /// The id of the extension it belongs to.
    pub fn extension(&self) -> &'static str {
        self.extension
    }

    /// Its view, when it is a `V`.
    pub fn downcast<V: 'static>(&self) -> Option<Entity<V>> {
        self.view.view.clone().downcast().ok()
    }

    /// Whether it shows a file (`Builtin::file_tab`).
    pub fn is_file(&self) -> bool {
        self.file.is_some()
    }

    pub fn is_dirty(&self, cx: &App) -> bool {
        self.file.as_ref().is_some_and(|file| file.is_dirty(cx))
    }

    /// Writes a file tab's unsaved changes; whether that succeeded.
    pub fn save(&self, cx: &mut App) -> bool {
        self.file.as_ref().is_none_or(|file| file.save(cx))
    }

    /// See `FileTabView::release`.
    pub fn release(&self, cx: &mut App) {
        if let Some(file) = &self.file {
            file.release(cx);
        }
    }

    /// See `FileTabView::reveal_part`; `None` when it shows no file.
    pub fn reveal_part(&self, index: usize, cx: &mut App) -> Option<bool> {
        Some(self.file.as_ref()?.reveal_part(index, cx))
    }

    /// See `FileTabView::context`; `None` when it shows no file.
    pub fn context(&self, project: &Project, limits: Limits, cx: &App) -> Option<Value> {
        Some(self.file.as_ref()?.context(project, limits, cx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::menus::{Item, MenuBar};

    #[test]
    fn commands_are_registered_in_the_extensions_namespace() {
        let registered: Vec<&str> = gpui::generate_list_of_all_registered_actions()
            .map(|action| action.name)
            .collect();
        for builtin in ALL {
            let labelled = builtin.labels.iter().map(|(command, _)| *command);
            let in_menus = builtin.menu_items.iter().flat_map(|item| item.items);
            for command in labelled.chain(in_menus.filter_map(Entry::command)) {
                assert!(registered.contains(&command), "{command} is not registered");
                assert_eq!(
                    command.split_once("::").map(|(namespace, _)| namespace),
                    Some(builtin.id),
                    "{command} is outside its extension's namespace"
                );
            }
        }
        assert_eq!(
            crate::commands::label("assembly::InspectAssembly"),
            "Inspect Assembly"
        );
    }

    #[gpui::test]
    fn assembly_is_off_until_turned_on_and_the_choice_is_saved(cx: &mut gpui::TestAppContext) {
        let store = cx.update(crate::extensions::shared);
        cx.run_until_parked();
        let hidden = |command: &'static str, cx: &mut gpui::TestAppContext| {
            cx.update(|cx| hidden(command, cx))
        };
        assert!(hidden("assembly::InspectAssembly", cx));
        assert!(!hidden("editor::Save", cx));
        store.update(cx, |store, cx| {
            store.set_builtin_enabled("assembly", true, cx)
        });
        cx.run_until_parked();
        assert!(!hidden("assembly::InspectAssembly", cx));
        // Read back from the file, as the next start does.
        store.update(cx, |store, cx| store.refresh(cx));
        cx.run_until_parked();
        assert!(!hidden("assembly::InspectAssembly", cx));
        // Leaves the file as the default for other tests.
        store.update(cx, |store, cx| {
            store.set_builtin_enabled("assembly", false, cx)
        });
        cx.run_until_parked();
        assert!(hidden("assembly::InspectAssembly", cx));
    }

    #[test]
    fn menu_items_follow_their_command() {
        let bar = MenuBar::default();
        let command = |item: &Item| match item {
            Item::Plain(command) | Item::Command { command, .. } => Some(command.clone()),
            _ => None,
        };
        for item in ALL.iter().flat_map(|builtin| builtin.menu_items) {
            let menu = bar
                .menus
                .iter()
                .find(|menu| menu.label == item.menu)
                .unwrap();
            let commands: Vec<String> = menu.items.iter().filter_map(command).collect();
            let after = commands
                .iter()
                .position(|existing| existing == item.after)
                .unwrap_or_else(|| panic!("{} is not in {}", item.after, item.menu));
            let expected: Vec<&str> = item
                .items
                .iter()
                .map(|entry| entry.command().unwrap_or(crate::menus::SEPARATOR))
                .collect();
            let first = expected
                .iter()
                .position(|entry| *entry != crate::menus::SEPARATOR);
            let first = first.unwrap_or_default();
            let start = after
                + 1
                + commands[after + 1..]
                    .iter()
                    .position(|existing| existing == expected[first])
                    .unwrap_or_else(|| panic!("{} is not in {}", expected[first], item.menu))
                - first;
            assert_eq!(
                commands[start..][..expected.len()],
                expected,
                "in {}",
                item.menu
            );
            // Between: only other extensions' items at the same place.
            let builtin_items: Vec<&str> = ALL
                .iter()
                .flat_map(|builtin| builtin.menu_items)
                .flat_map(|item| item.items)
                .filter_map(Entry::command)
                .collect();
            for between in &commands[after + 1..start] {
                assert!(
                    between == crate::menus::SEPARATOR || builtin_items.contains(&between.as_str()),
                    "{between} is between {} and its items",
                    item.after
                );
            }
        }
    }
}
