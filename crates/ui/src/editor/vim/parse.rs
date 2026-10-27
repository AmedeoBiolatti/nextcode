//! Vim key sequences parsed into commands: `["x][count]` then a motion, an
//! operator with its target (`[count]` motion, text object, or the
//! operator again for whole lines), or a command. Parsing restarts from
//! the first pending key on every key, so prefixes simply stay pending.

use gpui::Keystroke;

/// A key as Vim sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Key {
    Char(char),
    /// Ctrl with a letter or symbol (lowercase).
    Ctrl(char),
    Esc,
    Enter,
    Backspace,
    Delete,
    Tab,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
    /// A finished `/` or `?` search line, used as a motion.
    Search {
        forward: bool,
        pattern: String,
    },
}

impl Key {
    /// The key for a keystroke, or `None` for keys Vim leaves to the IDE
    /// (Alt and platform chords, function keys, Shift+Tab).
    pub fn from_keystroke(keystroke: &Keystroke) -> Option<Key> {
        let modifiers = &keystroke.modifiers;
        if modifiers.platform || modifiers.function {
            return None;
        }
        // AltGr arrives as Ctrl+Alt with the typed character.
        let alt_gr = modifiers.control && modifiers.alt;
        if modifiers.control && !alt_gr {
            if keystroke.key == "[" {
                return Some(Key::Esc);
            }
            let mut chars = keystroke.key.chars();
            return match (chars.next(), chars.next()) {
                (Some(c), None) if !modifiers.shift => Some(Key::Ctrl(c.to_ascii_lowercase())),
                _ => None,
            };
        }
        if modifiers.alt && !alt_gr {
            return None;
        }
        let named = match keystroke.key.as_str() {
            "escape" => Some(Key::Esc),
            "enter" => Some(Key::Enter),
            "backspace" => Some(Key::Backspace),
            "delete" => Some(Key::Delete),
            "tab" if modifiers.shift => return None,
            "tab" => Some(Key::Tab),
            "left" => Some(Key::Left),
            "right" => Some(Key::Right),
            "up" => Some(Key::Up),
            "down" => Some(Key::Down),
            "home" => Some(Key::Home),
            "end" => Some(Key::End),
            "pageup" => Some(Key::PageUp),
            "pagedown" => Some(Key::PageDown),
            "space" => Some(Key::Char(' ')),
            _ => None,
        };
        if named.is_some() {
            return named;
        }
        if let Some(text) = &keystroke.key_char {
            let mut chars = text.chars();
            return match (chars.next(), chars.next()) {
                (Some(c), None) => Some(Key::Char(c)),
                _ => None,
            };
        }
        let mut chars = keystroke.key.chars();
        match (chars.next(), chars.next()) {
            (Some(c), None) if modifiers.shift => Some(Key::Char(c.to_ascii_uppercase())),
            (Some(c), None) => Some(Key::Char(c)),
            _ => None,
        }
    }

    pub fn is_char(&self, c: char) -> bool {
        *self == Key::Char(c)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Operator {
    Delete,
    Change,
    Yank,
    Indent,
    Outdent,
    Format,
    ToggleCase,
    Lower,
    Upper,
}

impl Operator {
    /// Whether applying it edits the text (and so is repeated by `.`).
    pub fn edits(self) -> bool {
        self != Operator::Yank
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Motion {
    Left,
    Right,
    /// Backspace and Space: like h and l, but across line breaks.
    LeftWrap,
    RightWrap,
    Up,
    Down,
    /// gj, gk: screen rows of wrapped lines.
    RowDown,
    RowUp,
    WordStart {
        big: bool,
    },
    WordEnd {
        big: bool,
    },
    WordBack {
        big: bool,
    },
    WordEndBack {
        big: bool,
    },
    LineStart,
    FirstNonBlank,
    /// `$`; with a count, the end of the line count - 1 below.
    LineEnd,
    /// `g_`.
    LastNonBlank,
    /// `+` and Enter / `-`: first non-blank of a line below / above.
    NextLine,
    PrevLine,
    /// `|`.
    Column,
    /// gg (first line, or line count).
    FirstLine,
    /// G (last line, or line count).
    LastLine,
    Find {
        ch: char,
        forward: bool,
        till: bool,
    },
    RepeatFind {
        reverse: bool,
    },
    /// `%`: the bracket partner, or count% of the file.
    Match,
    ParagraphForward,
    ParagraphBack,
    ScreenTop,
    ScreenMiddle,
    ScreenBottom,
    HalfPageDown,
    HalfPageUp,
    PageDown,
    PageUp,
    SearchNext {
        reverse: bool,
    },
    /// `*` and `#`: the word at the cursor as a whole word.
    SearchWord {
        forward: bool,
    },
    Search {
        forward: bool,
        pattern: String,
    },
    /// `'x` (linewise) and `` `x ``.
    Mark {
        ch: char,
        line: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ObjectKind {
    Word,
    BigWord,
    Quote(char),
    Bracket(char, char),
    Tag,
    Paragraph,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Object {
    pub kind: ObjectKind,
    /// `i` (inner) rather than `a`.
    pub inner: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Target {
    Motion(Motion),
    Object(Object),
    /// The operator doubled (dd, >>, gUU): whole lines.
    Lines,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InsertAt {
    /// i
    Cursor,
    /// a
    After,
    /// I
    FirstNonBlank,
    /// gI
    LineStart,
    /// A
    LineEnd,
    /// o
    LineBelow,
    /// O
    LineAbove,
    /// gi: where insert mode was last left.
    LastInsert,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VisualKind {
    Char,
    Line,
    Block,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Scroll {
    /// zz, zt, zb.
    Center,
    Top,
    Bottom,
    /// Ctrl+E, Ctrl+Y.
    LineDown,
    LineUp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FoldCommand {
    Close,
    Open,
    Toggle,
    CloseAll,
    OpenAll,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Cmd {
    /// x, X
    DeleteChar {
        before: bool,
    },
    /// s, S
    Substitute {
        line: bool,
    },
    /// D, C
    ToEnd {
        change: bool,
    },
    /// Y
    YankLine,
    Put {
        before: bool,
    },
    Undo,
    Redo,
    Repeat,
    Join {
        spaces: bool,
    },
    ReplaceChar(char),
    ToggleCaseChar,
    Insert(InsertAt),
    /// R
    ReplaceMode,
    Visual(VisualKind),
    /// gv
    Reselect,
    /// Ctrl+A (1) and Ctrl+X (-1).
    Increment(i64),
    SetMark(char),
    /// q{reg}, or `None` to stop recording.
    Record(Option<char>),
    /// @{reg}; `@` repeats the last macro.
    PlayMacro(char),
    /// `:`
    Ex,
    Scroll(Scroll),
    Fold(FoldCommand),
    /// gd
    GoToDefinition,
    /// Visual: o (other end).
    SwapEnds,
    /// Visual block: I, A.
    BlockInsert {
        append: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Action {
    Move(Motion),
    Operate(Operator, Target),
    /// Visual mode: the operator over the selection; `lines` makes a
    /// charwise selection linewise (X, D, Y, R, S, C).
    VisualOperate {
        op: Operator,
        lines: bool,
    },
    /// Visual mode: select the text object.
    Select(Object),
    Do(Cmd),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Command {
    pub register: Option<char>,
    pub count: Option<usize>,
    pub action: Action,
}

impl Command {
    pub fn count(&self) -> usize {
        self.count.unwrap_or(1)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Parsed {
    /// More keys needed.
    Pending,
    /// Not a command: the keys are dropped.
    Invalid,
    /// `/` or `?` where a motion goes: read the pattern on the command
    /// line, then parse again with a [`Key::Search`] appended.
    Search {
        forward: bool,
    },
    Done(Command),
}

/// Counts beyond this are clamped (repeats stay bounded).
const MAX_COUNT: usize = 99_999;

pub(crate) fn is_register(c: char) -> bool {
    c.is_ascii_alphanumeric() || "\"+*_-.:/".contains(c)
}

/// Parses `keys` typed in normal (`visual: false`) or visual mode.
/// `recording` makes `q` stop the macro being recorded.
pub(crate) fn parse(keys: &[Key], visual: bool, recording: bool) -> Parsed {
    let mut i = 0;
    let mut register = None;
    if keys.first().is_some_and(|k| k.is_char('"')) {
        match keys.get(1) {
            None => return Parsed::Pending,
            Some(Key::Char(c)) if is_register(*c) => register = Some(*c),
            Some(_) => return Parsed::Invalid,
        }
        i = 2;
    }
    let count = parse_count(keys, &mut i);
    let Some(key) = keys.get(i) else {
        return Parsed::Pending;
    };
    i += 1;
    let done = |action| {
        Parsed::Done(Command {
            register,
            count,
            action,
        })
    };
    let next = keys.get(i);

    // Operators, including the g-prefixed ones.
    let operator = match key {
        Key::Char('d') => Some(Operator::Delete),
        Key::Char('c') => Some(Operator::Change),
        Key::Char('y') => Some(Operator::Yank),
        Key::Char('>') => Some(Operator::Indent),
        Key::Char('<') => Some(Operator::Outdent),
        Key::Char('=') => Some(Operator::Format),
        Key::Char('g') => match next {
            None => return Parsed::Pending,
            Some(Key::Char('~')) => Some(Operator::ToggleCase),
            Some(Key::Char('u')) => Some(Operator::Lower),
            Some(Key::Char('U')) => Some(Operator::Upper),
            _ => None,
        },
        _ => None,
    };
    if let Some(op) = operator {
        let prefixed = key.is_char('g');
        if prefixed {
            i += 1;
        }
        if visual {
            return done(Action::VisualOperate { op, lines: false });
        }
        let inner_count = parse_count(keys, &mut i);
        let count = multiply(count, inner_count);
        let done = |action| {
            Parsed::Done(Command {
                register,
                count,
                action,
            })
        };
        let Some(target) = keys.get(i) else {
            return Parsed::Pending;
        };
        // dd, >>, g~~, gUU, gUgU...
        let doubled = if prefixed {
            let letter = &keys[i - 1];
            if target == letter {
                Some(1)
            } else if target.is_char('g') {
                match keys.get(i + 1) {
                    None => return Parsed::Pending,
                    Some(k) if k == letter => Some(2),
                    _ => None,
                }
            } else {
                None
            }
        } else {
            (target == key).then_some(1)
        };
        if doubled.is_some() {
            return done(Action::Operate(op, Target::Lines));
        }
        if matches!(target, Key::Char('i' | 'a')) {
            return match parse_object(keys.get(i + 1), target.is_char('i')) {
                ObjectParse::Pending => Parsed::Pending,
                ObjectParse::Invalid => Parsed::Invalid,
                ObjectParse::Done(object) => done(Action::Operate(op, Target::Object(object))),
            };
        }
        return match parse_motion(keys, i) {
            MotionParse::Pending => Parsed::Pending,
            MotionParse::Invalid => Parsed::Invalid,
            MotionParse::Search(forward) => Parsed::Search { forward },
            MotionParse::Done(motion) => done(Action::Operate(op, Target::Motion(motion))),
        };
    }

    if visual {
        let visual_op = |op, lines| done(Action::VisualOperate { op, lines });
        match key {
            Key::Char('x') | Key::Delete => return visual_op(Operator::Delete, false),
            Key::Char('X' | 'D') => return visual_op(Operator::Delete, true),
            Key::Char('s') => return visual_op(Operator::Change, false),
            Key::Char('S' | 'C' | 'R') => return visual_op(Operator::Change, true),
            Key::Char('Y') => return visual_op(Operator::Yank, true),
            Key::Char('~') => return visual_op(Operator::ToggleCase, false),
            Key::Char('u') => return visual_op(Operator::Lower, false),
            Key::Char('U') => return visual_op(Operator::Upper, false),
            Key::Char('o' | 'O') => return done(Action::Do(Cmd::SwapEnds)),
            Key::Char('I') => return done(Action::Do(Cmd::BlockInsert { append: false })),
            Key::Char('A') => return done(Action::Do(Cmd::BlockInsert { append: true })),
            Key::Char('i' | 'a') => {
                return match parse_object(next, key.is_char('i')) {
                    ObjectParse::Pending => Parsed::Pending,
                    ObjectParse::Invalid => Parsed::Invalid,
                    ObjectParse::Done(object) => done(Action::Select(object)),
                };
            }
            _ => {}
        }
    }

    let cmd = match key {
        Key::Char('x') | Key::Delete => Some(Cmd::DeleteChar { before: false }),
        Key::Char('X') => Some(Cmd::DeleteChar { before: true }),
        Key::Char('s') => Some(Cmd::Substitute { line: false }),
        Key::Char('S') => Some(Cmd::Substitute { line: true }),
        Key::Char('D') => Some(Cmd::ToEnd { change: false }),
        Key::Char('C') => Some(Cmd::ToEnd { change: true }),
        Key::Char('Y') => Some(Cmd::YankLine),
        Key::Char('p') => Some(Cmd::Put { before: false }),
        Key::Char('P') => Some(Cmd::Put { before: true }),
        Key::Char('u') => Some(Cmd::Undo),
        Key::Ctrl('r') => Some(Cmd::Redo),
        Key::Char('.') => Some(Cmd::Repeat),
        Key::Char('J') => Some(Cmd::Join { spaces: true }),
        Key::Char('~') => Some(Cmd::ToggleCaseChar),
        Key::Char('i') => Some(Cmd::Insert(InsertAt::Cursor)),
        Key::Char('a') => Some(Cmd::Insert(InsertAt::After)),
        Key::Char('I') => Some(Cmd::Insert(InsertAt::FirstNonBlank)),
        Key::Char('A') => Some(Cmd::Insert(InsertAt::LineEnd)),
        Key::Char('o') => Some(Cmd::Insert(InsertAt::LineBelow)),
        Key::Char('O') => Some(Cmd::Insert(InsertAt::LineAbove)),
        Key::Char('R') => Some(Cmd::ReplaceMode),
        Key::Char('v') => Some(Cmd::Visual(VisualKind::Char)),
        Key::Char('V') => Some(Cmd::Visual(VisualKind::Line)),
        Key::Ctrl('v' | 'q') => Some(Cmd::Visual(VisualKind::Block)),
        Key::Ctrl('a') => Some(Cmd::Increment(1)),
        Key::Ctrl('x') => Some(Cmd::Increment(-1)),
        Key::Ctrl('e') => Some(Cmd::Scroll(Scroll::LineDown)),
        Key::Ctrl('y') => Some(Cmd::Scroll(Scroll::LineUp)),
        Key::Char(':') => Some(Cmd::Ex),
        Key::Char('q') if recording => Some(Cmd::Record(None)),
        _ => None,
    };
    if let Some(cmd) = cmd {
        return done(Action::Do(cmd));
    }

    // Commands that take one more key.
    let with_char = |make: fn(char) -> Cmd| match next {
        None => Parsed::Pending,
        Some(Key::Char(c)) => done(Action::Do(make(*c))),
        Some(Key::Enter) => done(Action::Do(make('\n'))),
        Some(_) => Parsed::Invalid,
    };
    match key {
        Key::Char('r') => return with_char(Cmd::ReplaceChar),
        Key::Char('m') => {
            return match next {
                None => Parsed::Pending,
                Some(Key::Char(c)) if c.is_ascii_lowercase() || "<>'`".contains(*c) => {
                    done(Action::Do(Cmd::SetMark(*c)))
                }
                Some(_) => Parsed::Invalid,
            };
        }
        Key::Char('q') => {
            return match next {
                None => Parsed::Pending,
                Some(Key::Char(c)) if c.is_ascii_alphanumeric() => {
                    done(Action::Do(Cmd::Record(Some(c.to_ascii_lowercase()))))
                }
                Some(_) => Parsed::Invalid,
            };
        }
        Key::Char('@') => {
            return match next {
                None => Parsed::Pending,
                Some(Key::Char(c)) if c.is_ascii_alphanumeric() || *c == '@' => {
                    done(Action::Do(Cmd::PlayMacro(c.to_ascii_lowercase())))
                }
                Some(_) => Parsed::Invalid,
            };
        }
        Key::Char('z') => {
            let cmd = match next {
                None => return Parsed::Pending,
                Some(Key::Char('z' | '.')) => Cmd::Scroll(Scroll::Center),
                Some(Key::Char('t') | Key::Enter) => Cmd::Scroll(Scroll::Top),
                Some(Key::Char('b' | '-')) => Cmd::Scroll(Scroll::Bottom),
                Some(Key::Char('c')) => Cmd::Fold(FoldCommand::Close),
                Some(Key::Char('o')) => Cmd::Fold(FoldCommand::Open),
                Some(Key::Char('a')) => Cmd::Fold(FoldCommand::Toggle),
                Some(Key::Char('M')) => Cmd::Fold(FoldCommand::CloseAll),
                Some(Key::Char('R')) => Cmd::Fold(FoldCommand::OpenAll),
                Some(_) => return Parsed::Invalid,
            };
            return done(Action::Do(cmd));
        }
        Key::Char('g') => {
            let cmd = match next {
                None => return Parsed::Pending,
                Some(Key::Char('v')) => Some(Cmd::Reselect),
                Some(Key::Char('i')) => Some(Cmd::Insert(InsertAt::LastInsert)),
                Some(Key::Char('I')) => Some(Cmd::Insert(InsertAt::LineStart)),
                Some(Key::Char('J')) => Some(Cmd::Join { spaces: false }),
                Some(Key::Char('d')) => Some(Cmd::GoToDefinition),
                _ => None,
            };
            if let Some(cmd) = cmd {
                return done(Action::Do(cmd));
            }
        }
        _ => {}
    }

    match parse_motion(keys, i - 1) {
        MotionParse::Pending => Parsed::Pending,
        MotionParse::Invalid => Parsed::Invalid,
        MotionParse::Search(forward) => Parsed::Search { forward },
        MotionParse::Done(motion) => done(Action::Move(motion)),
    }
}

fn multiply(a: Option<usize>, b: Option<usize>) -> Option<usize> {
    match (a, b) {
        (None, None) => None,
        (a, b) => Some((a.unwrap_or(1) * b.unwrap_or(1)).min(MAX_COUNT)),
    }
}

/// Digits at `keys[*i..]` (not starting with 0), advancing `i`.
fn parse_count(keys: &[Key], i: &mut usize) -> Option<usize> {
    let mut count: Option<usize> = None;
    while let Some(Key::Char(c)) = keys.get(*i) {
        let Some(digit) = c.to_digit(10) else {
            break;
        };
        if digit == 0 && count.is_none() {
            break;
        }
        count = Some((count.unwrap_or(0) * 10 + digit as usize).min(MAX_COUNT));
        *i += 1;
    }
    count
}

enum ObjectParse {
    Pending,
    Invalid,
    Done(Object),
}

fn parse_object(key: Option<&Key>, inner: bool) -> ObjectParse {
    let Some(key) = key else {
        return ObjectParse::Pending;
    };
    let kind = match key {
        Key::Char('w') => ObjectKind::Word,
        Key::Char('W') => ObjectKind::BigWord,
        Key::Char(c @ ('"' | '\'' | '`')) => ObjectKind::Quote(*c),
        Key::Char('(' | ')' | 'b') => ObjectKind::Bracket('(', ')'),
        Key::Char('[' | ']') => ObjectKind::Bracket('[', ']'),
        Key::Char('{' | '}' | 'B') => ObjectKind::Bracket('{', '}'),
        Key::Char('<' | '>') => ObjectKind::Bracket('<', '>'),
        Key::Char('t') => ObjectKind::Tag,
        Key::Char('p') => ObjectKind::Paragraph,
        _ => return ObjectParse::Invalid,
    };
    ObjectParse::Done(Object { kind, inner })
}

enum MotionParse {
    Pending,
    Invalid,
    Search(bool),
    Done(Motion),
}

/// The motion starting at `keys[i]`.
fn parse_motion(keys: &[Key], i: usize) -> MotionParse {
    let Some(key) = keys.get(i) else {
        return MotionParse::Pending;
    };
    let next = keys.get(i + 1);
    let motion = match key {
        Key::Char('h') | Key::Left => Motion::Left,
        Key::Char('l') | Key::Right => Motion::Right,
        Key::Backspace => Motion::LeftWrap,
        Key::Char(' ') => Motion::RightWrap,
        Key::Char('j') | Key::Down => Motion::Down,
        Key::Char('k') | Key::Up => Motion::Up,
        Key::Char('w') => Motion::WordStart { big: false },
        Key::Char('W') => Motion::WordStart { big: true },
        Key::Char('e') => Motion::WordEnd { big: false },
        Key::Char('E') => Motion::WordEnd { big: true },
        Key::Char('b') => Motion::WordBack { big: false },
        Key::Char('B') => Motion::WordBack { big: true },
        Key::Char('0') | Key::Home => Motion::LineStart,
        Key::Char('^') => Motion::FirstNonBlank,
        Key::Char('$') | Key::End => Motion::LineEnd,
        Key::Char('+') | Key::Enter => Motion::NextLine,
        Key::Char('-') => Motion::PrevLine,
        Key::Char('|') => Motion::Column,
        Key::Char('G') => Motion::LastLine,
        Key::Char(';') => Motion::RepeatFind { reverse: false },
        Key::Char(',') => Motion::RepeatFind { reverse: true },
        Key::Char('%') => Motion::Match,
        Key::Char('}') => Motion::ParagraphForward,
        Key::Char('{') => Motion::ParagraphBack,
        Key::Char('H') => Motion::ScreenTop,
        Key::Char('M') => Motion::ScreenMiddle,
        Key::Char('L') => Motion::ScreenBottom,
        Key::Ctrl('d') => Motion::HalfPageDown,
        Key::Ctrl('u') => Motion::HalfPageUp,
        Key::Ctrl('f') | Key::PageDown => Motion::PageDown,
        Key::Ctrl('b') | Key::PageUp => Motion::PageUp,
        Key::Char('n') => Motion::SearchNext { reverse: false },
        Key::Char('N') => Motion::SearchNext { reverse: true },
        Key::Char('*') => Motion::SearchWord { forward: true },
        Key::Char('#') => Motion::SearchWord { forward: false },
        Key::Char('/') => return MotionParse::Search(true),
        Key::Char('?') => return MotionParse::Search(false),
        Key::Search { forward, pattern } => Motion::Search {
            forward: *forward,
            pattern: pattern.clone(),
        },
        Key::Char(c @ ('f' | 'F' | 't' | 'T')) => {
            let ch = match next {
                None => return MotionParse::Pending,
                Some(Key::Char(ch)) => *ch,
                Some(Key::Tab) => '\t',
                Some(_) => return MotionParse::Invalid,
            };
            Motion::Find {
                ch,
                forward: c.is_ascii_lowercase(),
                till: c.eq_ignore_ascii_case(&'t'),
            }
        }
        Key::Char(c @ ('\'' | '`')) => match next {
            None => return MotionParse::Pending,
            Some(Key::Char(ch)) if ch.is_ascii_lowercase() || "<>'`.^[]".contains(*ch) => {
                Motion::Mark {
                    ch: *ch,
                    line: *c == '\'',
                }
            }
            Some(_) => return MotionParse::Invalid,
        },
        Key::Char('g') => match next {
            None => return MotionParse::Pending,
            Some(Key::Char('g')) => Motion::FirstLine,
            Some(Key::Char('e')) => Motion::WordEndBack { big: false },
            Some(Key::Char('E')) => Motion::WordEndBack { big: true },
            Some(Key::Char('j') | Key::Down) => Motion::RowDown,
            Some(Key::Char('k') | Key::Up) => Motion::RowUp,
            Some(Key::Char('_')) => Motion::LastNonBlank,
            Some(_) => return MotionParse::Invalid,
        },
        _ => return MotionParse::Invalid,
    };
    MotionParse::Done(motion)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(text: &str) -> Vec<Key> {
        text.chars().map(Key::Char).collect()
    }

    fn command(text: &str) -> Command {
        match parse(&keys(text), false, false) {
            Parsed::Done(command) => command,
            other => panic!("{text}: {other:?}"),
        }
    }

    #[test]
    fn counts_multiply_and_registers_lead() {
        let c = command("\"a2d3w");
        assert_eq!(c.register, Some('a'));
        assert_eq!(c.count, Some(6));
        assert_eq!(
            c.action,
            Action::Operate(
                Operator::Delete,
                Target::Motion(Motion::WordStart { big: false })
            )
        );
        assert_eq!(command("10j").count, Some(10));
        assert_eq!(command("0").action, Action::Move(Motion::LineStart));
        assert_eq!(command("10G").count, Some(10));
    }

    #[test]
    fn prefixes_stay_pending_and_nonsense_is_invalid() {
        for pending in [
            "d", "2", "\"", "\"a", "g", "gu", "f", "di", "r", "dg", "m", "'", "z",
        ] {
            assert_eq!(
                parse(&keys(pending), false, false),
                Parsed::Pending,
                "{pending}"
            );
        }
        for invalid in ["dZ", "diZ", "gQ", "\"!", "zQ"] {
            assert_eq!(
                parse(&keys(invalid), false, false),
                Parsed::Invalid,
                "{invalid}"
            );
        }
        assert_eq!(
            parse(&keys("d/"), false, false),
            Parsed::Search { forward: true }
        );
    }

    #[test]
    fn doubled_operators_take_lines() {
        for text in [
            "dd", "cc", "yy", ">>", "<<", "==", "g~~", "g~g~", "guu", "gugu", "gUU", "gUgU",
        ] {
            assert!(
                matches!(command(text).action, Action::Operate(_, Target::Lines)),
                "{text}"
            );
        }
        assert_eq!(
            command("ci(").action,
            Action::Operate(
                Operator::Change,
                Target::Object(Object {
                    kind: ObjectKind::Bracket('(', ')'),
                    inner: true
                })
            )
        );
    }

    #[test]
    fn visual_mode_applies_operators_at_once() {
        let Parsed::Done(c) = parse(&keys("d"), true, false) else {
            panic!()
        };
        assert_eq!(
            c.action,
            Action::VisualOperate {
                op: Operator::Delete,
                lines: false
            }
        );
        let Parsed::Done(c) = parse(&keys("aw"), true, false) else {
            panic!()
        };
        assert!(matches!(c.action, Action::Select(_)));
    }

    #[test]
    fn q_stops_recording() {
        let Parsed::Done(c) = parse(&keys("q"), false, true) else {
            panic!()
        };
        assert_eq!(c.action, Action::Do(Cmd::Record(None)));
        assert_eq!(parse(&keys("q"), false, false), Parsed::Pending);
    }
}
