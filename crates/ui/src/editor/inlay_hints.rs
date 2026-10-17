//! Inlay hints: inferred types, parameter names and the like, drawn inside
//! the text but never part of it.
//!
//! The editor asks for the hints of the visible lines (plus a screen above
//! and below) when the document opens on a server, [`DEBOUNCE`] after
//! edits, when scrolling leaves the covered lines, and when the server
//! asks for a refresh. Hints live in the [`DisplayMap`] as [`Inlay`]s,
//! which move with edits until fresh ones arrive; wrapping and screen
//! columns count them. Each laid-out row splices its hints into the shaped
//! text and keeps a [`RowInlays`] map between buffer offsets and positions
//! in that text, so cursors, selections and clicks only ever see buffer
//! offsets.

use lsp::types::{
    InlayHint, InlayHintLabel, InlayHintParams, OneOf, TextDocumentIdentifier,
    request::InlayHintRequest,
};

use super::{display_map::Inlay, *};
use crate::lsp_store::request_retrying;

/// Pause after an edit before asking for fresh hints.
const DEBOUNCE: Duration = Duration::from_millis(250);
/// Pause after scrolling to lines without hints.
const SCROLL_DEBOUNCE: Duration = Duration::from_millis(100);
/// Wait before retrying a request the server answered "content modified".
const RETRY_DELAY: Duration = Duration::from_millis(400);
/// Lines asked for before any layout tells how many are visible.
const DEFAULT_LINES: usize = 100;

actions!(editor, [ToggleInlayHints]);

/// The app-wide choice of [`ToggleInlayHints`], over `"inlay_hints"`.
pub(super) struct InlayHintsOverride(pub bool);

impl Global for InlayHintsOverride {}

/// Per-editor hint state (the hints themselves are in the display map).
#[derive(Default)]
pub(super) struct InlayHintState {
    /// Whether hints showed at the last check (setting and toggle).
    enabled: bool,
    /// Lines the current hints were asked for, at which document version.
    covered: Option<(i32, Range<usize>)>,
    /// A request is scheduled or in flight.
    pending: bool,
    /// Dropping it cancels the request.
    task: Option<Task<()>>,
}

impl Editor {
    /// Whether this editor shows inlay hints: the toggle, else the setting.
    pub(super) fn inlay_hints_wanted(&self, cx: &App) -> bool {
        !self.single_line
            && cx
                .try_global::<InlayHintsOverride>()
                .map_or_else(|| self.inlay_hints_setting(), |o| o.0)
    }

    fn serves_inlay_hints(server: &RunningServer) -> bool {
        matches!(
            server.capabilities.inlay_hint_provider,
            Some(OneOf::Left(true) | OneOf::Right(_))
        )
    }

    /// Like Alt+Z: for every editor until the app exits.
    pub(super) fn toggle_inlay_hints(
        &mut self,
        _: &ToggleInlayHints,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let show = !self.inlay_hints_wanted(cx);
        cx.set_global(InlayHintsOverride(show));
        cx.refresh_windows();
    }

    /// Buffer lines on screen, from the last layout and the current scroll.
    fn visible_lines(&self) -> Range<usize> {
        let Some(layout) = &self.layout else {
            return 0..DEFAULT_LINES.min(self.buffer.line_count());
        };
        let first_row = (self.scroll.y / layout.line_height).floor().max(0.) as usize;
        let rows = (layout.text_bounds.size.height / layout.line_height).ceil() as usize + 1;
        let first = self.display.row_line(first_row);
        let last = self.display.row_line(first_row + rows);
        first..last + 1
    }

    /// Asks for the hints of the visible lines and a screen around them
    /// after `delay`, replacing any pending request.
    fn request_inlay_hints(&mut self, delay: Duration, cx: &mut Context<Self>) {
        self.inlay.task = None;
        self.inlay.pending = false;
        let Some(doc) = &self.lsp else {
            return;
        };
        if !self.inlay.enabled || !Self::serves_inlay_hints(&doc.server) {
            return;
        }
        let visible = self.visible_lines();
        let margin = visible.len().max(1);
        let lines = visible.start.saturating_sub(margin)
            ..(visible.end + margin).min(self.buffer.line_count());
        let encoding = doc.server.encoding;
        let last = lines.end.max(lines.start + 1) - 1;
        let range = lsp::types::Range::new(
            self.server_position(self.buffer.line_start(lines.start), encoding),
            self.server_position(self.buffer.line_end(last), encoding),
        );
        let params = InlayHintParams {
            text_document: TextDocumentIdentifier::new(doc.uri.clone()),
            range,
            work_done_progress_params: Default::default(),
        };
        let server = doc.server.clone();
        let version = doc.version;
        self.inlay.pending = true;
        self.inlay.task = Some(cx.spawn(async move |this, cx| {
            let executor = cx.background_executor().clone();
            executor.timer(delay).await;
            let result =
                request_retrying::<InlayHintRequest>(&server, params, &executor, 1, RETRY_DELAY)
                    .await;
            this.update(cx, |this, cx| {
                // Edits replace this task, so an older version only shows
                // up if the server changed meanwhile.
                let current = this
                    .lsp
                    .as_ref()
                    .is_some_and(|doc| Arc::ptr_eq(&doc.server, &server) && doc.version == version);
                if !current {
                    return;
                }
                this.inlay.pending = false;
                // Failures count as covered too, or every frame would retry.
                this.inlay.covered = Some((version, lines));
                if let Ok(hints) = result {
                    let inlays = hints
                        .unwrap_or_default()
                        .into_iter()
                        .filter_map(|mut hint| {
                            hint.position = this.local_position(hint.position)?;
                            Some(inlay(&this.buffer, hint, server.encoding))
                        })
                        .collect();
                    if this.display.set_inlays(&this.buffer, inlays) {
                        cx.notify();
                    }
                }
            })
            .ok();
        }));
    }

    /// Re-requests hints now (the server asked, or the document opened).
    pub fn refresh_inlay_hints(&mut self, cx: &mut Context<Self>) {
        self.request_inlay_hints(Duration::ZERO, cx);
    }

    /// Requests hints for the edited text once typing pauses.
    pub(super) fn inlay_hints_edited(&mut self, cx: &mut Context<Self>) {
        self.request_inlay_hints(DEBOUNCE, cx);
    }

    /// Forgets hints (the server went away or hints were turned off).
    pub(super) fn clear_inlay_hints(&mut self) {
        self.inlay.task = None;
        self.inlay.pending = false;
        self.inlay.covered = None;
        self.display.set_inlays(&self.buffer, Vec::new());
    }

    /// Before each frame: applies the setting/toggle and asks for hints
    /// when scrolling reached lines the current ones don't cover.
    pub(super) fn sync_inlay_hints(&mut self, cx: &mut Context<Self>) {
        let enabled = self.inlay_hints_wanted(cx);
        if enabled != self.inlay.enabled {
            self.inlay.enabled = enabled;
            if enabled {
                self.refresh_inlay_hints(cx);
            } else {
                self.clear_inlay_hints();
            }
            return;
        }
        let Some(doc) = &self.lsp else {
            return;
        };
        if !enabled || self.inlay.pending {
            return;
        }
        let visible = self.visible_lines();
        let covered = self.inlay.covered.as_ref().is_some_and(|(version, lines)| {
            *version == doc.version && lines.start <= visible.start && visible.end <= lines.end
        });
        if !covered {
            self.request_inlay_hints(SCROLL_DEBOUNCE, cx);
        }
    }
}

/// A server hint as an [`Inlay`] of `buffer`.
fn inlay(buffer: &Buffer, hint: InlayHint, encoding: Encoding) -> Inlay {
    let label = match hint.label {
        InlayHintLabel::String(label) => label,
        InlayHintLabel::LabelParts(parts) => parts.into_iter().map(|part| part.value).collect(),
    };
    Inlay {
        offset: language::to_offset(buffer, hint.position, encoding),
        // Hints are one row; a multi-line label would break the layout.
        label: label.replace(['\n', '\r'], " "),
        padding_left: hint.padding_left.unwrap_or(false),
        padding_right: hint.padding_right.unwrap_or(false),
    }
}

/// Where one inlay sits in a row's shaped text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RowInlay {
    /// Row-relative buffer offset it is drawn before.
    at: usize,
    /// Its first byte in the shaped text.
    start: usize,
    /// Bytes it takes there (padding included).
    len: usize,
}

/// A row's inlays, mapping row-relative buffer offsets to indices in the
/// row's shaped text and back.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct RowInlays(Vec<RowInlay>);

impl RowInlays {
    /// The shaped-text index of buffer offset `at`; an inlay at `at` is
    /// drawn after it (the cursor stays before the hint).
    pub fn display_index(&self, at: usize) -> usize {
        at + self
            .0
            .iter()
            .take_while(|inlay| inlay.at < at)
            .map(|inlay| inlay.len)
            .sum::<usize>()
    }

    /// The buffer offset at shaped-text `index`; anywhere in or at the
    /// edges of an inlay is the offset it sits at.
    pub fn buffer_offset(&self, index: usize) -> usize {
        let mut shift = 0;
        for inlay in &self.0 {
            if index < inlay.start {
                break;
            }
            if index <= inlay.start + inlay.len {
                return inlay.at;
            }
            shift += inlay.len;
        }
        index - shift
    }

    /// Whether shaped-text `index` is a character of an inlay.
    pub fn is_inlay(&self, index: usize) -> bool {
        self.0
            .iter()
            .any(|inlay| (inlay.start..inlay.start + inlay.len).contains(&index))
    }
}

/// Inserts `inlays` (sorted; offsets relative to the row) into a row's
/// `text` and its `runs`: labels in `label` style, padding as plain
/// spaces. Returns the shaped text, its runs and the offset map.
pub(super) fn splice_inlays(
    text: &str,
    runs: Vec<TextRun>,
    inlays: &[(usize, &Inlay)],
    label: &TextRun,
) -> (String, Vec<TextRun>, RowInlays) {
    if inlays.is_empty() {
        return (text.to_owned(), runs, RowInlays::default());
    }
    let mut out = String::with_capacity(text.len() + inlays.len() * 8);
    let mut out_runs = Vec::with_capacity(runs.len() + inlays.len() * 3);
    let mut map = Vec::with_capacity(inlays.len());
    let mut runs = runs.into_iter();
    // The run being copied and how much of it is left.
    let mut current: Option<TextRun> = None;
    let mut copy = |out: &mut String, out_runs: &mut Vec<TextRun>, range: Range<usize>| {
        out.push_str(&text[range.clone()]);
        let mut len = range.len();
        while len > 0 {
            let Some(mut run) = current.take().or_else(|| runs.next()) else {
                break;
            };
            let take = run.len.min(len);
            out_runs.push(TextRun {
                len: take,
                ..run.clone()
            });
            len -= take;
            run.len -= take;
            if run.len > 0 {
                current = Some(run);
            }
        }
    };
    let mut copied = 0;
    for &(at, inlay) in inlays {
        let at = at.min(text.len());
        copy(&mut out, &mut out_runs, copied..at);
        copied = at;
        let start = out.len();
        let space = || TextRun {
            len: 1,
            background_color: None,
            ..label.clone()
        };
        if inlay.padding_left {
            out.push(' ');
            out_runs.push(space());
        }
        if !inlay.label.is_empty() {
            out.push_str(&inlay.label);
            out_runs.push(TextRun {
                len: inlay.label.len(),
                ..label.clone()
            });
        }
        if inlay.padding_right {
            out.push(' ');
            out_runs.push(space());
        }
        map.push(RowInlay {
            at,
            start,
            len: out.len() - start,
        });
    }
    copy(&mut out, &mut out_runs, copied..text.len());
    (out, out_runs, RowInlays(map))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(len: usize, color: Hsla) -> TextRun {
        TextRun {
            len,
            font: gpui::font("Mono"),
            color,
            background_color: None,
            underline: None,
            strikethrough: None,
        }
    }

    fn hint(label: &str, padding_left: bool, padding_right: bool) -> Inlay {
        Inlay {
            offset: 0,
            label: label.into(),
            padding_left,
            padding_right,
        }
    }

    #[test]
    fn hints_are_spliced_into_the_row_text_and_its_runs() {
        let (red, blue, gray) = (gpui::red(), gpui::blue(), gpui::black());
        let label = TextRun {
            background_color: Some(gpui::white()),
            ..run(0, gray)
        };
        // `let x = f(1);` with `: i32` after `x` and ` a:` (padded) before `1`.
        let text = "let x = f(1);";
        let runs = vec![run(4, red), run(9, blue)];
        let (ty, param) = (hint(": i32", false, false), hint("a:", false, true));
        let (shaped, runs, map) = splice_inlays(text, runs, &[(5, &ty), (10, &param)], &label);
        assert_eq!(shaped, "let x: i32 = f(a: 1);");
        let lens: Vec<usize> = runs.iter().map(|run| run.len).collect();
        // `let ` red, `x` blue, hint, ` = f(` blue, label, padding, `1);` blue.
        assert_eq!(lens, [4, 1, 5, 5, 2, 1, 3]);
        assert_eq!(runs.iter().map(|r| r.len).sum::<usize>(), shaped.len());
        assert_eq!(runs[2].background_color, Some(gpui::white()));
        assert_eq!(runs[5].background_color, None, "padding has no background");
        assert_eq!(runs[1].color, blue);

        // Buffer offsets before a hint stay put; after it they shift.
        assert_eq!(
            map.display_index(5),
            5,
            "the cursor is drawn before `: i32`"
        );
        assert_eq!(map.display_index(6), 11);
        assert_eq!(map.display_index(10), 15);
        assert_eq!(map.display_index(11), 19);
        // Every index inside a hint maps to the offset it sits at, so a
        // click there puts the cursor next to it and never inside.
        for index in 5..=10 {
            assert_eq!(map.buffer_offset(index), 5);
        }
        assert_eq!(map.buffer_offset(11), 6);
        for index in 15..=18 {
            assert_eq!(map.buffer_offset(index), 10);
        }
        assert_eq!(map.buffer_offset(21), 13);
        assert!(map.is_inlay(5) && map.is_inlay(9) && !map.is_inlay(10));
        for offset in 0..=text.len() {
            assert_eq!(map.buffer_offset(map.display_index(offset)), offset);
        }
    }

    #[test]
    fn a_hint_at_the_row_end_follows_the_text() {
        let gray = gpui::black();
        let text = "x";
        let (shaped, runs, map) = splice_inlays(
            text,
            vec![run(1, gray)],
            &[(1, &hint("i32", true, false))],
            &run(0, gray),
        );
        assert_eq!(shaped, "x i32");
        assert_eq!(runs.iter().map(|r| r.len).sum::<usize>(), 5);
        assert_eq!(map.display_index(1), 1);
        assert_eq!(map.buffer_offset(5), 1);
    }
}
