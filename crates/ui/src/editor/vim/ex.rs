//! The command line (`:`, `/`, `?`) with history, ex commands and
//! searching.

use regex::{Regex, RegexBuilder};

use super::{
    exec::Span,
    motion,
    parse::{self, Operator},
    *,
};

/// History entries kept per command-line kind.
const HISTORY_LIMIT: usize = 100;

/// A Vim search pattern as a regex: Rust regex syntax plus Vim's `\<`
/// and `\>` word boundaries; smartcase (case-insensitive unless the
/// pattern has an uppercase letter), `\c` / `\C` force it.
pub(super) fn search_regex(pattern: &str) -> Result<Regex, String> {
    search_regex_with(pattern, None)
}

fn search_regex_with(pattern: &str, ignore_case: Option<bool>) -> Result<Regex, String> {
    let mut insensitive = !pattern.chars().any(char::is_uppercase);
    let mut source = pattern.to_owned();
    if source.contains("\\c") {
        insensitive = true;
        source = source.replace("\\c", "");
    }
    if source.contains("\\C") {
        insensitive = false;
        source = source.replace("\\C", "");
    }
    let source = source.replace("\\<", "\\b").replace("\\>", "\\b");
    RegexBuilder::new(&source)
        .case_insensitive(ignore_case.unwrap_or(insensitive))
        .multi_line(true)
        .build()
        .map_err(|err| match err {
            regex::Error::Syntax(message) => format!(
                "E: invalid pattern: {}",
                message
                    .lines()
                    .last()
                    .unwrap_or("")
                    .trim_start_matches("error: ")
            ),
            other => other.to_string(),
        })
}

/// A Vim replacement (`&`, `\0`-`\9`, `\n`, `\t`) in the regex crate's
/// syntax.
pub(super) fn vim_replacement(replacement: &str) -> String {
    let mut out = String::new();
    let mut chars = replacement.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some(d @ '0'..='9') => out.push_str(&format!("${{{d}}}")),
                Some('n' | 'r') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('&') => out.push('&'),
                Some('$') => out.push_str("$$"),
                Some(other) => out.push(other),
                None => out.push('\\'),
            },
            '&' => out.push_str("${0}"),
            '$' => out.push_str("$$"),
            c => out.push(c),
        }
    }
    out
}

/// Dispatches the IDE action named `name` (a workspace command).
fn dispatch(window: &mut Window, name: &str, cx: &mut App) -> Result<(), String> {
    let action = cx
        .build_action(name, None)
        .map_err(|err| format!("E: {name}: {err}"))?;
    window.dispatch_action(action, cx);
    Ok(())
}

/// Splits `s/pattern/replacement/flags` (after the `s`) at unescaped
/// delimiters; escaped delimiters lose their backslash.
pub(super) fn split_substitute(arg: &str) -> Option<(String, String, String)> {
    let mut chars = arg.chars();
    let delimiter = chars.next()?;
    if delimiter.is_alphanumeric() || delimiter.is_whitespace() || delimiter == '\\' {
        return None;
    }
    let mut parts = vec![String::new()];
    let mut escaped = false;
    for c in chars {
        let split = !escaped && c == delimiter && parts.len() < 3;
        if split {
            parts.push(String::new());
            continue;
        }
        let part = parts.last_mut().expect("one part");
        if escaped {
            if c != delimiter {
                part.push('\\');
            }
            part.push(c);
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else {
            part.push(c);
        }
    }
    if escaped {
        parts.last_mut().expect("one part").push('\\');
    }
    let mut parts = parts.into_iter();
    Some((
        parts.next().unwrap_or_default(),
        parts.next().unwrap_or_default(),
        parts.next().unwrap_or_default(),
    ))
}

impl Editor {
    pub(super) fn open_ex(&mut self, kind: ExKind, text: String, cx: &mut Context<Self>) {
        self.vim.ex = Some(ExLine {
            kind,
            text,
            history: None,
            typed: String::new(),
        });
        cx.notify();
    }

    fn history(shared: &mut Shared, kind: ExKind) -> &mut Vec<String> {
        match kind {
            ExKind::Command => &mut shared.ex_history,
            ExKind::Search { .. } => &mut shared.search_history,
        }
    }

    /// A key on the command line.
    pub(super) fn ex_key(&mut self, key: Key, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(ex) = &mut self.vim.ex else {
            return false;
        };
        match key {
            Key::Esc => {
                self.vim.ex = None;
                self.vim.pending.clear();
            }
            Key::Enter => {
                let ex = self.vim.ex.take().expect("checked above");
                self.run_ex_line(ex, window, cx);
            }
            Key::Backspace => {
                if ex.text.pop().is_none() {
                    self.vim.ex = None;
                    self.vim.pending.clear();
                }
            }
            Key::Ctrl('u') => ex.text.clear(),
            Key::Ctrl('w') => {
                let trimmed = ex.text.trim_end().len();
                ex.text.truncate(trimmed);
                let keep = ex
                    .text
                    .char_indices()
                    .rev()
                    .find(|&(_, c)| !(c.is_alphanumeric() || c == '_'))
                    .map_or(0, |(i, c)| i + c.len_utf8());
                ex.text.truncate(keep);
            }
            Key::Ctrl('v') => {
                if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                    let ex = self.vim.ex.as_mut().expect("checked above");
                    ex.text.push_str(text.lines().next().unwrap_or_default());
                }
            }
            Key::Up | Key::Down => {
                let kind = ex.kind;
                let mut ex = self.vim.ex.take().expect("checked above");
                let history = Self::history(cx.default_global::<Shared>(), kind);
                let index = match (key, ex.history) {
                    (Key::Up, None) if !history.is_empty() => {
                        ex.typed = ex.text.clone();
                        Some(history.len() - 1)
                    }
                    (Key::Up, Some(i)) => Some(i.saturating_sub(1)),
                    (Key::Down, Some(i)) if i + 1 < history.len() => Some(i + 1),
                    _ => None,
                };
                match index {
                    Some(i) => ex.text = history[i].clone(),
                    None if ex.history.is_some() => ex.text = ex.typed.clone(),
                    None => {}
                }
                ex.history = index;
                self.vim.ex = Some(ex);
            }
            Key::Char(c) => ex.text.push(c),
            Key::Tab => ex.text.push('\t'),
            Key::Ctrl(_) => return false,
            _ => {}
        }
        cx.notify();
        true
    }

    fn run_ex_line(&mut self, ex: ExLine, window: &mut Window, cx: &mut Context<Self>) {
        if !ex.text.is_empty() {
            let history = Self::history(cx.default_global::<Shared>(), ex.kind);
            if history.last() != Some(&ex.text) {
                history.push(ex.text.clone());
                if history.len() > HISTORY_LIMIT {
                    history.remove(0);
                }
            }
        }
        match ex.kind {
            ExKind::Search { forward } => {
                self.vim.pending.push(Key::Search {
                    forward,
                    pattern: ex.text,
                });
                let visual = matches!(self.vim.mode, Mode::Visual(_));
                let recording = cx.default_global::<Shared>().recording.is_some();
                let parsed = parse::parse(&self.vim.pending, visual, recording);
                self.vim.pending.clear();
                if let parse::Parsed::Done(command) = parsed {
                    self.run_command(command, window, cx);
                }
            }
            ExKind::Command => {
                if ex.text.trim().is_empty() {
                    return;
                }
                cx.default_global::<Shared>().last_ex = Some(ex.text.clone());
                let depth = self.buffer.undo_depth();
                self.buffer.seal_undo();
                if let Err(message) = self.run_ex(&ex.text, window, cx) {
                    self.notice = Some(message.into());
                }
                if self.vim.mode == Mode::Normal {
                    self.group_undo_since(depth);
                }
            }
        }
        cx.notify();
    }

    /// One address (`12`, `.`, `$`, `'a`, with `+n`/`-n`), as a line.
    fn ex_address(&self, s: &mut &str) -> Result<Option<usize>, String> {
        let current = self.buffer.line_of(self.vim_cursor());
        let last = self.buffer.line_count() - 1;
        let mut line = if let Some(rest) = s.strip_prefix('.') {
            *s = rest;
            Some(current)
        } else if let Some(rest) = s.strip_prefix('$') {
            *s = rest;
            Some(last)
        } else if let Some(rest) = s.strip_prefix('\'') {
            let mut chars = rest.chars();
            let mark = chars.next().ok_or("E20: Mark not set")?;
            *s = chars.as_str();
            let offset = self.mark(mark).ok_or("E20: Mark not set")?;
            Some(self.buffer.line_of(offset.min(self.buffer.len())))
        } else {
            let digits = s.len() - s.trim_start_matches(|c: char| c.is_ascii_digit()).len();
            if digits > 0 {
                let number: usize = s[..digits].parse().map_err(|_| "E: bad line number")?;
                *s = &s[digits..];
                Some(number.saturating_sub(1))
            } else {
                None
            }
        };
        while let Some(sign) = s.chars().next().filter(|c| *c == '+' || *c == '-') {
            *s = &s[1..];
            let digits = s.len() - s.trim_start_matches(|c: char| c.is_ascii_digit()).len();
            let amount: usize = if digits == 0 {
                1
            } else {
                s[..digits].parse().map_err(|_| "E: bad offset")?
            };
            *s = &s[digits..];
            let base = line.unwrap_or(current);
            line = Some(if sign == '+' {
                base + amount
            } else {
                base.saturating_sub(amount)
            });
        }
        Ok(line.map(|line| line.min(last)))
    }

    /// The range at the start of `s` (`%`, `a`, `a,b`), as lines.
    fn ex_range(&self, s: &mut &str) -> Result<Option<(usize, usize)>, String> {
        if let Some(rest) = s.strip_prefix('%') {
            *s = rest;
            return Ok(Some((0, self.buffer.line_count() - 1)));
        }
        let Some(first) = self.ex_address(s)? else {
            return Ok(None);
        };
        if let Some(rest) = s.strip_prefix([',', ';']) {
            *s = rest;
            let current = self.buffer.line_of(self.vim_cursor());
            let second = self.ex_address(s)?.unwrap_or(current);
            return Ok(Some((first.min(second), first.max(second))));
        }
        Ok(Some((first, first)))
    }

    /// Runs an ex command line (without the `:`).
    pub(super) fn run_ex(
        &mut self,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let mut rest = text.trim_start_matches([':', ' ']);
        let range = self.ex_range(&mut rest)?;
        let rest = rest.trim_start();
        let name_len = rest
            .char_indices()
            .find(|(_, c)| !c.is_ascii_alphabetic())
            .map_or(rest.len(), |(i, _)| i);
        let (name, rest) = if name_len == 0 {
            let split = rest.chars().next().map_or(0, char::len_utf8);
            rest.split_at(split)
        } else {
            rest.split_at(name_len)
        };
        let (bang, arg) = match rest.strip_prefix('!') {
            Some(arg) => (true, arg.trim()),
            None => (false, rest.trim()),
        };
        let current = self.buffer.line_of(self.vim_cursor());
        let lines = range.unwrap_or((current, current));
        let unknown = || Err(format!("E492: Not an editor command: {text}"));
        match name {
            "" => {
                let Some((_, line)) = range else {
                    return Ok(());
                };
                let from = self.vim_cursor();
                self.vim.marks.insert('\'', from);
                if let Mode::Visual(_) = self.vim.mode {
                    self.exit_visual(cx);
                }
                let offset = motion::first_non_blank(&self.buffer, line);
                self.set_selections(SelectionSet::single(Selection::cursor(offset)), cx);
            }
            "w" | "write" => {
                if !arg.is_empty() {
                    return Err("E: :w with a file name is not supported".into());
                }
                self.save_with_format(cx);
            }
            "wa" | "wall" => dispatch(window, "workspace::SaveAll", cx)?,
            "q" | "quit" | "clo" | "close" => cx.emit(EditorEvent::Close { discard: bang }),
            "wq" | "x" | "xi" | "xit" | "exi" | "exit" => {
                if !(name.starts_with('w') || self.buffer.is_dirty()) {
                    cx.emit(EditorEvent::Close { discard: bang });
                    return Ok(());
                }
                // Closes once the write is done; a failed save stays open.
                let save = self.save(cx);
                cx.spawn(async move |this, cx| {
                    if save.await {
                        this.update(cx, |_, cx| cx.emit(EditorEvent::Close { discard: bang }))
                            .ok();
                    }
                })
                .detach();
            }
            "e" | "edit" => {
                if arg.is_empty() {
                    return match self.reload_from_disk(bang, cx) {
                        ReloadOutcome::Conflict => {
                            Err("E37: No write since last change (add ! to override)".into())
                        }
                        ReloadOutcome::Failed => Err("E: cannot read the file".into()),
                        ReloadOutcome::Unchanged | ReloadOutcome::Reloaded => Ok(()),
                    };
                }
                let path = self.ex_path(arg).ok_or("E32: No file name")?;
                cx.emit(EditorEvent::Navigate {
                    locations: vec![language::Location {
                        path,
                        range: lsp::types::Range::default(),
                    }],
                    title: "Edit".into(),
                });
            }
            "s" | "su" | "substitute" => {
                let message = self.substitute(lines, arg, cx)?;
                self.notice = Some(message.into());
            }
            "noh" | "nohl" | "nohlsearch" => cx.default_global::<Shared>().highlight = false,
            "d" | "de" | "del" | "delete" => {
                let register = arg.chars().next().filter(|c| parse::is_register(*c));
                self.apply(
                    Operator::Delete,
                    Span::Lines(lines.0, lines.1),
                    register,
                    1,
                    window,
                    cx,
                );
            }
            "y" | "ya" | "yank" => {
                let register = arg.chars().next().filter(|c| parse::is_register(*c));
                self.apply(
                    Operator::Yank,
                    Span::Lines(lines.0, lines.1),
                    register,
                    1,
                    window,
                    cx,
                );
            }
            ">" | "<" => {
                let shifts = 1 + arg.chars().filter(|c| c.to_string() == name).count();
                let op = if name == ">" {
                    Operator::Indent
                } else {
                    Operator::Outdent
                };
                self.apply(op, Span::Lines(lines.0, lines.1), None, shifts, window, cx);
            }
            "j" | "join" => {
                let last = if lines.0 == lines.1 {
                    lines.1 + 1
                } else {
                    lines.1
                };
                self.join_lines(lines.0, last, !bang, cx);
            }
            "sp" | "split" => dispatch(window, "workspace::SplitDown", cx)?,
            "vs" | "vsp" | "vsplit" => dispatch(window, "workspace::SplitRight", cx)?,
            "se" | "set" => match arg {
                "hls" | "hlsearch" => cx.default_global::<Shared>().highlight = true,
                "nohls" | "nohlsearch" => cx.default_global::<Shared>().highlight = false,
                _ => return Err(format!("E518: Unknown option: {arg}")),
            },
            _ => return unknown(),
        }
        Ok(())
    }

    /// `:e {file}`: relative to the folder of the current file's project
    /// root (absolute paths as they are).
    fn ex_path(&self, arg: &str) -> Option<std::path::PathBuf> {
        let project = self.project.as_ref()?;
        let root = self
            .buffer
            .path()
            .and_then(|path| project.root_index_of(path))
            .unwrap_or(0);
        Some(project.path_in(root, arg))
    }

    /// `:s/pattern/replacement/[gi]` over `lines`.
    fn substitute(
        &mut self,
        lines: (usize, usize),
        arg: &str,
        cx: &mut Context<Self>,
    ) -> Result<String, String> {
        let (pattern, replacement, flags) =
            split_substitute(arg).ok_or("E: usage :s/pattern/replacement/[flags]")?;
        let shared = cx.default_global::<Shared>();
        let pattern = if pattern.is_empty() {
            shared
                .search
                .as_ref()
                .map(|search| search.pattern.clone())
                .ok_or("E35: No previous regular expression")?
        } else {
            pattern
        };
        let mut global = false;
        let mut ignore_case = None;
        for flag in flags.chars() {
            match flag {
                'g' => global = true,
                'i' => ignore_case = Some(true),
                'I' => ignore_case = Some(false),
                'c' => return Err("E: the c (confirm) flag is not supported".into()),
                ' ' | 'e' => {}
                other => return Err(format!("E488: Trailing characters: {other}")),
            }
        }
        let regex = search_regex_with(&pattern, ignore_case)?;
        shared.search = Some(LastSearch {
            pattern: pattern.clone(),
            forward: true,
        });
        shared.highlight = true;
        let replacement = vim_replacement(&replacement);
        let start = self.buffer.line_start(lines.0);
        let end = self.buffer.line_end(lines.1);
        let text = self.buffer.slice(start..end);
        let mut edits: Vec<(Range<usize>, String)> = Vec::new();
        let mut last_line = None;
        let mut changed_lines = 0;
        for captures in regex.captures_iter(&text) {
            let whole = captures.get(0).expect("group 0");
            let line = self.buffer.line_of(start + whole.start());
            if !global && last_line == Some(line) {
                continue;
            }
            if last_line != Some(line) {
                changed_lines += 1;
            }
            last_line = Some(line);
            let mut expanded = String::new();
            captures.expand(&replacement, &mut expanded);
            let expanded = self.normalize_line_endings(&expanded);
            edits.push((start + whole.start()..start + whole.end(), expanded));
        }
        if edits.is_empty() {
            return Err(format!("E486: Pattern not found: {pattern}"));
        }
        let count = edits.len();
        let last_start = edits.last().map_or(start, |(r, _)| r.start);
        let shift: isize = edits[..count - 1]
            .iter()
            .map(|(r, t)| t.len() as isize - r.len() as isize)
            .sum();
        let anchor = last_start.saturating_add_signed(shift);
        if let Mode::Visual(_) = self.vim.mode {
            self.exit_visual(cx);
        }
        self.commit(
            edits,
            |_| SelectionSet::single(Selection::cursor(anchor)),
            cx,
        );
        let line = self.buffer.line_of(anchor.min(self.buffer.len()));
        let offset = motion::first_non_blank(&self.buffer, line);
        self.set_selections(SelectionSet::single(Selection::cursor(offset)), cx);
        Ok(format!(
            "{count} substitution{} on {changed_lines} line{}",
            if count == 1 { "" } else { "s" },
            if changed_lines == 1 { "" } else { "s" }
        ))
    }

    /// The `count`th match of `pattern` from `from`, wrapping around the
    /// buffer, and whether it wrapped.
    pub(super) fn find_pattern(
        &self,
        pattern: &str,
        from: usize,
        forward: bool,
        count: usize,
    ) -> Result<Option<(usize, bool)>, String> {
        let regex = search_regex(pattern)?;
        let text = self.text();
        let mut position = from;
        let mut wrapped = false;
        for _ in 0..count.max(1) {
            let found = if forward {
                let start = self.buffer.next_grapheme(position).min(text.len());
                regex.find_at(&text, start).map(|m| m.start()).or_else(|| {
                    wrapped = true;
                    regex.find(&text).map(|m| m.start())
                })
            } else {
                let mut last_before = None;
                let mut last = None;
                for m in regex.find_iter(&text) {
                    if m.start() < position {
                        last_before = Some(m.start());
                    }
                    last = Some(m.start());
                }
                last_before.or_else(|| {
                    wrapped = true;
                    last
                })
            };
            match found {
                Some(offset) => position = offset,
                None => return Ok(None),
            }
        }
        Ok(Some((position, wrapped)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replacements_translate_vim_syntax() {
        assert_eq!(vim_replacement(r"<&>"), "<${0}>");
        assert_eq!(vim_replacement(r"\1-\2\n$"), "${1}-${2}\n$$");
        assert_eq!(vim_replacement(r"a\&b\\"), r"a&b\");
    }

    #[test]
    fn substitute_arguments_split_at_unescaped_delimiters() {
        assert_eq!(
            split_substitute("/a\\/b/c/g"),
            Some(("a/b".into(), "c".into(), "g".into()))
        );
        assert_eq!(
            split_substitute("#x#y"),
            Some(("x".into(), "y".into(), String::new()))
        );
        assert_eq!(
            split_substitute("/\\d+/N/"),
            Some(("\\d+".into(), "N".into(), String::new()))
        );
        assert_eq!(split_substitute("abc"), None);
    }

    #[test]
    fn search_patterns_use_smartcase_and_vim_word_bounds() {
        let regex = search_regex("foo").unwrap();
        assert!(regex.is_match("FOO"));
        let regex = search_regex("Foo").unwrap();
        assert!(!regex.is_match("foo"));
        let regex = search_regex("\\Cfoo").unwrap();
        assert!(!regex.is_match("FOO"));
        let regex = search_regex("\\<is\\>").unwrap();
        assert!(!regex.is_match("this"));
        assert!(regex.is_match("it is"));
        assert!(search_regex("(").is_err());
    }
}
