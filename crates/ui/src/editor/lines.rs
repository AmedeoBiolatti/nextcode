//! Whole-line commands at every selection: move/copy/delete/insert lines,
//! comment toggling, indentation, and the file's detected indent style.

use super::*;

actions!(
    editor,
    [
        MoveLineUp,
        MoveLineDown,
        CopyLineUp,
        CopyLineDown,
        DeleteLine,
        InsertLineBelow,
        InsertLineAbove,
        ToggleLineComment,
        ToggleBlockComment,
        Indent,
        Outdent,
    ]
);

/// How the file indents: tabs or a number of spaces per level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct IndentStyle {
    pub tabs: bool,
    /// Spaces per level (also the tab stop width).
    pub width: usize,
}

impl IndentStyle {
    /// One level of indentation.
    pub fn unit(self) -> String {
        if self.tabs {
            "\t".into()
        } else {
            " ".repeat(self.width)
        }
    }

    pub fn label(self) -> String {
        if self.tabs {
            "Tabs".into()
        } else {
            format!("Spaces: {}", self.width)
        }
    }

    /// Default for `language` when the file shows no indentation.
    pub fn default_for(language: Option<Language>) -> Self {
        let width = match language {
            Some(
                Language::Json
                | Language::Yaml
                | Language::JavaScript
                | Language::TypeScript
                | Language::Tsx
                | Language::Html
                | Language::Css,
            ) => 2,
            _ => 4,
        };
        Self { tabs: false, width }
    }

    /// Detects the style from the leading whitespace of `lines`: tabs if most
    /// indented lines start with one, else the most common indent step.
    pub fn detect<'a>(lines: impl Iterator<Item = &'a str>, language: Option<Language>) -> Self {
        let mut tab_lines = 0;
        let mut space_lines = 0;
        // How often each indent step (1-8 spaces) occurs between lines.
        let mut steps = [0usize; 9];
        let mut previous = 0;
        for line in lines {
            if line.trim().is_empty() {
                continue;
            }
            if line.starts_with('\t') {
                tab_lines += 1;
                continue;
            }
            let spaces = line.len() - line.trim_start_matches(' ').len();
            if spaces > 0 {
                space_lines += 1;
            }
            let step = spaces.abs_diff(previous);
            if (2..=8).contains(&step) {
                steps[step] += 1;
            }
            previous = spaces;
        }
        let default = Self::default_for(language);
        if tab_lines > space_lines {
            return Self {
                tabs: true,
                ..default
            };
        }
        // Ties go to the larger step: 4-space code also has 8-space steps
        // less often than 2-space code has 4-space ones.
        let best = (2..=8)
            .filter(|&step| steps[step] > 0)
            .max_by_key(|&step| (steps[step], step));
        match best {
            Some(width) if space_lines > 0 => Self { tabs: false, width },
            _ => default,
        }
    }
}

/// Leading whitespace length (bytes) of `line`.
fn indent_len(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// Where `offset` moves when `edits` (sorted, non-overlapping, in the old
/// text) are applied.
pub(super) fn map_through(edits: &[(Range<usize>, String)], offset: usize, bias: Bias) -> usize {
    let mut shift = 0isize;
    for (range, text) in edits {
        if offset < range.start || (offset == range.start && bias == Bias::Left) {
            break;
        }
        if offset >= range.end {
            shift += text.len() as isize - range.len() as isize;
            continue;
        }
        let inside = match bias {
            Bias::Left => 0,
            Bias::Right => text.len(),
        };
        return (range.start as isize + shift) as usize + inside;
    }
    (offset as isize + shift) as usize
}

impl Editor {
    /// Line spans `(first, last)` covered by the selections, merged when
    /// they overlap or touch. A multi-line selection ending at column 0
    /// leaves that last line out.
    pub(super) fn line_blocks(&self) -> Vec<(usize, usize)> {
        let mut blocks: Vec<(usize, usize)> = Vec::new();
        for s in self.selections.all() {
            let first = self.buffer.line_of(s.start());
            let mut last = self.buffer.line_of(s.end());
            if last > first && s.end() == self.buffer.line_start(last) {
                last -= 1;
            }
            match blocks.last_mut() {
                Some(block) if first <= block.1 + 1 => block.1 = block.1.max(last),
                _ => blocks.push((first, last)),
            }
        }
        blocks
    }

    /// Index of the block holding `selection`'s start.
    fn block_of(blocks: &[(usize, usize)], line: usize) -> usize {
        blocks
            .iter()
            .position(|&(first, last)| (first..=last).contains(&line))
            .unwrap_or(0)
    }

    /// The selections after `edits` that keep their text: cursors move
    /// past insertions at them, selections grow over insertions at their
    /// start.
    pub(super) fn map_selections(&self, edits: &[(Range<usize>, String)]) -> SelectionSet {
        self.selections.map(|s| {
            let map = |offset: usize, start: bool| {
                let bias = if !s.is_empty() && start {
                    Bias::Left
                } else {
                    Bias::Right
                };
                map_through(edits, offset, bias)
            };
            let anchor_is_start = s.anchor <= s.head;
            Selection {
                anchor: map(s.anchor, anchor_is_start),
                head: map(s.head, !anchor_is_start),
                goal: None,
            }
        })
    }

    /// Applies line edits, keeping the selections on their text.
    fn commit_mapped(&mut self, edits: Vec<(Range<usize>, String)>, cx: &mut Context<Self>) {
        let selections = self.map_selections(&edits);
        self.commit(edits, |_| selections, cx);
    }

    /// Shifts each selection by its block's offset (`delta(block index)`).
    fn shift_selections(&self, delta: impl Fn(usize) -> isize) -> SelectionSet {
        let blocks = self.line_blocks();
        self.selections.map(|s| {
            let block = Self::block_of(&blocks, self.buffer.line_of(s.start()));
            let shift = delta(block);
            Selection {
                anchor: s.anchor.saturating_add_signed(shift),
                head: s.head.saturating_add_signed(shift),
                goal: s.goal,
            }
        })
    }

    fn move_lines(&mut self, up: bool, cx: &mut Context<Self>) {
        let blocks = self.line_blocks();
        let last_line = self.buffer.line_count() - 1;
        let at_edge = if up {
            blocks[0].0 == 0
        } else {
            blocks[blocks.len() - 1].1 == last_line
        };
        if at_edge {
            return;
        }
        let b = &self.buffer;
        let mut edits = Vec::with_capacity(blocks.len());
        let mut deltas = Vec::with_capacity(blocks.len());
        for &(first, last) in &blocks {
            let block = b.slice(b.line_start(first)..b.line_end(last));
            if up {
                let start = b.line_start(first - 1);
                let previous = b.slice(start..b.line_end(first - 1));
                let line_break = b.slice(b.line_end(first - 1)..b.line_start(first));
                deltas.push(-((previous.len() + line_break.len()) as isize));
                edits.push((
                    start..b.line_end(last),
                    format!("{block}{line_break}{previous}"),
                ));
            } else {
                let next_start = b.line_start(last + 1);
                let next = b.slice(next_start..b.line_end(last + 1));
                let line_break = b.slice(b.line_end(last)..next_start);
                deltas.push((next.len() + line_break.len()) as isize);
                edits.push((
                    b.line_start(first)..b.line_end(last + 1),
                    format!("{next}{line_break}{block}"),
                ));
            }
        }
        let selections = self.shift_selections(|block| deltas[block]);
        self.commit(edits, |_| selections, cx);
    }

    pub(super) fn move_line_up(&mut self, _: &MoveLineUp, _: &mut Window, cx: &mut Context<Self>) {
        self.move_lines(true, cx);
    }

    pub(super) fn move_line_down(
        &mut self,
        _: &MoveLineDown,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_lines(false, cx);
    }

    /// Duplicates each block; the selections go with the lower copy when
    /// copying down, stay on the upper one when copying up.
    fn copy_lines(&mut self, down: bool, cx: &mut Context<Self>) {
        let blocks = self.line_blocks();
        let ending = self.buffer.line_ending();
        let b = &self.buffer;
        let mut edits = Vec::with_capacity(blocks.len());
        // Total insertion before each block, plus its own when copying down.
        let mut deltas = Vec::with_capacity(blocks.len());
        let mut inserted = 0isize;
        for &(first, last) in &blocks {
            let block = b.slice(b.line_start(first)..b.line_end(last));
            let len = (block.len() + ending.len()) as isize;
            if down {
                deltas.push(inserted + len);
                edits.push((
                    b.line_end(last)..b.line_end(last),
                    format!("{ending}{block}"),
                ));
            } else {
                deltas.push(inserted);
                edits.push((
                    b.line_start(first)..b.line_start(first),
                    format!("{block}{ending}"),
                ));
            }
            inserted += len;
        }
        let selections = self.shift_selections(|block| deltas[block]);
        self.commit(edits, |_| selections, cx);
    }

    pub(super) fn copy_line_up(&mut self, _: &CopyLineUp, _: &mut Window, cx: &mut Context<Self>) {
        self.copy_lines(false, cx);
    }

    pub(super) fn copy_line_down(
        &mut self,
        _: &CopyLineDown,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.copy_lines(true, cx);
    }

    /// Deletes the selected lines; each cursor keeps its column on the line
    /// that takes their place.
    pub(super) fn delete_line(&mut self, _: &DeleteLine, _: &mut Window, cx: &mut Context<Self>) {
        let blocks = self.line_blocks();
        let b = &self.buffer;
        let line_count = b.line_count();
        let mut edits = Vec::with_capacity(blocks.len());
        let mut targets = Vec::with_capacity(blocks.len());
        let mut removed = 0;
        for &(first, last) in &blocks {
            let range = if last + 1 < line_count {
                b.line_start(first)..b.line_start(last + 1)
            } else if first > 0 {
                b.line_end(first - 1)..b.len()
            } else {
                0..b.len()
            };
            edits.push((range, String::new()));
            let line = first - removed;
            targets.push(if last + 1 < line_count {
                line
            } else {
                line.saturating_sub(1)
            });
            removed += last - first + 1;
        }
        let cursors: Vec<(usize, usize)> = self
            .selections
            .all()
            .iter()
            .map(|s| {
                let block = Self::block_of(&blocks, b.line_of(s.start()));
                (targets[block], s.goal.unwrap_or_else(|| b.column(s.head)))
            })
            .collect();
        let primary = self.selections.primary_index();
        self.commit(
            edits,
            |buffer| {
                let items = cursors
                    .iter()
                    .map(|&(line, column)| Selection {
                        goal: Some(column),
                        ..Selection::cursor(buffer.offset_at(line, column))
                    })
                    .collect();
                SelectionSet::new(items, primary)
            },
            cx,
        );
    }

    fn insert_line(&mut self, below: bool, cx: &mut Context<Self>) {
        let ending = self.buffer.line_ending();
        let edits = self
            .selections
            .all()
            .iter()
            .map(|s| {
                let line = self.buffer.line_of(s.head);
                if below {
                    let end = self.buffer.line_end(line);
                    let (indent, _) = self.newline_indent(end);
                    SelectionEdit::insert(end..end, format!("{ending}{indent}"))
                } else {
                    let start = self.buffer.line_start(line);
                    let text = self.buffer.line_text(line);
                    let indent = &text[..indent_len(&text)];
                    SelectionEdit {
                        range: start..start,
                        text: format!("{indent}{ending}"),
                        anchor: indent.len(),
                        head: indent.len(),
                    }
                }
            })
            .collect();
        self.edit_selections(edits, cx);
    }

    pub(super) fn insert_line_below(
        &mut self,
        _: &InsertLineBelow,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.insert_line(true, cx);
    }

    pub(super) fn insert_line_above(
        &mut self,
        _: &InsertLineAbove,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.insert_line(false, cx);
    }

    /// Lines of all blocks, in order.
    fn selected_lines(&self) -> Vec<usize> {
        self.line_blocks()
            .into_iter()
            .flat_map(|(first, last)| first..=last)
            .collect()
    }

    /// Ctrl+/: comments out the selected lines at their smallest indent,
    /// or uncomments them when all non-blank ones are comments. Languages
    /// with only block comments wrap each block of lines instead.
    pub(super) fn toggle_line_comment(
        &mut self,
        _: &ToggleLineComment,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(language) = self.language() else {
            return;
        };
        let Some(token) = language.line_comment() else {
            if let Some(delimiters) = language.block_comment() {
                self.toggle_block_comment_lines(delimiters, cx);
            }
            return;
        };
        let lines = self.selected_lines();
        let texts: Vec<String> = lines.iter().map(|&l| self.buffer.line_text(l)).collect();
        let non_blank: Vec<usize> = (0..lines.len())
            .filter(|&i| !texts[i].trim().is_empty())
            .collect();
        let targets: Vec<usize> = if non_blank.is_empty() {
            (0..lines.len()).collect()
        } else {
            non_blank.clone()
        };
        let commented = !non_blank.is_empty()
            && non_blank
                .iter()
                .all(|&i| texts[i].trim_start().starts_with(token));
        let mut edits = Vec::with_capacity(targets.len());
        if commented {
            for &i in &targets {
                let text = &texts[i];
                let start = self.buffer.line_start(lines[i]) + indent_len(text);
                let rest = &text[indent_len(text) + token.len()..];
                let len = token.len() + usize::from(rest.starts_with(' '));
                edits.push((start..start + len, String::new()));
            }
        } else {
            let column = targets
                .iter()
                .map(|&i| indent_len(&texts[i]))
                .min()
                .unwrap_or(0);
            for &i in &targets {
                let at = self.buffer.line_start(lines[i]) + column.min(texts[i].len());
                edits.push((at..at, format!("{token} ")));
            }
        }
        self.commit_mapped(edits, cx);
    }

    /// Edits that toggle `open`/`close` around `range`: removes them when
    /// `range` starts and ends with them (or sits right inside them), adds
    /// them with a space of padding otherwise.
    fn block_comment_edits(
        &self,
        range: Range<usize>,
        (open, close): (&str, &str),
    ) -> Vec<(Range<usize>, String)> {
        let text = self.buffer.slice(range.clone());
        if text.len() >= open.len() + close.len() && text.starts_with(open) && text.ends_with(close)
        {
            let inner = &text[open.len()..text.len() - close.len()];
            let open_len = open.len() + usize::from(inner.starts_with(' '));
            let close_len = close.len() + usize::from(inner.len() > 1 && inner.ends_with(' '));
            return vec![
                (range.start..range.start + open_len, String::new()),
                (range.end - close_len..range.end, String::new()),
            ];
        }
        let b = &self.buffer;
        let before_start = range.start.saturating_sub(open.len() + 1);
        let before = b.slice(before_start..range.start);
        let after = b.slice(range.end..(range.end + close.len() + 1).min(b.len()));
        let open_len = if before.ends_with(&format!("{open} ")) {
            Some(open.len() + 1)
        } else {
            before.ends_with(open).then_some(open.len())
        };
        let close_len = if after.starts_with(&format!(" {close}")) {
            Some(close.len() + 1)
        } else {
            after.starts_with(close).then_some(close.len())
        };
        if let (Some(open_len), Some(close_len)) = (open_len, close_len) {
            return vec![
                (range.start - open_len..range.start, String::new()),
                (range.end..range.end + close_len, String::new()),
            ];
        }
        vec![
            (range.start..range.start, format!("{open} ")),
            (range.end..range.end, format!(" {close}")),
        ]
    }

    /// Block-comment toggle over each block of lines (HTML, CSS, Markdown).
    fn toggle_block_comment_lines(&mut self, delimiters: (&str, &str), cx: &mut Context<Self>) {
        let mut edits = Vec::new();
        for (first, last) in self.line_blocks() {
            let text = self.buffer.line_text(first);
            let start = self.buffer.line_start(first) + indent_len(&text);
            let end = self.buffer.line_end(last);
            if start < end {
                edits.extend(self.block_comment_edits(start..end, delimiters));
            }
        }
        self.commit_mapped(edits, cx);
    }

    /// Shift+Alt+A: toggles a block comment around each selection; an empty
    /// selection gets an empty comment with the cursor inside.
    pub(super) fn toggle_block_comment(
        &mut self,
        _: &ToggleBlockComment,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(delimiters) = self.language().and_then(Language::block_comment) else {
            return;
        };
        let (open, close) = delimiters;
        let mut edits = Vec::new();
        for s in self.selections.all() {
            if s.is_empty() {
                edits.push((s.head..s.head, format!("{open}  {close}")));
            } else {
                edits.extend(self.block_comment_edits(s.range(), delimiters));
            }
        }
        // Selections stay on the commented text; cursors go inside the new
        // empty comment.
        let selections = self.selections.map(|s| {
            if s.is_empty() {
                return Selection::cursor(map_through(&edits, s.head, Bias::Left) + open.len() + 1);
            }
            let (start, end) = (Bias::Right, Bias::Left);
            let (anchor, head) = if s.reversed() {
                (end, start)
            } else {
                (start, end)
            };
            Selection::new(
                map_through(&edits, s.anchor, anchor),
                map_through(&edits, s.head, head),
            )
        });
        self.commit(edits, |_| selections, cx);
    }

    /// Adds one level of indentation to every selected non-blank line (any
    /// line when only blank ones are selected).
    fn indent_lines(&mut self, cx: &mut Context<Self>) {
        let unit = self.indent.unit();
        let lines = self.selected_lines();
        let all_blank = lines
            .iter()
            .all(|&l| self.buffer.line_text(l).trim().is_empty());
        let edits = lines
            .into_iter()
            .filter(|&l| all_blank || !self.buffer.line_text(l).trim().is_empty())
            .map(|l| {
                let start = self.buffer.line_start(l);
                (start..start, unit.clone())
            })
            .collect();
        self.commit_mapped(edits, cx);
    }

    /// Removes up to one level of indentation from every selected line.
    fn outdent_lines(&mut self, cx: &mut Context<Self>) {
        let width = self.indent.width;
        let edits = self
            .selected_lines()
            .into_iter()
            .filter_map(|l| {
                let text = self.buffer.line_text(l);
                let len = if text.starts_with('\t') {
                    1
                } else {
                    let spaces = text.len() - text.trim_start_matches(' ').len();
                    match spaces % width {
                        0 => spaces.min(width),
                        partial => partial,
                    }
                };
                let start = self.buffer.line_start(l);
                (len > 0).then(|| (start..start + len, String::new()))
            })
            .collect();
        self.commit_mapped(edits, cx);
    }

    /// Tab: indents the lines of a multi-line selection, otherwise inserts
    /// indentation (to the next tab stop) at every selection.
    pub(super) fn tab(&mut self, _: &Tab, _: &mut Window, cx: &mut Context<Self>) {
        let multi_line = self
            .selections
            .all()
            .iter()
            .any(|s| self.buffer.line_of(s.start()) != self.buffer.line_of(s.end()));
        if multi_line {
            return self.indent_lines(cx);
        }
        let edits = self
            .selections
            .all()
            .iter()
            .map(|s| {
                let text = if self.indent.tabs {
                    "\t".to_owned()
                } else {
                    let width = self.indent.width;
                    " ".repeat(width - self.buffer.column(s.start()) % width)
                };
                SelectionEdit::insert(s.range(), text)
            })
            .collect();
        self.edit_selections(edits, cx);
    }

    pub(super) fn outdent(&mut self, _: &Outdent, _: &mut Window, cx: &mut Context<Self>) {
        self.outdent_lines(cx);
    }

    pub(super) fn indent(&mut self, _: &Indent, _: &mut Window, cx: &mut Context<Self>) {
        self.indent_lines(cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detect(text: &str, language: Option<Language>) -> IndentStyle {
        IndentStyle::detect(text.lines(), language)
    }

    #[test]
    fn indent_style_comes_from_the_file_then_the_language() {
        let four = "def f():\n    if x:\n        y()\n    return 1\n";
        assert_eq!(
            detect(four, None),
            IndentStyle {
                tabs: false,
                width: 4
            }
        );
        let two = "a:\n  b:\n    c: 1\n  d: 2\n";
        assert_eq!(
            detect(two, Some(Language::Python)),
            IndentStyle {
                tabs: false,
                width: 2
            }
        );
        let tabs = "fn f() {\n\tlet x = 1;\n\tif x {\n\t\ty();\n\t}\n}\n";
        assert!(detect(tabs, None).tabs);
        assert_eq!(detect("{}\n", Some(Language::Json)).width, 2);
        assert_eq!(detect("x = 1\n", Some(Language::Python)).width, 4);
        assert_eq!(detect("", Some(Language::TypeScript)).width, 2);
    }

    #[test]
    fn offsets_map_through_sorted_edits() {
        let edits = vec![(2..2, "ab".to_owned()), (5..8, "x".to_owned())];
        assert_eq!(map_through(&edits, 1, Bias::Right), 1);
        assert_eq!(map_through(&edits, 2, Bias::Left), 2);
        assert_eq!(map_through(&edits, 2, Bias::Right), 4);
        assert_eq!(map_through(&edits, 6, Bias::Left), 7);
        assert_eq!(map_through(&edits, 6, Bias::Right), 8);
        assert_eq!(map_through(&edits, 9, Bias::Right), 9);
    }
}
