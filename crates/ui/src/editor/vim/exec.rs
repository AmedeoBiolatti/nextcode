//! Running parsed commands: motions, operators over spans of text,
//! insert and replace mode, registers, visual mode, `.`, marks and
//! macros.

use super::{
    motion::{self, ObjectRange},
    parse::{
        Action, Cmd, FoldCommand, InsertAt, Motion, Object, ObjectKind, Operator, Scroll, Target,
    },
    *,
};

/// How a motion's target bounds an operator's text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    Exclusive,
    Inclusive,
    Linewise,
}

/// Where a motion goes.
pub(super) struct Reach {
    pub offset: usize,
    pub kind: Kind,
    /// Sets the `''` mark (G, %, searches...).
    pub jump: bool,
    /// Column for following j/k.
    pub goal: Option<usize>,
    /// Lines the view scrolls with the cursor (Ctrl+D...).
    pub scroll: isize,
}

impl Reach {
    fn new(offset: usize, kind: Kind) -> Self {
        Self {
            offset,
            kind,
            jump: false,
            goal: None,
            scroll: 0,
        }
    }

    fn jump(mut self) -> Self {
        self.jump = true;
        self
    }
}

/// The text an operator acts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Span {
    /// Sorted, non-overlapping ranges (several from multiple selections).
    Chars(Vec<Range<usize>>),
    /// Lines `first..=last`.
    Lines(usize, usize),
    /// One range per line.
    Block(Vec<Range<usize>>),
}

impl Span {
    /// One range of characters.
    fn chars(range: Range<usize>) -> Self {
        Self::Chars(vec![range])
    }
}

impl Editor {
    /// Where Vim's cursor is: the primary cursor, or visual mode's.
    pub(super) fn vim_cursor(&self) -> usize {
        match self.vim.mode {
            Mode::Visual(_) => self.vim.cursor,
            _ => self.primary().head,
        }
    }

    pub(super) fn place_cursor(&mut self, offset: usize, cx: &mut Context<Self>) {
        let offset = offset.min(self.buffer.len());
        let offset = if self.vim.mode == Mode::Normal {
            self.normal_cursor(offset)
        } else {
            offset
        };
        self.set_selections(SelectionSet::single(Selection::cursor(offset)), cx);
    }

    fn line_indent(&self, line: usize) -> String {
        let start = self.buffer.line_start(line);
        self.buffer
            .slice(start..motion::first_non_blank(&self.buffer, line))
    }

    pub(super) fn run_command(
        &mut self,
        command: Command,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let depth = self.buffer.undo_depth();
        self.buffer.seal_undo();
        let extent = self.visual_extent();
        let changes = is_change(&command);
        let count = command.count();
        match &command.action {
            Action::Move(motion) => self.do_move(motion, command.count, cx),
            Action::Operate(op, target) => {
                self.collapse_to_primary(cx);
                self.operate(*op, target, &command, window, cx)
            }
            Action::VisualOperate { op, lines } => {
                self.visual_operate(*op, *lines, &command, window, cx)
            }
            Action::Select(object) => self.select_object(*object, count, cx),
            Action::Do(cmd) => self.do_cmd(cmd, &command, window, cx),
        }
        if changes && self.buffer.undo_depth() > depth {
            let head = self.primary().head;
            self.vim.marks.insert('.', head);
        }
        if !matches!(self.vim.mode, Mode::Insert | Mode::Replace) {
            self.group_undo_since(depth);
        } else if let Some(session) = &mut self.vim.insert {
            session.undo_depth = depth;
            if changes && self.vim.replaying == 0 {
                session.command = Some(command.clone());
                session.visual = extent;
            }
        }
        if changes
            && self.vim.replaying == 0
            && !matches!(self.vim.mode, Mode::Insert | Mode::Replace)
        {
            self.vim.last_change = Some(Change {
                command,
                inserted: None,
                visual: extent,
            });
        }
        cx.notify();
    }

    fn collapse_to_primary(&mut self, cx: &mut Context<Self>) {
        if self.selections.len() > 1 && self.vim.mode == Mode::Normal {
            let head = self.primary().head;
            self.set_selections(SelectionSet::single(Selection::cursor(head)), cx);
        }
    }

    fn do_move(&mut self, motion: &Motion, count: Option<usize>, cx: &mut Context<Self>) {
        let visual = matches!(self.vim.mode, Mode::Visual(_));
        let from = self.vim_cursor();
        let Some(reach) = self.reach(motion, count, from, false, cx) else {
            return;
        };
        if reach.jump {
            self.vim.marks.insert('\'', from);
        }
        if reach.scroll != 0
            && let Some(layout) = &self.layout
        {
            self.scroll.y += layout.line_height * reach.scroll as f32;
        }
        self.vim.goal = match motion {
            Motion::LineEnd => Some(usize::MAX),
            _ => reach.goal,
        };
        if visual {
            let line = self.buffer.line_of(reach.offset);
            let allow_end = matches!(motion, Motion::LineEnd);
            self.vim.cursor = if allow_end {
                reach.offset
            } else {
                let last = motion::last_char(&self.buffer, line);
                reach.offset.min(last.max(self.buffer.line_start(line)))
            };
            self.vim.to_line_end = allow_end && self.vim.mode == Mode::Visual(VisualKind::Block);
            self.show_visual(cx);
        } else {
            let head = self.primary().head;
            let others = self.selections.len() > 1;
            self.place_cursor(reach.offset, cx);
            if others && head == reach.offset {
                cx.notify();
            }
        }
    }

    /// The offset at char column `goal` (`usize::MAX`: the end) of `line`.
    fn column_offset(&self, line: usize, goal: usize, operator: bool) -> usize {
        if goal == usize::MAX {
            return if operator {
                self.buffer.line_end(line)
            } else {
                motion::last_char(&self.buffer, line)
            };
        }
        let offset = self.buffer.offset_at(line, goal);
        if operator {
            offset
        } else {
            offset.min(motion::last_char(&self.buffer, line))
        }
    }

    /// Lines fully on screen (first, last).
    fn screen_lines(&self) -> (usize, usize) {
        let Some(layout) = &self.layout else {
            let line = self.buffer.line_of(self.vim_cursor());
            return (line, line);
        };
        let first_row = (self.scroll.y / layout.line_height).ceil().max(0.) as usize;
        let rows = (layout.text_bounds.size.height / layout.line_height)
            .floor()
            .max(1.) as usize;
        let last_row = (first_row + rows - 1).min(self.display.row_count().saturating_sub(1));
        (
            self.display.row_line(first_row),
            self.display.row_line(last_row),
        )
    }

    /// Where `motion` goes from `from`; `None` when it cannot move.
    /// `operator`: the target of an operator (may reach line ends).
    pub(super) fn reach(
        &mut self,
        motion: &Motion,
        count: Option<usize>,
        from: usize,
        operator: bool,
        cx: &mut Context<Self>,
    ) -> Option<Reach> {
        let n = count.unwrap_or(1).max(1);
        let buffer = &self.buffer;
        let line = buffer.line_of(from);
        let last_line = buffer.line_count() - 1;
        let first_non_blank = |line| motion::first_non_blank(buffer, line);
        let reach = match motion {
            Motion::Left => {
                let start = buffer.line_start(line);
                if from <= start {
                    return None;
                }
                let mut offset = from;
                for _ in 0..n {
                    if offset <= start {
                        break;
                    }
                    offset = buffer.prev_grapheme(offset).max(start);
                }
                Reach::new(offset, Kind::Exclusive)
            }
            Motion::Right => {
                let limit = if operator {
                    buffer.line_end(line)
                } else {
                    motion::last_char(buffer, line)
                };
                if from >= limit {
                    return None;
                }
                let mut offset = from;
                for _ in 0..n {
                    if offset >= limit {
                        break;
                    }
                    offset = buffer.next_grapheme(offset).min(limit);
                }
                Reach::new(offset, Kind::Exclusive)
            }
            Motion::LeftWrap => {
                let mut offset = from;
                for _ in 0..n {
                    offset = buffer.prev_grapheme(offset);
                    let line = buffer.line_of(offset);
                    if offset == buffer.line_end(line) && !motion::is_empty_line(buffer, line) {
                        offset = motion::last_char(buffer, line);
                    }
                }
                Reach::new(offset, Kind::Exclusive)
            }
            Motion::RightWrap => {
                let mut offset = from;
                for _ in 0..n {
                    offset = buffer.next_grapheme(offset);
                    let line = buffer.line_of(offset);
                    if offset == buffer.line_end(line) && !motion::is_empty_line(buffer, line) {
                        offset = buffer.next_grapheme(offset);
                    }
                }
                Reach::new(offset.min(buffer.len()), Kind::Exclusive)
            }
            Motion::Up | Motion::Down => {
                let target = if *motion == Motion::Up {
                    line.saturating_sub(n)
                } else {
                    (line + n).min(last_line)
                };
                if target == line {
                    return None;
                }
                let goal = self.vim.goal.unwrap_or_else(|| self.buffer.column(from));
                let mut reach =
                    Reach::new(self.column_offset(target, goal, operator), Kind::Linewise);
                reach.goal = Some(goal);
                reach
            }
            Motion::RowUp | Motion::RowDown => {
                let (row, column) = self.display.point(buffer, from, false);
                let rows = self.display.row_count();
                let target = if *motion == Motion::RowUp {
                    row.saturating_sub(n)
                } else {
                    (row + n).min(rows.saturating_sub(1))
                };
                let (offset, _) = self.display.offset_at(buffer, target, column);
                Reach::new(offset, Kind::Exclusive)
            }
            Motion::WordStart { big } => {
                let mut offset = from;
                for _ in 0..n {
                    offset = motion::next_word_start(buffer, offset, *big);
                }
                Reach::new(offset, Kind::Exclusive)
            }
            Motion::WordEnd { big } => {
                let mut offset = from;
                for _ in 0..n {
                    offset = motion::word_end(buffer, offset, *big);
                }
                Reach::new(offset, Kind::Inclusive)
            }
            Motion::WordBack { big } => {
                let mut offset = from;
                for _ in 0..n {
                    offset = motion::word_back(buffer, offset, *big);
                }
                Reach::new(offset, Kind::Exclusive)
            }
            Motion::WordEndBack { big } => {
                let mut offset = from;
                for _ in 0..n {
                    offset = motion::word_end_back(buffer, offset, *big);
                }
                Reach::new(offset, Kind::Inclusive)
            }
            Motion::LineStart => Reach::new(buffer.line_start(line), Kind::Exclusive),
            Motion::FirstNonBlank => Reach::new(first_non_blank(line), Kind::Exclusive),
            Motion::LineEnd => {
                let line = (line + n - 1).min(last_line);
                let mut reach = Reach::new(buffer.line_end(line), Kind::Exclusive);
                reach.goal = Some(usize::MAX);
                reach
            }
            Motion::LastNonBlank => {
                let line = (line + n - 1).min(last_line);
                let start = buffer.line_start(line);
                let end = buffer.line_end(line);
                let offset = buffer
                    .chars_before(end)
                    .take_while(|&(o, _)| o >= start)
                    .find(|(_, c)| !c.is_whitespace())
                    .map_or(start, |(o, _)| o);
                Reach::new(offset, Kind::Inclusive)
            }
            Motion::NextLine | Motion::PrevLine => {
                let target = if *motion == Motion::NextLine {
                    line + n
                } else {
                    line.checked_sub(n)?
                };
                if target > last_line {
                    return None;
                }
                Reach::new(first_non_blank(target), Kind::Linewise)
            }
            Motion::Column => {
                let offset = buffer.offset_at(line, n - 1);
                Reach::new(offset, Kind::Exclusive)
            }
            Motion::FirstLine | Motion::LastLine => {
                let default = if *motion == Motion::FirstLine {
                    0
                } else {
                    last_line
                };
                let target = count
                    .map_or(default, |c| c.saturating_sub(1))
                    .min(last_line);
                Reach::new(first_non_blank(target), Kind::Linewise).jump()
            }
            Motion::Find { ch, forward, till } => {
                cx.default_global::<Shared>().find = Some((*ch, *forward, *till));
                let buffer = &self.buffer;
                let offset = motion::find_in_line(buffer, from, *ch, *forward, *till, n, false)?;
                Reach::new(
                    offset,
                    if *forward {
                        Kind::Inclusive
                    } else {
                        Kind::Exclusive
                    },
                )
            }
            Motion::RepeatFind { reverse } => {
                let (ch, forward, till) = cx.default_global::<Shared>().find?;
                let forward = forward != *reverse;
                let buffer = &self.buffer;
                let offset = motion::find_in_line(buffer, from, ch, forward, till, n, true)?;
                Reach::new(
                    offset,
                    if forward {
                        Kind::Inclusive
                    } else {
                        Kind::Exclusive
                    },
                )
            }
            Motion::Match => match count {
                Some(percent) => {
                    let lines = last_line + 1;
                    let target = ((percent.min(100) * lines).div_ceil(100)).max(1) - 1;
                    Reach::new(first_non_blank(target), Kind::Linewise).jump()
                }
                None => Reach::new(motion::match_pair(buffer, from)?, Kind::Inclusive).jump(),
            },
            Motion::ParagraphForward => {
                Reach::new(motion::paragraph_forward(buffer, from, n), Kind::Exclusive).jump()
            }
            Motion::ParagraphBack => {
                Reach::new(motion::paragraph_back(buffer, from, n), Kind::Exclusive).jump()
            }
            Motion::ScreenTop | Motion::ScreenMiddle | Motion::ScreenBottom => {
                let (first, last) = self.screen_lines();
                let target = match motion {
                    Motion::ScreenTop => (first + n - 1).min(last),
                    Motion::ScreenBottom => last.saturating_sub(n - 1).max(first),
                    _ => (first + last) / 2,
                };
                Reach::new(
                    motion::first_non_blank(&self.buffer, target),
                    Kind::Linewise,
                )
                .jump()
            }
            Motion::HalfPageDown | Motion::HalfPageUp | Motion::PageDown | Motion::PageUp => {
                let page = self.page_rows().max(1) as usize;
                let rows = match motion {
                    Motion::HalfPageDown | Motion::HalfPageUp => count.unwrap_or((page / 2).max(1)),
                    _ => page.saturating_sub(2).max(1) * n,
                };
                let down = matches!(motion, Motion::HalfPageDown | Motion::PageDown);
                let target = if down {
                    (line + rows).min(last_line)
                } else {
                    line.saturating_sub(rows)
                };
                if target == line {
                    return None;
                }
                let goal = self.vim.goal.unwrap_or_else(|| self.buffer.column(from));
                let mut reach =
                    Reach::new(self.column_offset(target, goal, operator), Kind::Linewise);
                reach.goal = Some(goal);
                reach.scroll = if down {
                    rows as isize
                } else {
                    -(rows as isize)
                };
                reach
            }
            Motion::SearchNext { reverse } => {
                let search = cx.default_global::<Shared>().search.clone()?;
                let forward = search.forward != *reverse;
                let offset = self.search_motion(&search.pattern, from, forward, n, cx)?;
                Reach::new(offset, Kind::Exclusive).jump()
            }
            Motion::SearchWord { forward } => {
                let (word, start) = self.word_for_search(from)?;
                let pattern = format!("\\C{word}");
                let shared = cx.default_global::<Shared>();
                shared.search = Some(LastSearch {
                    pattern: pattern.clone(),
                    forward: *forward,
                });
                // From the word's start, so `#` skips the word itself.
                let from = if *forward { from } else { start };
                let offset = self.search_motion(&pattern, from, *forward, n, cx)?;
                Reach::new(offset, Kind::Exclusive).jump()
            }
            Motion::Search { forward, pattern } => {
                let shared = cx.default_global::<Shared>();
                let pattern = if pattern.is_empty() {
                    shared.search.as_ref()?.pattern.clone()
                } else {
                    pattern.clone()
                };
                shared.search = Some(LastSearch {
                    pattern: pattern.clone(),
                    forward: *forward,
                });
                let offset = self.search_motion(&pattern, from, *forward, n, cx)?;
                Reach::new(offset, Kind::Exclusive).jump()
            }
            Motion::Mark { ch, line } => {
                let offset = self.mark(*ch)?.min(self.buffer.len());
                if *line {
                    let target = self.buffer.line_of(offset);
                    Reach::new(
                        motion::first_non_blank(&self.buffer, target),
                        Kind::Linewise,
                    )
                    .jump()
                } else {
                    Reach::new(offset, Kind::Exclusive).jump()
                }
            }
        };
        Some(reach)
    }

    /// The keyword at or after `from` on its line, as a whole-word
    /// pattern, and its start.
    fn word_for_search(&self, from: usize) -> Option<(String, usize)> {
        let line_end = self.buffer.line_end(self.buffer.line_of(from));
        let word_char = |c: char| c.is_alphanumeric() || c == '_';
        let mut start = self
            .buffer
            .chars_from(from)
            .take_while(|&(o, _)| o < line_end)
            .find(|&(_, c)| word_char(c))
            .map(|(o, _)| o)?;
        if start == from {
            start = self
                .buffer
                .chars_before(from)
                .take_while(|&(_, c)| word_char(c))
                .last()
                .map_or(from, |(o, _)| o);
        }
        let word: String = self
            .buffer
            .chars_from(start)
            .map(|(_, c)| c)
            .take_while(|&c| word_char(c))
            .collect();
        Some((format!("\\b{}\\b", regex::escape(&word)), start))
    }

    fn search_motion(
        &mut self,
        pattern: &str,
        from: usize,
        forward: bool,
        count: usize,
        cx: &mut Context<Self>,
    ) -> Option<usize> {
        cx.default_global::<Shared>().highlight = true;
        match self.find_pattern(pattern, from, forward, count) {
            Ok(Some((offset, wrapped))) => {
                if wrapped {
                    self.notice = Some(
                        if forward {
                            "search hit BOTTOM, continuing at TOP"
                        } else {
                            "search hit TOP, continuing at BOTTOM"
                        }
                        .into(),
                    );
                }
                Some(offset)
            }
            Ok(None) => {
                self.notice = Some(format!("E486: Pattern not found: {pattern}").into());
                None
            }
            Err(err) => {
                self.notice = Some(err.into());
                None
            }
        }
    }

    pub(super) fn mark(&self, ch: char) -> Option<usize> {
        match ch {
            '`' => self.vim.marks.get(&'\'').copied(),
            c => self.vim.marks.get(&c).copied(),
        }
    }

    /// `cw`: up to the end of the word (the cursor's character when it
    /// ends a word), like `ce`.
    fn change_word_end(&self, from: usize, n: usize, big: bool) -> usize {
        let ends_run = |offset: usize| {
            let k = self.char_at(offset).map(|c| motion::class(c, big));
            let next = self.buffer.next_grapheme(offset);
            let next_class = self.char_at(next).map(|c| motion::class(c, big));
            next >= self.buffer.len() || next_class != k
        };
        let mut offset = from;
        for i in 0..n {
            if i == 0 && ends_run(offset) {
                continue;
            }
            offset = motion::word_end(&self.buffer, offset, big);
        }
        offset
    }

    fn object(&self, object: Object, from: usize, count: usize) -> Option<ObjectRange> {
        let buffer = &self.buffer;
        match object.kind {
            ObjectKind::Word => motion::word_object(buffer, from, false, object.inner, count),
            ObjectKind::BigWord => motion::word_object(buffer, from, true, object.inner, count),
            ObjectKind::Quote(quote) => motion::quote_object(buffer, from, quote, object.inner),
            ObjectKind::Bracket(open, close) => {
                motion::bracket_object(buffer, from, open, close, object.inner, count)
            }
            ObjectKind::Tag => motion::tag_object(&self.text(), from, object.inner, count),
            ObjectKind::Paragraph => motion::paragraph_object(buffer, from, object.inner, count),
        }
    }

    fn object_span(&self, object: ObjectRange) -> Span {
        if object.linewise {
            Span::Lines(
                self.buffer.line_of(object.range.start),
                self.buffer.line_of(object.range.end),
            )
        } else {
            Span::chars(object.range)
        }
    }

    fn operate(
        &mut self,
        op: Operator,
        target: &Target,
        command: &Command,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let n = command.count();
        let from = self.primary().head;
        let line = self.buffer.line_of(from);
        let span = match target {
            Target::Lines => {
                let last = (line + n - 1).min(self.buffer.line_count() - 1);
                Span::Lines(line, last)
            }
            Target::Object(object) => match self.object(*object, from, n) {
                Some(object) => self.object_span(object),
                None => return,
            },
            Target::Motion(m) => match self.motion_span(op, m, command.count, from, cx) {
                Some(span) => span,
                None => return,
            },
        };
        self.apply(op, span, command.register, 1, window, cx);
    }

    /// The text `op` with motion `m` covers, with Vim's special cases.
    fn motion_span(
        &mut self,
        op: Operator,
        m: &Motion,
        count: Option<usize>,
        from: usize,
        cx: &mut Context<Self>,
    ) -> Option<Span> {
        let n = count.unwrap_or(1);
        if op == Operator::Change
            && let Motion::WordStart { big } = m
            && self.char_at(from).is_some_and(|c| !c.is_whitespace())
        {
            let end = self.change_word_end(from, n, *big);
            return Some(Span::chars(from..self.buffer.next_grapheme(end)));
        }
        let reach = self.reach(m, count, from, true, cx)?;
        if reach.jump {
            self.vim.marks.insert('\'', from);
        }
        let (start, end) = (from.min(reach.offset), from.max(reach.offset));
        let buffer = &self.buffer;
        let span = match reach.kind {
            Kind::Linewise => Span::Lines(buffer.line_of(start), buffer.line_of(end)),
            Kind::Inclusive => Span::chars(start..buffer.next_grapheme(end).min(buffer.len())),
            Kind::Exclusive => {
                let mut end = end;
                let start_line = buffer.line_of(start);
                let end_line = buffer.line_of(end);
                if let Motion::WordStart { .. } = m
                    && end_line > start_line
                    && end <= motion::first_non_blank(buffer, end_line)
                {
                    // The last word moved over ends a line: stop there.
                    return Some(Span::chars(start..buffer.line_end(end_line - 1).max(start)));
                }
                if end_line > start_line && end == buffer.line_start(end_line) && end > start {
                    // An exclusive motion ending at a line start ends at
                    // the previous line's end instead; from the
                    // indentation on it covers whole lines.
                    end = buffer.line_end(end_line - 1);
                    if start <= motion::first_non_blank(buffer, start_line) {
                        return Some(Span::Lines(start_line, end_line - 1));
                    }
                }
                if start == end {
                    return None;
                }
                Span::chars(start..end)
            }
        };
        Some(span)
    }

    /// Text of `span` (line breaks as `\n`) and its register kind.
    fn span_text(&self, span: &Span) -> (String, RegisterKind) {
        let normalize = |text: String| text.replace("\r\n", "\n");
        match span {
            Span::Chars(ranges) => (
                normalize(
                    ranges
                        .iter()
                        .map(|r| self.buffer.slice(r.clone()))
                        .collect::<Vec<_>>()
                        .join("\n"),
                ),
                RegisterKind::Chars,
            ),
            Span::Lines(first, last) => {
                let range = self.buffer.line_start(*first)..self.buffer.line_end(*last);
                (
                    normalize(self.buffer.slice(range)) + "\n",
                    RegisterKind::Lines,
                )
            }
            Span::Block(ranges) => (
                normalize(
                    ranges
                        .iter()
                        .map(|r| self.buffer.slice(r.clone()))
                        .collect::<Vec<_>>()
                        .join("\n"),
                ),
                RegisterKind::Block,
            ),
        }
    }

    /// The ranges of `span` as text (lines without their last break).
    fn span_ranges(&self, span: &Span) -> Vec<Range<usize>> {
        match span {
            Span::Chars(ranges) | Span::Block(ranges) => ranges.clone(),
            Span::Lines(first, last) => {
                let range = self.buffer.line_start(*first)..self.buffer.line_end(*last);
                vec![range]
            }
        }
    }

    fn span_lines(&self, span: &Span) -> (usize, usize) {
        match span {
            Span::Lines(first, last) => (*first, *last),
            Span::Chars(ranges) | Span::Block(ranges) => {
                let start = ranges.first().map_or(0, |r| r.start);
                let end = ranges.last().map_or(0, |r| r.end);
                let last = self.buffer.line_of(end);
                // A range ending at a line start leaves that line out.
                let last = if end > start && end == self.buffer.line_start(last) && last > 0 {
                    last - 1
                } else {
                    last
                };
                (
                    self.buffer.line_of(start),
                    last.max(self.buffer.line_of(start)),
                )
            }
        }
    }

    /// Applies `op` to `span`; `shifts` repeats `>` and `<`.
    pub(super) fn apply(
        &mut self,
        op: Operator,
        span: Span,
        register: Option<char>,
        shifts: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match op {
            Operator::Yank => {
                let (text, kind) = self.span_text(&span);
                self.store_register(register, op, text, kind, cx);
                let cursor = self.primary().head;
                let target = match &span {
                    Span::Lines(first, _) => {
                        let line = self.buffer.line_of(cursor);
                        if *first < line {
                            let column = self.buffer.column(cursor);
                            self.buffer.offset_at(*first, column)
                        } else {
                            cursor
                        }
                    }
                    Span::Chars(ranges) | Span::Block(ranges) => {
                        ranges.first().map_or(cursor, |r| r.start)
                    }
                };
                self.place_cursor(target, cx);
            }
            Operator::Delete => self.delete_span(span, register, cx),
            Operator::Change => self.change_span(span, register, cx),
            Operator::Indent | Operator::Outdent => {
                let (first, last) = self.span_lines(&span);
                for _ in 0..shifts.max(1) {
                    // Indent/Outdent work on the lines of the selections.
                    let selection =
                        Selection::new(self.buffer.line_start(first), self.buffer.line_end(last));
                    self.selections = SelectionSet::single(selection);
                    if op == Operator::Indent {
                        self.indent(&Indent, window, cx);
                    } else {
                        self.outdent(&Outdent, window, cx);
                    }
                }
                self.place_cursor(motion::first_non_blank(&self.buffer, first), cx);
            }
            Operator::Format => {
                let (first, last) = self.span_lines(&span);
                let selection =
                    Selection::new(self.buffer.line_start(first), self.buffer.line_end(last));
                self.selections = SelectionSet::single(selection);
                self.format_selection(&FormatSelection, window, cx);
                self.place_cursor(motion::first_non_blank(&self.buffer, first), cx);
            }
            Operator::ToggleCase | Operator::Lower | Operator::Upper => {
                let ranges = self.span_ranges(&span);
                let edits: Vec<(Range<usize>, String)> = ranges
                    .iter()
                    .map(|r| (r.clone(), change_case(&self.buffer.slice(r.clone()), op)))
                    .collect();
                let start = match &span {
                    Span::Lines(first, _) => self
                        .primary()
                        .head
                        .max(self.buffer.line_start(*first))
                        .min(self.buffer.line_end(*first)),
                    _ => ranges.first().map_or(0, |r| r.start),
                };
                self.commit(
                    edits,
                    |_| SelectionSet::single(Selection::cursor(start)),
                    cx,
                );
                self.place_cursor(start, cx);
            }
        }
    }

    fn delete_span(&mut self, span: Span, register: Option<char>, cx: &mut Context<Self>) {
        let (text, kind) = self.span_text(&span);
        self.store_register(register, Operator::Delete, text, kind, cx);
        match span {
            Span::Chars(ranges) | Span::Block(ranges) => {
                let start = ranges.first().map_or(0, |r| r.start);
                let edits = ranges.into_iter().map(|r| (r, String::new())).collect();
                self.commit(
                    edits,
                    |_| SelectionSet::single(Selection::cursor(start)),
                    cx,
                );
                self.place_cursor(start, cx);
            }
            Span::Lines(first, last) => {
                let buffer = &self.buffer;
                let range = if last + 1 < buffer.line_count() {
                    buffer.line_start(first)..buffer.line_start(last + 1)
                } else if first > 0 {
                    buffer.line_end(first - 1)..buffer.len()
                } else {
                    0..buffer.len()
                };
                self.commit(
                    vec![(range, String::new())],
                    |_| SelectionSet::single(Selection::cursor(0)),
                    cx,
                );
                let line = first.min(self.buffer.line_count() - 1);
                self.place_cursor(motion::first_non_blank(&self.buffer, line), cx);
            }
        }
    }

    fn change_span(&mut self, span: Span, register: Option<char>, cx: &mut Context<Self>) {
        let (text, kind) = self.span_text(&span);
        let block = matches!(span, Span::Block(_));
        self.store_register(register, Operator::Change, text, kind, cx);
        match span {
            Span::Chars(ranges) | Span::Block(ranges) => {
                // A cursor at each range: typing goes to all of them. A
                // block's top line ends up with the cursor.
                let primary_at = self.vim_cursor();
                let primary = if block {
                    0
                } else {
                    ranges
                        .iter()
                        .position(|r| r.start <= primary_at && primary_at <= r.end)
                        .unwrap_or(0)
                };
                let items = ranges
                    .iter()
                    .map(|r| Selection::new(r.start, r.end))
                    .collect();
                self.selections = SelectionSet::new(items, primary);
                let edits = self
                    .selections
                    .all()
                    .iter()
                    .map(|s| SelectionEdit::insert(s.range(), String::new()))
                    .collect();
                self.edit_selections(edits, cx);
            }
            Span::Lines(first, last) => {
                let indent = self.line_indent(first);
                let start = self.buffer.line_start(first);
                let range = start..self.buffer.line_end(last);
                let cursor = start + indent.len();
                self.commit(
                    vec![(range, indent)],
                    |_| SelectionSet::single(Selection::cursor(cursor)),
                    cx,
                );
            }
        }
        self.start_insert(1);
    }

    pub(super) fn start_insert(&mut self, count: usize) {
        let head = self.primary().head;
        self.vim.mode = Mode::Insert;
        self.vim.goal = None;
        self.vim.insert = Some(InsertSession {
            command: None,
            visual: None,
            count: count.max(1),
            start: head,
            end: head,
            erased_before: 0,
            erased_after: 0,
            undo_depth: self.buffer.undo_depth(),
            replaced: Vec::new(),
            typed: String::new(),
        });
    }

    /// Escape in insert or replace mode.
    pub(super) fn leave_insert(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let replace = self.vim.mode == Mode::Replace;
        let Some(session) = self.vim.insert.take() else {
            self.vim.mode = Mode::Normal;
            cx.notify();
            return;
        };
        let head = self.primary().head;
        let inserted = if replace {
            Inserted {
                text: session.typed.clone(),
                ..Inserted::default()
            }
        } else {
            let start = session.start.min(head).min(self.buffer.len());
            let end = session.end.max(head).clamp(start, self.buffer.len());
            Inserted {
                text: self.buffer.slice(start..end),
                cursor: head - start,
                erased_before: session.erased_before,
                erased_after: session.erased_after,
            }
        };
        if !replace {
            for _ in 1..session.count {
                if let Some(Command {
                    action:
                        Action::Do(Cmd::Insert(at @ (InsertAt::LineBelow | InsertAt::LineAbove))),
                    ..
                }) = &session.command
                {
                    let at = *at;
                    self.collapse_insert_cursor(cx);
                    self.open_line(at == InsertAt::LineAbove, cx);
                }
                self.replay_inserted(&inserted, cx);
            }
        }
        let shared = cx.default_global::<Shared>();
        shared.registers.insert(
            '.',
            Register {
                text: inserted.text.replace("\r\n", "\n"),
                kind: RegisterKind::Chars,
            },
        );
        if self.vim.replaying == 0
            && let Some(command) = session.command
        {
            self.vim.last_change = Some(Change {
                command,
                inserted: Some(inserted),
                visual: session.visual,
            });
        }
        let head = self.primary().head;
        self.vim.marks.insert('^', head);
        self.vim.marks.insert('.', head);
        self.vim.mode = Mode::Normal;
        self.group_undo_since(session.undo_depth);
        if self.in_snippet() {
            self.end_snippet(&EndSnippet, window, cx);
        }
        let head = self.primary().head;
        let line_start = self.buffer.line_start(self.buffer.line_of(head));
        let target = if head > line_start {
            self.buffer.prev_grapheme(head)
        } else {
            head
        };
        self.place_cursor(target, cx);
    }

    fn collapse_insert_cursor(&mut self, cx: &mut Context<Self>) {
        let head = self.primary().head;
        self.set_selections(SelectionSet::single(Selection::cursor(head)), cx);
    }

    /// Inserts `inserted` at the cursor as it was typed (erasing what
    /// Backspace/Delete erased around it).
    fn replay_inserted(&mut self, inserted: &Inserted, cx: &mut Context<Self>) {
        let head = self.primary().head;
        let clamp = |offset: usize| {
            let offset = offset.min(self.buffer.len());
            self.buffer
                .offset_of(self.buffer.point(offset), Encoding::Utf8)
        };
        let start = clamp(head.saturating_sub(inserted.erased_before));
        let end = clamp(head + inserted.erased_after).max(start);
        let cursor = start + inserted.cursor.min(inserted.text.len());
        self.commit(
            vec![(start..end, inserted.text.clone())],
            |_| SelectionSet::single(Selection::cursor(cursor)),
            cx,
        );
    }

    /// o / O: a new indented line below or above the cursor's.
    fn open_line(&mut self, above: bool, cx: &mut Context<Self>) {
        let head = self.primary().head;
        let line = self.buffer.line_of(head);
        let ending = self.buffer.line_ending();
        if above {
            let indent = self.line_indent(line);
            let start = self.buffer.line_start(line);
            let cursor = start + indent.len();
            self.commit(
                vec![(start..start, format!("{indent}{ending}"))],
                |_| SelectionSet::single(Selection::cursor(cursor)),
                cx,
            );
        } else {
            let end = self.buffer.line_end(line);
            let (indent, _) = self.newline_indent(end);
            let text = format!("{ending}{indent}");
            let cursor = end + text.len();
            self.commit(
                vec![(end..end, text)],
                |_| SelectionSet::single(Selection::cursor(cursor)),
                cx,
            );
        }
    }

    fn insert_at(&mut self, at: InsertAt, count: usize, cx: &mut Context<Self>) {
        let head = self.primary().head;
        let line = self.buffer.line_of(head);
        match at {
            InsertAt::Cursor => {
                let selections = self.selections.map(|s| Selection::cursor(s.start()));
                self.set_selections(selections, cx);
            }
            InsertAt::After => {
                let selections = self.selections.map(|s| {
                    let line = self.buffer.line_of(s.end());
                    let end = self.buffer.line_end(line);
                    Selection::cursor(if s.end() < end {
                        self.buffer.next_grapheme(s.end())
                    } else {
                        s.end()
                    })
                });
                self.set_selections(selections, cx);
            }
            InsertAt::FirstNonBlank => {
                let offset = motion::first_non_blank(&self.buffer, line);
                self.set_selections(SelectionSet::single(Selection::cursor(offset)), cx);
            }
            InsertAt::LineStart => {
                let offset = self.buffer.line_start(line);
                self.set_selections(SelectionSet::single(Selection::cursor(offset)), cx);
            }
            InsertAt::LineEnd => {
                let offset = self.buffer.line_end(line);
                self.set_selections(SelectionSet::single(Selection::cursor(offset)), cx);
            }
            InsertAt::LineBelow | InsertAt::LineAbove => {
                self.collapse_insert_cursor(cx);
                self.open_line(at == InsertAt::LineAbove, cx);
            }
            InsertAt::LastInsert => {
                let offset = self.vim.marks.get(&'^').copied().unwrap_or(head);
                self.set_selections(
                    SelectionSet::single(Selection::cursor(offset.min(self.buffer.len()))),
                    cx,
                );
            }
        }
        self.start_insert(count);
    }

    /// Replace mode: Escape, typed characters, Backspace and Enter.
    pub(super) fn replace_key(
        &mut self,
        key: Key,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        match key {
            Key::Esc if !self.popover_open() => self.leave_insert(window, cx),
            Key::Char(c) => self.replace_typed(c, cx),
            Key::Enter => self.replace_typed('\n', cx),
            Key::Backspace => {
                let head = self.primary().head;
                let Some(session) = &mut self.vim.insert else {
                    return true;
                };
                let previous = self.buffer.prev_grapheme(head);
                match session.replaced.pop() {
                    Some(original) => {
                        session.typed.pop();
                        let text = original.map(String::from).unwrap_or_default();
                        self.commit(
                            vec![(previous..head, text)],
                            |_| SelectionSet::single(Selection::cursor(previous)),
                            cx,
                        );
                    }
                    None => {
                        self.set_selections(SelectionSet::single(Selection::cursor(previous)), cx)
                    }
                }
            }
            _ => return false,
        }
        true
    }

    fn replace_typed(&mut self, c: char, cx: &mut Context<Self>) {
        let head = self.primary().head;
        let line_end = self.buffer.line_end(self.buffer.line_of(head));
        let (range, original) = if head < line_end && c != '\n' {
            (head..self.buffer.next_grapheme(head), self.char_at(head))
        } else {
            (head..head, None)
        };
        let text = if c == '\n' {
            self.buffer.line_ending().to_owned()
        } else {
            c.to_string()
        };
        let cursor = head + text.len();
        self.commit(
            vec![(range, text)],
            |_| SelectionSet::single(Selection::cursor(cursor)),
            cx,
        );
        if let Some(session) = &mut self.vim.insert {
            session.replaced.push(original);
            session.typed.push(c);
        }
    }

    /// The visual selection's size, in visual mode.
    fn visual_extent(&self) -> Option<VisualExtent> {
        let Mode::Visual(kind) = self.vim.mode else {
            return None;
        };
        let (start, end) = (
            self.vim.anchor.min(self.vim.cursor),
            self.vim.anchor.max(self.vim.cursor),
        );
        let lines = self.buffer.line_of(end) - self.buffer.line_of(start);
        let columns = match kind {
            VisualKind::Char if lines == 0 => {
                self.buffer.column(end) - self.buffer.column(start) + 1
            }
            VisualKind::Char => self.buffer.column(end),
            VisualKind::Line => 0,
            VisualKind::Block => {
                let (a, b) = (
                    self.buffer.column(self.vim.anchor),
                    self.buffer.column(self.vim.cursor),
                );
                a.abs_diff(b) + 1
            }
        };
        Some(VisualExtent {
            kind,
            lines,
            columns,
        })
    }

    /// Visual mode over `extent` from the cursor (for `.`).
    fn select_extent(&mut self, extent: VisualExtent, cx: &mut Context<Self>) {
        let head = self.primary().head;
        let line = self.buffer.line_of(head);
        let last = (line + extent.lines).min(self.buffer.line_count() - 1);
        let column = self.buffer.column(head);
        let cursor = match extent.kind {
            VisualKind::Char if extent.lines == 0 => self
                .buffer
                .offset_at(line, column + extent.columns.saturating_sub(1)),
            VisualKind::Char => self.buffer.offset_at(last, extent.columns),
            VisualKind::Line => self.buffer.offset_at(last, column),
            VisualKind::Block => self
                .buffer
                .offset_at(last, column + extent.columns.saturating_sub(1)),
        };
        self.vim.mode = Mode::Visual(extent.kind);
        self.vim.anchor = head;
        self.vim.cursor = cursor;
        self.vim.to_line_end = false;
        self.show_visual(cx);
    }

    pub(super) fn enter_visual(&mut self, kind: VisualKind, cx: &mut Context<Self>) {
        let head = self.primary().head;
        self.vim.mode = Mode::Visual(kind);
        self.vim.anchor = head;
        self.vim.cursor = head;
        self.vim.to_line_end = false;
        self.show_visual(cx);
    }

    /// Sets the editor's selections from visual mode's anchor and cursor.
    pub(super) fn show_visual(&mut self, cx: &mut Context<Self>) {
        let Mode::Visual(kind) = self.vim.mode else {
            return;
        };
        let buffer = &self.buffer;
        let (anchor, cursor) = (
            self.vim.anchor.min(buffer.len()),
            self.vim.cursor.min(buffer.len()),
        );
        let selections = match kind {
            VisualKind::Char => {
                let selection = if cursor >= anchor {
                    Selection::new(anchor, buffer.next_grapheme(cursor))
                } else {
                    Selection::new(buffer.next_grapheme(anchor), cursor)
                };
                if self.selections.len() > 1 && self.vim.shown.is_some() {
                    self.selections.with_primary(selection)
                } else {
                    SelectionSet::single(selection)
                }
            }
            VisualKind::Line => {
                let (first, last) = (buffer.line_of(anchor), buffer.line_of(cursor));
                let selection = if last >= first {
                    Selection::new(buffer.line_start(first), buffer.line_end(last))
                } else {
                    Selection::new(buffer.line_end(first), buffer.line_start(last))
                };
                SelectionSet::single(selection)
            }
            VisualKind::Block => {
                let ranges = self.block_ranges();
                let cursor_line = buffer.line_of(cursor);
                let primary = ranges
                    .iter()
                    .position(|r| buffer.line_of(r.start) == cursor_line)
                    .unwrap_or(0);
                let items: Vec<Selection> = ranges
                    .iter()
                    .map(|r| Selection::new(r.start, r.end))
                    .collect();
                if items.is_empty() {
                    SelectionSet::single(Selection::cursor(cursor))
                } else {
                    SelectionSet::new(items, primary)
                }
            }
        };
        let primary = selections.primary();
        self.vim.shown = Some((primary.anchor, primary.head));
        self.set_selections(selections, cx);
    }

    /// Visual block: per line between anchor and cursor, the columns
    /// between them (lines too short are left out).
    fn block_ranges(&self) -> Vec<Range<usize>> {
        let buffer = &self.buffer;
        let (anchor, cursor) = (
            self.vim.anchor.min(buffer.len()),
            self.vim.cursor.min(buffer.len()),
        );
        let (a_line, c_line) = (buffer.line_of(anchor), buffer.line_of(cursor));
        let (a_col, c_col) = (buffer.column(anchor), buffer.column(cursor));
        let (left, right) = (a_col.min(c_col), a_col.max(c_col));
        (a_line.min(c_line)..=a_line.max(c_line))
            .filter_map(|line| {
                let start = buffer.offset_at(line, left);
                if buffer.column(start) < left || start == buffer.line_end(line) {
                    return None;
                }
                let end = if self.vim.to_line_end {
                    buffer.line_end(line)
                } else {
                    buffer
                        .next_grapheme(buffer.offset_at(line, right))
                        .min(buffer.line_end(line))
                };
                Some(start..end.max(start))
            })
            .collect()
    }

    /// Leaves visual mode, remembering the selection for `gv` and `'<`.
    pub(super) fn exit_visual(&mut self, cx: &mut Context<Self>) {
        let Mode::Visual(kind) = self.vim.mode else {
            return;
        };
        self.remember_visual(kind);
        self.vim.mode = Mode::Normal;
        self.vim.shown = None;
        let cursor = self.vim.cursor;
        self.place_cursor(cursor, cx);
    }

    fn remember_visual(&mut self, kind: VisualKind) {
        let (anchor, cursor) = (self.vim.anchor, self.vim.cursor);
        self.vim.last_visual = Some((anchor, cursor, kind));
        let (start, end) = (anchor.min(cursor), anchor.max(cursor));
        let (start, end) = if kind == VisualKind::Line {
            let buffer = &self.buffer;
            (
                buffer.line_start(buffer.line_of(start)),
                buffer.line_end(buffer.line_of(end)),
            )
        } else {
            (start, end)
        };
        self.vim.marks.insert('<', start);
        self.vim.marks.insert('>', end);
    }

    /// The visual selection as a span.
    fn visual_span(&self, lines: bool) -> Span {
        let kind = match self.vim.mode {
            Mode::Visual(kind) => kind,
            _ => VisualKind::Char,
        };
        let buffer = &self.buffer;
        let (anchor, cursor) = (
            self.vim.anchor.min(buffer.len()),
            self.vim.cursor.min(buffer.len()),
        );
        match kind {
            VisualKind::Line => Span::Lines(
                buffer.line_of(anchor.min(cursor)),
                buffer.line_of(anchor.max(cursor)),
            ),
            VisualKind::Char if lines => Span::Lines(
                buffer.line_of(anchor.min(cursor)),
                buffer.line_of(anchor.max(cursor)),
            ),
            VisualKind::Char => Span::Chars(
                self.selections
                    .all()
                    .iter()
                    .filter(|s| !s.is_empty())
                    .map(Selection::range)
                    .collect(),
            ),
            VisualKind::Block if lines => {
                let (first, last) = (buffer.line_of(anchor), buffer.line_of(cursor));
                Span::Lines(first.min(last), first.max(last))
            }
            VisualKind::Block => Span::Block(self.block_ranges()),
        }
    }

    fn visual_operate(
        &mut self,
        op: Operator,
        lines: bool,
        command: &Command,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Mode::Visual(kind) = self.vim.mode else {
            return;
        };
        let span = self.visual_span(lines);
        if matches!(&span, Span::Chars(r) | Span::Block(r) if r.is_empty()) {
            return self.exit_visual(cx);
        }
        self.remember_visual(kind);
        self.vim.mode = Mode::Normal;
        self.vim.shown = None;
        let shifts = if matches!(op, Operator::Indent | Operator::Outdent) {
            command.count()
        } else {
            1
        };
        self.apply(op, span, command.register, shifts, window, cx);
    }

    fn select_object(&mut self, object: Object, count: usize, cx: &mut Context<Self>) {
        let Some(found) = self.object(object, self.vim.cursor, count) else {
            return;
        };
        if found.range.is_empty() {
            return;
        }
        if found.linewise {
            self.vim.mode = Mode::Visual(VisualKind::Line);
        } else if self.vim.mode == Mode::Visual(VisualKind::Line) {
            self.vim.mode = Mode::Visual(VisualKind::Char);
        }
        self.vim.anchor = found.range.start;
        self.vim.cursor = self
            .buffer
            .prev_grapheme(found.range.end)
            .max(found.range.start);
        self.show_visual(cx);
    }

    fn do_cmd(
        &mut self,
        cmd: &Cmd,
        command: &Command,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let n = command.count();
        let register = command.register;
        let visual = matches!(self.vim.mode, Mode::Visual(_));
        let simple = |op, target| Command {
            register,
            count: command.count,
            action: Action::Operate(op, target),
        };
        if !visual
            && !matches!(
                cmd,
                Cmd::Insert(InsertAt::Cursor | InsertAt::After)
                    | Cmd::Undo
                    | Cmd::Redo
                    | Cmd::Repeat
                    | Cmd::Record(_)
                    | Cmd::PlayMacro(_)
            )
        {
            self.collapse_to_primary(cx);
        }
        match cmd {
            Cmd::DeleteChar { before } => {
                let target = Target::Motion(if *before { Motion::Left } else { Motion::Right });
                let command = simple(Operator::Delete, target.clone());
                self.operate(Operator::Delete, &target, &command, window, cx);
            }
            Cmd::Substitute { line } => {
                let target = if *line {
                    Target::Lines
                } else {
                    Target::Motion(Motion::Right)
                };
                let command = simple(Operator::Change, target.clone());
                if !*line
                    && self
                        .buffer
                        .line_end(self.buffer.line_of(self.primary().head))
                        == self.primary().head
                {
                    // `s` on an empty line or past the end just inserts.
                    self.start_insert(1);
                } else {
                    self.operate(Operator::Change, &target, &command, window, cx);
                }
            }
            Cmd::ToEnd { change } => {
                let op = if *change {
                    Operator::Change
                } else {
                    Operator::Delete
                };
                let head = self.primary().head;
                if self.buffer.line_end(self.buffer.line_of(head)) == head && command.count() == 1 {
                    if *change {
                        self.start_insert(1);
                    }
                    return;
                }
                let target = Target::Motion(Motion::LineEnd);
                let command = simple(op, target.clone());
                self.operate(op, &target, &command, window, cx);
            }
            Cmd::YankLine => {
                if visual {
                    return self.visual_operate(Operator::Yank, true, command, window, cx);
                }
                let command = simple(Operator::Yank, Target::Lines);
                self.operate(Operator::Yank, &Target::Lines, &command, window, cx);
            }
            Cmd::Put { before } => self.put(*before, n, register, cx),
            Cmd::Undo => {
                for _ in 0..n {
                    self.undo(&Undo, window, cx);
                }
                self.after_history(cx);
            }
            Cmd::Redo => {
                for _ in 0..n {
                    self.redo(&Redo, window, cx);
                }
                self.after_history(cx);
            }
            Cmd::Repeat => self.repeat_change(command.count, window, cx),
            Cmd::Join { spaces } => {
                let (first, last) = if visual {
                    let span = self.visual_span(true);
                    self.leave_visual_quietly();
                    let (first, last) = self.span_lines(&span);
                    (first, last.max(first + 1))
                } else {
                    let line = self.buffer.line_of(self.primary().head);
                    (line, line + n.max(2) - 1)
                };
                self.join_lines(first, last, *spaces, cx);
            }
            Cmd::ReplaceChar(c) => {
                if visual {
                    self.replace_selection_chars(*c, cx);
                } else {
                    self.replace_chars(*c, n, cx);
                }
            }
            Cmd::ToggleCaseChar => self.toggle_case_chars(n, cx),
            Cmd::Insert(at) => {
                if visual {
                    self.leave_visual_quietly();
                }
                self.insert_at(*at, n, cx);
            }
            Cmd::ReplaceMode => {
                self.start_insert(1);
                self.vim.mode = Mode::Replace;
            }
            Cmd::Visual(kind) => match self.vim.mode {
                Mode::Visual(current) if current == *kind => self.exit_visual(cx),
                Mode::Visual(_) => {
                    self.vim.mode = Mode::Visual(*kind);
                    self.vim.to_line_end = false;
                    self.show_visual(cx);
                }
                _ => self.enter_visual(*kind, cx),
            },
            Cmd::Reselect => {
                let Some((anchor, cursor, kind)) = self.vim.last_visual else {
                    return;
                };
                if let Mode::Visual(current) = self.vim.mode {
                    self.remember_visual(current);
                }
                let len = self.buffer.len();
                self.vim.mode = Mode::Visual(kind);
                self.vim.anchor = anchor.min(len);
                self.vim.cursor = cursor.min(len);
                self.vim.to_line_end = false;
                self.show_visual(cx);
            }
            Cmd::Increment(delta) => {
                if visual {
                    self.exit_visual(cx);
                }
                self.increment(*delta * n as i64, cx);
            }
            Cmd::SetMark(c) => {
                let cursor = self.vim_cursor();
                self.vim.marks.insert(*c, cursor);
            }
            Cmd::Record(register) => {
                let shared = cx.default_global::<Shared>();
                match register {
                    Some(register) => shared.recording = Some((*register, Vec::new())),
                    None => {
                        if let Some((register, mut keys)) = shared.recording.take() {
                            // The `q` that stopped it.
                            keys.pop();
                            shared.macros.insert(register, keys);
                        }
                    }
                }
            }
            Cmd::PlayMacro(register) => self.play_macro(*register, n, window, cx),
            Cmd::Ex => {
                let text = if visual {
                    self.exit_visual(cx);
                    "'<,'>".to_owned()
                } else if n > 1 {
                    format!(".,.+{}", n - 1)
                } else {
                    String::new()
                };
                self.open_ex(ExKind::Command, text, cx);
            }
            Cmd::Scroll(scroll) => self.scroll_view(*scroll, n, cx),
            Cmd::Fold(fold) => {
                if visual {
                    self.exit_visual(cx);
                }
                match fold {
                    FoldCommand::Close => self.fold(&Fold, window, cx),
                    FoldCommand::Open => self.unfold(&Unfold, window, cx),
                    FoldCommand::Toggle => self.toggle_fold(&ToggleFold, window, cx),
                    FoldCommand::CloseAll => self.fold_all(&FoldAll, window, cx),
                    FoldCommand::OpenAll => self.unfold_all(&UnfoldAll, window, cx),
                }
            }
            Cmd::GoToDefinition => {
                if visual {
                    self.exit_visual(cx);
                }
                self.go_to_definition(&GoToDefinition, window, cx);
            }
            Cmd::SwapEnds => {
                std::mem::swap(&mut self.vim.anchor, &mut self.vim.cursor);
                self.show_visual(cx);
            }
            Cmd::BlockInsert { append } => self.block_insert(*append, cx),
        }
    }

    /// Visual mode ends without moving the cursor (the command that ends
    /// it places it).
    fn leave_visual_quietly(&mut self) {
        if let Mode::Visual(kind) = self.vim.mode {
            self.remember_visual(kind);
            self.vim.mode = Mode::Normal;
            self.vim.shown = None;
            let cursor = self.vim.cursor.min(self.buffer.len());
            self.selections = SelectionSet::single(Selection::cursor(cursor));
        }
    }

    /// After undo/redo: normal mode with a cursor at the change.
    fn after_history(&mut self, cx: &mut Context<Self>) {
        self.vim.mode = Mode::Normal;
        self.vim.shown = None;
        let start = self.primary().start();
        self.place_cursor(start, cx);
    }

    fn repeat_change(&mut self, count: Option<usize>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(change) = self.vim.last_change.clone() else {
            return;
        };
        if self.vim.replaying > 100 {
            return;
        }
        self.vim.replaying += 1;
        let mut command = change.command.clone();
        if count.is_some() {
            command.count = count;
        }
        if let Some(extent) = change.visual {
            self.select_extent(extent, cx);
        }
        self.run_command(command, window, cx);
        match self.vim.mode {
            Mode::Insert => {
                if let Some(inserted) = &change.inserted {
                    self.replay_inserted(inserted, cx);
                }
                self.leave_insert(window, cx);
            }
            Mode::Replace => {
                for c in change.inserted.iter().flat_map(|i| i.text.chars()) {
                    self.replace_typed(c, cx);
                }
                self.leave_insert(window, cx);
            }
            _ => {}
        }
        self.vim.replaying -= 1;
    }

    fn play_macro(
        &mut self,
        register: char,
        count: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let shared = cx.default_global::<Shared>();
        let register = if register == '@' {
            match shared.last_macro {
                Some(register) => register,
                None => return,
            }
        } else {
            register
        };
        let Some(keys) = shared.macros.get(&register).cloned() else {
            return;
        };
        shared.last_macro = Some(register);
        if self.vim.replaying > 100 {
            return;
        }
        self.vim.replaying += 1;
        for _ in 0..count {
            for key in &keys {
                self.feed_key(key.clone(), window, cx);
            }
        }
        self.vim.replaying -= 1;
    }

    /// A key as if typed: Vim's, else the editor's own handling (text in
    /// insert mode).
    pub(super) fn feed_key(&mut self, key: Key, window: &mut Window, cx: &mut Context<Self>) {
        if self.vim_key(key.clone(), window, cx) {
            return;
        }
        match key {
            Key::Char(c) => self.type_text(&c.to_string(), cx),
            Key::Enter => self.newline(&Newline, window, cx),
            Key::Backspace => self.backspace(&Backspace, window, cx),
            Key::Delete => self.delete(&Delete, window, cx),
            Key::Tab => self.tab(&Tab, window, cx),
            Key::Left => self.left(&Left, window, cx),
            Key::Right => self.right(&Right, window, cx),
            Key::Up => self.up(&Up, window, cx),
            Key::Down => self.down(&Down, window, cx),
            Key::Home => self.home(&Home, window, cx),
            Key::End => self.end(&End, window, cx),
            Key::Esc if self.completion.is_some() => {
                self.close_completions(&CloseCompletions, window, cx);
            }
            _ => {}
        }
    }

    pub(super) fn join_lines(
        &mut self,
        first: usize,
        last: usize,
        spaces: bool,
        cx: &mut Context<Self>,
    ) {
        let last = last.min(self.buffer.line_count() - 1);
        if last <= first {
            return;
        }
        let buffer = &self.buffer;
        let mut edits = Vec::new();
        for line in first..last {
            let end = buffer.line_end(line);
            let next = line + 1;
            if spaces {
                let next_text_start = motion::first_non_blank(buffer, next);
                let next_empty = next_text_start == buffer.line_end(next);
                let trailing_blank = buffer
                    .chars_before(end)
                    .next()
                    .is_some_and(|(o, c)| o >= buffer.line_start(line) && (c == ' ' || c == '\t'));
                let closer = self.char_at(next_text_start) == Some(')');
                let line_empty = end == buffer.line_start(line);
                let space = if next_empty || trailing_blank || closer || line_empty {
                    ""
                } else {
                    " "
                };
                edits.push((end..next_text_start, space.to_owned()));
            } else {
                edits.push((end..buffer.line_start(next), String::new()));
            }
        }
        // The cursor goes to the last join.
        let shift: isize = edits[..edits.len() - 1]
            .iter()
            .map(|(r, t)| t.len() as isize - r.len() as isize)
            .sum();
        let cursor = edits[edits.len() - 1].0.start.saturating_add_signed(shift);
        self.commit(
            edits,
            |_| SelectionSet::single(Selection::cursor(cursor)),
            cx,
        );
        self.place_cursor(cursor, cx);
    }

    fn replace_chars(&mut self, c: char, n: usize, cx: &mut Context<Self>) {
        let head = self.primary().head;
        let line_end = self.buffer.line_end(self.buffer.line_of(head));
        let mut end = head;
        for _ in 0..n {
            if end >= line_end {
                return;
            }
            end = self.buffer.next_grapheme(end);
        }
        let (text, cursor) = if c == '\n' {
            let ending = self.buffer.line_ending().to_owned();
            let cursor = head + ending.len();
            (ending, cursor)
        } else {
            let text = c.to_string().repeat(n);
            let cursor = head + text.len() - c.len_utf8();
            (text, cursor)
        };
        self.commit(
            vec![(head..end, text)],
            |_| SelectionSet::single(Selection::cursor(cursor)),
            cx,
        );
        self.place_cursor(cursor, cx);
    }

    fn replace_selection_chars(&mut self, c: char, cx: &mut Context<Self>) {
        let span = self.visual_span(false);
        let start = self.span_ranges(&span).first().map_or(0, |r| r.start);
        self.leave_visual_quietly();
        let edits = self
            .span_ranges(&span)
            .into_iter()
            .map(|range| {
                let text: String = self
                    .buffer
                    .slice(range.clone())
                    .chars()
                    .map(|old| if old == '\n' || old == '\r' { old } else { c })
                    .collect();
                (range, text)
            })
            .collect();
        self.commit(
            edits,
            |_| SelectionSet::single(Selection::cursor(start)),
            cx,
        );
        self.place_cursor(start, cx);
    }

    fn toggle_case_chars(&mut self, n: usize, cx: &mut Context<Self>) {
        let head = self.primary().head;
        let line_end = self.buffer.line_end(self.buffer.line_of(head));
        let mut end = head;
        for _ in 0..n {
            if end >= line_end {
                break;
            }
            end = self.buffer.next_grapheme(end);
        }
        if end == head {
            return;
        }
        let text = change_case(&self.buffer.slice(head..end), Operator::ToggleCase);
        let cursor = head + text.len();
        self.commit(
            vec![(head..end, text)],
            |_| SelectionSet::single(Selection::cursor(cursor)),
            cx,
        );
        self.place_cursor(cursor, cx);
    }

    /// Ctrl+A / Ctrl+X: adds `delta` to the number at or after the cursor
    /// on its line (decimal, or hex after `0x`).
    fn increment(&mut self, delta: i64, cx: &mut Context<Self>) {
        let head = self.primary().head;
        let line = self.buffer.line_of(head);
        let line_start = self.buffer.line_start(line);
        let text = self.buffer.line_text(line);
        let Some((range, replacement)) = increment_in(&text, head - line_start, delta) else {
            return;
        };
        let start = line_start + range.start;
        let cursor = start + replacement.len() - 1;
        self.commit(
            vec![(start..line_start + range.end, replacement)],
            |_| SelectionSet::single(Selection::cursor(cursor)),
            cx,
        );
        self.place_cursor(cursor, cx);
    }

    fn put(&mut self, before: bool, n: usize, register: Option<char>, cx: &mut Context<Self>) {
        let Some(value) = self.read_register(register, cx) else {
            return;
        };
        if let Mode::Visual(_) = self.vim.mode {
            return self.put_over_selection(value, n, cx);
        }
        let head = self.primary().head;
        let line = self.buffer.line_of(head);
        let ending = self.buffer.line_ending().to_owned();
        match value.kind {
            RegisterKind::Chars => {
                let text = self.normalize_line_endings(&value.text.repeat(n.max(1)));
                if text.is_empty() {
                    return;
                }
                let at = if before || head == self.buffer.line_end(line) {
                    head
                } else {
                    self.buffer.next_grapheme(head)
                };
                let cursor = if text.contains('\n') {
                    at
                } else {
                    at + text.len() - text.chars().next_back().map_or(0, char::len_utf8)
                };
                self.commit(
                    vec![(at..at, text)],
                    |_| SelectionSet::single(Selection::cursor(cursor)),
                    cx,
                );
                self.place_cursor(cursor, cx);
            }
            RegisterKind::Lines => {
                let text = self.normalize_line_endings(&value.text.repeat(n.max(1)));
                let (at, text, first_line) = if before {
                    (self.buffer.line_start(line), text, line)
                } else if line + 1 < self.buffer.line_count() {
                    (self.buffer.line_start(line + 1), text, line + 1)
                } else {
                    // After the last line, which has no line break.
                    let body = text.strip_suffix(ending.as_str()).unwrap_or(&text);
                    (
                        self.buffer.line_end(line),
                        format!("{ending}{body}"),
                        line + 1,
                    )
                };
                self.commit(
                    vec![(at..at, text)],
                    |_| SelectionSet::single(Selection::cursor(at)),
                    cx,
                );
                self.place_cursor(motion::first_non_blank(&self.buffer, first_line), cx);
            }
            RegisterKind::Block => {
                let column = if before || head == self.buffer.line_end(line) {
                    self.buffer.column(head)
                } else {
                    self.buffer.column(head) + 1
                };
                let rows: Vec<&str> = value.text.split('\n').collect();
                let mut edits = Vec::new();
                for (i, row) in rows.iter().enumerate() {
                    let target = line + i;
                    let piece = row.repeat(n.max(1));
                    if target >= self.buffer.line_count() {
                        let end = self.buffer.len();
                        let padding = " ".repeat(column);
                        edits.push((end..end, format!("{ending}{padding}{piece}")));
                        continue;
                    }
                    let line_len = self.buffer.column(self.buffer.line_end(target));
                    let at = self.buffer.offset_at(target, column);
                    let padding = " ".repeat(column.saturating_sub(line_len));
                    edits.push((at..at, format!("{padding}{piece}")));
                }
                // Several rows past the end insert at the same offset; one edit.
                let mut merged: Vec<(Range<usize>, String)> = Vec::new();
                for (range, text) in edits {
                    match merged.last_mut() {
                        Some((last, last_text)) if *last == range => last_text.push_str(&text),
                        _ => merged.push((range, text)),
                    }
                }
                let cursor = self.buffer.offset_at(line, column);
                self.commit(
                    merged,
                    |_| SelectionSet::single(Selection::cursor(cursor)),
                    cx,
                );
                self.place_cursor(cursor, cx);
            }
        }
    }

    /// Visual `p`: the selection is replaced (and goes to the unnamed
    /// register).
    fn put_over_selection(&mut self, value: Register, n: usize, cx: &mut Context<Self>) {
        let span = self.visual_span(false);
        let (old, old_kind) = self.span_text(&span);
        let Mode::Visual(kind) = self.vim.mode else {
            return;
        };
        self.remember_visual(kind);
        self.vim.mode = Mode::Normal;
        self.vim.shown = None;
        let ending = self.buffer.line_ending().to_owned();
        let text = self.normalize_line_endings(&value.text.repeat(n.max(1)));
        let (range, text) = match (&span, value.kind) {
            (Span::Lines(first, last), RegisterKind::Lines) => (
                self.buffer.line_start(*first)..self.buffer.line_end(*last),
                text.strip_suffix(ending.as_str())
                    .unwrap_or(&text)
                    .to_owned(),
            ),
            (Span::Lines(first, last), _) => (
                self.buffer.line_start(*first)..self.buffer.line_end(*last),
                text,
            ),
            (_, RegisterKind::Lines) => {
                let range = self.span_ranges(&span).first().cloned().unwrap_or(0..0);
                (range, format!("{ending}{text}"))
            }
            _ => (
                self.span_ranges(&span).first().cloned().unwrap_or(0..0),
                text,
            ),
        };
        let start = range.start;
        // On the last character put, or a pasted line's first non-blank.
        let cursor = if text.contains('\n') {
            let line = self.buffer.line_of(start) + usize::from(text.starts_with(['\r', '\n']));
            Err(line)
        } else {
            Ok(start + text.len() - text.chars().next_back().map_or(0, char::len_utf8))
        };
        self.commit(
            vec![(range, text)],
            |_| SelectionSet::single(Selection::cursor(start)),
            cx,
        );
        let cursor = cursor.unwrap_or_else(|line| motion::first_non_blank(&self.buffer, line));
        self.place_cursor(cursor, cx);
        self.store_register(None, Operator::Delete, old, old_kind, cx);
    }

    fn block_insert(&mut self, append: bool, cx: &mut Context<Self>) {
        let Mode::Visual(kind) = self.vim.mode else {
            return;
        };
        let buffer = &self.buffer;
        let (anchor, cursor) = (
            self.vim.anchor.min(buffer.len()),
            self.vim.cursor.min(buffer.len()),
        );
        let selections = match kind {
            VisualKind::Block => {
                let (a_col, c_col) = (buffer.column(anchor), buffer.column(cursor));
                let column = if append {
                    a_col.max(c_col) + 1
                } else {
                    a_col.min(c_col)
                };
                let (first, last) = (
                    buffer.line_of(anchor.min(cursor)),
                    buffer.line_of(anchor.max(cursor)),
                );
                let to_end = self.vim.to_line_end && append;
                let items: Vec<Selection> = (first..=last)
                    .filter_map(|line| {
                        let offset = if to_end {
                            buffer.line_end(line)
                        } else {
                            buffer.offset_at(line, column)
                        };
                        (to_end || append || buffer.column(offset) == column)
                            .then(|| Selection::cursor(offset))
                    })
                    .collect();
                if items.is_empty() {
                    SelectionSet::single(Selection::cursor(anchor.min(cursor)))
                } else {
                    SelectionSet::new(items, 0)
                }
            }
            _ => {
                let offset = if append {
                    buffer
                        .next_grapheme(anchor.max(cursor))
                        .min(buffer.line_end(buffer.line_of(anchor.max(cursor))))
                } else {
                    anchor.min(cursor)
                };
                SelectionSet::single(Selection::cursor(offset))
            }
        };
        self.remember_visual(kind);
        self.vim.mode = Mode::Normal;
        self.vim.shown = None;
        self.set_selections(selections, cx);
        self.start_insert(1);
    }

    fn scroll_view(&mut self, scroll: Scroll, n: usize, cx: &mut Context<Self>) {
        let head = self.vim_cursor();
        let line = self.buffer.line_of(head);
        match scroll {
            Scroll::Center => self.center_cursor = true,
            Scroll::Top => self.pending_scroll_line = Some(line),
            Scroll::Bottom => {
                let rows = self.page_rows().max(1) as usize;
                self.pending_scroll_line = Some(line.saturating_sub(rows - 1));
            }
            Scroll::LineDown | Scroll::LineUp => {
                let Some(layout) = &self.layout else {
                    return;
                };
                let rows = n as f32 * if scroll == Scroll::LineDown { 1. } else { -1. };
                self.scroll.y = (self.scroll.y + layout.line_height * rows).max(px(0.));
                // Keep the cursor on screen.
                let (first, last) = self.screen_lines();
                let first = if scroll == Scroll::LineDown {
                    first + n
                } else {
                    first
                };
                let last = if scroll == Scroll::LineUp {
                    last.saturating_sub(n)
                } else {
                    last
                };
                if line < first || line > last {
                    let target = line
                        .clamp(first, last.max(first))
                        .min(self.buffer.line_count() - 1);
                    let column = self.buffer.column(head);
                    let offset = self.column_offset(target, column, false);
                    self.selections = SelectionSet::single(Selection::cursor(offset));
                }
            }
        }
        cx.notify();
    }

    /// Stores yanked or deleted text as Vim does: the named register (or
    /// `"0` for yanks, `"1`-`"9` for multi-line deletes, `"-` for small
    /// ones) and the unnamed one; `"+`/`"*` go to the clipboard.
    fn store_register(
        &mut self,
        register: Option<char>,
        op: Operator,
        text: String,
        kind: RegisterKind,
        cx: &mut Context<Self>,
    ) {
        if register == Some('_') {
            return;
        }
        let value = Register { text, kind };
        if matches!(register, Some('+' | '*')) {
            cx.write_to_clipboard(ClipboardItem::new_string(value.text.clone()));
        }
        let shared = cx.default_global::<Shared>();
        match register {
            Some(c) if c.is_ascii_uppercase() => {
                let lower = c.to_ascii_lowercase();
                let entry = shared.registers.entry(lower).or_insert_with(|| Register {
                    text: String::new(),
                    kind: value.kind,
                });
                if entry.kind == RegisterKind::Lines
                    && !entry.text.is_empty()
                    && !entry.text.ends_with('\n')
                {
                    entry.text.push('\n');
                }
                entry.text.push_str(&value.text);
                if value.kind == RegisterKind::Lines {
                    entry.kind = RegisterKind::Lines;
                }
                let appended = entry.clone();
                shared.registers.insert('"', appended);
                return;
            }
            Some(c) if c.is_ascii_alphanumeric() => {
                shared.registers.insert(c, value.clone());
            }
            _ => {}
        }
        if register.is_none() {
            if op == Operator::Yank {
                shared.registers.insert('0', value.clone());
            } else if value.kind == RegisterKind::Lines || value.text.contains('\n') {
                for n in (1..9).rev() {
                    let from = char::from_digit(n, 10).expect("digit");
                    let to = char::from_digit(n + 1, 10).expect("digit");
                    if let Some(old) = shared.registers.remove(&from) {
                        shared.registers.insert(to, old);
                    }
                }
                shared.registers.insert('1', value.clone());
            } else {
                shared.registers.insert('-', value.clone());
            }
        }
        shared.registers.insert('"', value);
    }

    fn read_register(
        &mut self,
        register: Option<char>,
        cx: &mut Context<Self>,
    ) -> Option<Register> {
        match register {
            Some('+' | '*') => {
                let text = cx.read_from_clipboard()?.text()?.replace("\r\n", "\n");
                let kind = if text.ends_with('\n') {
                    RegisterKind::Lines
                } else {
                    RegisterKind::Chars
                };
                Some(Register { text, kind })
            }
            Some('_') => None,
            Some(':') => {
                let text = cx.default_global::<Shared>().last_ex.clone()?;
                Some(Register {
                    text,
                    kind: RegisterKind::Chars,
                })
            }
            Some('/') => {
                let text = cx
                    .default_global::<Shared>()
                    .search
                    .as_ref()?
                    .pattern
                    .clone();
                Some(Register {
                    text,
                    kind: RegisterKind::Chars,
                })
            }
            Some(c) => cx
                .default_global::<Shared>()
                .registers
                .get(&c.to_ascii_lowercase())
                .cloned(),
            None => cx.default_global::<Shared>().registers.get(&'"').cloned(),
        }
    }
}

/// Whether `command` changes the text (and so `.` repeats it).
fn is_change(command: &Command) -> bool {
    match &command.action {
        Action::Operate(op, _) | Action::VisualOperate { op, .. } => op.edits(),
        Action::Do(cmd) => matches!(
            cmd,
            Cmd::DeleteChar { .. }
                | Cmd::Substitute { .. }
                | Cmd::ToEnd { .. }
                | Cmd::Put { .. }
                | Cmd::Join { .. }
                | Cmd::ReplaceChar(_)
                | Cmd::ToggleCaseChar
                | Cmd::Insert(_)
                | Cmd::ReplaceMode
                | Cmd::Increment(_)
                | Cmd::BlockInsert { .. }
        ),
        Action::Move(_) | Action::Select(_) => false,
    }
}

pub(super) fn change_case(text: &str, op: Operator) -> String {
    match op {
        Operator::Lower => text.to_lowercase(),
        Operator::Upper => text.to_uppercase(),
        _ => text
            .chars()
            .flat_map(|c| {
                let flipped: Vec<char> = if c.is_uppercase() {
                    c.to_lowercase().collect()
                } else {
                    c.to_uppercase().collect()
                };
                flipped
            })
            .collect(),
    }
}

/// The number in `line` at or after byte `column`, plus `delta`: its
/// range and new text.
pub(super) fn increment_in(
    line: &str,
    column: usize,
    delta: i64,
) -> Option<(Range<usize>, String)> {
    let bytes = line.as_bytes();
    let column = column.min(bytes.len());
    let hex = |digits_start: usize| -> Option<(Range<usize>, String)> {
        let digits_end = digits_start
            + bytes[digits_start..]
                .iter()
                .take_while(|b| b.is_ascii_hexdigit())
                .count();
        let digits = &line[digits_start..digits_end];
        let value = i64::from_str_radix(digits, 16).ok()?;
        let new = value.saturating_add(delta).max(0);
        let width = digits.len();
        let text = if digits.chars().any(|c| c.is_ascii_uppercase()) {
            format!("{new:0width$X}")
        } else {
            format!("{new:0width$x}")
        };
        Some((digits_start..digits_end, text))
    };
    let is_hex_prefix = |at: usize| {
        bytes.get(at) == Some(&b'0')
            && matches!(bytes.get(at + 1), Some(b'x' | b'X'))
            && bytes.get(at + 2).is_some_and(u8::is_ascii_hexdigit)
    };
    // The cursor inside a hex literal (on its digits or its `0x`).
    let mut start = column;
    while start > 0 && bytes[start - 1].is_ascii_hexdigit() {
        start -= 1;
    }
    if start >= 2 && is_hex_prefix(start - 2) && column >= start - 2 {
        return hex(start);
    }
    if column >= 1 && is_hex_prefix(column - 1) {
        return hex(column + 1);
    }
    // Else the number under the cursor, or the first one after it.
    let mut start = column;
    if bytes.get(column).is_some_and(u8::is_ascii_digit) {
        while start > 0 && bytes[start - 1].is_ascii_digit() {
            start -= 1;
        }
    }
    let first = start + bytes[start..].iter().position(u8::is_ascii_digit)?;
    if is_hex_prefix(first) {
        return hex(first + 2);
    }
    let end = first
        + bytes[first..]
            .iter()
            .take_while(|b| b.is_ascii_digit())
            .count();
    let negative = first > 0 && bytes[first - 1] == b'-';
    let begin = if negative { first - 1 } else { first };
    let value: i64 = line[begin..end].parse().ok()?;
    let new = value.saturating_add(delta);
    Some((begin..end, new.to_string()))
}
