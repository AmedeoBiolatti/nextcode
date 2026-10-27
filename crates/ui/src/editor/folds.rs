//! Code folding: foldable regions (from the syntax tree, or indentation
//! where the tree has no useful nodes), the fold commands, and what keeps
//! folds out of the way of cursors. Folds belong to the view
//! (`DisplayMap`); two panes on one file fold independently.

use std::ops::Range;

use gpui::{Context, Window, actions};
use text::{Bias, Buffer};

use super::{
    Editor,
    display_map::Fold as Folded,
    selections::{Selection, SelectionSet},
};

actions!(
    editor,
    [
        Fold,
        Unfold,
        ToggleFold,
        FoldAll,
        UnfoldAll,
        FoldRecursively,
        UnfoldRecursively,
    ]
);

/// A foldable region: `(header line, last hidden line)`.
pub(super) type Region = (usize, usize);

/// Columns of a line's indentation (tabs count 4), or `None` when blank.
fn indentation(text: &str) -> Option<usize> {
    let trimmed = text.trim_start();
    if trimmed.is_empty() {
        return None;
    }
    let width = text[..text.len() - trimmed.len()]
        .chars()
        .map(|c| if c == '\t' { 4 } else { 1 })
        .sum();
    Some(width)
}

/// Regions by indentation intersecting `lines`: a non-blank line followed
/// by more indented lines heads a region ending at the last non-blank line
/// before indentation returns to its level.
pub(super) fn indentation_regions(buffer: &Buffer, lines: Range<usize>) -> Vec<Region> {
    let count = buffer.line_count();
    let indent_of = |line: usize| indentation(&buffer.line_text(line));
    // Start from the outermost line enclosing the range.
    let mut start = lines.start.min(count - 1);
    let mut level = indent_of(start).unwrap_or(usize::MAX);
    let mut line = start;
    while level > 0 && line > 0 {
        line -= 1;
        if let Some(indent) = indent_of(line)
            && indent < level
        {
            level = indent;
            start = line;
        }
    }
    let mut regions = Vec::new();
    let mut open: Vec<(usize, usize)> = Vec::new();
    let mut last_text = None;
    let close = |header: usize, last: Option<usize>, regions: &mut Vec<Region>| {
        if let Some(last) = last
            && last > header
            && header < lines.end
            && last >= lines.start
        {
            regions.push((header, last));
        }
    };
    let mut finished = false;
    for line in start..count {
        let Some(indent) = indent_of(line) else {
            continue;
        };
        while let Some(&(level, header)) = open.last() {
            if level < indent {
                break;
            }
            open.pop();
            close(header, last_text, &mut regions);
        }
        if line >= lines.end && open.iter().all(|&(_, header)| header >= lines.end) {
            finished = true;
            break;
        }
        open.push((indent, line));
        last_text = Some(line);
    }
    if !finished {
        while let Some((_, header)) = open.pop() {
            close(header, last_text, &mut regions);
        }
    }
    regions.sort_unstable();
    regions
}

impl Editor {
    /// Foldable regions intersecting `lines`, sorted by header line (the
    /// largest per header line).
    pub(super) fn fold_regions(&self, lines: Range<usize>) -> Vec<Region> {
        match &self.syntax {
            Some(syntax) if !syntax.language().folds_by_indentation() => syntax.fold_ranges(lines),
            _ => indentation_regions(&self.buffer, lines),
        }
    }

    /// `fold_regions` for the gutter, cached per buffer version and lines.
    pub(super) fn cached_fold_regions(&mut self, lines: Range<usize>) -> &[Region] {
        let fresh = self
            .fold_cache
            .as_ref()
            .is_some_and(|(version, cached, _)| *version == self.version && *cached == lines);
        if !fresh {
            let regions = self.fold_regions(lines.clone());
            self.fold_cache = Some((self.version, lines, regions));
        }
        &self.fold_cache.as_ref().expect("filled above").2
    }

    fn folded(&self, (header, last): Region) -> Folded {
        Folded::lines(&self.buffer, header, last)
    }

    fn is_folded(&self, region: Region) -> bool {
        let fold = self.folded(region);
        self.display
            .folds()
            .iter()
            .any(|f| f.start == fold.start && f.end == fold.end)
    }

    /// The region a command at `line` acts on: the one `line` heads, else
    /// the innermost containing it; `unfolded` skips folded ones.
    fn region_at(&self, line: usize, unfolded: bool) -> Option<Region> {
        let regions = self.fold_regions(line..line + 1);
        let candidates = regions
            .into_iter()
            .filter(|&(header, last)| header <= line && line <= last)
            .filter(|&region| !unfolded || !self.is_folded(region));
        candidates.max_by_key(|&(header, last)| (header, std::cmp::Reverse(last)))
    }

    /// Lines of the primary and other cursors, deduplicated.
    fn cursor_lines(&self) -> Vec<usize> {
        let mut lines: Vec<usize> = self
            .selections
            .all()
            .iter()
            .map(|s| self.buffer.line_of(s.head))
            .collect();
        lines.dedup();
        lines
    }

    fn apply_folds(&mut self, regions: Vec<Region>, cx: &mut Context<Self>) {
        if regions.is_empty() {
            return;
        }
        let folds: Vec<Folded> = regions.into_iter().map(|r| self.folded(r)).collect();
        self.display.fold(&self.buffer, folds);
        self.move_selections_out_of_folds();
        self.autoscroll = true;
        cx.notify();
    }

    /// Cursors a fold just hid move to the end of its header line.
    fn move_selections_out_of_folds(&mut self) {
        let clip = |offset: usize| self.display.clip(&self.buffer, offset, Bias::Left);
        let selections = self.selections.map(|s| Selection {
            anchor: clip(s.anchor),
            head: clip(s.head),
            goal: None,
            at_row_end: false,
        });
        self.selections = selections;
    }

    fn remove_folds(
        &mut self,
        mut remove: impl FnMut(&Buffer, &Folded) -> bool,
        cx: &mut Context<Self>,
    ) {
        let buffer = &self.buffer;
        if self.display.unfold(buffer, |fold| remove(buffer, fold)) {
            cx.notify();
        }
    }

    /// Opens folds hiding a cursor (moved there by an edit, a search, a
    /// jump…) or one end of a selection that does not cover the fold.
    pub(super) fn reveal_selections(&mut self) {
        if self.display.folds().is_empty() {
            return;
        }
        let selections = self.selections.all();
        self.display.unfold(&self.buffer, |fold| {
            selections.iter().any(|s| {
                fold.hides(s.head)
                    || fold.hides(s.anchor) && !(s.start() <= fold.start && fold.end <= s.end())
            })
        });
    }

    pub(super) fn fold(&mut self, _: &Fold, _: &mut Window, cx: &mut Context<Self>) {
        let regions = self
            .cursor_lines()
            .into_iter()
            .filter_map(|line| self.region_at(line, true))
            .collect();
        self.apply_folds(regions, cx);
    }

    pub(super) fn unfold(&mut self, _: &Unfold, _: &mut Window, cx: &mut Context<Self>) {
        let lines = self.cursor_lines();
        self.remove_folds(
            |buffer, fold| lines.binary_search(&fold.header_line(buffer)).is_ok(),
            cx,
        );
    }

    /// Folds or unfolds the region `line` heads (gutter chevron clicks).
    pub(super) fn toggle_fold_at(&mut self, line: usize, cx: &mut Context<Self>) {
        if self.display.is_folded_header(&self.buffer, line) {
            self.remove_folds(|buffer, fold| fold.header_line(buffer) == line, cx);
        } else if let Some(region) = self.region_at(line, false).filter(|r| r.0 == line) {
            self.apply_folds(vec![region], cx);
        }
    }

    pub(super) fn toggle_fold(&mut self, _: &ToggleFold, _: &mut Window, cx: &mut Context<Self>) {
        let line = self.buffer.line_of(self.primary().head);
        if self.display.is_folded_header(&self.buffer, line) {
            self.toggle_fold_at(line, cx);
        } else if let Some(region) = self.region_at(line, true) {
            self.apply_folds(vec![region], cx);
        }
    }

    pub(super) fn fold_all(&mut self, _: &FoldAll, _: &mut Window, cx: &mut Context<Self>) {
        let regions = self.fold_regions(0..self.buffer.line_count());
        self.apply_folds(regions, cx);
    }

    pub(super) fn unfold_all(&mut self, _: &UnfoldAll, _: &mut Window, cx: &mut Context<Self>) {
        self.remove_folds(|_, _| true, cx);
    }

    /// The region at `line` and every region inside it.
    fn regions_within(&self, line: usize) -> Option<(Region, Vec<Region>)> {
        let outer = self.region_at(line, false)?;
        let inner = self
            .fold_regions(outer.0..outer.1 + 1)
            .into_iter()
            .filter(|&(header, last)| outer.0 <= header && last <= outer.1)
            .collect();
        Some((outer, inner))
    }

    pub(super) fn fold_recursively(
        &mut self,
        _: &FoldRecursively,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let regions = self
            .cursor_lines()
            .into_iter()
            .filter_map(|line| self.regions_within(line))
            .flat_map(|(_, inner)| inner)
            .collect();
        self.apply_folds(regions, cx);
    }

    pub(super) fn unfold_recursively(
        &mut self,
        _: &UnfoldRecursively,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let buffer = &self.buffer;
        let spans: Vec<Range<usize>> = self
            .cursor_lines()
            .into_iter()
            .map(|line| {
                // A fold headed here, else the region around the cursor.
                let folded = self
                    .display
                    .folds()
                    .iter()
                    .filter(|fold| fold.header_line(buffer) == line)
                    .map(|fold| fold.header..fold.end)
                    .max_by_key(|range| range.end);
                folded.unwrap_or_else(|| match self.region_at(line, false) {
                    Some(region) => {
                        let fold = self.folded(region);
                        fold.header..fold.end
                    }
                    None => buffer.line_start(line)..buffer.line_end(line),
                })
            })
            .collect();
        self.remove_folds(
            |_, fold| {
                spans
                    .iter()
                    .any(|span| span.start <= fold.header && fold.end <= span.end)
            },
            cx,
        );
    }

    /// Unfolds the fold whose placeholder was clicked.
    pub(super) fn unfold_at(&mut self, start: usize, cx: &mut Context<Self>) {
        self.remove_folds(|_, fold| fold.start == start, cx);
        let selection = Selection::cursor(start);
        self.selections = SelectionSet::single(selection);
    }

    /// Folded regions as `(header line, last hidden line)`, for sessions.
    pub(super) fn folded_lines(&self) -> Vec<Region> {
        self.display
            .folds()
            .iter()
            .map(|fold| (fold.header_line(&self.buffer), fold.last_line(&self.buffer)))
            .collect()
    }

    /// Folds saved regions (those that no longer fit the text are skipped).
    pub(super) fn restore_folds(&mut self, regions: &[Region]) {
        let count = self.buffer.line_count();
        let folds: Vec<Folded> = regions
            .iter()
            .filter(|&&(header, last)| header < last && last < count)
            .map(|&region| self.folded(region))
            .collect();
        self.display.unfold(&self.buffer, |_| true);
        self.display.fold(&self.buffer, folds);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indentation_regions_nest_and_skip_blank_lines() {
        let buffer = Buffer::new("a:\n  b:\n    c\n\n  d\ne\n  f\n");
        assert_eq!(
            indentation_regions(&buffer, 0..buffer.line_count()),
            [(0, 4), (1, 2), (5, 6)]
        );
        // Only regions touching line 2, found from the enclosing header.
        assert_eq!(indentation_regions(&buffer, 2..3), [(0, 4), (1, 2)]);
        assert_eq!(indentation_regions(&buffer, 5..6), [(5, 6)]);
    }
}
