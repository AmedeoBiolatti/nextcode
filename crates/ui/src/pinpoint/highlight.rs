//! The highlights agents ask for, per window (UI thread only).

use std::{cell::RefCell, ops::Range, time::Instant};

use gpui::{Hsla, SharedString, WindowId};

use super::{
    HighlightId,
    target::{CodeWant, Target, TerminalWant, TextSelect},
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Style {
    /// An outline with a soft glow (elements).
    #[default]
    Outline,
    /// A translucent tint (text and code).
    Fill,
    /// A bar under each line (text).
    Underline,
}

impl Style {
    pub fn parse(name: &str) -> Result<Self, String> {
        match name {
            "outline" => Ok(Self::Outline),
            "fill" => Ok(Self::Fill),
            "underline" => Ok(Self::Underline),
            _ => Err(format!(
                "unknown style \"{name}\": use \"outline\", \"fill\" or \"underline\""
            )),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Highlight {
    pub id: HighlightId,
    pub window: WindowId,
    pub target: Target,
    pub style: Style,
    /// `None`: the theme's accent.
    pub color: Option<Hsla>,
    /// The numbered badge (a step of a guide).
    pub badge: Option<u32>,
    /// The callout's text.
    pub note: Option<SharedString>,
    pub created: Instant,
    pub pulse: bool,
    /// Removed then; `None`: until cleared or dismissed.
    pub until: Option<Instant>,
}

thread_local! {
    static HIGHLIGHTS: RefCell<Vec<Highlight>> = const { RefCell::new(Vec::new()) };
    static NEXT_ID: std::cell::Cell<HighlightId> = const { std::cell::Cell::new(1) };
}

pub fn next_id() -> HighlightId {
    NEXT_ID.with(|next| {
        let id = next.get();
        next.set(id + 1);
        id
    })
}

pub fn add(highlight: Highlight) {
    HIGHLIGHTS.with_borrow_mut(|all| all.push(highlight));
}

/// Whether `window` has any highlight.
pub fn any(window: WindowId) -> bool {
    HIGHLIGHTS.with_borrow(|all| all.iter().any(|h| h.window == window))
}

/// `window`'s highlights, dropping expired ones.
pub fn of(window: WindowId) -> Vec<Highlight> {
    let now = Instant::now();
    HIGHLIGHTS.with_borrow_mut(|all| {
        all.retain(|h| h.until.is_none_or(|until| until > now));
        all.iter().filter(|h| h.window == window).cloned().collect()
    })
}

/// Removes `ids` (all of `window`'s when `None`); returns the removed ids.
pub fn clear(window: Option<WindowId>, ids: Option<&[HighlightId]>) -> Vec<HighlightId> {
    HIGHLIGHTS.with_borrow_mut(|all| {
        let mut removed = Vec::new();
        all.retain(|h| {
            let gone = window.is_none_or(|window| h.window == window)
                && ids.is_none_or(|ids| ids.contains(&h.id));
            if gone {
                removed.push(h.id);
            }
            !gone
        });
        removed
    })
}

pub fn forget(window: WindowId) {
    clear(Some(window), None);
}

/// The target of highlight `id`, while it exists.
pub fn target_of(id: HighlightId) -> Option<Target> {
    HIGHLIGHTS.with_borrow(|all| all.iter().find(|h| h.id == id).map(|h| h.target.clone()))
}

pub fn wanted_text(window: WindowId, path: &str, text: &str) -> Vec<(HighlightId, Range<usize>)> {
    HIGHLIGHTS.with_borrow(|all| {
        all.iter()
            .filter(|h| h.window == window)
            .filter_map(|h| match &h.target {
                Target::Text { path: p, select } if p == path => {
                    Some((h.id, resolve_text(text, select).ok()?))
                }
                _ => None,
            })
            .collect()
    })
}

pub fn wanted_code(window: WindowId) -> Vec<(HighlightId, CodeWant)> {
    HIGHLIGHTS.with_borrow(|all| {
        all.iter()
            .filter(|h| h.window == window)
            .filter_map(|h| match &h.target {
                Target::Code(code) => Some((h.id, code.clone())),
                _ => None,
            })
            .collect()
    })
}

pub fn wanted_terminal(window: WindowId) -> Vec<(HighlightId, TerminalWant)> {
    HIGHLIGHTS.with_borrow(|all| {
        all.iter()
            .filter(|h| h.window == window)
            .filter_map(|h| match &h.target {
                Target::Terminal(terminal) => Some((h.id, terminal.clone())),
                _ => None,
            })
            .collect()
    })
}

/// The byte range `select` names in `text`: the nth match (case-sensitive
/// first, else ignoring case), or a range checked against the text.
pub fn resolve_text(text: &str, select: &TextSelect) -> Result<Range<usize>, String> {
    match select {
        TextSelect::Range(range) => {
            if range.end > text.len()
                || !text.is_char_boundary(range.start)
                || !text.is_char_boundary(range.end)
            {
                Err(format!(
                    "range {}..{} is outside the text ({} bytes) or splits a character",
                    range.start,
                    range.end,
                    text.len()
                ))
            } else {
                Ok(range.clone())
            }
        }
        TextSelect::Match { text: needle, nth } => {
            if needle.is_empty() {
                return Err("\"text\" is empty".into());
            }
            let nth = (*nth).max(1);
            if let Some((start, _)) = text.match_indices(needle.as_str()).nth(nth - 1) {
                return Ok(start..start + needle.len());
            }
            // Ignoring case, on a lowercased copy that keeps byte offsets
            // (only when lowercasing keeps lengths).
            let lower = text.to_lowercase();
            let needle_lower = needle.to_lowercase();
            if lower.len() == text.len()
                && let Some((start, _)) = lower.match_indices(needle_lower.as_str()).nth(nth - 1)
            {
                return Ok(start..start + needle_lower.len());
            }
            let count = text.matches(needle.as_str()).count();
            Err(if count == 0 {
                format!("\"{needle}\" does not occur in the element's text")
            } else {
                format!("\"{needle}\" occurs {count} time(s), not {nth}")
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_selection() {
        let m = |text: &str, nth| TextSelect::Match {
            text: text.into(),
            nth,
        };
        assert_eq!(resolve_text("Run all, run one", &m("run", 1)), Ok(9..12));
        assert_eq!(resolve_text("Run all, run one", &m("Run", 1)), Ok(0..3));
        assert_eq!(resolve_text("Run all, run one", &m("RUN", 2)), Ok(9..12));
        assert!(
            resolve_text("abc", &m("x", 1))
                .unwrap_err()
                .contains("does not occur")
        );
        assert!(
            resolve_text("a a", &m("a", 3))
                .unwrap_err()
                .contains("2 time(s)")
        );
        assert_eq!(resolve_text("héllo", &TextSelect::Range(0..3)), Ok(0..3));
        assert!(resolve_text("héllo", &TextSelect::Range(0..2)).is_err());
        assert!(resolve_text("abc", &TextSelect::Range(1..9)).is_err());
    }
}
