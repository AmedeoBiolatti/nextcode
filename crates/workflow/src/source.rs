//! Where things are in a workflow file's text: each step object's byte
//! range, its `"id"` and the ids its `needs` and `after` list. Read from
//! the raw text with a tolerant scanner (comments, trailing commas,
//! unfinished files), so an editor can map steps to lines and back even
//! while the file does not parse.

/// One element of the top-level `steps` array, in file order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepSpan {
    /// Byte range of the object, braces included (to the end of the text
    /// when it is not closed).
    pub start: usize,
    pub end: usize,
    /// The `id` value, if it is a string.
    pub id: Option<String>,
    /// Where the `"id"` key starts.
    pub id_offset: Option<usize>,
    /// Each string in `needs` and where it starts.
    pub needs: Vec<(String, usize)>,
    /// Each string in `after` and where it starts.
    pub after: Vec<(String, usize)>,
}

/// A problem in a workflow file and where it is, when known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    pub message: String,
    /// Byte offset in the text.
    pub offset: Option<usize>,
}

impl Problem {
    /// 1-based line and column (in characters) of the problem in `text`.
    pub fn line_column(&self, text: &str) -> Option<(usize, usize)> {
        self.offset.map(|offset| line_column(text, offset))
    }
}

/// 1-based line and character column of byte `offset` in `text`.
pub fn line_column(text: &str, offset: usize) -> (usize, usize) {
    let mut offset = offset.min(text.len());
    while !text.is_char_boundary(offset) {
        offset -= 1;
    }
    let before = &text[..offset];
    let line_start = before.rfind('\n').map_or(0, |at| at + 1);
    (
        before.matches('\n').count() + 1,
        before[line_start..].chars().count() + 1,
    )
}

/// Byte offset of 1-based `line` and byte `column` (as `serde_json`
/// reports them), clamped to the text.
pub fn offset_of(text: &str, line: usize, column: usize) -> usize {
    let mut start = 0;
    for _ in 1..line {
        match text[start..].find('\n') {
            Some(at) => start += at + 1,
            None => return text.len(),
        }
    }
    let line_end = text[start..].find('\n').map_or(text.len(), |at| start + at);
    let mut offset = (start + column.saturating_sub(1)).min(line_end);
    while !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    Root,
    Steps,
    Step(usize),
    Needs(usize),
    After(usize),
    Other,
}

struct Frame {
    object: bool,
    role: Role,
    /// Objects: the key whose value comes next, and where it starts.
    key: Option<(String, usize)>,
    expecting_key: bool,
}

/// The elements of the top-level `steps` array, in order (one per
/// element, objects or not, so indices match the array's).
pub fn step_spans(text: &str) -> Vec<StepSpan> {
    let bytes = text.as_bytes();
    let mut spans: Vec<StepSpan> = Vec::new();
    let mut stack: Vec<Frame> = Vec::new();
    let mut i = 0;

    // The role of a value starting at `start`; takes the parent object's
    // key, and opens a span for an element of `steps`.
    fn begin_value(
        stack: &mut [Frame],
        spans: &mut Vec<StepSpan>,
        start: usize,
        end: usize,
    ) -> (Role, Option<(String, usize)>) {
        let Some(top) = stack.last_mut() else {
            return (Role::Root, None);
        };
        let key = if top.object { top.key.take() } else { None };
        let role = match (
            top.role,
            top.object,
            key.as_ref().map(|(key, _)| key.as_str()),
        ) {
            (Role::Root, true, Some("steps")) => Role::Steps,
            (Role::Steps, false, _) => {
                spans.push(StepSpan {
                    start,
                    end,
                    id: None,
                    id_offset: None,
                    needs: Vec::new(),
                    after: Vec::new(),
                });
                Role::Step(spans.len() - 1)
            }
            (Role::Step(index), true, Some("needs")) => Role::Needs(index),
            (Role::Step(index), true, Some("after")) => Role::After(index),
            (role, _, _) => role_inside(role),
        };
        (role, key)
    }

    // What a value nested in a frame of `role` is when nothing names it.
    fn role_inside(role: Role) -> Role {
        match role {
            Role::Needs(index) => Role::Needs(index),
            Role::After(index) => Role::After(index),
            _ => Role::Other,
        }
    }

    while i < bytes.len() {
        match bytes[i] {
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i < bytes.len() && !(bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/')) {
                    i += 1;
                }
                i = (i + 2).min(bytes.len());
            }
            b'{' | b'[' => {
                let object = bytes[i] == b'{';
                let (role, _) = begin_value(&mut stack, &mut spans, i, text.len());
                stack.push(Frame {
                    object,
                    role,
                    key: None,
                    expecting_key: object,
                });
                i += 1;
            }
            b'}' | b']' => {
                if let Some(frame) = stack.pop()
                    && let Role::Step(index) = frame.role
                {
                    spans[index].end = i + 1;
                }
                i += 1;
            }
            b',' => {
                if let Some(top) = stack.last_mut()
                    && top.object
                {
                    top.expecting_key = true;
                    top.key = None;
                }
                i += 1;
            }
            b'"' => {
                let start = i;
                i += 1;
                while i < bytes.len() && bytes[i] != b'"' && bytes[i] != b'\n' {
                    if bytes[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
                i = (i + 1).min(bytes.len());
                let raw = &text[start..i];
                let value = serde_json::from_str::<String>(raw)
                    .unwrap_or_else(|_| raw.trim_matches('"').to_owned());
                if let Some(top) = stack.last_mut()
                    && top.object
                    && top.expecting_key
                {
                    top.key = Some((value, start));
                    top.expecting_key = false;
                    continue;
                }
                let parent = stack.last().map(|top| top.role);
                let (role, key) = begin_value(&mut stack, &mut spans, start, i);
                match (parent, role, key) {
                    (Some(Role::Step(index)), _, Some((key, key_at))) if key == "id" => {
                        spans[index].id = Some(value);
                        spans[index].id_offset = Some(key_at);
                    }
                    (_, Role::Needs(index), _) => spans[index].needs.push((value, start)),
                    (_, Role::After(index), _) => spans[index].after.push((value, start)),
                    _ => {}
                }
            }
            byte if byte.is_ascii_whitespace() || byte == b':' => i += 1,
            _ => {
                // Numbers, true/false/null and stray text.
                let start = i;
                i += 1;
                while i < bytes.len()
                    && !matches!(bytes[i], b',' | b'}' | b']' | b'"' | b'/' | b'{' | b'[')
                    && !bytes[i].is_ascii_whitespace()
                {
                    i += 1;
                }
                begin_value(&mut stack, &mut spans, start, i);
            }
        }
    }
    spans
}

/// The step whose object contains byte `offset`.
pub fn step_at(spans: &[StepSpan], offset: usize) -> Option<&StepSpan> {
    spans
        .iter()
        .find(|span| span.start <= offset && offset < span.end.max(span.start + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE: &str = r#"{
  // The name.
  "name": "Dev",
  "steps": [
    { "id": "a", "run": "x", "env": { "id": "not a step" } },
    {
      /* block */ "title": "B \"quoted\"",
      "id": "b",
      "needs": ["a"],
      "ready": { "port": 80 },
    },
    { "id": "c", "needs": [ "a", "b" ], "run": "y" },
  ],
}
"#;

    #[test]
    fn steps_map_to_their_text_and_ids_to_lines() {
        let spans = step_spans(FILE);
        let ids: Vec<Option<&str>> = spans.iter().map(|span| span.id.as_deref()).collect();
        assert_eq!(ids, [Some("a"), Some("b"), Some("c")]);
        let line_of = |offset: usize| line_column(FILE, offset).0;
        assert_eq!(line_of(spans[0].id_offset.unwrap()), 5);
        assert_eq!(line_of(spans[1].id_offset.unwrap()), 8);
        assert_eq!(line_of(spans[1].start), 6);
        assert_eq!(line_of(spans[1].end - 1), 11);
        assert_eq!(
            &FILE[spans[0].start..spans[0].end].chars().last(),
            &Some('}')
        );
        let needs: Vec<&str> = spans[2].needs.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(needs, ["a", "b"]);
        assert_eq!(&FILE[spans[2].needs[1].1..spans[2].needs[1].1 + 3], "\"b\"");
        // The cursor anywhere inside a step's object finds it.
        let inside_b = FILE.find("\"ready\"").unwrap();
        assert_eq!(step_at(&spans, inside_b).unwrap().id.as_deref(), Some("b"));
        assert_eq!(step_at(&spans, FILE.find("\"name\"").unwrap()), None);
    }

    #[test]
    fn unfinished_text_still_maps_the_steps_so_far() {
        let text = "{ \"steps\": [ { \"id\": \"a\" }, { \"id\": \"b\", \"needs\": [\"a";
        let spans = step_spans(text);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[1].id.as_deref(), Some("b"));
        assert_eq!(spans[1].end, text.len());
        assert_eq!(spans[1].needs[0].0, "a");
        // Every element takes its index; nested `steps` keys do not count.
        let text = r#"{"steps": [1, {"id": "x", "env": {"steps": [{"id": "y"}]}}]}"#;
        let spans = step_spans(text);
        let ids: Vec<Option<&str>> = spans.iter().map(|span| span.id.as_deref()).collect();
        assert_eq!(ids, [None, Some("x")]);
        assert_eq!(&text[spans[0].start..spans[0].end], "1");
    }

    #[test]
    fn line_columns_and_offsets_convert_both_ways() {
        let text = "ab\nçd\n";
        assert_eq!(line_column(text, 0), (1, 1));
        assert_eq!(line_column(text, 3), (2, 1));
        assert_eq!(line_column(text, 5), (2, 2), "characters, not bytes");
        assert_eq!(offset_of(text, 2, 3), 5);
        assert_eq!(offset_of(text, 9, 1), text.len());
        assert_eq!(offset_of(text, 1, 99), 2, "clamped to the line");
    }
}
