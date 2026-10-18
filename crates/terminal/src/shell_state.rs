//! What the shell reports about itself in OSC sequences the emulator
//! ignores: its directory (OSC 7 `file://host/path`, VS Code's
//! `633;P;Cwd=`, Windows Terminal's `9;9;"path"`) and the last command's
//! exit code (FinalTerm/VS Code shell integration `133;D;<code>` /
//! `633;D;<code>`). A byte scanner on the reader thread: no allocation
//! outside OSC payloads, which are capped.

/// What the shell last reported.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShellState {
    /// The current directory, as the shell names it.
    pub cwd: Option<String>,
    /// The last finished command's exit code.
    pub last_exit: Option<i32>,
}

/// Longest OSC payload kept; longer ones (images, clipboard) are skipped.
const MAX_PAYLOAD: usize = 4096;

#[derive(Debug, Default)]
enum Mode {
    #[default]
    Ground,
    Escape,
    /// Inside `ESC ]`; `overflow` once the payload passed the cap.
    Osc {
        overflow: bool,
    },
    /// `ESC` inside an OSC: `\` ends it.
    OscEscape {
        overflow: bool,
    },
}

#[derive(Debug, Default)]
pub struct ShellScanner {
    mode: Mode,
    payload: Vec<u8>,
    pub state: ShellState,
}

impl ShellScanner {
    /// Scans `data`; true when the state changed.
    pub fn feed(&mut self, data: &[u8]) -> bool {
        let mut changed = false;
        if matches!(self.mode, Mode::Ground) && !data.contains(&0x1b) {
            return false;
        }
        for &byte in data {
            self.mode = match std::mem::take(&mut self.mode) {
                // C1 forms (0x9d/0x9c) are not handled: they are UTF-8 continuation bytes.
                Mode::Ground if byte == 0x1b => Mode::Escape,
                Mode::Ground => Mode::Ground,
                Mode::Escape if byte == b']' => self.start(),
                Mode::Escape if byte == 0x1b => Mode::Escape,
                Mode::Escape => Mode::Ground,
                Mode::Osc { overflow } => match byte {
                    0x07 => {
                        changed |= self.finish(overflow);
                        Mode::Ground
                    }
                    0x1b => Mode::OscEscape { overflow },
                    _ => Mode::Osc {
                        overflow: self.push(overflow, byte),
                    },
                },
                Mode::OscEscape { overflow } => {
                    if byte == b'\\' {
                        changed |= self.finish(overflow);
                        Mode::Ground
                    } else if byte == b']' {
                        // Unterminated OSC followed by a new one.
                        self.start()
                    } else {
                        Mode::Escape
                    }
                }
            };
        }
        changed
    }

    fn start(&mut self) -> Mode {
        self.payload.clear();
        Mode::Osc { overflow: false }
    }

    fn push(&mut self, overflow: bool, byte: u8) -> bool {
        if overflow || self.payload.len() >= MAX_PAYLOAD {
            return true;
        }
        self.payload.push(byte);
        false
    }

    fn finish(&mut self, overflow: bool) -> bool {
        if overflow {
            return false;
        }
        let Ok(payload) = std::str::from_utf8(&self.payload) else {
            return false;
        };
        let before = self.state.clone();
        if let Some(url) = payload.strip_prefix("7;") {
            if let Some(path) = file_url_path(url) {
                self.state.cwd = Some(path);
            }
        } else if let Some(path) = payload.strip_prefix("633;P;Cwd=") {
            self.state.cwd = Some(unescape_633(path));
        } else if let Some(path) = payload.strip_prefix("9;9;") {
            self.state.cwd = Some(path.trim_matches('"').to_owned());
        } else if let Some(rest) = payload
            .strip_prefix("133;D")
            .or_else(|| payload.strip_prefix("633;D"))
        {
            // `D` alone: no command ran (an empty line); keep the last code.
            if let Some(code) = rest.strip_prefix(';') {
                let code = code.split(';').next().unwrap_or_default();
                if let Ok(code) = code.trim().parse() {
                    self.state.last_exit = Some(code);
                }
            }
        }
        self.state != before
    }
}

/// The path of a `file://host/path` URL, percent-decoded (`/C:/x` → `C:/x`).
fn file_url_path(url: &str) -> Option<String> {
    let rest = url.strip_prefix("file://")?;
    let path = &rest[rest.find('/')?..];
    let path = percent_decode(path);
    let bytes = path.as_bytes();
    if bytes.len() >= 3 && bytes[0] == b'/' && bytes[1].is_ascii_alphabetic() && bytes[2] == b':' {
        return Some(path[1..].to_owned());
    }
    Some(path)
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(hex) = text.get(i + 1..i + 3)
            && let Ok(byte) = u8::from_str_radix(hex, 16)
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// VS Code's `633` values escape `\` and bytes below 0x20 as `\xNN`.
fn unescape_633(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('\\') {
        out.push_str(&rest[..at]);
        let tail = &rest[at..];
        if let Some(hex) = tail.strip_prefix("\\x").and_then(|t| t.get(..2))
            && let Ok(byte) = u8::from_str_radix(hex, 16)
        {
            out.push(byte as char);
            rest = &tail[4..];
        } else if let Some(after) = tail.strip_prefix("\\\\") {
            out.push('\\');
            rest = after;
        } else {
            out.push('\\');
            rest = &tail[1..];
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(chunks: &[&[u8]]) -> ShellState {
        let mut scanner = ShellScanner::default();
        for chunk in chunks {
            scanner.feed(chunk);
        }
        scanner.state
    }

    #[test]
    fn osc7_sets_the_directory_split_across_reads() {
        let state = scan(&[b"out\x1b]7;file://box/home/me/my%20dir", b"\x07$ "]);
        assert_eq!(state.cwd.as_deref(), Some("/home/me/my dir"));
        // ST terminator, Windows drive.
        let state = scan(&[b"\x1b]7;file://pc/C:/Users/me\x1b\\"]);
        assert_eq!(state.cwd.as_deref(), Some("C:/Users/me"));
        let state = scan(&[b"\x1b]7;file:///tmp/x\x07"]);
        assert_eq!(state.cwd.as_deref(), Some("/tmp/x"));
    }

    #[test]
    fn other_directory_reports() {
        let state = scan(&[b"\x1b]633;P;Cwd=/srv/a\\x3bb\x07"]);
        assert_eq!(state.cwd.as_deref(), Some("/srv/a;b"));
        let state = scan(&[b"\x1b]9;9;\"C:\\work\"\x1b\\"]);
        assert_eq!(state.cwd.as_deref(), Some("C:\\work"));
    }

    #[test]
    fn command_finished_reports_the_exit_code() {
        let mut scanner = ShellScanner::default();
        assert!(scanner.feed(b"\x1b]133;A\x07$ false\r\n\x1b]133;D;1\x07"));
        assert_eq!(scanner.state.last_exit, Some(1));
        // `D` without a code (empty line) keeps it; 633 works too.
        assert!(!scanner.feed(b"\x1b]133;D\x07"));
        assert_eq!(scanner.state.last_exit, Some(1));
        scanner.feed(b"\x1b]633;D;0\x07");
        assert_eq!(scanner.state.last_exit, Some(0));
        // `133;D;<code>;aid=…` (kitty / iTerm2 extras).
        scanner.feed(b"\x1b]133;D;130;aid=7\x1b\\");
        assert_eq!(scanner.state.last_exit, Some(130));
    }

    #[test]
    fn titles_colors_and_huge_payloads_are_ignored() {
        let mut big = b"\x1b]52;c;".to_vec();
        big.extend(std::iter::repeat_n(b'A', MAX_PAYLOAD * 2));
        big.extend(b"\x07\x1b]0;title\x07\x1b[31mred\x1b]7;file://h/ok\x07");
        let state = scan(&[&big]);
        assert_eq!(
            state,
            ShellState {
                cwd: Some("/ok".into()),
                last_exit: None
            }
        );
    }
}
