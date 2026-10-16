//! Where things are in a workflow file's text: each step object's byte
//! range, its `"id"`, the ids its `needs` and `after` list, and the steps
//! of a group inside it; each input object's range, `"id"` and keys. Read
//! from the raw text with a tolerant scanner (comments, trailing commas,
//! unfinished files), so an editor can map steps to lines and back even
//! while the file does not parse.

/// One element of a `steps` array (the top level's or a group's), in
/// file order.
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
    /// The elements of its own `steps` array (a group's), in order.
    pub steps: Vec<StepSpan>,
}

impl StepSpan {
    /// Where its `"id"` is, else where it starts.
    pub fn id_or_start(&self) -> usize {
        self.id_offset.unwrap_or(self.start)
    }
}

/// One element of the top-level `inputs` array, in file order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputSpan {
    /// Byte range of the object, braces included (to the end of the text
    /// when it is not closed).
    pub start: usize,
    pub end: usize,
    /// The `id` value, if it is a string.
    pub id: Option<String>,
    /// Each key of the object and where it starts.
    pub keys: Vec<(String, usize)>,
}

impl InputSpan {
    /// Where key `key` is, else where the object starts.
    pub fn key_or_start(&self, key: &str) -> usize {
        self.keys
            .iter()
            .find(|(known, _)| known == key)
            .map_or(self.start, |(_, at)| *at)
    }
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
    /// A `steps` array: the top level's (`None`) or a step's.
    Steps(Option<usize>),
    Step(usize),
    Needs(usize),
    After(usize),
    /// The top-level `inputs` array.
    Inputs,
    Input(usize),
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
/// element, objects or not, so indices match the array's), each with the
/// elements of its own `steps` (a group's) nested the same way.
pub fn step_spans(text: &str) -> Vec<StepSpan> {
    scan(text).0
}

/// The elements of the top-level `inputs` array, in order (one per
/// element, objects or not).
pub fn input_spans(text: &str) -> Vec<InputSpan> {
    scan(text).1
}

/// [`step_spans`] and [`input_spans`] in one pass.
pub(crate) fn scan(text: &str) -> (Vec<StepSpan>, Vec<InputSpan>) {
    let bytes = text.as_bytes();
    // Every span, in the order they start, with its parent's index.
    let mut spans: Vec<(StepSpan, Option<usize>)> = Vec::new();
    let mut inputs: Vec<InputSpan> = Vec::new();
    let mut stack: Vec<Frame> = Vec::new();
    let mut i = 0;

    // The role of a value starting at `start`; takes the parent object's
    // key, and opens a span for an element of a `steps` or `inputs`
    // array.
    fn begin_value(
        stack: &mut [Frame],
        spans: &mut Vec<(StepSpan, Option<usize>)>,
        inputs: &mut Vec<InputSpan>,
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
            (Role::Root, true, Some("steps")) => Role::Steps(None),
            (Role::Root, true, Some("inputs")) => Role::Inputs,
            (Role::Inputs, false, _) => {
                inputs.push(InputSpan {
                    start,
                    end,
                    id: None,
                    keys: Vec::new(),
                });
                Role::Input(inputs.len() - 1)
            }
            (Role::Step(index), true, Some("steps")) => Role::Steps(Some(index)),
            (Role::Steps(parent), false, _) => {
                spans.push((
                    StepSpan {
                        start,
                        end,
                        id: None,
                        id_offset: None,
                        needs: Vec::new(),
                        after: Vec::new(),
                        steps: Vec::new(),
                    },
                    parent,
                ));
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
                let (role, _) = begin_value(&mut stack, &mut spans, &mut inputs, i, text.len());
                stack.push(Frame {
                    object,
                    role,
                    key: None,
                    expecting_key: object,
                });
                i += 1;
            }
            b'}' | b']' => {
                match stack.pop().map(|frame| frame.role) {
                    Some(Role::Step(index)) => spans[index].0.end = i + 1,
                    Some(Role::Input(index)) => inputs[index].end = i + 1,
                    _ => {}
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
                    if let Role::Input(index) = top.role {
                        inputs[index].keys.push((value.clone(), start));
                    }
                    top.key = Some((value, start));
                    top.expecting_key = false;
                    continue;
                }
                let parent = stack.last().map(|top| top.role);
                let (role, key) = begin_value(&mut stack, &mut spans, &mut inputs, start, i);
                match (parent, role, key) {
                    (Some(Role::Input(index)), _, Some((key, _))) if key == "id" => {
                        inputs[index].id = Some(value);
                    }
                    (Some(Role::Step(index)), _, Some((key, key_at))) if key == "id" => {
                        spans[index].0.id = Some(value);
                        spans[index].0.id_offset = Some(key_at);
                    }
                    (_, Role::Needs(index), _) => spans[index].0.needs.push((value, start)),
                    (_, Role::After(index), _) => spans[index].0.after.push((value, start)),
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
                begin_value(&mut stack, &mut spans, &mut inputs, start, i);
            }
        }
    }
    // Children start after their parents: nest them from the last one.
    let mut top = Vec::new();
    let mut slots: Vec<Option<StepSpan>> = Vec::with_capacity(spans.len());
    let parents: Vec<Option<usize>> = spans.iter().map(|(_, parent)| *parent).collect();
    slots.extend(spans.into_iter().map(|(span, _)| Some(span)));
    for index in (0..slots.len()).rev() {
        let span = slots[index].take().expect("each span is moved once");
        match parents[index] {
            Some(parent) => slots[parent]
                .as_mut()
                .expect("a parent comes before its children")
                .steps
                .insert(0, span),
            None => top.insert(0, span),
        }
    }
    (top, inputs)
}

/// The span at `path` (indices in the top-level `steps`, then in each
/// group's).
pub fn span_at<'a>(spans: &'a [StepSpan], path: &[usize]) -> Option<&'a StepSpan> {
    let (first, rest) = path.split_first()?;
    let span = spans.get(*first)?;
    if rest.is_empty() {
        Some(span)
    } else {
        span_at(&span.steps, rest)
    }
}

/// The steps whose objects contain byte `offset`, from the top-level one
/// down to the innermost (empty: none).
pub fn step_at(spans: &[StepSpan], offset: usize) -> Vec<&StepSpan> {
    let mut chain = Vec::new();
    let mut level = spans;
    while let Some(span) = level
        .iter()
        .find(|span| span.start <= offset && offset < span.end.max(span.start + 1))
    {
        chain.push(span);
        level = &span.steps;
    }
    chain
}

/// The span of the step with qualified id `path` (`group/child`).
pub fn span_of<'a>(spans: &'a [StepSpan], path: &str) -> Option<&'a StepSpan> {
    let mut level = spans;
    let mut found = None;
    for id in path.split('/') {
        let span = level.iter().find(|span| span.id.as_deref() == Some(id))?;
        found = Some(span);
        level = &span.steps;
    }
    found
}

/// The qualified id (`group/child`) of the innermost step whose object
/// contains byte `offset`; `None` outside every step or when a step on
/// the way has no id.
pub fn path_at(spans: &[StepSpan], offset: usize) -> Option<String> {
    let chain = step_at(spans, offset);
    if chain.is_empty() {
        return None;
    }
    let ids: Option<Vec<&str>> = chain.iter().map(|span| span.id.as_deref()).collect();
    Some(ids?.join("/"))
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
        let chain = step_at(&spans, inside_b);
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].id.as_deref(), Some("b"));
        assert_eq!(path_at(&spans, inside_b).as_deref(), Some("b"));
        assert!(step_at(&spans, FILE.find("\"name\"").unwrap()).is_empty());
        assert_eq!(path_at(&spans, 0), None);
    }

    #[test]
    fn groups_nest_their_steps() {
        let text = r#"{"steps": [
  {"id": "svc", "needs": ["x"], "steps": [
    {"id": "build", "run": "b"},
    {"id": "inner", "steps": [{"id": "deep", "after": "build", "run": "d"}]}
  ]},
  {"id": "x", "env": {"steps": [{"id": "no"}]}, "run": "x"}
]}"#;
        let spans = step_spans(text);
        let ids = |spans: &[StepSpan]| -> Vec<Option<String>> {
            spans.iter().map(|span| span.id.clone()).collect()
        };
        assert_eq!(ids(&spans), [Some("svc".into()), Some("x".into())]);
        assert_eq!(
            ids(&spans[0].steps),
            [Some("build".into()), Some("inner".into())]
        );
        assert!(spans[1].steps.is_empty(), "`steps` in env is not a group");
        assert_eq!(spans[0].needs[0].0, "x");
        let deep = span_at(&spans, &[0, 1, 0]).unwrap();
        assert_eq!(deep.id.as_deref(), Some("deep"));
        assert_eq!(deep.after[0].0, "build");
        assert_eq!(span_of(&spans, "svc/inner/deep"), Some(deep));
        assert_eq!(span_of(&spans, "svc/deep"), None);
        let inside = text.find("\"d\"").unwrap();
        assert_eq!(path_at(&spans, inside).as_deref(), Some("svc/inner/deep"));
        let chain: Vec<_> = step_at(&spans, inside)
            .iter()
            .map(|span| span.id.clone().unwrap())
            .collect();
        assert_eq!(chain, ["svc", "inner", "deep"]);
        // On the group's own text: the group.
        let own = text.find("\"needs\"").unwrap();
        assert_eq!(path_at(&spans, own).as_deref(), Some("svc"));
        assert_eq!(&text[deep.start..deep.end].chars().last(), &Some('}'));
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
    fn inputs_are_mapped_with_their_keys() {
        let text = r#"{
  "inputs": [
    { "id": "env", "type": "choice", "default": "qa" },
    "stray",
    { "type": "number", "options": { "id": "inner" } }
  ],
  "steps": [{ "id": "a", "env": { "inputs": [{ "id": "no" }] } }]
}"#;
        let inputs = input_spans(text);
        assert_eq!(inputs.len(), 3);
        assert_eq!(inputs[0].id.as_deref(), Some("env"));
        let keys: Vec<&str> = inputs[0].keys.iter().map(|(key, _)| key.as_str()).collect();
        assert_eq!(keys, ["id", "type", "default"]);
        assert!(text[inputs[0].key_or_start("default")..].starts_with("\"default\""));
        assert!(text[inputs[0].start..inputs[0].end].ends_with('}'));
        assert_eq!(&text[inputs[1].start..inputs[1].end], "\"stray\"");
        // Nested objects' ids are not the input's.
        assert_eq!(inputs[2].id, None);
        assert_eq!(inputs[2].key_or_start("missing"), inputs[2].start);
        // The steps are mapped as before.
        assert_eq!(step_spans(text)[0].id.as_deref(), Some("a"));
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
