//! What the user set up for debugging, kept across sessions and saved in
//! the project's `session.json`: line breakpoints per file, exception
//! breakpoint filter choices, and watch expressions.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use gpui::{Context, EventEmitter};
use serde::{Deserialize, Serialize};

/// A line breakpoint (0-based line).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Breakpoint {
    pub line: usize,
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub enabled: bool,
    /// Stop only when this expression is true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub condition: Option<String>,
    /// Stop when the hit count satisfies this (adapter syntax, e.g. `5`,
    /// `>= 3`, `% 2`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hit_condition: Option<String>,
    /// Log this message (`{expression}` interpolated) instead of stopping.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_message: Option<String>,
}

fn yes() -> bool {
    true
}

fn is_true(value: &bool) -> bool {
    *value
}

impl Breakpoint {
    pub fn at(line: usize) -> Self {
        Self {
            line,
            enabled: true,
            condition: None,
            hit_condition: None,
            log_message: None,
        }
    }

    pub fn source_breakpoint(&self) -> dap::types::SourceBreakpoint {
        dap::types::SourceBreakpoint {
            line: self.line as u32 + 1,
            condition: self.condition.clone(),
            hit_condition: self.hit_condition.clone(),
            log_message: self.log_message.clone(),
        }
    }
}

/// The store as saved in `session.json`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SavedDebugState {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub breakpoints: Vec<SavedFile>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub exception_filters: BTreeMap<String, bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub watches: Vec<String>,
}

impl SavedDebugState {
    pub fn is_empty(&self) -> bool {
        self.breakpoints.is_empty() && self.exception_filters.is_empty() && self.watches.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedFile {
    pub path: PathBuf,
    pub breakpoints: Vec<Breakpoint>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DebugStoreEvent {
    /// The breakpoints of this file changed.
    Breakpoints(PathBuf),
    ExceptionFilters,
    Watches,
}

#[derive(Debug, Default)]
pub struct DebugStore {
    /// Per file, sorted by line, one per line.
    files: BTreeMap<PathBuf, Vec<Breakpoint>>,
    /// Exception filters the user turned on or off (others follow the
    /// adapter's default).
    exception_filters: BTreeMap<String, bool>,
    watches: Vec<String>,
}

impl EventEmitter<DebugStoreEvent> for DebugStore {}

impl DebugStore {
    pub fn breakpoints(&self, path: &Path) -> &[Breakpoint] {
        self.files.get(path).map_or(&[], Vec::as_slice)
    }

    pub fn breakpoint(&self, path: &Path, line: usize) -> Option<&Breakpoint> {
        self.breakpoints(path).iter().find(|bp| bp.line == line)
    }

    /// Every file with breakpoints, by path.
    pub fn files(&self) -> impl Iterator<Item = (&PathBuf, &[Breakpoint])> {
        self.files
            .iter()
            .map(|(path, breakpoints)| (path, breakpoints.as_slice()))
    }

    /// Adds a breakpoint at `line`, or removes the one there.
    pub fn toggle(&mut self, path: &Path, line: usize, cx: &mut Context<Self>) {
        if self.breakpoint(path, line).is_some() {
            self.remove(path, line, cx);
        } else {
            self.set(path, Breakpoint::at(line), cx);
        }
    }

    /// Adds `breakpoint`, replacing one on its line.
    pub fn set(&mut self, path: &Path, breakpoint: Breakpoint, cx: &mut Context<Self>) {
        let breakpoints = self.files.entry(path.to_path_buf()).or_default();
        match breakpoints.binary_search_by_key(&breakpoint.line, |bp| bp.line) {
            Ok(index) if breakpoints[index] == breakpoint => return,
            Ok(index) => breakpoints[index] = breakpoint,
            Err(index) => breakpoints.insert(index, breakpoint),
        }
        cx.emit(DebugStoreEvent::Breakpoints(path.to_path_buf()));
    }

    pub fn remove(&mut self, path: &Path, line: usize, cx: &mut Context<Self>) {
        let Some(breakpoints) = self.files.get_mut(path) else {
            return;
        };
        let before = breakpoints.len();
        breakpoints.retain(|bp| bp.line != line);
        if breakpoints.len() == before {
            return;
        }
        if breakpoints.is_empty() {
            self.files.remove(path);
        }
        cx.emit(DebugStoreEvent::Breakpoints(path.to_path_buf()));
    }

    pub fn set_enabled(&mut self, path: &Path, line: usize, enabled: bool, cx: &mut Context<Self>) {
        if let Some(breakpoint) = self.breakpoint(path, line)
            && breakpoint.enabled != enabled
        {
            let breakpoint = Breakpoint {
                enabled,
                ..breakpoint.clone()
            };
            self.set(path, breakpoint, cx);
        }
    }

    /// Enables every breakpoint if any is disabled, else disables all.
    pub fn toggle_all_enabled(&mut self, cx: &mut Context<Self>) {
        let enable = self.files.values().flatten().any(|bp| !bp.enabled);
        for (path, breakpoints) in &mut self.files {
            if breakpoints.iter().any(|bp| bp.enabled != enable) {
                breakpoints.iter_mut().for_each(|bp| bp.enabled = enable);
                cx.emit(DebugStoreEvent::Breakpoints(path.clone()));
            }
        }
    }

    pub fn remove_all(&mut self, cx: &mut Context<Self>) {
        for (path, _) in std::mem::take(&mut self.files) {
            cx.emit(DebugStoreEvent::Breakpoints(path));
        }
    }

    /// The file's breakpoints moved with edits to `lines` (in the order
    /// of [`Self::breakpoints`]); breakpoints landing on one line merge.
    pub fn moved(&mut self, path: &Path, lines: &[usize], cx: &mut Context<Self>) {
        let Some(breakpoints) = self.files.get_mut(path) else {
            return;
        };
        if breakpoints.len() != lines.len()
            || breakpoints
                .iter()
                .zip(lines)
                .all(|(bp, &line)| bp.line == line)
        {
            return;
        }
        for (breakpoint, &line) in breakpoints.iter_mut().zip(lines) {
            breakpoint.line = line;
        }
        breakpoints.sort_by_key(|bp| bp.line);
        breakpoints.dedup_by_key(|bp| bp.line);
        cx.emit(DebugStoreEvent::Breakpoints(path.to_path_buf()));
    }

    /// Moves the breakpoints of a renamed file (or of files under a
    /// renamed directory).
    pub fn renamed(
        &mut self,
        from: &Path,
        to: &Path,
        project: &crate::project::Project,
        cx: &mut Context<Self>,
    ) {
        let moved: Vec<PathBuf> = self
            .files
            .keys()
            .filter(|path| path.starts_with(from))
            .cloned()
            .collect();
        for old in moved {
            let new = match old.strip_prefix(from) {
                Ok(rest) if rest.as_os_str().is_empty() => to.to_path_buf(),
                Ok(rest) => project.join(to, &rest.to_string_lossy()),
                Err(_) => continue,
            };
            if let Some(breakpoints) = self.files.remove(&old) {
                self.files.insert(new.clone(), breakpoints);
                cx.emit(DebugStoreEvent::Breakpoints(old));
                cx.emit(DebugStoreEvent::Breakpoints(new));
            }
        }
    }

    /// Whether the exception filter is on: the user's choice, else the
    /// adapter's default.
    pub fn exception_filter(&self, filter: &dap::types::ExceptionBreakpointsFilter) -> bool {
        self.exception_filters
            .get(&filter.filter)
            .copied()
            .unwrap_or(filter.default)
    }

    pub fn set_exception_filter(&mut self, filter: &str, on: bool, cx: &mut Context<Self>) {
        self.exception_filters.insert(filter.to_owned(), on);
        cx.emit(DebugStoreEvent::ExceptionFilters);
    }

    /// The filters to send, given the adapter's.
    pub fn enabled_exception_filters(
        &self,
        filters: &[dap::types::ExceptionBreakpointsFilter],
    ) -> Vec<String> {
        filters
            .iter()
            .filter(|filter| self.exception_filter(filter))
            .map(|filter| filter.filter.clone())
            .collect()
    }

    pub fn watches(&self) -> &[String] {
        &self.watches
    }

    pub fn add_watch(&mut self, expression: String, cx: &mut Context<Self>) {
        let expression = expression.trim().to_owned();
        if expression.is_empty() || self.watches.contains(&expression) {
            return;
        }
        self.watches.push(expression);
        cx.emit(DebugStoreEvent::Watches);
    }

    pub fn remove_watch(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.watches.len() {
            self.watches.remove(index);
            cx.emit(DebugStoreEvent::Watches);
        }
    }

    pub fn saved(&self) -> SavedDebugState {
        SavedDebugState {
            breakpoints: self
                .files
                .iter()
                .map(|(path, breakpoints)| SavedFile {
                    path: path.clone(),
                    breakpoints: breakpoints.clone(),
                })
                .collect(),
            exception_filters: self.exception_filters.clone(),
            watches: self.watches.clone(),
        }
    }

    /// Replaces everything with a saved state (sorting and merging lines
    /// of hand-edited files).
    pub fn restore(&mut self, saved: SavedDebugState, cx: &mut Context<Self>) {
        let old: Vec<PathBuf> = self.files.keys().cloned().collect();
        self.files.clear();
        for file in saved.breakpoints {
            let mut breakpoints = file.breakpoints;
            breakpoints.sort_by_key(|bp| bp.line);
            breakpoints.dedup_by_key(|bp| bp.line);
            if !breakpoints.is_empty() {
                self.files.insert(file.path, breakpoints);
            }
        }
        self.exception_filters = saved.exception_filters;
        self.watches = saved.watches;
        let mut changed: Vec<PathBuf> = self.files.keys().cloned().collect();
        changed.extend(old);
        changed.sort();
        changed.dedup();
        for path in changed {
            cx.emit(DebugStoreEvent::Breakpoints(path));
        }
        cx.emit(DebugStoreEvent::ExceptionFilters);
        cx.emit(DebugStoreEvent::Watches);
    }
}

#[cfg(test)]
mod tests {
    use gpui::{AppContext, TestAppContext};

    use super::*;

    #[gpui::test]
    fn toggling_moving_and_saving_breakpoints(cx: &mut TestAppContext) {
        let store = cx.new(|_| DebugStore::default());
        let a = PathBuf::from("/p/a.py");
        store.update(cx, |store, cx| {
            store.toggle(&a, 9, cx);
            store.toggle(&a, 2, cx);
            store.toggle(&a, 5, cx);
            store.toggle(&a, 5, cx);
            store.set(
                &a,
                Breakpoint {
                    condition: Some("i == 3".into()),
                    ..Breakpoint::at(9)
                },
                cx,
            );
            store.set_enabled(&a, 2, false, cx);
            store.add_watch(" total ".into(), cx);
            store.add_watch("total".into(), cx);
            store.set_exception_filter("raised", true, cx);
        });
        store.read_with(cx, |store, _| {
            let lines: Vec<_> = store
                .breakpoints(&a)
                .iter()
                .map(|bp| (bp.line, bp.enabled))
                .collect();
            assert_eq!(lines, [(2, false), (9, true)]);
            assert_eq!(store.watches(), ["total"]);
        });

        // Edits move breakpoints; two landing on one line merge.
        store.update(cx, |store, cx| store.moved(&a, &[4, 4], cx));
        store.read_with(cx, |store, _| {
            assert_eq!(store.breakpoints(&a).len(), 1);
            assert_eq!(store.breakpoints(&a)[0].line, 4);
        });

        // The saved form round-trips through JSON (as in session.json).
        let saved = store.read_with(cx, |store, _| store.saved());
        let json = serde_json::to_string(&saved).unwrap();
        assert!(json.contains("\"enabled\":false"), "{json}");
        let parsed: SavedDebugState = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, saved);
        let restored = cx.new(|_| DebugStore::default());
        restored.update(cx, |store, cx| store.restore(parsed, cx));
        restored.read_with(cx, |store, _| {
            assert_eq!(store.saved(), saved);
            let filter = dap::types::ExceptionBreakpointsFilter {
                filter: "raised".into(),
                label: "Raised".into(),
                description: None,
                default: false,
            };
            assert!(store.exception_filter(&filter));
        });
        // Older sessions without the field (and minimal entries) load.
        let minimal: SavedDebugState = serde_json::from_str(
            r#"{"breakpoints":[{"path":"/p/b.rs","breakpoints":[{"line":3},{"line":1}]}]}"#,
        )
        .unwrap();
        restored.update(cx, |store, cx| store.restore(minimal, cx));
        restored.read_with(cx, |store, _| {
            let b = store.breakpoints(Path::new("/p/b.rs"));
            assert_eq!((b[0].line, b[1].line, b[0].enabled), (1, 3, true));
            assert!(store.breakpoints(&a).is_empty());
        });
    }
}
