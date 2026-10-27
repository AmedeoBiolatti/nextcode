//! Just enough Markdown for hover and completion docs: fenced code, rules,
//! and paragraphs with inline markup stripped.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    Code {
        language: Option<String>,
        text: String,
    },
    Paragraph(String),
    Rule,
}

pub fn parse(markdown: &str) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut paragraph: Vec<String> = Vec::new();
    let mut lines = markdown.lines();
    let flush = |paragraph: &mut Vec<String>, blocks: &mut Vec<Block>| {
        if !paragraph.is_empty() {
            blocks.push(Block::Paragraph(paragraph.join("\n")));
            paragraph.clear();
        }
    };
    while let Some(line) = lines.next() {
        let trimmed = line.trim();
        if let Some(fence) = ["```", "~~~"]
            .into_iter()
            .find(|fence| trimmed.starts_with(fence))
        {
            flush(&mut paragraph, &mut blocks);
            let language = trimmed[fence.len()..].trim();
            let mut code = Vec::new();
            for line in lines.by_ref() {
                if line.trim_start().starts_with(fence) {
                    break;
                }
                code.push(line);
            }
            blocks.push(Block::Code {
                language: (!language.is_empty()).then(|| language.to_owned()),
                text: code.join("\n"),
            });
        } else if trimmed.is_empty() {
            flush(&mut paragraph, &mut blocks);
        } else if trimmed.len() >= 3
            && ["-", "*", "_"]
                .iter()
                .any(|c| trimmed.chars().all(|ch| ch.to_string() == *c))
        {
            flush(&mut paragraph, &mut blocks);
            blocks.push(Block::Rule);
        } else {
            paragraph.push(inline(line.trim_end()));
        }
    }
    flush(&mut paragraph, &mut blocks);
    blocks
}

/// Strips inline markup: backslash escapes, `code` ticks, `**bold**`,
/// `[links](url)` and heading hashes. Single `*`/`_` are kept because they
/// are common in identifiers (`__init__`, `*args`).
fn inline(line: &str) -> String {
    let line = line.trim_start_matches('#').trim_start_matches(' ');
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek().is_some_and(|next| next.is_ascii_punctuation()) => {
                out.extend(chars.next());
            }
            '`' => {}
            '*' if chars.peek() == Some(&'*') => {
                chars.next();
            }
            '[' => {
                let rest: String = chars.clone().collect();
                if let Some(close) = rest.find("](")
                    && let Some(end) = rest[close..].find(')')
                {
                    out.push_str(&rest[..close]);
                    for _ in 0..rest[..close + end + 1].chars().count() {
                        chars.next();
                    }
                } else {
                    out.push('[');
                }
            }
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pyright_hover_splits_into_signature_rule_and_docs() {
        let hover = "```python\n(function) def add(a: int, b: int) -> int\n```\n---\nAdd **two** numbers.\nSee `math.fsum` and [docs](https://x.y).\n\nReturns\\_value is `int`.";
        assert_eq!(
            parse(hover),
            [
                Block::Code {
                    language: Some("python".into()),
                    text: "(function) def add(a: int, b: int) -> int".into(),
                },
                Block::Rule,
                Block::Paragraph("Add two numbers.\nSee math.fsum and docs.".into()),
                Block::Paragraph("Returns_value is int.".into()),
            ]
        );
    }

    #[test]
    fn identifiers_with_underscores_and_stars_survive() {
        assert_eq!(
            parse("Call __init__(*args, **kwargs)"),
            [Block::Paragraph("Call __init__(*args, kwargs)".into())]
        );
    }

    #[test]
    fn unterminated_fence_runs_to_the_end() {
        assert_eq!(
            parse("```rust\nfn f() {}"),
            [Block::Code {
                language: Some("rust".into()),
                text: "fn f() {}".into(),
            }]
        );
    }
}
