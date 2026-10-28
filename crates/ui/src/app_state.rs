//! The open windows (their tabs, active tab, bounds and which one had
//! focus), kept as `windows.json` in the local data dir next to
//! `recent.json` so a launch without a project reopens them, as VS Code's
//! `window.restoreWindows: all` does.
//!
//! Every window reports itself (`update`) when its tabs, active tab or
//! bounds change; the file follows 500 ms later. Closing a window while
//! others remain drops it; closing the last one and quitting keep every
//! window open at that moment. A window without tabs is kept only when it
//! is the only one. An editor that stays running after its last window
//! (`instance::stays_resident`) starts tracking afresh: the next launch
//! reopens what was saved.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use gpui::{App, Bounds, Global, Pixels, Task, WindowBounds, WindowId, point, px, size};
use serde::{Deserialize, Serialize};

use crate::{project::Location, settings::Settings};

const SAVE_DELAY: Duration = Duration::from_millis(500);
/// The smallest window a saved state opens.
const MIN_WIDTH: f32 = 320.;
const MIN_HEIGHT: f32 = 240.;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AppState {
    /// In the order they were opened.
    pub windows: Vec<SavedWindow>,
    /// The index of the window that had focus last.
    #[serde(default)]
    pub focused: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedWindow {
    /// Project tabs as `recent.json` stores locations, and built-in
    /// extensions' file tabs.
    pub tabs: Vec<TabTarget>,
    #[serde(default)]
    pub active: usize,
    #[serde(default)]
    pub bounds: Option<SavedBounds>,
}

/// What a window tab shows: a project, or a file in a built-in
/// extension's tab of kind `kind` (`builtins::WindowTabKind`; `project`:
/// the project holding it, `None` on this machine). A project is saved as
/// its location; a file as `{"<kind>": file, "project": …}`.
#[derive(Debug, Clone, PartialEq)]
pub enum TabTarget {
    Project(Location),
    File {
        kind: String,
        path: PathBuf,
        project: Option<Location>,
    },
}

impl TabTarget {
    pub fn project(&self) -> Option<&Location> {
        match self {
            Self::Project(location) => Some(location),
            Self::File { .. } => None,
        }
    }
}

impl Serialize for TabTarget {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        match self {
            Self::Project(location) => location.serialize(serializer),
            Self::File {
                kind,
                path,
                project,
            } => {
                let mut map = serializer.serialize_map(None)?;
                map.serialize_entry(kind, path)?;
                if let Some(project) = project {
                    map.serialize_entry("project", project)?;
                }
                map.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for TabTarget {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let mut value = serde_json::Value::deserialize(deserializer)?;
        if let Ok(location) = Location::deserialize(&value) {
            return Ok(Self::Project(location));
        }
        let unknown = || D::Error::custom("neither a project nor a file tab");
        let fields = value.as_object_mut().ok_or_else(unknown)?;
        let project = match fields.remove("project") {
            Some(project) => Option::<Location>::deserialize(project).map_err(D::Error::custom)?,
            None => None,
        };
        let mut fields = fields.iter();
        match (fields.next(), fields.next()) {
            (Some((kind, serde_json::Value::String(path))), None) => Ok(Self::File {
                kind: kind.clone(),
                path: path.into(),
                project,
            }),
            _ => Err(unknown()),
        }
    }
}

/// Logical pixels in screen coordinates; for a maximized or fullscreen
/// window the size it returns to.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SavedBounds {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    #[serde(default)]
    pub state: WindowState,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowState {
    #[default]
    Windowed,
    Maximized,
    Fullscreen,
}

impl From<WindowBounds> for SavedBounds {
    fn from(bounds: WindowBounds) -> Self {
        let (inner, state) = match bounds {
            WindowBounds::Windowed(inner) => (inner, WindowState::Windowed),
            WindowBounds::Maximized(inner) => (inner, WindowState::Maximized),
            WindowBounds::Fullscreen(inner) => (inner, WindowState::Fullscreen),
        };
        Self {
            x: inner.origin.x.into(),
            y: inner.origin.y.into(),
            width: inner.size.width.into(),
            height: inner.size.height.into(),
            state,
        }
    }
}

impl SavedBounds {
    /// Where to open the window: on the display it overlaps most (the
    /// first, the primary, when it overlaps none, e.g. its monitor was
    /// unplugged), shrunk to fit and moved fully onto it.
    pub fn clamped(&self, displays: &[Bounds<Pixels>]) -> WindowBounds {
        let rect = |bounds: &Bounds<Pixels>| -> [f32; 4] {
            [
                bounds.origin.x.into(),
                bounds.origin.y.into(),
                bounds.size.width.into(),
                bounds.size.height.into(),
            ]
        };
        let overlap = |[x, y, width, height]: [f32; 4]| {
            let across = (x + width).min(self.x + self.width) - x.max(self.x);
            let down = (y + height).min(self.y + self.height) - y.max(self.y);
            across.max(0.) * down.max(0.)
        };
        let mut width = self.width.max(MIN_WIDTH);
        let mut height = self.height.max(MIN_HEIGHT);
        let (mut x, mut y) = (self.x, self.y);
        let best = displays
            .iter()
            .map(rect)
            .map(|display| (overlap(display), display))
            .fold(
                None,
                |best: Option<(f32, [f32; 4])>, (area, display)| match best {
                    Some((most, _)) if most >= area => best,
                    _ => Some((area, display)),
                },
            );
        let display = match best {
            Some((area, display)) if area > 0. => Some(display),
            _ => displays.first().map(rect),
        };
        if let Some([left, top, display_width, display_height]) = display {
            width = width.min(display_width);
            height = height.min(display_height);
            x = x.clamp(left, left + display_width - width);
            y = y.clamp(top, top + display_height - height);
        }
        let bounds = Bounds {
            origin: point(px(x), px(y)),
            size: size(px(width), px(height)),
        };
        match self.state {
            WindowState::Windowed => WindowBounds::Windowed(bounds),
            WindowState::Maximized => WindowBounds::Maximized(bounds),
            WindowState::Fullscreen => WindowBounds::Fullscreen(bounds),
        }
    }
}

impl AppState {
    /// Without project folders and local files that no longer exist, and without windows left with no tabs by that; `active` and
    /// `focused` in range.
    fn existing(self) -> Self {
        fn exists(location: &Location) -> bool {
            match location {
                Location::Local(root) => root.is_dir(),
                Location::DevContainer { folder, .. } => exists(folder),
                Location::Wsl { .. }
                | Location::Remote { .. }
                | Location::Workspace {
                    distro: Some(_), ..
                } => true,
                Location::Workspace {
                    distro: None,
                    file: Some(file),
                    ..
                } => Path::new(file).is_file(),
                // An untitled workspace reopens while a folder is left.
                Location::Workspace {
                    distro: None,
                    file: None,
                    folders,
                } => folders
                    .iter()
                    .any(|folder| Path::new(&folder.path).is_dir()),
            }
        }
        fn tab_exists(tab: &TabTarget) -> bool {
            match tab {
                TabTarget::Project(location) => exists(location),
                TabTarget::File { path, project, .. } => match project {
                    None | Some(Location::Local(_)) => path.is_file(),
                    Some(location) => exists(location),
                },
            }
        }
        let mut focused = 0;
        let mut windows = Vec::new();
        for (index, mut window) in self.windows.into_iter().enumerate() {
            let had_tabs = !window.tabs.is_empty();
            let active = window.tabs.get(window.active).cloned();
            window.tabs.retain(tab_exists);
            if had_tabs && window.tabs.is_empty() {
                continue;
            }
            window.active = active
                .and_then(|active| window.tabs.iter().position(|tab| *tab == active))
                .unwrap_or(0);
            if index == self.focused {
                focused = windows.len();
            }
            windows.push(window);
        }
        Self { windows, focused }
    }

    /// The project tab that will show in the focused window.
    pub fn focused_tab(&self) -> Option<&Location> {
        let window = self.windows.get(self.focused)?;
        window.tabs.get(window.active)?.project()
    }
}

fn path() -> Option<PathBuf> {
    Some(crate::layout::state_root()?.join("windows.json"))
}

/// The saved state; `None` when missing or unreadable.
pub fn load() -> Option<AppState> {
    load_json_backed(&path()?)
}

/// Reads and parses `path`, or `<path>.bak` (the previous good copy, see
/// [`save_json_backed`]) when `path` is missing or does not parse.
pub(crate) fn load_json_backed<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    let parse = |path: &Path| {
        let json = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&json).ok()
    };
    parse(path).or_else(|| parse(&backup_path(path)))
}

/// Writes `value` as pretty JSON to `path` without ever leaving it
/// truncated (`worktree::write_atomic`), keeping the previous file, when
/// it parsed, as `<path>.bak` for [`load_json_backed`] to fall back to.
pub(crate) fn save_json_backed<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let json = serde_json::to_string_pretty(value).map_err(std::io::Error::other)?;
    if let Ok(old) = std::fs::read_to_string(path)
        && serde_json::from_str::<serde_json::Value>(&old).is_ok()
    {
        std::fs::rename(path, backup_path(path)).ok();
    }
    worktree::write_atomic(path, json.as_bytes())
}

fn backup_path(path: &Path) -> PathBuf {
    let mut backup = path.as_os_str().to_owned();
    backup.push(".bak");
    PathBuf::from(backup)
}

/// What a launch without a project reopens: nothing (the welcome screen)
/// when `restore_windows` is off or nothing is saved.
pub fn restorable() -> AppState {
    if !Settings::load().0.restore_windows {
        return AppState::default();
    }
    load().map(AppState::existing).unwrap_or_default()
}

/// The open windows as their shells last reported them.
#[derive(Default)]
struct Tracker {
    windows: Vec<(WindowId, SavedWindow)>,
    focused: Option<WindowId>,
    /// Quitting: windows closing now stay in the saved state.
    quitting: bool,
    save: Option<Task<()>>,
}

impl Global for Tracker {}

impl Tracker {
    fn state(&self) -> AppState {
        let focused = self
            .focused
            .and_then(|focused| self.windows.iter().position(|(id, _)| *id == focused));
        let mut state = AppState::default();
        for (index, (_, window)) in self.windows.iter().enumerate() {
            if !window.tabs.is_empty() {
                if Some(index) == focused {
                    state.focused = state.windows.len();
                }
                state.windows.push(window.clone());
            }
        }
        // Only windows with nothing open: reopen one, on the welcome screen.
        if state.windows.is_empty()
            && let Some((_, window)) = focused
                .and_then(|index| self.windows.get(index))
                .or(self.windows.first())
        {
            state.windows.push(window.clone());
        }
        state
    }

    fn save(&mut self) {
        self.save = None;
        let Some(path) = path() else {
            return;
        };
        save_json_backed(&path, &self.state()).ok();
    }
}

/// Keeps the saved state in step with windows closing and the app quitting.
pub fn init(cx: &mut App) {
    cx.on_window_closed(window_closed).detach();
    cx.on_app_quit(|cx| {
        quit(cx);
        async {}
    })
    .detach();
}

/// Records window `id` as it is now and saves soon.
pub fn update(id: WindowId, window: SavedWindow, cx: &mut App) {
    let tracker = cx.default_global::<Tracker>();
    if tracker.quitting {
        return;
    }
    match tracker.windows.iter_mut().find(|(known, _)| *known == id) {
        Some((_, known)) if *known == window => return,
        Some((_, known)) => *known = window,
        None => tracker.windows.push((id, window)),
    }
    schedule_save(cx);
}

/// Window `id` got focus.
pub fn focused(id: WindowId, cx: &mut App) {
    let tracker = cx.default_global::<Tracker>();
    if tracker.quitting || tracker.focused == Some(id) {
        return;
    }
    tracker.focused = Some(id);
    schedule_save(cx);
}

/// Saves every open window now; windows closing after this (the quit's
/// teardown) stay saved.
pub fn quit(cx: &mut App) {
    let tracker = cx.default_global::<Tracker>();
    if !tracker.quitting {
        tracker.quitting = true;
        tracker.save();
    }
}

fn schedule_save(cx: &mut App) {
    let task = cx.spawn(async |cx| {
        cx.background_executor().timer(SAVE_DELAY).await;
        cx.update(|cx| cx.default_global::<Tracker>().save()).ok();
    });
    cx.default_global::<Tracker>().save = Some(task);
}

fn window_closed(cx: &mut App) {
    if cx.default_global::<Tracker>().quitting {
        return;
    }
    let open: Vec<WindowId> = cx
        .windows()
        .iter()
        .map(|handle| handle.window_id())
        .collect();
    if open.is_empty() {
        // The last window: it reopens next time. The editor quits, or stays
        // for the next launch with nothing tracked (that launch restores
        // the saved windows, which report themselves again).
        if crate::instance::stays_resident() {
            let tracker = cx.default_global::<Tracker>();
            tracker.save();
            tracker.windows.clear();
            tracker.focused = None;
        } else {
            quit(cx);
        }
        return;
    }
    let tracker = cx.default_global::<Tracker>();
    tracker.windows.retain(|(id, _)| open.contains(id));
    if tracker.focused.is_some_and(|id| !open.contains(&id)) {
        tracker.focused = None;
    }
    tracker.save();
}

/// The window that reported focus last, if it is still open.
pub fn focused_window(cx: &mut App) -> Option<gpui::AnyWindowHandle> {
    let focused = cx.default_global::<Tracker>().focused?;
    cx.windows()
        .into_iter()
        .find(|handle| handle.window_id() == focused)
}

/// The state as the next save would write it.
#[cfg(test)]
pub fn current(cx: &mut App) -> AppState {
    cx.default_global::<Tracker>().state()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local(path: &std::path::Path) -> Location {
        Location::Local(path.to_owned())
    }

    #[test]
    fn the_state_round_trips_through_json() {
        let state = AppState {
            windows: vec![
                SavedWindow {
                    tabs: vec![
                        TabTarget::Project(Location::Local(PathBuf::from("/src/a"))),
                        TabTarget::Project(Location::Wsl {
                            distro: "Ubuntu".into(),
                            path: "/home/me/b".into(),
                        }),
                        TabTarget::Project(Location::DevContainer {
                            folder: Box::new(Location::Local(PathBuf::from("/src/c"))),
                            config: None,
                        }),
                        TabTarget::File {
                            kind: "workflow".into(),
                            path: PathBuf::from("/cfg/workflows/dev.yaml"),
                            project: None,
                        },
                        TabTarget::File {
                            kind: "workflow".into(),
                            path: PathBuf::from("/home/me/b/.next_code/workflows/w.json"),
                            project: Some(Location::Wsl {
                                distro: "Ubuntu".into(),
                                path: "/home/me/b".into(),
                            }),
                        },
                    ],
                    active: 1,
                    bounds: Some(SavedBounds {
                        x: -1600.,
                        y: 20.5,
                        width: 1280.,
                        height: 800.,
                        state: WindowState::Maximized,
                    }),
                },
                SavedWindow {
                    tabs: Vec::new(),
                    active: 0,
                    bounds: None,
                },
            ],
            focused: 1,
        };
        let json = serde_json::to_string_pretty(&state).unwrap();
        assert_eq!(serde_json::from_str::<AppState>(&json).unwrap(), state);
        // Hand-edited files may leave out everything but the tabs.
        let minimal: AppState =
            serde_json::from_str(r#"{ "windows": [ { "tabs": [ { "Local": "/src/a" } ] } ] }"#)
                .unwrap();
        assert_eq!(
            minimal.windows[0].tabs,
            [TabTarget::Project(Location::Local(PathBuf::from("/src/a")))]
        );
        assert!(
            json.contains(r#""workflow": "/cfg/workflows/dev.yaml""#),
            "{json}"
        );
        // As files from before built-in extensions saved workflow tabs.
        let saved: TabTarget = serde_json::from_str(
            r#"{ "project": { "Wsl": { "distro": "Ubuntu", "path": "/p" } }, "workflow": "/p/w.yaml" }"#,
        )
        .unwrap();
        assert_eq!(
            saved,
            TabTarget::File {
                kind: "workflow".into(),
                path: PathBuf::from("/p/w.yaml"),
                project: Some(Location::Wsl {
                    distro: "Ubuntu".into(),
                    path: "/p".into(),
                }),
            }
        );
        assert!(serde_json::from_str::<TabTarget>(r#"{ "a": "/x", "b": "/y" }"#).is_err());
        assert_eq!(minimal.focused, 0);
        assert_eq!(minimal.windows[0].active, 0);
        assert_eq!(minimal.windows[0].bounds, None);
    }

    #[test]
    fn folders_and_files_that_are_gone_are_not_reopened() {
        let root = std::env::temp_dir().join(format!("nc-app-state-{}", std::process::id()));
        std::fs::create_dir_all(root.join("kept")).unwrap();
        std::fs::write(root.join("dev.yaml"), "{}").unwrap();
        let kept = local(&root.join("kept"));
        let gone = local(&root.join("gone"));
        let wsl = Location::Wsl {
            distro: "Ubuntu".into(),
            path: "/p".into(),
        };
        let project = |location: &Location| TabTarget::Project(location.clone());
        let workflow = |name: &str, project: Option<Location>| TabTarget::File {
            kind: "workflow".into(),
            path: root.join(name),
            project,
        };
        let window = |tabs: Vec<TabTarget>, active| SavedWindow {
            tabs,
            active,
            bounds: None,
        };
        let remote_workflow = TabTarget::File {
            kind: "workflow".into(),
            path: PathBuf::from("/p/.next_code/workflows/w.json"),
            project: Some(wsl.clone()),
        };
        let state = AppState {
            windows: vec![
                window(vec![project(&gone), workflow("gone.json", None)], 0),
                window(
                    vec![
                        project(&gone),
                        project(&wsl),
                        workflow("dev.yaml", None),
                        project(&kept),
                        remote_workflow.clone(),
                    ],
                    3,
                ),
                window(vec![project(&kept), project(&gone)], 1),
            ],
            focused: 1,
        }
        .existing();
        assert_eq!(
            state,
            AppState {
                windows: vec![
                    window(
                        vec![
                            project(&wsl),
                            workflow("dev.yaml", None),
                            project(&kept),
                            remote_workflow,
                        ],
                        2
                    ),
                    // The active tab is gone: the first one shows.
                    window(vec![project(&kept)], 0),
                ],
                focused: 0,
            }
        );
        assert_eq!(state.focused_tab(), Some(&kept));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn windows_open_on_a_display_that_is_still_there() {
        let display = |x: f32, y: f32, width: f32, height: f32| Bounds {
            origin: point(px(x), px(y)),
            size: size(px(width), px(height)),
        };
        let saved = |x, y, width, height| SavedBounds {
            x,
            y,
            width,
            height,
            state: WindowState::Windowed,
        };
        let opened = |bounds: SavedBounds, displays: &[Bounds<Pixels>]| {
            SavedBounds::from(bounds.clamped(displays))
        };
        let primary = display(0., 0., 1920., 1080.);
        let left = display(-1280., 0., 1280., 1024.);

        // Fully on a display: unchanged.
        let on_left = saved(-1200., 50., 1000., 700.);
        assert_eq!(opened(on_left, &[primary, left]), on_left);
        // That monitor is gone: moved onto the primary display.
        assert_eq!(
            opened(on_left, &[primary]),
            saved(0., 50., 1000., 700.),
            "moved right until it is on screen"
        );
        // Far away from every display, and larger than the primary one.
        assert_eq!(
            opened(saved(5000., 3000., 2500., 1400.), &[primary]),
            saved(0., 0., 1920., 1080.)
        );
        // Straddling two displays: onto the one it overlaps most.
        assert_eq!(
            opened(saved(-300., 100., 1000., 600.), &[primary, left]),
            saved(0., 100., 1000., 600.)
        );
        assert_eq!(
            opened(saved(-900., 100., 1000., 600.), &[primary, left]),
            saved(-1000., 100., 1000., 600.)
        );
        // A size too small to use grows; no displays known: kept as saved.
        assert_eq!(
            opened(saved(10., 10., 5., 5.), &[]),
            saved(10., 10., MIN_WIDTH, MIN_HEIGHT)
        );
        // Maximized windows stay maximized, with their restore size clamped.
        let maximized = SavedBounds {
            state: WindowState::Maximized,
            ..saved(-1200., 50., 1000., 700.)
        };
        assert_eq!(
            opened(maximized, &[primary]),
            SavedBounds {
                state: WindowState::Maximized,
                ..saved(0., 50., 1000., 700.)
            }
        );
    }

    #[test]
    fn only_a_lone_window_without_projects_is_kept() {
        let mut tracker = Tracker::default();
        let project = local(std::path::Path::new("/src/a"));
        let window = |tabs: Vec<Location>| SavedWindow {
            tabs: tabs.into_iter().map(TabTarget::Project).collect(),
            active: 0,
            bounds: None,
        };
        let id = |n: u64| WindowId::from(n);
        tracker.windows.push((id(1), window(Vec::new())));
        assert_eq!(tracker.state().windows, [window(Vec::new())]);
        tracker.windows.push((id(2), window(vec![project.clone()])));
        tracker.focused = Some(id(2));
        assert_eq!(
            tracker.state(),
            AppState {
                windows: vec![window(vec![project])],
                focused: 0,
            }
        );
    }

    #[test]
    fn backed_json_files_fall_back_to_the_previous_good_copy() {
        let dir = std::env::temp_dir().join(format!("next_code_backed_{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let path = dir.join("state.json");
        assert_eq!(load_json_backed::<Vec<u32>>(&path), None);
        save_json_backed(&path, &vec![1u32]).unwrap();
        assert!(!backup_path(&path).exists(), "nothing to keep yet");
        save_json_backed(&path, &vec![1u32, 2]).unwrap();
        assert_eq!(load_json_backed::<Vec<u32>>(&path), Some(vec![1, 2]));
        assert_eq!(
            std::fs::read_to_string(backup_path(&path)).unwrap(),
            "[\n  1\n]"
        );
        // A truncated main file: the previous copy stands in.
        std::fs::write(&path, "[1, ").unwrap();
        assert_eq!(load_json_backed::<Vec<u32>>(&path), Some(vec![1]));
        // And is not replaced by the broken one on the next save.
        save_json_backed(&path, &vec![3u32]).unwrap();
        assert_eq!(load_json_backed::<Vec<u32>>(&path), Some(vec![3]));
        assert_eq!(
            std::fs::read_to_string(backup_path(&path)).unwrap(),
            "[\n  1\n]"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
