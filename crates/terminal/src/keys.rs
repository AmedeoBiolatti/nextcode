//! Key and paste encoding: turns UI key presses into the bytes a terminal
//! application expects (xterm conventions).

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    /// A character typed together with Ctrl and/or Alt. Plain text input
    /// should be written directly instead.
    Char(char),
    Enter,
    Tab,
    Backspace,
    Escape,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    /// F1–F12.
    F(u8),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Modifiers {
    pub shift: bool,
    pub alt: bool,
    pub ctrl: bool,
}

impl Modifiers {
    /// The xterm modifier parameter (`1 + shift + 2·alt + 4·ctrl`), or `None`
    /// when no modifier is held.
    fn parameter(self) -> Option<u8> {
        let value = 1 + u8::from(self.shift) + 2 * u8::from(self.alt) + 4 * u8::from(self.ctrl);
        (value > 1).then_some(value)
    }
}

/// Bytes for `key`, or `None` if the combination sends nothing.
/// `app_cursor` is the terminal's DECCKM mode (set by full-screen apps).
pub fn encode_key(key: Key, mods: Modifiers, app_cursor: bool) -> Option<Vec<u8>> {
    let with_alt = |bytes: &[u8]| {
        let mut out = Vec::with_capacity(bytes.len() + 1);
        if mods.alt {
            out.push(0x1b);
        }
        out.extend_from_slice(bytes);
        out
    };
    // CSI sequences ending in a letter: `ESC [ A`, `ESC O A` in app mode,
    // `ESC [ 1 ; m A` with modifiers.
    let cursor_key = |letter: u8, app: bool| match mods.parameter() {
        Some(m) => format!("\x1b[1;{m}{}", letter as char).into_bytes(),
        None if app => vec![0x1b, b'O', letter],
        None => vec![0x1b, b'[', letter],
    };
    // `ESC [ n ~` with an optional `; m` modifier.
    let tilde_key = |n: u8| match mods.parameter() {
        Some(m) => format!("\x1b[{n};{m}~").into_bytes(),
        None => format!("\x1b[{n}~").into_bytes(),
    };
    Some(match key {
        Key::Char(c) if mods.ctrl => {
            let control = match c.to_ascii_lowercase() {
                c @ 'a'..='z' => c as u8 - b'a' + 1,
                '@' | ' ' | '2' => 0,
                '[' | '3' => 0x1b,
                '\\' | '4' => 0x1c,
                ']' | '5' => 0x1d,
                '^' | '6' => 0x1e,
                '_' | '-' | '7' => 0x1f,
                '?' | '8' => 0x7f,
                _ => return None,
            };
            with_alt(&[control])
        }
        Key::Char(c) => with_alt(c.encode_utf8(&mut [0; 4]).as_bytes()),
        Key::Enter => with_alt(b"\r"),
        Key::Tab if mods.shift => b"\x1b[Z".to_vec(),
        Key::Tab => with_alt(b"\t"),
        Key::Backspace if mods.ctrl => with_alt(&[0x08]),
        Key::Backspace => with_alt(&[0x7f]),
        Key::Escape => with_alt(&[0x1b]),
        Key::Up => cursor_key(b'A', app_cursor),
        Key::Down => cursor_key(b'B', app_cursor),
        Key::Right => cursor_key(b'C', app_cursor),
        Key::Left => cursor_key(b'D', app_cursor),
        Key::Home => cursor_key(b'H', app_cursor),
        Key::End => cursor_key(b'F', app_cursor),
        Key::Insert => tilde_key(2),
        Key::Delete => tilde_key(3),
        Key::PageUp => tilde_key(5),
        Key::PageDown => tilde_key(6),
        // F1–F4 use SS3 letters; the rest use numbered tilde sequences.
        Key::F(n @ 1..=4) => cursor_key(b'P' + n - 1, true),
        Key::F(n @ 5..=12) => tilde_key([15, 17, 18, 19, 20, 21, 23, 24][usize::from(n) - 5]),
        Key::F(_) => return None,
    })
}

/// Bytes for pasting `text`: line breaks become carriage returns, and in
/// bracketed-paste mode the text is wrapped so shells don't run it line by line.
pub fn encode_paste(text: &str, bracketed: bool) -> Vec<u8> {
    let normalized = text.replace("\r\n", "\r").replace('\n', "\r");
    if bracketed {
        // A pasted end marker would let the text escape the bracket.
        let normalized = normalized.replace("\x1b[201~", "");
        format!("\x1b[200~{normalized}\x1b[201~").into_bytes()
    } else {
        normalized.into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: Modifiers = Modifiers {
        shift: false,
        alt: false,
        ctrl: false,
    };
    const CTRL: Modifiers = Modifiers {
        shift: false,
        alt: false,
        ctrl: true,
    };

    fn enc(key: Key, mods: Modifiers) -> Vec<u8> {
        encode_key(key, mods, false).unwrap()
    }

    #[test]
    fn control_characters() {
        assert_eq!(enc(Key::Char('c'), CTRL), [0x03]);
        assert_eq!(enc(Key::Char('C'), CTRL), [0x03]);
        assert_eq!(enc(Key::Char('['), CTRL), [0x1b]);
        assert_eq!(enc(Key::Char(' '), CTRL), [0x00]);
        assert_eq!(enc(Key::Char('?'), CTRL), [0x7f]);
        assert_eq!(encode_key(Key::Char('é'), CTRL, false), None);
    }

    #[test]
    fn alt_prefixes_escape() {
        let alt = Modifiers { alt: true, ..NONE };
        assert_eq!(enc(Key::Char('b'), alt), b"\x1bb");
        assert_eq!(enc(Key::Backspace, alt), b"\x1b\x7f");
        assert_eq!(
            enc(
                Key::Char('x'),
                Modifiers {
                    alt: true,
                    ctrl: true,
                    ..NONE
                }
            ),
            [0x1b, 0x18]
        );
    }

    #[test]
    fn cursor_keys_follow_application_mode_and_modifiers() {
        assert_eq!(enc(Key::Up, NONE), b"\x1b[A");
        assert_eq!(encode_key(Key::Up, NONE, true).unwrap(), b"\x1bOA");
        // Modifiers always use the CSI form, even in application mode.
        assert_eq!(encode_key(Key::Left, CTRL, true).unwrap(), b"\x1b[1;5D");
        let shift_alt = Modifiers {
            shift: true,
            alt: true,
            ..NONE
        };
        assert_eq!(enc(Key::Home, shift_alt), b"\x1b[1;4H");
    }

    #[test]
    fn editing_and_function_keys() {
        assert_eq!(enc(Key::Delete, NONE), b"\x1b[3~");
        assert_eq!(enc(Key::PageDown, CTRL), b"\x1b[6;5~");
        assert_eq!(enc(Key::F(1), NONE), b"\x1bOP");
        assert_eq!(
            enc(
                Key::F(4),
                Modifiers {
                    shift: true,
                    ..NONE
                }
            ),
            b"\x1b[1;2S"
        );
        assert_eq!(enc(Key::F(5), NONE), b"\x1b[15~");
        assert_eq!(enc(Key::F(12), NONE), b"\x1b[24~");
        assert_eq!(
            enc(
                Key::Tab,
                Modifiers {
                    shift: true,
                    ..NONE
                }
            ),
            b"\x1b[Z"
        );
        assert_eq!(encode_key(Key::F(13), NONE, false), None);
    }

    #[test]
    fn pastes_use_carriage_returns_and_brackets() {
        assert_eq!(encode_paste("a\r\nb\nc", false), b"a\rb\rc");
        assert_eq!(encode_paste("ls\n", true), b"\x1b[200~ls\r\x1b[201~");
        assert_eq!(
            encode_paste("x\x1b[201~rm -rf /", true),
            b"\x1b[200~xrm -rf /\x1b[201~"
        );
    }
}
