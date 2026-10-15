//! Which panels live in which dock, and how docks are sized; saved as a
//! global default plus a per-project override.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PanelId {
    Files,
    Search,
    Git,
    Python,
    Terminal,
    Problems,
}

impl PanelId {
    pub const ALL: [Self; 6] = [
        Self::Files,
        Self::Search,
        Self::Git,
        Self::Python,
        Self::Terminal,
        Self::Problems,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Self::Files => "Files",
            Self::Search => "Search",
            Self::Git => "Git",
            Self::Python => "Python",
            Self::Terminal => "Terminal",
            Self::Problems => "Problems",
        }
    }

    fn default_position(self) -> DockPosition {
        match self {
            Self::Files | Self::Search | Self::Git | Self::Python => DockPosition::Left,
            Self::Terminal | Self::Problems => DockPosition::Bottom,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DockPosition {
    Left,
    Right,
    Bottom,
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
    pub panels: Vec<PanelId>,
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
        Self {
            left: DockLayout {
                panels: vec![
                    PanelId::Files,
                    PanelId::Search,
                    PanelId::Git,
                    PanelId::Python,
                ],
                active: Some(PanelId::Files),
                size: 280.,
                open: true,
            },
            right: DockLayout {
                size: 320.,
                ..DockLayout::default()
            },
            bottom: DockLayout {
                panels: vec![PanelId::Terminal, PanelId::Problems],
                active: Some(PanelId::Terminal),
                size: 260.,
                open: false,
            },
        }
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
        for panel in PanelId::ALL {
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
        std::fs::write(path, json)
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
fn state_root() -> Option<PathBuf> {
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

        assert_eq!(layout.right.panels, [PanelId::Files]);
        assert_eq!(
            (layout.right.active, layout.right.open),
            (Some(PanelId::Files), true)
        );
        // The left dock falls back to a remaining panel and stays open.
        assert_eq!(
            layout.left.panels,
            [PanelId::Search, PanelId::Git, PanelId::Python]
        );
        assert_eq!(layout.left.active, Some(PanelId::Search));

        layout.move_panel(PanelId::Search, DockPosition::Bottom, Some(0));
        assert_eq!(
            layout.bottom.panels,
            [PanelId::Search, PanelId::Terminal, PanelId::Problems]
        );
        assert_eq!(layout.left.active, Some(PanelId::Git));
        layout.move_panel(PanelId::Git, DockPosition::Right, None);
        layout.move_panel(PanelId::Python, DockPosition::Right, None);
        assert!(!layout.left.open, "an emptied dock closes");
        assert_eq!(layout.left.active, None);
    }

    #[test]
    fn reordering_within_a_dock_keeps_the_panel_once() {
        let mut layout = Layout::default();
        layout.move_panel(PanelId::Problems, DockPosition::Bottom, Some(0));
        assert_eq!(layout.bottom.panels, [PanelId::Problems, PanelId::Terminal]);
        layout.move_panel(PanelId::Problems, DockPosition::Bottom, Some(99));
        assert_eq!(layout.bottom.panels, [PanelId::Terminal, PanelId::Problems]);
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
        assert!(!layout.right.open, "an empty dock cannot open");
    }

    #[test]
    fn normalize_repairs_hand_edited_layouts() {
        let mut layout: Layout = serde_json::from_str(
            r#"{ "left": { "panels": ["files", "files", "terminal"], "active": "problems", "size": 5 },
                 "right": { "panels": ["terminal"] } }"#,
        )
        .unwrap();
        layout.normalize();
        // Search was missing and returns to its default (left) dock, as do
        // Git and Python for layouts saved before they existed.
        assert_eq!(
            layout.left.panels,
            [
                PanelId::Files,
                PanelId::Terminal,
                PanelId::Search,
                PanelId::Git,
                PanelId::Python
            ]
        );
        assert_eq!(layout.left.active, Some(PanelId::Files));
        assert_eq!(layout.left.size, MIN_DOCK_SIZE);
        assert!(layout.right.panels.is_empty(), "duplicate dropped");
        // Missing panels return to their default docks.
        assert_eq!(
            layout.position_of(PanelId::Problems),
            Some(DockPosition::Bottom)
        );
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
}
