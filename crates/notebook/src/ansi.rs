//! ANSI SGR colours in text outputs (tracebacks, coloured logs).

use std::ops::Range;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Color {
    /// 0-15: the terminal palette (8-15 bright); 16-255: the xterm cube
    /// and greys.
    Indexed(u8),
    Rgb(u8, u8, u8),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Style {
    pub fg: Option<Color>,
    pub bg: Option<Color>,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
}

/// Text without escape sequences, with the byte ranges of `text` that
/// have a non-default style (sorted, not overlapping).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Styled {
    pub text: String,
    pub spans: Vec<(Range<usize>, Style)>,
}

/// Applies SGR sequences (`ESC[…m`: reset, bold, dim, italic, underline,
/// 30-37/90-97/39, 40-47/100-107/49, 38/48;5;n and 38/48;2;r;g;b); other
/// CSI and OSC sequences are dropped.
pub fn parse(input: &str) -> Styled {
    let mut out = Styled {
        text: String::with_capacity(input.len()),
        spans: Vec::new(),
    };
    let mut style = Style::default();
    let bytes = input.as_bytes();
    let mut i = 0;
    let mut run_start = 0;
    while i < bytes.len() {
        let Some(offset) = input[i..].find('\x1b') else {
            out.text.push_str(&input[i..]);
            break;
        };
        out.text.push_str(&input[i..i + offset]);
        i += offset + 1;
        match bytes.get(i) {
            Some(b'[') => {
                i += 1;
                let params_start = i;
                // Parameter and intermediate bytes, then a final byte in 0x40..=0x7e.
                while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                    i += 1;
                }
                if i >= bytes.len() {
                    break;
                }
                let final_byte = bytes[i];
                let params = &input[params_start..i];
                i += 1;
                if final_byte == b'm' {
                    let next = apply_sgr(style, params);
                    if next != style {
                        close_run(&mut out, run_start, style);
                        run_start = out.text.len();
                        style = next;
                    }
                }
            }
            Some(b']') => {
                // OSC: up to BEL or ESC \.
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == 0x07 {
                        i += 1;
                        break;
                    }
                    if bytes[i] == 0x1b && bytes.get(i + 1) == Some(&b'\\') {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
            }
            Some(_) => {
                // nF escapes (`ESC ( B`): intermediates then a final
                // character; otherwise a two-character escape.
                while i < bytes.len() && (0x20..=0x2f).contains(&bytes[i]) {
                    i += 1;
                }
                i += input[i..].chars().next().map_or(0, char::len_utf8);
            }
            None => {}
        }
    }
    close_run(&mut out, run_start, style);
    out
}

fn close_run(out: &mut Styled, start: usize, style: Style) {
    let end = out.text.len();
    if end <= start || style == Style::default() {
        return;
    }
    if let Some((range, last)) = out.spans.last_mut()
        && range.end == start
        && *last == style
    {
        range.end = end;
        return;
    }
    out.spans.push((start..end, style));
}

fn apply_sgr(mut style: Style, params: &str) -> Style {
    let codes: Vec<u32> = if params.is_empty() {
        vec![0]
    } else {
        params
            .split([';', ':'])
            .map(|p| p.parse().unwrap_or(0))
            .collect()
    };
    let mut i = 0;
    while i < codes.len() {
        let code = codes[i];
        match code {
            0 => style = Style::default(),
            1 => style.bold = true,
            2 => style.dim = true,
            3 => style.italic = true,
            4 => style.underline = true,
            22 => {
                style.bold = false;
                style.dim = false;
            }
            23 => style.italic = false,
            24 => style.underline = false,
            30..=37 => style.fg = Some(Color::Indexed((code - 30) as u8)),
            39 => style.fg = None,
            40..=47 => style.bg = Some(Color::Indexed((code - 40) as u8)),
            49 => style.bg = None,
            90..=97 => style.fg = Some(Color::Indexed((code - 90 + 8) as u8)),
            100..=107 => style.bg = Some(Color::Indexed((code - 100 + 8) as u8)),
            38 | 48 => {
                let color = match codes.get(i + 1) {
                    Some(5) => {
                        let color = codes.get(i + 2).map(|&n| Color::Indexed(n.min(255) as u8));
                        i += 2;
                        color
                    }
                    Some(2) => {
                        let channel = |k: usize| codes.get(i + k).map(|&n| n.min(255) as u8);
                        let color = match (channel(2), channel(3), channel(4)) {
                            (Some(r), Some(g), Some(b)) => Some(Color::Rgb(r, g, b)),
                            _ => None,
                        };
                        i += 4;
                        color
                    }
                    _ => None,
                };
                if let Some(color) = color {
                    if code == 38 {
                        style.fg = Some(color);
                    } else {
                        style.bg = Some(color);
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }
    style
}

/// `input` without escape sequences.
pub fn strip(input: &str) -> String {
    parse(input).text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_has_no_spans() {
        let styled = parse("hello\nworld");
        assert_eq!(styled.text, "hello\nworld");
        assert!(styled.spans.is_empty());
    }

    #[test]
    fn ipython_traceback_colours() {
        // As IPython 8 writes a traceback line.
        let line = "\x1b[0;31m---------------------------------------------------------------------------\x1b[0m\n\x1b[0;31mZeroDivisionError\x1b[0m                         Traceback (most recent call last)\nCell \x1b[0;32mIn[1], line 1\x1b[0m\n\x1b[0;32m----> 1\x1b[0m \x1b[38;5;241;43m1\x1b[39;49m\x1b[38;5;241;43m/\x1b[39;49m\x1b[38;5;241;43m0\x1b[39;49m\n";
        let styled = parse(line);
        assert!(!styled.text.contains('\x1b'));
        assert!(styled.text.starts_with("-----"));
        let red = Style {
            fg: Some(Color::Indexed(1)),
            ..Style::default()
        };
        assert_eq!(styled.spans[0], (0..75, red));
        let zero = styled.text.find("ZeroDivisionError").unwrap();
        assert_eq!(styled.spans[1], (zero..zero + 17, red));
        let one = styled.text.find("1/0").unwrap();
        let grey_on_yellow = Style {
            fg: Some(Color::Indexed(241)),
            bg: Some(Color::Indexed(3)),
            ..Style::default()
        };
        // Adjacent runs with the same style merge.
        assert!(styled.spans.contains(&(one..one + 3, grey_on_yellow)));
    }

    #[test]
    fn attributes_rgb_and_bright() {
        let styled =
            parse("\x1b[1;3;4mA\x1b[22mB\x1b[0m\x1b[38;2;10;20;30;101mC\x1b[mD\x1b[2;94mE");
        assert_eq!(styled.text, "ABCDE");
        assert_eq!(
            styled.spans,
            vec![
                (
                    0..1,
                    Style {
                        bold: true,
                        italic: true,
                        underline: true,
                        ..Style::default()
                    }
                ),
                (
                    1..2,
                    Style {
                        italic: true,
                        underline: true,
                        ..Style::default()
                    }
                ),
                (
                    2..3,
                    Style {
                        fg: Some(Color::Rgb(10, 20, 30)),
                        bg: Some(Color::Indexed(9)),
                        ..Style::default()
                    }
                ),
                (
                    4..5,
                    Style {
                        fg: Some(Color::Indexed(12)),
                        dim: true,
                        ..Style::default()
                    }
                ),
            ]
        );
    }

    #[test]
    fn other_sequences_are_dropped() {
        assert_eq!(
            strip("a\x1b[2Kb\x1b]8;;http://x\x07link\x1b]8;;\x1b\\c\x1b(Bd"),
            "ablinkcd"
        );
        // Unterminated CSI at the end.
        assert_eq!(strip("abc\x1b[31"), "abc");
    }
}
