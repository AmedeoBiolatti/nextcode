//! Session restore: per project, the editor tabs of each pane, the active
//! tab and pane, the split structure and sizes, and each editor's
//! selections and scroll. Kept in `session.json` next to the project's
//! saved layout (`layout::project_state_dir`), written shortly after a
//! change and when the project closes, read when it opens. Files that no
//! longer exist are skipped; diff, history and comparison tabs are not
//! kept. `"restore_session": false` in settings.json turns it off.
//!
//! ```json
//! { "root": { "split": { "axis": "horizontal", "ratio": 0.5,
//!     "first": { "pane": { "tabs": [{ "path": "/p/src/main.rs", "editor": { … } }], "active": 0 } },
//!     "second": { "pane": { "tabs": [{ "path": "/p/README.md" }], "active": 0 } } } },
//!   "active_pane": 1 }
//! ```

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::{
    panes::{Pane, PaneId, PaneNode, SplitAxis},
    *,
};
use crate::{debugger::store::SavedDebugState, editor::EditorState};

/// How long changes settle before the session is written.
const SAVE_DELAY: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Session {
    pub root: Node,
    /// Index of the active pane in reading order.
    #[serde(default)]
    pub active_pane: usize,
    /// Breakpoints, exception filter choices and watch expressions.
    #[serde(default, skip_serializing_if = "SavedDebugState::is_empty")]
    pub debug: SavedDebugState,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Node {
    Pane {
        tabs: Vec<TabState>,
        #[serde(default)]
        active: usize,
    },
    Split {
        axis: SplitAxis,
        ratio: f32,
        first: Box<Node>,
        second: Box<Node>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct TabState {
    pub path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub editor: Option<EditorState>,
}

impl Session {
    fn path(project_key: &str) -> Option<PathBuf> {
        Some(crate::layout::project_state_dir(project_key)?.join("session.json"))
    }

    /// The saved session; missing or unreadable files mean none.
    pub fn load(project_key: &str) -> Option<Self> {
        let json = std::fs::read_to_string(Self::path(project_key)?).ok()?;
        serde_json::from_str(&json).ok()
    }

    pub fn save(&self, project_key: &str) -> std::io::Result<()> {
        let path = Self::path(project_key)
            .ok_or_else(|| std::io::Error::other("no local data directory"))?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let json = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(path, json)
    }
}

impl Workspace {
    /// The open editors as a session.
    pub(super) fn session(&self, cx: &App) -> Session {
        Session {
            root: self.node_state(&self.pane_tree, cx),
            active_pane: self
                .panes
                .iter()
                .position(|pane| pane.id == self.active_pane)
                .unwrap_or_default(),
            debug: self.saved_debug_state(cx),
        }
    }

    fn node_state(&self, node: &PaneNode, cx: &App) -> Node {
        match node {
            PaneNode::Leaf(id) => {
                let Some(pane) = self.pane(*id) else {
                    return Node::Pane {
                        tabs: Vec::new(),
                        active: 0,
                    };
                };
                let mut tabs = Vec::new();
                let mut active = 0;
                for (index, tab) in pane.tabs.iter().enumerate() {
                    let Some(editor) = tab.editor() else {
                        continue;
                    };
                    if index <= pane.active {
                        active = tabs.len();
                    }
                    tabs.push(TabState {
                        path: tab.path.clone(),
                        editor: Some(editor.read(cx).state()),
                    });
                }
                Node::Pane { tabs, active }
            }
            PaneNode::Split(split) => Node::Split {
                axis: split.axis,
                ratio: split.ratio,
                first: Box::new(self.node_state(&split.first, cx)),
                second: Box::new(self.node_state(&split.second, cx)),
            },
        }
    }

    /// Writes the session soon, once changes settle.
    pub(super) fn schedule_session_save(&mut self, cx: &mut Context<Self>) {
        if !self.restore_session {
            return;
        }
        self.session_save = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SAVE_DELAY).await;
            this.update(cx, |this, cx| {
                if let Err(err) = this.save_session(cx) {
                    this.notice = Some(format!("Could not save the session: {err}").into());
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    /// Writes the session now (also when the project closes).
    pub(super) fn save_session(&mut self, cx: &App) -> std::io::Result<()> {
        self.session_save = None;
        if !self.restore_session {
            return Ok(());
        }
        self.session(cx).save(&self.project.state_key())
    }

    /// Reopens the saved session, if any, in a workspace with no tabs yet.
    pub(super) fn restore_session(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.restore_session {
            return;
        }
        let Some(session) = Session::load(&self.project.state_key()) else {
            return;
        };
        self.restore_debug_state(session.debug.clone(), cx);
        if self.all_tabs().next().is_some() {
            return;
        }
        self.panes.clear();
        let tree = self.restore_node(&session.root, window, cx);
        match tree {
            Some(tree) => self.pane_tree = tree,
            None => {
                let pane = PaneId(self.next_id());
                self.panes.push(Pane::new(pane));
                self.pane_tree = PaneNode::Leaf(pane);
            }
        }
        self.order_panes();
        let pane = &self.panes[session.active_pane.min(self.panes.len() - 1)];
        let (pane, active) = (pane.id, pane.active);
        self.active_pane = pane;
        if self.active_pane().tabs.is_empty() {
            window.focus(&self.focus_handle);
        } else {
            self.activate(pane, active, window, cx);
        }
    }

    /// Rebuilds a saved subtree; `None` if none of its files could be
    /// opened.
    fn restore_node(
        &mut self,
        node: &Node,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<PaneNode> {
        match node {
            Node::Pane { tabs, active } => {
                let id = PaneId(self.next_id());
                self.panes.push(Pane::new(id));
                self.active_pane = id;
                let mut restored_active = 0;
                for (index, tab) in tabs.iter().enumerate() {
                    if !matches!(
                        self.project.kind(&tab.path),
                        Ok(Some(worktree::FileKind::File))
                    ) {
                        continue;
                    }
                    let Some(editor) = self.open_editor(&tab.path, window, cx) else {
                        continue;
                    };
                    if let Some(state) = &tab.editor {
                        editor.update(cx, |editor, cx| editor.restore_state(state, cx));
                    }
                    if index <= *active {
                        restored_active = self.active_pane().tabs.len() - 1;
                    }
                }
                if self.pane(id)?.tabs.is_empty() {
                    self.panes.retain(|pane| pane.id != id);
                    return None;
                }
                self.pane_mut(id)?.active = restored_active;
                Some(PaneNode::Leaf(id))
            }
            Node::Split {
                axis,
                ratio,
                first,
                second,
            } => {
                let first = self.restore_node(first, window, cx);
                let second = self.restore_node(second, window, cx);
                match (first, second) {
                    (Some(first), Some(second)) => {
                        let id = self.next_id();
                        Some(PaneNode::split(id, *axis, *ratio, first, second))
                    }
                    (side, None) | (None, side) => side,
                }
            }
        }
    }
}
