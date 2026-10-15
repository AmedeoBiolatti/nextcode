//! pytest helpers: the test under the cursor, and where tracebacks point.

/// Indentation width of a line (tabs count as one level of 8).
fn indent(line: &str) -> usize {
    line.chars()
        .take_while(|c| c.is_whitespace())
        .map(|c| if c == '\t' { 8 } else { 1 })
        .sum()
}

/// The name defined by a `def`/`async def`/`class` line.
fn definition(line: &str) -> Option<(&'static str, &str)> {
    let text = line.trim_start();
    let (kind, rest) = match text
        .strip_prefix("async def ")
        .or_else(|| text.strip_prefix("def "))
    {
        Some(rest) => ("def", rest),
        None => ("class", text.strip_prefix("class ")?),
    };
    let end = rest
        .find(|c: char| !(c.is_alphanumeric() || c == '_'))
        .unwrap_or(rest.len());
    Some((kind, &rest[..end]))
}

/// The pytest node id suffix (`TestThing::test_case`, `test_case`) of the
/// test function or class enclosing 0-based `line` in `source`, or `None`
/// outside any test.
pub fn test_at(source: &str, line: usize) -> Option<String> {
    let lines: Vec<&str> = source.lines().collect();
    let mut row = line.min(lines.len().checked_sub(1)?);
    // Blank lines take the indentation of the next code line.
    let mut limit = lines[row..]
        .iter()
        .find(|text| !text.trim().is_empty())
        .map_or(0, |text| indent(text) + 1);
    let mut chain = Vec::new();
    loop {
        let text = lines[row];
        if !text.trim().is_empty() && indent(text) < limit {
            if let Some((kind, name)) = definition(text) {
                chain.push((kind, name.to_owned()));
            }
            limit = indent(text);
            if limit == 0 {
                break;
            }
        }
        if row == 0 {
            break;
        }
        row -= 1;
    }
    chain.reverse();
    // pytest collects `test*` functions, inside `Test*` classes; anything
    // nested in a test function belongs to it.
    let mut parts = Vec::new();
    for (kind, name) in chain {
        match kind {
            "class" if name.starts_with("Test") => parts.push(name),
            "def" if name.starts_with("test") => {
                parts.push(name);
                break;
            }
            _ => break,
        }
    }
    (!parts.is_empty()).then(|| parts.join("::"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCE: &str = "import pytest

def helper():
    return 1

class TestMath:
    def test_add(self):
        x = 1

        assert x == 1

    def helper(self):
        pass

async def test_async():
    def inner():
        pass
    inner()
";

    #[test]
    fn the_test_under_the_cursor_is_named_like_pytest_does() {
        assert_eq!(test_at(SOURCE, 7).as_deref(), Some("TestMath::test_add"));
        // A blank line inside the test still belongs to it.
        assert_eq!(test_at(SOURCE, 8).as_deref(), Some("TestMath::test_add"));
        assert_eq!(test_at(SOURCE, 5).as_deref(), Some("TestMath"));
        assert_eq!(test_at(SOURCE, 12).as_deref(), Some("TestMath"));
        assert_eq!(test_at(SOURCE, 16).as_deref(), Some("test_async"));
        assert_eq!(test_at(SOURCE, 3), None);
        assert_eq!(test_at(SOURCE, 0), None);
    }
}
