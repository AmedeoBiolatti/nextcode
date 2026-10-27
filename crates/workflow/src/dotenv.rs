//! `.env` files as Docker Compose and python-dotenv read them: `NAME=value`
//! lines (`export ` allowed before), `#` comments and blank lines; values
//! unquoted (a ` #` starts a comment), in single quotes (as written) or
//! double quotes (`\n`, `\t`, `\"`, `\\` escaped; may span lines).

/// The variables in `text`, in order (a later one wins); an error names
/// the line.
pub fn parse(text: &str) -> Result<Vec<(String, String)>, String> {
    let mut found = Vec::new();
    let mut lines = text.lines().enumerate();
    while let Some((number, line)) = lines.next() {
        let line = line.trim_start();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((name, value)) = line.split_once('=') else {
            return Err(format!("line {}: expected NAME=value", number + 1));
        };
        let name = name.trim();
        if name.is_empty()
            || name.starts_with(|c: char| c.is_ascii_digit())
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
        {
            return Err(format!(
                "line {}: `{name}` is not a variable name",
                number + 1
            ));
        }
        let value = value.trim_start();
        let value = if let Some(rest) = value.strip_prefix('"') {
            // Double quotes: escapes, and lines up to the closing quote.
            let mut raw = rest.to_owned();
            let close = loop {
                if let Some(at) = closing_quote(&raw) {
                    break at;
                }
                let Some((_, more)) = lines.next() else {
                    return Err(format!("line {}: unclosed `\"`", number + 1));
                };
                raw.push('\n');
                raw.push_str(more);
            };
            unescape(&raw[..close])
        } else if let Some(rest) = value.strip_prefix('\'') {
            match rest.find('\'') {
                Some(close) => rest[..close].to_owned(),
                None => return Err(format!("line {}: unclosed `'`", number + 1)),
            }
        } else {
            value
                .split(" #")
                .next()
                .unwrap_or_default()
                .trim_end()
                .to_owned()
        };
        found.push((name.to_owned(), value));
    }
    Ok(found)
}

/// The byte index of the first `"` not escaped by a backslash.
fn closing_quote(text: &str) -> Option<usize> {
    let mut escaped = false;
    for (at, c) in text.char_indices() {
        match c {
            '\\' if !escaped => escaped = true,
            '"' if !escaped => return Some(at),
            _ => escaped = false,
        }
    }
    None
}

fn unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_quotes_comments_and_exports() {
        let text = "# settings\nA=1\nexport B = two words # note\nC='single # kept'\nD=\"line\\nnext \\\"q\\\"\"\nE=\"multi\nline\"\n\nF=\nURL=http://x/#frag\n";
        assert_eq!(
            parse(text).unwrap(),
            [
                ("A".into(), "1".into()),
                ("B".into(), "two words".into()),
                ("C".into(), "single # kept".into()),
                ("D".into(), "line\nnext \"q\"".into()),
                ("E".into(), "multi\nline".into()),
                ("F".into(), String::new()),
                ("URL".into(), "http://x/#frag".into()),
            ]
        );
        assert!(
            parse("just text")
                .unwrap_err()
                .contains("line 1: expected NAME=value")
        );
        assert!(parse("1A=x").unwrap_err().contains("not a variable name"));
        assert!(parse("A=\"open").unwrap_err().contains("unclosed"));
    }
}
