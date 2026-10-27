//! User-installed WASM extensions. One model per application keeps permission
//! grants, cancellation and command availability consistent across windows.

use std::{
    collections::{BTreeMap, VecDeque},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use anyhow::{Context as _, bail};
use extension_api::Permission;
use extensions::Extension;
use gpui::{
    App, AppContext, Context, Entity, EventEmitter, FocusHandle, Focusable, Global, KeyBinding,
    MouseButton, SharedString, Subscription, Window, actions, div, prelude::*, px,
};
use serde::{Deserialize, Serialize};

use crate::{
    builtins::{self, Builtin},
    editor::{self, Editor},
    pinpoint::Pin,
    selectable::selectable,
    theme,
};

actions!(extensions, [ManageExtensions, InstallExtension, Dismiss]);

#[derive(Clone, PartialEq, Debug, gpui::Action)]
#[action(namespace = extensions, no_json)]
pub struct RunExtensionCommand {
    pub extension: String,
    pub command: String,
}

#[derive(Clone, PartialEq, Debug, gpui::Action)]
#[action(namespace = extensions, no_json)]
pub struct OpenExtensionPanel {
    pub extension: String,
    pub panel: String,
}

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([KeyBinding::new("escape", Dismiss, Some("Extensions"))]);
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    /// A grant records the approved capability set, not merely an extension ID.
    enabled: BTreeMap<String, Vec<Permission>>,
    /// Built-in extensions turned on or off; the others keep their default.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    builtin: BTreeMap<String, bool>,
}

fn config_dir() -> anyhow::Result<PathBuf> {
    #[cfg(test)]
    {
        Ok(std::env::temp_dir().join(format!("next-code-extension-tests-{}", std::process::id())))
    }
    #[cfg(not(test))]
    {
        dirs::config_dir()
            .map(|dir| dir.join("next_code"))
            .ok_or_else(|| anyhow::anyhow!("No user configuration directory"))
    }
}

fn read_state() -> anyhow::Result<State> {
    let path = config_dir()?.join("extension-state.json");
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).context("Reading extension permission grants"),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
        Err(err) => Err(err).context("Reading extension permission grants"),
    }
}

fn save_state(state: &State) -> anyhow::Result<()> {
    let dir = config_dir()?;
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("extension-state.json");
    let temporary = dir.join("extension-state.json.tmp");
    std::fs::write(&temporary, serde_json::to_vec_pretty(state)?)?;
    std::fs::rename(temporary, path).context("Saving extension permission grants")
}

pub fn settings_path(id: &str) -> anyhow::Result<PathBuf> {
    Ok(config_dir()?
        .join("extension-settings")
        .join(format!("{id}.json")))
}

pub fn ensure_settings(id: &str) -> anyhow::Result<PathBuf> {
    let path = settings_path(id)?;
    std::fs::create_dir_all(path.parent().unwrap())?;
    use std::io::Write;
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(mut file) => file.write_all(b"{}\n")?,
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(err) => return Err(err.into()),
    }
    Ok(path)
}

pub fn read_settings(id: &str) -> anyhow::Result<serde_json::Value> {
    let path = settings_path(id)?;
    use std::io::Read;
    let file = match std::fs::File::open(&path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(serde_json::json!({})),
        Err(err) => return Err(err.into()),
    };
    let mut bytes = Vec::new();
    file.take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() <= 1024 * 1024,
        "Extension settings exceed 1 MiB"
    );
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).with_context(|| format!("Reading {}", path.display()))?;
    anyhow::ensure!(
        value.is_object(),
        "Extension settings must be a JSON object"
    );
    Ok(value)
}

pub struct Entry {
    pub extension: Extension,
    pub enabled: bool,
}

pub struct Extensions {
    pub entries: Vec<Entry>,
    state: State,
    pub loading: bool,
    pub running: Option<(String, Arc<AtomicBool>)>,
    /// Changes only when installed WASM snapshots are reloaded; unrelated
    /// installs and permission changes must not reset other panels' state.
    pub generation: u64,
    output: VecDeque<(String, SharedString)>,
    output_bytes: usize,
}

struct Registry(Entity<Extensions>);
impl Global for Registry {}

pub fn shared(cx: &mut App) -> Entity<Extensions> {
    if let Some(registry) = cx.try_global::<Registry>() {
        return registry.0.clone();
    }
    let store = cx.new(|cx| {
        let mut store = Extensions {
            entries: Vec::new(),
            state: State::default(),
            loading: false,
            running: None,
            generation: 0,
            output: VecDeque::new(),
            output_bytes: 0,
        };
        store.refresh(cx);
        store
    });
    cx.set_global(Registry(store.clone()));
    store
}

impl Extensions {
    pub fn log(&mut self, id: &str, message: impl Into<SharedString>, cx: &mut Context<Self>) {
        let message = message.into();
        self.output_bytes += id.len() + message.len();
        self.output.push_back((id.to_owned(), message));
        while self.output.len() > 1000 || self.output_bytes > 4 * 1024 * 1024 {
            if let Some((id, message)) = self.output.pop_front() {
                self.output_bytes -= id.len() + message.len();
            }
        }
        cx.notify();
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.loading || self.running.is_some() {
            return;
        }
        self.loading = true;
        self.generation += 1;
        let task = cx.background_spawn(async move {
            let state = read_state()?;
            let root = config_dir()?.join("extensions");
            Ok::<_, anyhow::Error>((state, Extension::discover(&root)))
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.loading = false;
                match result {
                    Ok((state, found)) => {
                        this.state = state;
                        this.entries.clear();
                        for (path, result) in found {
                            match result {
                                Ok(extension) => {
                                    let enabled =
                                        this.state.enabled.get(&extension.manifest.id).is_some_and(
                                            |granted| {
                                                extension
                                                    .manifest
                                                    .permissions
                                                    .iter()
                                                    .all(|permission| granted.contains(permission))
                                            },
                                        );
                                    this.entries.push(Entry { extension, enabled });
                                }
                                Err(err) => {
                                    this.log("host", format!("{}: {err:#}", path.display()), cx)
                                }
                            }
                        }
                    }
                    Err(err) => {
                        // A malformed grant file must never leave old approvals active.
                        this.entries.clear();
                        this.state = State::default();
                        this.log("host", format!("{err:#}"), cx);
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    pub fn install(&mut self, source: PathBuf, cx: &mut Context<Self>) {
        if self.loading || self.running.is_some() {
            return;
        }
        self.loading = true;
        let task = cx.background_spawn(async move {
            Extension::install(&source, &config_dir()?.join("extensions"))
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.loading = false;
                match result {
                    Ok(extension) => {
                        let id = extension.manifest.id.clone();
                        // Installation never grants access or executes guest code.
                        this.entries.push(Entry {
                            extension,
                            enabled: false,
                        });
                        this.entries
                            .sort_by(|a, b| a.extension.manifest.id.cmp(&b.extension.manifest.id));
                        this.log(
                            &id,
                            "Installed, disabled. Enable to review and approve permissions.",
                            cx,
                        );
                    }
                    Err(err) => this.log("host", format!("Installation failed: {err:#}"), cx),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    pub fn set_enabled(&mut self, id: &str, enabled: bool, cx: &mut Context<Self>) {
        let Some(entry) = self
            .entries
            .iter()
            .find(|entry| entry.extension.manifest.id == id)
        else {
            return;
        };
        if self.loading {
            return;
        }
        let mut next = self.state.clone();
        if enabled {
            next.enabled
                .insert(id.to_owned(), entry.extension.manifest.permissions.clone());
        } else {
            next.enabled.remove(id);
            if let Some((running, cancelled)) = &self.running
                && running == id
            {
                cancelled.store(true, Ordering::Relaxed);
            }
        }
        // Grants are small user configuration; persist off-thread and do not
        // expose commands until saving succeeds.
        self.loading = true;
        let task = cx.background_spawn(async move { save_state(&next).map(|()| next) });
        let id = id.to_owned();
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.loading = false;
                match result {
                    Ok(state) => {
                        this.state = state;
                        if let Some(entry) = this
                            .entries
                            .iter_mut()
                            .find(|entry| entry.extension.manifest.id == id)
                        {
                            entry.enabled = enabled;
                        }
                        this.log(&id, if enabled { "Enabled" } else { "Disabled" }, cx);
                    }
                    Err(err) => this.log(&id, format!("Could not save permissions: {err:#}"), cx),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// Whether `builtin` is on: as last chosen, else its default.
    pub fn builtin_enabled(&self, builtin: &Builtin) -> bool {
        self.state
            .builtin
            .get(builtin.id)
            .copied()
            .unwrap_or(builtin.default_enabled)
    }

    pub fn set_builtin_enabled(&mut self, id: &str, enabled: bool, cx: &mut Context<Self>) {
        if self.loading || !builtins::ALL.iter().any(|builtin| builtin.id == id) {
            return;
        }
        let mut next = self.state.clone();
        next.builtin.insert(id.to_owned(), enabled);
        // As with grants: windows change only once the choice is saved.
        self.loading = true;
        let task = cx.background_spawn(async move { save_state(&next).map(|()| next) });
        let id = id.to_owned();
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.loading = false;
                match result {
                    Ok(state) => {
                        this.state = state;
                        this.log(&id, if enabled { "Enabled" } else { "Disabled" }, cx);
                    }
                    Err(err) => this.log(&id, format!("Could not save: {err:#}"), cx),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// Turns a built-in extension on or off without saving the choice.
    #[cfg(test)]
    pub fn set_builtin_unsaved(&mut self, id: &str, enabled: bool, cx: &mut Context<Self>) {
        self.state.builtin.insert(id.to_owned(), enabled);
        cx.notify();
    }

    pub fn begin(
        &mut self,
        id: &str,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<(Extension, Arc<AtomicBool>)> {
        if self.loading {
            bail!("Extensions are still loading");
        }
        if self.running.is_some() {
            bail!(
                "An extension command is already running; wait or cancel it in Manage Extensions"
            );
        }
        let extension = self
            .entries
            .iter()
            .find(|entry| entry.enabled && entry.extension.manifest.id == id)
            .map(|entry| entry.extension.clone())
            .context("Extension is disabled or not installed")?;
        let cancelled = Arc::new(AtomicBool::new(false));
        self.running = Some((id.to_owned(), cancelled.clone()));
        self.log(id, "Running…", cx);
        Ok((extension, cancelled))
    }

    pub fn finish(
        &mut self,
        id: &str,
        logs: Vec<String>,
        result: Result<(), String>,
        cx: &mut Context<Self>,
    ) {
        self.running = None;
        for message in logs {
            self.log(id, message, cx);
        }
        match result {
            Ok(()) => self.log(id, "Completed", cx),
            Err(err) => self.log(id, format!("Error: {err}"), cx),
        }
        cx.notify();
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        if let Some((id, cancelled)) = &self.running {
            cancelled.store(true, Ordering::Relaxed);
            let id = id.clone();
            self.log(&id, "Cancelled; any pending edits will be discarded", cx);
        }
    }
}

pub enum PanelEvent {
    Close,
    Toggle(String),
    Settings(String),
    InstallFolder,
    OpenPanel { extension: String, panel: String },
}

pub struct ExtensionPanel {
    store: Entity<Extensions>,
    source: Entity<Editor>,
    focus: FocusHandle,
    selected: Option<String>,
    _subscription: Subscription,
}
impl EventEmitter<PanelEvent> for ExtensionPanel {}

impl ExtensionPanel {
    pub fn new(store: Entity<Extensions>, cx: &mut Context<Self>) -> Self {
        let subscription = cx.observe(&store, |_, _, cx| cx.notify());
        let source = cx.new(Editor::single_line);
        Self {
            store,
            source,
            focus: cx.focus_handle(),
            selected: None,
            _subscription: subscription,
        }
    }
    fn dismiss(&mut self, _: &Dismiss, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(PanelEvent::Close);
    }
    fn install_path(&mut self, _: &editor::Newline, _: &mut Window, cx: &mut Context<Self>) {
        let source = self.source.read(cx).text();
        if !source.trim().is_empty() {
            self.store.update(cx, |store, cx| {
                store.install(Path::new(source.trim()).to_path_buf(), cx)
            });
        }
    }
}
impl Focusable for ExtensionPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for ExtensionPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let store = self.store.read(cx);
        let loading = store.loading;
        let running = store.running.is_some();
        let entries: Vec<_> = store
            .entries
            .iter()
            .map(|entry| {
                (
                    entry.extension.manifest.id.clone(),
                    entry.extension.manifest.name.clone(),
                    entry.extension.manifest.version.clone(),
                    entry.enabled,
                    entry.extension.manifest.panels.clone(),
                )
            })
            .collect();
        let output: Vec<_> = store
            .output
            .iter()
            .filter(|(id, _)| {
                self.selected
                    .as_ref()
                    .is_none_or(|selected| selected == id || id == "host")
            })
            .map(|(id, message)| format!("[{id}] {message}"))
            .collect();
        let builtins: Vec<_> = builtins::ALL
            .iter()
            .map(|builtin| (*builtin, store.builtin_enabled(builtin)))
            .collect();
        div().key_context("Extensions").track_focus(&self.focus)
            .on_action(cx.listener(Self::dismiss))
            .on_action(cx.listener(Self::install_path))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .w(px(760.)).max_h(px(650.)).flex().flex_col().gap_2().p_3()
            .bg(theme::panel()).text_color(theme::text()).text_sm()
            .border_1().border_color(theme::border()).rounded_md().shadow_lg()
            .child(div().flex().justify_between().child("Extensions")
                .child(div().id("close").cursor_pointer().child("Close")
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(PanelEvent::Close))).pin("close")))
            .child(div().text_color(theme::muted()).child("Built-in — part of next_code; turn on what you use."))
            .child(div().id("builtins").max_h(px(160.)).overflow_y_scroll().flex().flex_col()
                .children(builtins.into_iter().map(|(builtin, enabled)| {
                    let id = builtin.id;
                    div().id(id).flex().items_center().gap_3().py_1()
                        .border_b_1().border_color(theme::border())
                        .child(div().flex_1().flex().flex_col()
                            .child(builtin.name)
                            .child(div().text_xs().text_color(theme::muted()).child(builtin.description)))
                        .child(div().id("toggle").cursor_pointer().child(if enabled { "Disable" } else { "Enable" })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.store.update(cx, |store, cx| store.set_builtin_enabled(id, !enabled, cx));
                            }))
                            .pin("toggle"))
                        .pin(format!("builtin:{id}")).label(builtin.name)
                })))
            .child(div().text_color(theme::muted()).child("Installed — Rust / WebAssembly. Local packages start disabled. Enable to review document permissions. No filesystem, network or process access."))
            .child(div().flex().gap_2().items_center()
                .child("Package folder:")
                .child(div().flex_1().h(px(28.)).px_1().border_1().border_color(theme::border())
                    .child(self.source.clone()).pin("source").label("Extension package folder"))
                .child(div().id("install-path").cursor_pointer().child("Install path")
                    .on_click(cx.listener(|this, _, window, cx| this.install_path(&editor::Newline, window, cx)))
                    .pin("install-path"))
                .child(div().id("browse").cursor_pointer().child("Browse…")
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(PanelEvent::InstallFolder))).pin("browse")))
            .when(loading, |panel| panel.child(div().text_color(theme::muted()).child("Loading…")))
            .child(div().id("packages").max_h(px(230.)).overflow_y_scroll().flex().flex_col()
                .when(entries.is_empty() && !loading, |list| list.child("No extensions installed"))
                .children(entries.into_iter().map(|(id, name, version, enabled, panels)| {
                    let toggle = id.clone();
                    let settings = id.clone();
                    let select = id.clone();
                    div().id(SharedString::from(id.clone())).flex().flex_col().py_2()
                        .child(div().flex().items_center().gap_3()
                        .border_b_1().border_color(theme::border())
                        .child(div().id("select").flex_1().cursor_pointer()
                            .child(format!("{name} {version} ({id})"))
                            .on_click(cx.listener(move |this, _, _, cx| { this.selected = Some(select.clone()); cx.notify(); })))
                        .child(div().id("toggle").cursor_pointer().child(if enabled { "Disable" } else { "Enable…" })
                            .on_click(cx.listener(move |_, _, _, cx| cx.emit(PanelEvent::Toggle(toggle.clone()))))
                            .pin(format!("toggle:{id}")))
                        .child(div().id("settings").cursor_pointer().child("Settings")
                            .on_click(cx.listener(move |_, _, _, cx| cx.emit(PanelEvent::Settings(settings.clone()))))
                            .pin(format!("settings:{id}"))))
                        .children(panels.into_iter().map(|panel| {
                            let extension = id.clone();
                            let panel_id = panel.id.clone();
                            div().id(SharedString::from(panel.id.clone())).px_2().py_1()
                                .cursor_pointer().text_color(if enabled { theme::accent() } else { theme::muted() })
                                .child(format!("Open {}", panel.title))
                                .on_click(cx.listener(move |_, _, _, cx| {
                                    if enabled { cx.emit(PanelEvent::OpenPanel {
                                        extension: extension.clone(), panel: panel_id.clone(),
                                    }); }
                                }))
                                .pin(format!("open:{id}:{}", panel.id))
                        }))
                        .pin(format!("extension:{id}"))
                })))
            .child(div().flex().justify_between()
                .child(div().id("all-output").cursor_pointer().child("Extension output (all)")
                    .on_click(cx.listener(|this, _, _, cx| { this.selected = None; cx.notify(); })).pin("all-output"))
                .child(div().id("reload").cursor_pointer().child("Reload packages")
                    .on_click(cx.listener(|this, _, _, cx| this.store.update(cx, |store, cx| store.refresh(cx))))
                    .pin("reload"))
                .when(running, |row| row.child(div().id("cancel").cursor_pointer().child("Cancel command")
                    .on_click(cx.listener(|this, _, _, cx| this.store.update(cx, |store, cx| store.cancel(cx)))).pin("cancel"))))
            .child(div().id("output").h(px(200.)).overflow_y_scroll().p_2().bg(theme::background())
                .child(selectable("messages", output.join("\n")))
                .pin("output").label("Extension output"))
            .pin("extensions").label("Manage Extensions")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_in_choices_are_saved_beside_grants_once_made() {
        // Files from before built-in extensions load, and are written back
        // unchanged.
        let json = r#"{"enabled":{"example.upper":["read-document"]}}"#;
        let old: State = serde_json::from_str(json).unwrap();
        assert!(old.builtin.is_empty());
        assert_eq!(serde_json::to_string(&old).unwrap(), json);
        let mut state = old.clone();
        state.builtin.insert("assembly".into(), true);
        let saved = serde_json::to_string(&state).unwrap();
        assert_eq!(serde_json::from_str::<State>(&saved).unwrap(), state);
    }
}
