//! Where things are in a workflow file's text: each step's byte range
//! (from its key to the end of its settings), the ids its `depends_on`
//! lists, and the steps of a group inside it; each input's range and
//! keys. Read from the YAML tree, which keeps what was read before a
//! syntax error, so an editor can map steps to lines and back even while
//! the file is being typed.

use crate::yaml::{self, Data, Node};

/// One step of a `steps` mapping (the top level's or a group's), in file
/// order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepSpan {
    /// Byte range from the step's key to the end of its value (to the end
    /// of the text when a syntax error cut it short).
    pub start: usize,
    pub end: usize,
    /// Its key.
    pub id: String,
    /// Each id its `depends_on` lists and where it is.
    pub depends_on: Vec<(String, usize)>,
    /// The steps of its own `steps` (a group's), in order.
    pub steps: Vec<StepSpan>,
}

impl StepSpan {
    /// Where its key (its id) is.
    pub fn id_or_start(&self) -> usize {
        self.start
    }
}

/// One input of the top-level `inputs`, in file order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputSpan {
    /// Byte range from the input's key to the end of its settings.
    pub start: usize,
    pub end: usize,
    /// Its key.
    pub id: String,
    /// Each key of its settings and where it starts.
    pub keys: Vec<(String, usize)>,
}

impl InputSpan {
    /// Where key `key` is, else where the input starts.
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

/// The top-level steps, in order, each with the steps of its group nested
/// the same way.
pub fn step_spans(text: &str) -> Vec<StepSpan> {
    yaml::load(text)
        .root
        .map(|root| spans_of(&root).0)
        .unwrap_or_default()
}

/// The top-level inputs, in order.
pub fn input_spans(text: &str) -> Vec<InputSpan> {
    yaml::load(text)
        .root
        .map(|root| spans_of(&root).1)
        .unwrap_or_default()
}

/// [`step_spans`] and [`input_spans`] of a loaded file.
pub(crate) fn spans_of(root: &Node) -> (Vec<StepSpan>, Vec<InputSpan>) {
    let steps = root.get("steps").map(steps_of).unwrap_or_default();
    let inputs = root
        .get("inputs")
        .and_then(Node::as_map)
        .map(|entries| {
            entries
                .iter()
                .map(|entry| InputSpan {
                    start: entry.key_start,
                    end: entry.end(),
                    id: entry.key.clone(),
                    keys: entry
                        .value
                        .as_map()
                        .unwrap_or_default()
                        .iter()
                        .map(|key| (key.key.clone(), key.key_start))
                        .collect(),
                })
                .collect()
        })
        .unwrap_or_default();
    (steps, inputs)
}

fn steps_of(node: &Node) -> Vec<StepSpan> {
    node.as_map()
        .unwrap_or_default()
        .iter()
        .map(|entry| StepSpan {
            start: entry.key_start,
            end: entry.end(),
            id: entry.key.clone(),
            depends_on: entry
                .value
                .get("depends_on")
                .map(dependencies_of)
                .unwrap_or_default(),
            steps: entry.value.get("steps").map(steps_of).unwrap_or_default(),
        })
        .collect()
}

/// The ids a `depends_on` lists, at the id (a list item or a key).
fn dependencies_of(node: &Node) -> Vec<(String, usize)> {
    match &node.data {
        Data::List(items) => items
            .iter()
            .filter_map(|item| Some((item.text()?.to_owned(), item.start)))
            .collect(),
        Data::Map(entries) => entries
            .iter()
            .map(|entry| (entry.key.clone(), entry.key_start))
            .collect(),
        _ => node
            .text()
            .map(|id| vec![(id.to_owned(), node.start)])
            .unwrap_or_default(),
    }
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

/// The steps whose text contains byte `offset`, from the top-level one
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
        let span = level.iter().find(|span| span.id == id)?;
        found = Some(span);
        level = &span.steps;
    }
    found
}

/// The qualified id (`group/child`) of the innermost step whose text
/// contains byte `offset`; `None` outside every step.
pub fn path_at(spans: &[StepSpan], offset: usize) -> Option<String> {
    let chain = step_at(spans, offset);
    if chain.is_empty() {
        return None;
    }
    let ids: Vec<&str> = chain.iter().map(|span| span.id.as_str()).collect();
    Some(ids.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE: &str = "# The name.
name: Dev
steps:
  a: {run: x, env: {steps: not a group}}
  b:
    title: B \"quoted\"
    depends_on: [a]
    ready: {port: 80}

  c:
    depends_on:
      a: ready
      b: completed
    run: y
";

    #[test]
    fn steps_map_to_their_text_and_ids_to_lines() {
        let spans = step_spans(FILE);
        let ids: Vec<&str> = spans.iter().map(|span| span.id.as_str()).collect();
        assert_eq!(ids, ["a", "b", "c"]);
        let line_of = |offset: usize| line_column(FILE, offset).0;
        assert_eq!(line_of(spans[0].id_or_start()), 4);
        assert_eq!(line_of(spans[1].start), 5);
        assert_eq!(line_of(spans[1].end - 1), 8, "up to its last value");
        assert!(FILE[spans[0].start..spans[0].end].ends_with("}}"));
        assert!(spans[0].steps.is_empty(), "`steps` in env is not a group");
        let dependencies: Vec<&str> = spans[2]
            .depends_on
            .iter()
            .map(|(id, _)| id.as_str())
            .collect();
        assert_eq!(dependencies, ["a", "b"]);
        assert!(FILE[spans[2].depends_on[1].1..].starts_with("b: completed"));
        assert!(FILE[spans[1].depends_on[0].1..].starts_with("a]"));
        // The cursor anywhere in a step's text finds it.
        let inside_b = FILE.find("ready").unwrap();
        let chain = step_at(&spans, inside_b);
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].id, "b");
        assert_eq!(path_at(&spans, inside_b).as_deref(), Some("b"));
        assert!(step_at(&spans, FILE.find("name").unwrap()).is_empty());
        assert_eq!(path_at(&spans, 0), None);
    }

    #[test]
    fn groups_nest_their_steps() {
        let text = "steps:
  svc:
    depends_on: x
    steps:
      build: {run: b}
      inner:
        steps:
          deep: {depends_on: {build: completed}, run: d}
  x: {run: x}
";
        let spans = step_spans(text);
        let ids = |spans: &[StepSpan]| -> Vec<String> {
            spans.iter().map(|span| span.id.clone()).collect()
        };
        assert_eq!(ids(&spans), ["svc", "x"]);
        assert_eq!(ids(&spans[0].steps), ["build", "inner"]);
        assert_eq!(spans[0].depends_on[0].0, "x");
        let deep = span_at(&spans, &[0, 1, 0]).unwrap();
        assert_eq!(deep.id, "deep");
        assert_eq!(deep.depends_on[0].0, "build");
        assert_eq!(span_of(&spans, "svc/inner/deep"), Some(deep));
        assert_eq!(span_of(&spans, "svc/deep"), None);
        let inside = text.find("run: d").unwrap();
        assert_eq!(path_at(&spans, inside).as_deref(), Some("svc/inner/deep"));
        let chain: Vec<&str> = step_at(&spans, inside)
            .iter()
            .map(|span| span.id.as_str())
            .collect();
        assert_eq!(chain, ["svc", "inner", "deep"]);
        // On the group's own text: the group.
        let own = text.find("depends_on: x").unwrap();
        assert_eq!(path_at(&spans, own).as_deref(), Some("svc"));
        assert!(text[deep.start..deep.end].ends_with('}'));
    }

    #[test]
    fn unfinished_text_still_maps_the_steps_so_far() {
        let text = "steps:\n  a:\n    run: x\n  b:\n    depends_on: [a\n";
        let spans = step_spans(text);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[1].id, "b");
        assert_eq!(spans[1].end, text.len());
        assert_eq!(spans[1].depends_on[0].0, "a");
        let text = "steps:\n  a:\n    run: x\n  b:\n    run: [\n  c: 1\n";
        let ids: Vec<String> = step_spans(text).into_iter().map(|span| span.id).collect();
        assert_eq!(ids, ["a", "b"]);
        assert!(step_spans("steps: [").is_empty());
        assert!(step_spans("").is_empty());
    }

    #[test]
    fn inputs_are_mapped_with_their_keys() {
        let text = "inputs:
  env: {type: choice, default: qa}
  stray:
  n:
    type: number
    options: {id: inner}
steps:
  a: {run: x, env: {inputs: no}}
";
        let inputs = input_spans(text);
        assert_eq!(inputs.len(), 3);
        assert_eq!(inputs[0].id, "env");
        let keys: Vec<&str> = inputs[0].keys.iter().map(|(key, _)| key.as_str()).collect();
        assert_eq!(keys, ["type", "default"]);
        assert!(text[inputs[0].key_or_start("default")..].starts_with("default"));
        assert!(text[inputs[0].start..inputs[0].end].ends_with('}'));
        assert_eq!(inputs[1].id, "stray");
        assert!(inputs[1].keys.is_empty());
        assert_eq!(inputs[2].key_or_start("missing"), inputs[2].start);
        assert_eq!(step_spans(text)[0].id, "a");
    }

    #[test]
    fn line_columns_count_characters() {
        let text = "ab\nçd\n";
        assert_eq!(line_column(text, 0), (1, 1));
        assert_eq!(line_column(text, 3), (2, 1));
        assert_eq!(line_column(text, 5), (2, 2), "characters, not bytes");
    }
}
