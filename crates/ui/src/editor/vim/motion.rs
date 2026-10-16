//! Motions and text objects over a buffer, as Vim defines them. Pure
//! functions of the text: offsets in, offsets out.

use std::ops::Range;

use text::Buffer;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Class {
    Blank,
    Word,
    Punct,
}

/// Vim's character classes; with `big` (W, B, E) only blank vs not.
pub(crate) fn class(c: char, big: bool) -> Class {
    if c.is_whitespace() {
        Class::Blank
    } else if big || c.is_alphanumeric() || c == '_' {
        Class::Word
    } else {
        Class::Punct
    }
}

/// Whether `line` has no characters at all.
pub(crate) fn is_empty_line(buffer: &Buffer, line: usize) -> bool {
    buffer.line_start(line) == buffer.line_end(line)
}

fn is_blank_line(buffer: &Buffer, line: usize) -> bool {
    buffer
        .chars_from(buffer.line_start(line))
        .take_while(|&(_, c)| c != '\n')
        .all(|(_, c)| c.is_whitespace())
}

/// Offset of `line`'s first non-blank character (its end when blank).
pub(crate) fn first_non_blank(buffer: &Buffer, line: usize) -> usize {
    let end = buffer.line_end(line);
    buffer
        .chars_from(buffer.line_start(line))
        .take_while(|&(offset, _)| offset < end)
        .find(|&(_, c)| c != ' ' && c != '\t')
        .map_or(end, |(offset, _)| offset)
}

/// Offset of `line`'s last character (its start when empty): where the
/// normal-mode cursor sits at the end of a line.
pub(crate) fn last_char(buffer: &Buffer, line: usize) -> usize {
    let start = buffer.line_start(line);
    let end = buffer.line_end(line);
    if end == start {
        start
    } else {
        buffer.prev_grapheme(end).max(start)
    }
}

/// The empty line starting at the line break at `offset`, if `offset` is
/// the break of an empty line.
fn empty_line_at(buffer: &Buffer, offset: usize) -> Option<usize> {
    let line = buffer.line_of(offset);
    is_empty_line(buffer, line).then(|| buffer.line_start(line))
}

/// `w`: start of the next word; stops at empty lines.
pub(crate) fn next_word_start(buffer: &Buffer, pos: usize, big: bool) -> usize {
    let mut chars = buffer.chars_from(pos);
    let Some((_, first)) = chars.next() else {
        return buffer.len();
    };
    let start_class = class(first, big);
    let mut in_first_run = start_class != Class::Blank;
    let mut prev = first;
    for (offset, c) in chars {
        if c == '\r' {
            continue;
        }
        let k = class(c, big);
        if in_first_run {
            if k == start_class {
                prev = c;
                continue;
            }
            in_first_run = false;
        }
        if k != Class::Blank {
            return offset;
        }
        if c == '\n'
            && prev == '\n'
            && let Some(start) = empty_line_at(buffer, offset)
        {
            return start;
        }
        prev = c;
    }
    buffer.len()
}

/// `e`: end of the current or next word (inclusive).
pub(crate) fn word_end(buffer: &Buffer, pos: usize, big: bool) -> usize {
    let mut chars = buffer
        .chars_from(pos)
        .skip(1)
        .skip_while(|&(_, c)| c.is_whitespace());
    let Some((mut last, first)) = chars.next() else {
        return last_offset(buffer);
    };
    let k = class(first, big);
    for (offset, c) in chars {
        if class(c, big) != k {
            break;
        }
        last = offset;
    }
    last
}

/// `b`: start of the current or previous word; stops at empty lines.
pub(crate) fn word_back(buffer: &Buffer, pos: usize, big: bool) -> usize {
    let mut chars = buffer.chars_before(pos).peekable();
    // Blanks before the cursor, stopping at an empty line.
    while let Some(&(offset, c)) = chars.peek() {
        if !c.is_whitespace() {
            break;
        }
        if c == '\n' {
            let line = buffer.line_of(offset);
            if is_empty_line(buffer, line) && buffer.line_start(line) < pos {
                return buffer.line_start(line);
            }
        }
        chars.next();
    }
    let Some((mut start, first)) = chars.next() else {
        return 0;
    };
    let k = class(first, big);
    for (offset, c) in chars {
        if class(c, big) != k {
            break;
        }
        start = offset;
    }
    start
}

/// `ge`: end of the previous word (inclusive); stops at empty lines.
pub(crate) fn word_end_back(buffer: &Buffer, pos: usize, big: bool) -> usize {
    let current = buffer.chars_from(pos).next().map(|(_, c)| class(c, big));
    let mut chars = buffer.chars_before(pos).peekable();
    if let Some(k) = current.filter(|&k| k != Class::Blank) {
        while chars.peek().is_some_and(|&(_, c)| class(c, big) == k) {
            chars.next();
        }
    }
    for (offset, c) in chars {
        if !c.is_whitespace() {
            return offset;
        }
        if c == '\n' {
            let line = buffer.line_of(offset);
            if is_empty_line(buffer, line) && buffer.line_start(line) < pos {
                return buffer.line_start(line);
            }
        }
    }
    0
}

/// The last character's offset (0 for an empty buffer).
pub(crate) fn last_offset(buffer: &Buffer) -> usize {
    buffer.prev_grapheme(buffer.len())
}

/// f, F, t, T: the `count`th `ch` on the line; `skip_adjacent` (a repeated
/// t/T) looks past the character right next to the cursor.
pub(crate) fn find_in_line(
    buffer: &Buffer,
    pos: usize,
    ch: char,
    forward: bool,
    till: bool,
    count: usize,
    skip_adjacent: bool,
) -> Option<usize> {
    let line = buffer.line_of(pos);
    let (start, end) = (buffer.line_start(line), buffer.line_end(line));
    let mut found = None;
    let mut remaining = count;
    if forward {
        let from = buffer.next_grapheme(pos).min(end);
        let from = if till
            && skip_adjacent
            && buffer.chars_from(from).next().map(|(_, c)| c) == Some(ch)
        {
            buffer.next_grapheme(from).min(end)
        } else {
            from
        };
        for (offset, c) in buffer.chars_from(from).take_while(|&(o, _)| o < end) {
            if c == ch {
                remaining -= 1;
                if remaining == 0 {
                    found = Some(offset);
                    break;
                }
            }
        }
        let offset = found?;
        Some(if till {
            buffer.prev_grapheme(offset)
        } else {
            offset
        })
    } else {
        let from =
            if till && skip_adjacent && buffer.chars_before(pos).next().map(|(_, c)| c) == Some(ch)
            {
                buffer.prev_grapheme(pos).max(start)
            } else {
                pos
            };
        for (offset, c) in buffer.chars_before(from).take_while(|&(o, _)| o >= start) {
            if c == ch {
                remaining -= 1;
                if remaining == 0 {
                    found = Some(offset);
                    break;
                }
            }
        }
        let offset = found?;
        Some(if till {
            buffer.next_grapheme(offset)
        } else {
            offset
        })
    }
}

const PAIRS: [(char, char); 3] = [('(', ')'), ('[', ']'), ('{', '}')];

/// `%`: the partner of the first bracket at or after the cursor on its line.
pub(crate) fn match_pair(buffer: &Buffer, pos: usize) -> Option<usize> {
    let end = buffer.line_end(buffer.line_of(pos));
    let (at, c) = buffer
        .chars_from(pos)
        .take_while(|&(o, _)| o < end)
        .find(|&(_, c)| PAIRS.iter().any(|&(open, close)| c == open || c == close))?;
    if let Some(&(open, close)) = PAIRS.iter().find(|&&(open, _)| open == c) {
        matching_close(buffer, at + open.len_utf8(), open, close)
    } else {
        let &(open, close) = PAIRS.iter().find(|&&(_, close)| close == c)?;
        matching_open(buffer, at, open, close)
    }
}

/// The unmatched `close` after `from`.
fn matching_close(buffer: &Buffer, from: usize, open: char, close: char) -> Option<usize> {
    let mut depth = 0usize;
    for (offset, c) in buffer.chars_from(from) {
        if c == open {
            depth += 1;
        } else if c == close {
            if depth == 0 {
                return Some(offset);
            }
            depth -= 1;
        }
    }
    None
}

/// The unmatched `open` before `before`.
fn matching_open(buffer: &Buffer, before: usize, open: char, close: char) -> Option<usize> {
    let mut depth = 0usize;
    for (offset, c) in buffer.chars_before(before) {
        if c == close {
            depth += 1;
        } else if c == open {
            if depth == 0 {
                return Some(offset);
            }
            depth -= 1;
        }
    }
    None
}

/// `}`: the next empty line after a run of non-empty ones (the last
/// character of the buffer when there is none).
pub(crate) fn paragraph_forward(buffer: &Buffer, pos: usize, count: usize) -> usize {
    let lines = buffer.line_count();
    let mut line = buffer.line_of(pos);
    for _ in 0..count {
        while line < lines && is_empty_line(buffer, line) {
            line += 1;
        }
        while line < lines && !is_empty_line(buffer, line) {
            line += 1;
        }
        if line >= lines {
            return buffer.len();
        }
    }
    buffer.line_start(line)
}

/// `{`: the previous empty line before a run of non-empty ones (the
/// buffer start when there is none).
pub(crate) fn paragraph_back(buffer: &Buffer, pos: usize, count: usize) -> usize {
    let mut line = buffer.line_of(pos) as isize;
    for _ in 0..count {
        while line >= 0 && is_empty_line(buffer, line as usize) {
            line -= 1;
        }
        while line >= 0 && !is_empty_line(buffer, line as usize) {
            line -= 1;
        }
        if line < 0 {
            return 0;
        }
    }
    buffer.line_start(line as usize)
}

/// A text object's extent; `linewise` objects cover whole lines
/// (`range` from the first line's start to the last line's end).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ObjectRange {
    pub range: Range<usize>,
    pub linewise: bool,
}

impl ObjectRange {
    fn chars(range: Range<usize>) -> Self {
        Self {
            range,
            linewise: false,
        }
    }
}

/// iw, aw, iW, aW: runs of the cursor's line.
pub(crate) fn word_object(
    buffer: &Buffer,
    pos: usize,
    big: bool,
    inner: bool,
    count: usize,
) -> Option<ObjectRange> {
    let line = buffer.line_of(pos);
    let line_start = buffer.line_start(line);
    let line_end = buffer.line_end(line);
    let chars: Vec<(usize, char)> = buffer
        .chars_from(line_start)
        .take_while(|&(o, _)| o < line_end)
        .collect();
    if chars.is_empty() {
        return None;
    }
    let index = chars.iter().rposition(|&(o, _)| o <= pos).unwrap_or(0);
    let kind = |i: usize| class(chars[i].1, big);
    // The run of one class around `i`: [start, end).
    let run = |i: usize| {
        let k = kind(i);
        let mut start = i;
        while start > 0 && kind(start - 1) == k {
            start -= 1;
        }
        let mut end = i + 1;
        while end < chars.len() && kind(end) == k {
            end += 1;
        }
        (start, end)
    };
    let (mut start, mut end) = run(index);
    if inner {
        for _ in 1..count {
            if end >= chars.len() {
                break;
            }
            end = run(end).1;
        }
    } else {
        let on_blank = kind(index) == Class::Blank;
        for n in 0..count {
            if n > 0 {
                if end >= chars.len() {
                    break;
                }
                end = run(end).1;
            }
            if on_blank {
                // Blanks, then the word after them.
                if n == 0 && end < chars.len() {
                    end = run(end).1;
                }
            } else if end < chars.len() && kind(end) == Class::Blank {
                end = run(end).1;
            } else if n == 0 && start > 0 && kind(start - 1) == Class::Blank {
                start = run(start - 1).0;
            }
        }
    }
    let byte_end = |i: usize| chars.get(i).map_or(line_end, |&(offset, _)| offset);
    Some(ObjectRange::chars(chars[start].0..byte_end(end)))
}

/// i", a", i', a', i`, a`: a quoted string on the cursor's line (the
/// next one when the cursor is not inside one).
pub(crate) fn quote_object(
    buffer: &Buffer,
    pos: usize,
    quote: char,
    inner: bool,
) -> Option<ObjectRange> {
    let line = buffer.line_of(pos);
    let line_start = buffer.line_start(line);
    let line_end = buffer.line_end(line);
    let mut quotes = Vec::new();
    let mut backslashes = 0;
    for (offset, c) in buffer
        .chars_from(line_start)
        .take_while(|&(o, _)| o < line_end)
    {
        if c == quote && backslashes % 2 == 0 {
            quotes.push(offset);
        }
        backslashes = if c == '\\' { backslashes + 1 } else { 0 };
    }
    let pair = if let Some(k) = quotes.iter().position(|&q| q == pos) {
        if k % 2 == 0 {
            (*quotes.get(k)?, *quotes.get(k + 1)?)
        } else {
            (quotes[k - 1], quotes[k])
        }
    } else {
        let pairs = quotes.as_chunks::<2>().0;
        pairs
            .iter()
            .map(|&[open, close]| (open, close))
            .find(|&(open, close)| open < pos && pos < close)
            .or_else(|| {
                pairs
                    .iter()
                    .map(|&[open, close]| (open, close))
                    .find(|&(open, _)| open > pos)
            })?
    };
    let (open, close) = pair;
    let width = quote.len_utf8();
    if inner {
        return Some(ObjectRange::chars(open + width..close));
    }
    let mut end = close + width;
    let trailing = buffer
        .chars_from(end)
        .take_while(|&(o, c)| o < line_end && (c == ' ' || c == '\t'))
        .count();
    let mut start = open;
    if trailing > 0 {
        end += trailing;
    } else {
        start = buffer
            .chars_before(open)
            .take_while(|&(o, c)| o >= line_start && (c == ' ' || c == '\t'))
            .last()
            .map_or(open, |(o, _)| o);
    }
    Some(ObjectRange::chars(start..end))
}

/// The `count`th pair of `open`/`close` around `pos` (a bracket under the
/// cursor counts as inside).
fn enclosing_pair(
    buffer: &Buffer,
    pos: usize,
    open: char,
    close: char,
    count: usize,
) -> Option<(usize, usize)> {
    let at = buffer.chars_from(pos).next().map(|(_, c)| c);
    // A `close` under the cursor is outside the text before it, so its
    // partner is the unmatched `open` before it, as from inside.
    let mut open_at = if at == Some(open) {
        pos
    } else {
        matching_open(buffer, pos, open, close)?
    };
    for _ in 1..count {
        open_at = matching_open(buffer, open_at, open, close)?;
    }
    let close_at = matching_close(buffer, open_at + open.len_utf8(), open, close)?;
    Some((open_at, close_at))
}

/// i(, a(, i[, i{, i<... Inside a pair whose brackets sit alone at the
/// ends of their lines, the inner object is the lines in between.
pub(crate) fn bracket_object(
    buffer: &Buffer,
    pos: usize,
    open: char,
    close: char,
    inner: bool,
    count: usize,
) -> Option<ObjectRange> {
    let (open_at, close_at) = enclosing_pair(buffer, pos, open, close, count)?;
    if !inner {
        return Some(ObjectRange::chars(open_at..close_at + close.len_utf8()));
    }
    let start = open_at + open.len_utf8();
    let open_line = buffer.line_of(open_at);
    let close_line = buffer.line_of(close_at);
    let rest_blank = |from: usize, to: usize| {
        buffer
            .chars_from(from)
            .take_while(|&(o, _)| o < to)
            .all(|(_, c)| c.is_whitespace())
    };
    if close_line > open_line && rest_blank(start, buffer.line_end(open_line)) {
        let close_line_start = buffer.line_start(close_line);
        if rest_blank(close_line_start, close_at) {
            if close_line == open_line + 1 {
                // Nothing between the lines.
                return Some(ObjectRange::chars(close_line_start..close_line_start));
            }
            return Some(ObjectRange {
                range: buffer.line_start(open_line + 1)..buffer.line_end(close_line - 1),
                linewise: true,
            });
        }
        return Some(ObjectRange::chars(
            buffer.line_start(open_line + 1)..close_at,
        ));
    }
    Some(ObjectRange::chars(start..close_at))
}

/// it, at: the `count`th element around `pos` in `text` (the whole
/// buffer).
pub(crate) fn tag_object(text: &str, pos: usize, inner: bool, count: usize) -> Option<ObjectRange> {
    static TAG: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"<(/?)([A-Za-z][^\s/>]*)[^>]*?(/?)>").expect("valid regex")
    });
    // (open tag, close tag) ranges.
    let mut pairs: Vec<(Range<usize>, Range<usize>)> = Vec::new();
    let mut stack: Vec<(String, Range<usize>)> = Vec::new();
    for captures in TAG.captures_iter(text) {
        let whole = captures.get(0).expect("group 0").range();
        if !captures[3].is_empty() {
            continue;
        }
        let name = &captures[2];
        if captures[1].is_empty() {
            stack.push((name.to_owned(), whole));
        } else if let Some(index) = stack.iter().rposition(|(open, _)| open == name) {
            let (_, open) = stack.swap_remove(index);
            stack.truncate(index);
            pairs.push((open, whole));
        }
    }
    let mut around: Vec<&(Range<usize>, Range<usize>)> = pairs
        .iter()
        .filter(|(open, close)| open.start <= pos && pos < close.end)
        .collect();
    // Innermost first.
    around.sort_by_key(|(open, _)| std::cmp::Reverse(open.start));
    let (open, close) = around.get(count.saturating_sub(1))?;
    Some(ObjectRange::chars(if inner {
        open.end..close.start
    } else {
        open.start..close.end
    }))
}

/// ip, ap: lines of the paragraph (or blank run) at `pos`; `ap` adds the
/// blank lines after it (or before it, at the end of the buffer).
pub(crate) fn paragraph_object(
    buffer: &Buffer,
    pos: usize,
    inner: bool,
    count: usize,
) -> Option<ObjectRange> {
    let lines = buffer.line_count();
    let line = buffer.line_of(pos);
    let blank = |l: usize| is_blank_line(buffer, l);
    let run_end = |from: usize| {
        let b = blank(from);
        let mut l = from;
        while l + 1 < lines && blank(l + 1) == b {
            l += 1;
        }
        l
    };
    let mut first = line;
    let started_blank = blank(line);
    while first > 0 && blank(first - 1) == started_blank {
        first -= 1;
    }
    let mut last = run_end(line);
    let runs = if inner { count } else { count * 2 };
    for _ in 1..runs {
        if last + 1 >= lines {
            break;
        }
        last = run_end(last + 1);
    }
    if !inner && !started_blank {
        let ended_blank = blank(last);
        if !ended_blank && first > 0 {
            // No blank lines after: take the ones before.
            while first > 0 && blank(first - 1) {
                first -= 1;
            }
        }
    }
    Some(ObjectRange {
        range: buffer.line_start(first)..buffer.line_end(last),
        linewise: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> (Buffer, usize) {
        let pos = text.find('|').expect("cursor");
        (Buffer::new(&text.replacen('|', "", 1)), pos)
    }

    #[test]
    fn word_motions_follow_vim_classes() {
        type WordMotion = fn(&Buffer, usize, bool) -> usize;
        let cases: &[(&str, WordMotion, bool, usize)] = &[
            ("|foo.bar baz", next_word_start, false, 3),
            ("foo|.bar baz", next_word_start, false, 4),
            ("|foo.bar baz", next_word_start, true, 8),
            ("fo|o\n\nbar", next_word_start, false, 4),
            ("foo\n|\nbar", next_word_start, false, 5),
            ("|foo bar", word_end, false, 2),
            ("fo|o bar", word_end, false, 6),
            ("foo ba|r", word_back, false, 4),
            ("foo |bar", word_back, false, 0),
            ("foo\n\n|bar", word_back, false, 4),
            ("foo.ba|r", word_back, true, 0),
            ("foo ba|r", word_end_back, false, 2),
            ("foo\n\nb|ar", word_end_back, false, 4),
        ];
        for (text, motion, big, expected) in cases {
            let (buffer, pos) = at(text);
            assert_eq!(motion(&buffer, pos, *big), *expected, "{text}");
        }
    }

    #[test]
    fn find_stays_on_the_line_and_counts() {
        let (buffer, pos) = at("|a,b,c\nd,");
        assert_eq!(
            find_in_line(&buffer, pos, ',', true, false, 2, false),
            Some(3)
        );
        assert_eq!(
            find_in_line(&buffer, pos, ',', true, true, 1, false),
            Some(0)
        );
        assert_eq!(find_in_line(&buffer, pos, ',', true, false, 3, false), None);
        // A repeated t skips the character next to the cursor.
        assert_eq!(find_in_line(&buffer, 0, ',', true, true, 1, true), Some(2));
        let (buffer, pos) = at("a,b,|c");
        assert_eq!(
            find_in_line(&buffer, pos, ',', false, true, 1, false),
            Some(4)
        );
        assert_eq!(
            find_in_line(&buffer, pos, ',', false, false, 2, false),
            Some(1)
        );
    }

    #[test]
    fn percent_and_paragraphs() {
        let (buffer, pos) = at("|x = f(a[1], {b});");
        assert_eq!(match_pair(&buffer, pos), Some(15));
        assert_eq!(match_pair(&buffer, 15), Some(5));
        let (buffer, pos) = at("|a\nb\n\nc\n\n\nd");
        assert_eq!(paragraph_forward(&buffer, pos, 1), 4);
        assert_eq!(paragraph_forward(&buffer, pos, 2), 7);
        assert_eq!(paragraph_forward(&buffer, pos, 3), buffer.len());
        assert_eq!(paragraph_back(&buffer, buffer.len(), 1), 8);
        assert_eq!(paragraph_back(&buffer, buffer.len(), 3), 0);
    }

    #[test]
    fn objects_cover_vim_extents() {
        let range = |o: Option<ObjectRange>| o.map(|o| o.range);
        let (buffer, pos) = at("one tw|o  three");
        assert_eq!(range(word_object(&buffer, pos, false, true, 1)), Some(4..7));
        assert_eq!(
            range(word_object(&buffer, pos, false, false, 1)),
            Some(4..9)
        );
        assert_eq!(
            range(word_object(&buffer, pos, false, true, 3)),
            Some(4..14)
        );
        let (buffer, pos) = at("one tw|o");
        assert_eq!(
            range(word_object(&buffer, pos, false, false, 1)),
            Some(3..7)
        );
        let (buffer, pos) = at(r#"say "a \"b|\" c" now"#);
        assert_eq!(range(quote_object(&buffer, pos, '"', true)), Some(5..14));
        assert_eq!(range(quote_object(&buffer, pos, '"', false)), Some(4..16));
        let (buffer, pos) = at("|x = 'a' + 'b'");
        assert_eq!(range(quote_object(&buffer, pos, '\'', true)), Some(5..6));
        let (buffer, pos) = at("f(a, (b|), c)");
        assert_eq!(
            range(bracket_object(&buffer, pos, '(', ')', true, 1)),
            Some(6..7)
        );
        assert_eq!(
            range(bracket_object(&buffer, pos, '(', ')', false, 2)),
            Some(1..12)
        );
        let (buffer, pos) = at("fn x() {\n    a|;\n    b;\n}");
        let object = bracket_object(&buffer, pos, '{', '}', true, 1).unwrap();
        assert!(object.linewise);
        assert_eq!(object.range, 9..22);
        let text = "<div><p>he|llo</p></div>";
        let (_, pos) = at(text);
        let text = text.replace('|', "");
        assert_eq!(range(tag_object(&text, pos, true, 1)), Some(8..13));
        assert_eq!(range(tag_object(&text, pos, false, 2)), Some(0..23));
        let (buffer, pos) = at("a\n|b\n\n\nc");
        let object = paragraph_object(&buffer, pos, false, 1).unwrap();
        assert_eq!(object.range, 0..5);
        assert_eq!(paragraph_object(&buffer, pos, true, 1).unwrap().range, 0..3);
    }
}
