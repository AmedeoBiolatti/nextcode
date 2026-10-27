//! Which panels live in which dock, and how docks are sized; saved as a
//! global default plus a per-project override.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PanelId {
    Files,
    Search,
    /// PyCharm's Commit tool window. Saved layouts from before it was
    /// renamed call it `git`.
    Commit,
    Terminal,
    Problems,
    /// PyCharm's Git tool window (the Log).
    GitLog,
    Ports,
    Outline,
    CallHierarchy,
    Debug,
    DebugConsole,
    Tests,
    Extensions,
    /// A built-in extension's panel, by its key (`builtins::Panel`). Layouts
    /// keep it while the extension is off; docks show it while it is on.
    Builtin(&'static str),
}

impl PanelId {
    /// The editor's own panels.
    const CORE: [Self; 13] = [
        Self::Files,
        Self::Search,
        Self::Commit,
        Self::Terminal,
        Self::Problems,
        Self::GitLog,
        Self::Ports,
        Self::Outline,
        Self::CallHierarchy,
        Self::Debug,
        Self::DebugConsole,
        Self::Tests,
        Self::Extensions,
    ];

    /// Every panel: the editor's, then the built-in extensions'.
    pub fn all() -> impl Iterator<Item = Self> {
        Self::CORE
            .into_iter()
            .chain(crate::builtins::panels().map(|panel| Self::Builtin(panel.key)))
    }

    pub fn title(self) -> &'static str {
        match self {
            Self::Files => "Files",
            Self::Search => "Search",
            Self::Commit => "Commit",
            Self::Terminal => "Terminal",
            Self::Problems => "Problems",
            Self::GitLog => "Git",
            Self::Ports => "Ports",
            Self::Outline => "Outline",
            Self::CallHierarchy => "Call Hierarchy",
            Self::Debug => "Debug",
            Self::DebugConsole => "Debug Console",
            Self::Tests => "Tests",
            Self::Extensions => "Extensions",
            Self::Builtin(key) => crate::builtins::panel(key).map_or(key, |panel| panel.title),
        }
    }

    /// Its stable pinpoint key (`panel:<key>`): the saved name, kebab-cased.
    pub fn key(self) -> &'static str {
        match self {
            Self::Files => "files",
            Self::Search => "search",
            Self::Commit => "commit",
            Self::Terminal => "terminal",
            Self::Problems => "problems",
            Self::GitLog => "git-log",
            Self::Ports => "ports",
            Self::Outline => "outline",
            Self::CallHierarchy => "call-hierarchy",
            Self::Debug => "debug",
            Self::DebugConsole => "debug-console",
            Self::Tests => "tests",
            Self::Extensions => "extensions",
            Self::Builtin(key) => key,
        }
    }

    /// Its name in saved layouts.
    fn saved_name(self) -> &'static str {
        match self {
            Self::GitLog => "git_log",
            Self::CallHierarchy => "call_hierarchy",
            Self::DebugConsole => "debug_console",
            other => other.key(),
        }
    }

    fn from_saved_name(name: &str) -> Option<Self> {
        if name == "git" {
            return Some(Self::Commit);
        }
        Self::all().find(|panel| panel.saved_name() == name)
    }

    fn default_position(self) -> DockPosition {
        match self {
            Self::Files | Self::Search | Self::Commit | Self::Tests => DockPosition::Left,
            Self::Outline | Self::Debug => DockPosition::Right,
            Self::Terminal
            | Self::Problems
            | Self::Ports
            | Self::GitLog
            | Self::CallHierarchy
            | Self::Extensions
            | Self::DebugConsole => DockPosition::Bottom,
            Self::Builtin(key) => {
                crate::builtins::panel(key).map_or(DockPosition::Bottom, |panel| panel.position)
            }
        }
    }
}

impl Serialize for PanelId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.saved_name())
    }
}

impl<'de> Deserialize<'de> for PanelId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let name = String::deserialize(deserializer)?;
        Self::from_saved_name(&name)
            .ok_or_else(|| serde::de::Error::custom(format!("unknown panel `{name}`")))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DockPosition {
    Left,
    Right,
    Bottom,
}

/// A panel name in a saved layout: one of today's, or a panel that no
/// longer exists (the Workflows panel, now workflow tabs), which is
/// dropped so the rest of the layout still loads.
#[derive(Deserialize)]
#[serde(untagged)]
enum SavedPanel {
    Known(PanelId),
    Removed(#[allow(dead_code)] String),
}

impl SavedPanel {
    fn known(self) -> Option<PanelId> {
        match self {
            Self::Known(panel) => Some(panel),
            Self::Removed(_) => None,
        }
    }
}

fn known_panels<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<PanelId>, D::Error> {
    let saved = Vec::<SavedPanel>::deserialize(deserializer)?;
    Ok(saved.into_iter().filter_map(SavedPanel::known).collect())
}

fn known_panel<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<PanelId>, D::Error> {
    Ok(Option::<SavedPanel>::deserialize(deserializer)?.and_then(SavedPanel::known))
}

impl DockPosition {
    pub const ALL: [Self; 3] = [Self::Left, Self::Right, Self::Bottom];

    pub fn label(self) -> &'static str {
        match self {
            Self::Left => "Left",
            Self::Right => "Right",
            Self::Bottom => "Bottom",
        }
    }
}

/// Smallest dock size, in logical pixels, so a dock never disappears by resizing.
pub const MIN_DOCK_SIZE: f32 = 120.;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DockLayout {
    /// Tab order.
    #[serde(deserialize_with = "known_panels")]
    pub panels: Vec<PanelId>,
    #[serde(deserialize_with = "known_panel")]
    pub active: Option<PanelId>,
    /// Width (side docks) or height (bottom dock) in logical pixels.
    pub size: f32,
    pub open: bool,
}

impl Default for DockLayout {
    fn default() -> Self {
        Self {
            panels: Vec::new(),
            active: None,
            size: 280.,
            open: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Layout {
    pub left: DockLayout,
    pub right: DockLayout,
    pub bottom: DockLayout,
}

impl Default for Layout {
    fn default() -> Self {
        let mut layout = Self {
            left: DockLayout {
                panels: vec![
                    PanelId::Files,
                    PanelId::Search,
                    PanelId::Commit,
                    PanelId::Tests,
                ],
                active: Some(PanelId::Files),
                size: 280.,
                open: true,
            },
            right: DockLayout {
                panels: vec![PanelId::Outline, PanelId::Debug],
                active: Some(PanelId::Outline),
                size: 320.,
                open: false,
            },
            bottom: DockLayout {
                panels: vec![
                    PanelId::Terminal,
                    PanelId::Problems,
                    PanelId::GitLog,
                    PanelId::Ports,
                    PanelId::CallHierarchy,
                    PanelId::DebugConsole,
                    PanelId::Extensions,
                ],
                active: Some(PanelId::Terminal),
                size: 260.,
                open: false,
            },
        };
        // Built-in extensions' panels, after the panel each names.
        for panel in crate::builtins::panels() {
            let dock = layout.dock_mut(panel.position);
            let at = dock
                .panels
                .iter()
                .position(|existing| existing.key() == panel.after)
                .map_or(dock.panels.len(), |index| index + 1);
            dock.panels.insert(at, PanelId::Builtin(panel.key));
        }
        layout
    }
}

impl Layout {
    pub fn dock(&self, position: DockPosition) -> &DockLayout {
        match position {
            DockPosition::Left => &self.left,
            DockPosition::Right => &self.right,
            DockPosition::Bottom => &self.bottom,
        }
    }

    pub fn dock_mut(&mut self, position: DockPosition) -> &mut DockLayout {
        match position {
            DockPosition::Left => &mut self.left,
            DockPosition::Right => &mut self.right,
            DockPosition::Bottom => &mut self.bottom,
        }
    }

    pub fn position_of(&self, panel: PanelId) -> Option<DockPosition> {
        DockPosition::ALL
            .into_iter()
            .find(|&position| self.dock(position).panels.contains(&panel))
    }

    /// Shows `panel`: activates it and opens its dock.
    pub fn activate(&mut self, panel: PanelId) {
        if let Some(position) = self.position_of(panel) {
            let dock = self.dock_mut(position);
            dock.active = Some(panel);
            dock.open = true;
        }
    }

    /// Hides the panel if it is the visible one, otherwise shows it.
    pub fn toggle_panel(&mut self, panel: PanelId) {
        let Some(position) = self.position_of(panel) else {
            return;
        };
        let dock = self.dock_mut(position);
        if dock.open && dock.active == Some(panel) {
            dock.open = false;
        } else {
            dock.active = Some(panel);
            dock.open = true;
        }
    }

    /// Opens or closes a dock; a dock without panels stays closed.
    pub fn toggle_dock(&mut self, position: DockPosition) {
        let dock = self.dock_mut(position);
        dock.open = !dock.open && !dock.panels.is_empty();
    }

    /// Moves `panel` to `to` at tab `index` (end if `None` or past it), then
    /// shows it there. The source dock activates a neighbor, or closes when
    /// left empty.
    pub fn move_panel(&mut self, panel: PanelId, to: DockPosition, index: Option<usize>) {
        if let Some(from) = self.position_of(panel) {
            let source = self.dock_mut(from);
            let old_index = source
                .panels
                .iter()
                .position(|&p| p == panel)
                .unwrap_or_default();
            source.panels.remove(old_index);
            if source.active == Some(panel) {
                source.active = source
                    .panels
                    .get(old_index.saturating_sub(1))
                    .or(source.panels.first())
                    .copied();
            }
            if source.panels.is_empty() {
                source.open = false;
            }
        }
        let target = self.dock_mut(to);
        let index = index
            .unwrap_or(target.panels.len())
            .min(target.panels.len());
        target.panels.insert(index, panel);
        target.active = Some(panel);
        target.open = true;
    }

    /// Repairs a loaded layout: every panel exactly once, valid actives,
    /// sane sizes.
    pub fn normalize(&mut self) {
        let mut seen = Vec::new();
        for position in DockPosition::ALL {
            let dock = self.dock_mut(position);
            dock.panels.retain(|panel| {
                let first = !seen.contains(panel);
                seen.push(*panel);
                first
            });
        }
        for panel in PanelId::all() {
            if !seen.contains(&panel) {
                self.dock_mut(panel.default_position()).panels.push(panel);
            }
        }
        for position in DockPosition::ALL {
            let dock = self.dock_mut(position);
            if dock
                .active
                .is_none_or(|active| !dock.panels.contains(&active))
            {
                dock.active = dock.panels.first().copied();
            }
            if dock.panels.is_empty() {
                dock.open = false;
            }
            if !dock.size.is_finite() || dock.size < MIN_DOCK_SIZE {
                dock.size = MIN_DOCK_SIZE;
            }
        }
    }

    /// The project's layout, else the global default, else the built-in one.
    pub fn load(project_key: &str) -> Self {
        let project = project_layout_path(project_key);
        Self::load_from(project.as_deref(), global_layout_path().as_deref())
    }

    fn load_from(project: Option<&Path>, global: Option<&Path>) -> Self {
        let read = |path: Option<&Path>| -> Option<Self> {
            serde_json::from_str(&std::fs::read_to_string(path?).ok()?).ok()
        };
        let mut layout = read(project).or_else(|| read(global)).unwrap_or_default();
        layout.normalize();
        layout
    }

    pub fn save_for_project(&self, project_key: &str) -> std::io::Result<()> {
        self.save_to(project_layout_path(project_key))
    }

    /// Makes this the starting layout for projects without their own.
    pub fn save_as_default(&self) -> std::io::Result<()> {
        self.save_to(global_layout_path())
    }

    fn save_to(&self, path: Option<PathBuf>) -> std::io::Result<()> {
        let path = path.ok_or_else(|| std::io::Error::other("no config directory"))?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let json = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        worktree::write_atomic(&path, json.as_bytes())
    }
}

fn global_layout_path() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("next_code").join("layout.json"))
}

fn project_layout_path(project_key: &str) -> Option<PathBuf> {
    Some(project_state_dir(project_key)?.join("layout.json"))
}

/// Where per-project state (layout, session) lives: outside the project so
/// it never shows up in git, in the local data dir
/// (`next_code/projects/<key>`, the key with non-alphanumerics as `_`).
pub(crate) fn project_state_dir(project_key: &str) -> Option<PathBuf> {
    let key: String = project_key
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    Some(state_root()?.join("projects").join(key))
}

#[cfg(not(test))]
pub(crate) fn state_root() -> Option<PathBuf> {
    Some(dirs::data_local_dir()?.join("next_code"))
}

/// Unit tests open real workspaces: their state stays out of the user's,
/// and out of each other's (tests run on threads of one process). Tests
/// remove it when they end.
#[cfg(test)]
pub(crate) fn state_root() -> Option<PathBuf> {
    let thread = format!("{:?}", std::thread::current().id());
    let thread: String = thread.chars().filter(char::is_ascii_digit).collect();
    Some(std::env::temp_dir().join(format!(
        "next_code-test-state-{}-{thread}",
        std::process::id()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moving_a_panel_shows_it_and_fixes_up_the_source_dock() {
        let mut layout = Layout::default();
        layout.move_panel(PanelId::Files, DockPosition::Right, None);

        assert_eq!(layout.right.panels.last(), Some(&PanelId::Files));
        assert!(!layout.left.panels.contains(&PanelId::Files));
        assert_eq!(
            (layout.right.active, layout.right.open),
            (Some(PanelId::Files), true)
        );
        // The left dock falls back to a remaining panel and stays open.
        assert_eq!(layout.left.active, Some(PanelId::Search));

        layout.move_panel(PanelId::Search, DockPosition::Bottom, Some(0));
        assert_eq!(layout.bottom.panels.first(), Some(&PanelId::Search));
        assert!(!layout.left.panels.contains(&PanelId::Search));
        assert_eq!(layout.left.active, Some(PanelId::Commit));
        layout.move_panel(PanelId::Commit, DockPosition::Right, None);
        layout.move_panel(PanelId::Builtin("python"), DockPosition::Right, None);
        layout.move_panel(PanelId::Tests, DockPosition::Right, None);
        assert!(!layout.left.open, "an emptied dock closes");
        assert_eq!(layout.left.active, None);
    }

    #[test]
    fn reordering_within_a_dock_keeps_the_panel_once() {
        let mut layout = Layout::default();
        layout.move_panel(PanelId::Problems, DockPosition::Bottom, Some(0));
        let count = |layout: &Layout| {
            layout
                .bottom
                .panels
                .iter()
                .filter(|&&panel| panel == PanelId::Problems)
                .count()
        };
        assert_eq!(layout.bottom.panels.first(), Some(&PanelId::Problems));
        assert_eq!(count(&layout), 1);
        layout.move_panel(PanelId::Problems, DockPosition::Bottom, Some(99));
        assert_eq!(layout.bottom.panels.last(), Some(&PanelId::Problems));
        assert_eq!(count(&layout), 1);
    }

    #[test]
    fn toggling_a_panel_hides_only_when_it_is_the_visible_one() {
        let mut layout = Layout::default();
        layout.toggle_panel(PanelId::Problems);
        assert_eq!(
            (layout.bottom.open, layout.bottom.active),
            (true, Some(PanelId::Problems))
        );
        layout.toggle_panel(PanelId::Terminal);
        assert_eq!(
            (layout.bottom.open, layout.bottom.active),
            (true, Some(PanelId::Terminal))
        );
        layout.toggle_panel(PanelId::Terminal);
        assert!(!layout.bottom.open);
        layout.toggle_dock(DockPosition::Right);
        assert!(layout.right.open, "the Outline lives in the right dock");
        layout.move_panel(PanelId::Outline, DockPosition::Left, None);
        layout.move_panel(PanelId::Debug, DockPosition::Left, None);
        assert!(!layout.right.open, "an emptied dock closes");
        layout.toggle_dock(DockPosition::Right);
        assert!(!layout.right.open, "an empty dock cannot open");
    }

    #[test]
    fn normalize_repairs_hand_edited_layouts() {
        let mut layout: Layout = serde_json::from_str(
            r#"{ "left": { "panels": ["files", "files", "terminal", "git"], "active": "problems", "size": 5 },
                 "right": { "panels": ["terminal"] } }"#,
        )
        .unwrap();
        layout.normalize();
        // Search was missing and returns to its default (left) dock, as do
        // Python and Tests for layouts saved before they existed; `git` is
        // the Commit window's old name.
        assert_eq!(
            &layout.left.panels[..3],
            &[PanelId::Files, PanelId::Terminal, PanelId::Commit]
        );
        assert_eq!(
            layout.position_of(PanelId::Search),
            Some(DockPosition::Left)
        );
        assert_eq!(layout.left.active, Some(PanelId::Files));
        assert_eq!(layout.left.size, MIN_DOCK_SIZE);
        // The duplicate terminal is dropped; the Outline and Debug panels
        // return to their default (right) dock, as panels missing from
        // older layouts do.
        assert!(!layout.right.panels.contains(&PanelId::Terminal));
        // Missing panels return to their default docks.
        assert_eq!(
            layout.position_of(PanelId::Problems),
            Some(DockPosition::Bottom)
        );
    }

    #[test]
    fn built_in_panels_start_after_their_anchor_and_keep_their_saved_names() {
        let python = PanelId::Builtin("python");
        assert_eq!(
            Layout::default().left.panels,
            [
                PanelId::Files,
                PanelId::Search,
                PanelId::Commit,
                python,
                PanelId::Tests
            ]
        );
        // Layouts saved while Python was one of the editor's own panels.
        let mut layout: Layout = serde_json::from_str(
            r#"{ "left": { "panels": ["files", "search", "commit", "tests"], "active": "files", "size": 280, "open": true },
                 "right": { "panels": ["outline", "python"], "active": "python", "size": 300, "open": true } }"#,
        )
        .unwrap();
        layout.normalize();
        assert_eq!(
            layout.right.panels,
            [PanelId::Outline, python, PanelId::Debug]
        );
        assert_eq!(layout.right.active, Some(python));
        let saved = serde_json::to_string(&layout).unwrap();
        assert!(saved.contains(r#""active":"python""#), "{saved}");
        assert_eq!(serde_json::from_str::<Layout>(&saved).unwrap(), layout);
    }

    #[test]
    fn layouts_saved_with_the_workflows_panel_still_load() {
        // Workflows were a bottom panel before they became tabs.
        let mut layout: Layout = serde_json::from_str(
            r#"{ "bottom": { "panels": ["terminal", "workflows", "problems"],
                             "active": "workflows", "size": 300, "open": true },
                 "left": { "panels": ["files"], "active": "files", "size": 250, "open": true } }"#,
        )
        .unwrap();
        layout.normalize();
        assert_eq!(layout.left.size, 250.);
        assert_eq!(
            &layout.bottom.panels[..2],
            [PanelId::Terminal, PanelId::Problems]
        );
        assert_eq!(layout.bottom.active, Some(PanelId::Terminal));
        assert_eq!((layout.bottom.size, layout.bottom.open), (300., true));
    }

    #[test]
    fn project_layout_overrides_the_global_default() {
        let dir = std::env::temp_dir().join(format!("next_code_layout_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (project, global) = (dir.join("project.json"), dir.join("global.json"));

        let mut global_layout = Layout::default();
        global_layout.move_panel(PanelId::Terminal, DockPosition::Right, None);
        global_layout.save_to(Some(global.clone())).unwrap();
        assert_eq!(
            Layout::load_from(Some(&project), Some(&global)),
            global_layout
        );

        let mut project_layout = Layout::default();
        project_layout.left.size = 400.;
        project_layout.save_to(Some(project.clone())).unwrap();
        assert_eq!(
            Layout::load_from(Some(&project), Some(&global)),
            project_layout
        );

        // A corrupt project file falls back to the global default.
        std::fs::write(&project, "{ not json").unwrap();
        assert_eq!(
            Layout::load_from(Some(&project), Some(&global)),
            global_layout
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn extension_host_keeps_its_moved_dock_and_size_on_restore() {
        let mut layout = Layout::default();
        layout.move_panel(PanelId::Extensions, DockPosition::Right, Some(0));
        layout.right.size = 412.;
        let saved = serde_json::to_string(&layout).unwrap();
        let mut restored: Layout = serde_json::from_str(&saved).unwrap();
        restored.normalize();
        assert_eq!(restored, layout);
        assert_eq!(
            restored.position_of(PanelId::Extensions),
            Some(DockPosition::Right)
        );
        assert_eq!(restored.right.panels.first(), Some(&PanelId::Extensions));
        assert!(!restored.bottom.panels.contains(&PanelId::Extensions));
    }
}
