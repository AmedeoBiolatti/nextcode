//! What a workflow file holds as far as it can be read while being typed
//! (the YAML tree before a syntax error, the steps' and inputs' spans),
//! what a context path names ([`Place`]), which steps `${steps.…}` can
//! read from a step, and where steps and inputs are referred to.

use std::ops::Range;

use workflow::{
    InputSpan, StepSpan,
    yaml::{self, Data, Node},
};

use crate::context::Seg;

/// The file, read tolerantly.
pub struct Analysis<'a> {
    pub text: &'a str,
    pub root: Option<Node>,
    pub steps: Vec<StepSpan>,
    pub inputs: Vec<InputSpan>,
}

/// What the mapping or list at a context path is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Place {
    Top,
    /// A `steps` mapping: of the top level (`chain` empty) or of a group.
    Steps(Vec<String>),
    /// The step with these ids from the top down.
    Step(Vec<String>),
    Ready(Vec<String>),
    Project(Vec<String>),
    /// A dev container's folder.
    Folder(Vec<String>),
    /// A step's `depends_on` (a list, or a mapping of ids).
    DependsOn(Vec<String>),
    /// One dependency's `{condition: …}`.
    Dependency(Vec<String>),
    Artifacts(Vec<String>),
    Artifact(Vec<String>),
    Inputs,
    Input(String),
    Options(String),
    Option(String),
    Env,
    /// Anything else (`x-` parts, values).
    Other,
}

impl Place {
    /// What `path` names.
    pub fn of(path: &[Seg]) -> Place {
        let mut place = Place::Top;
        for seg in path {
            place = match (place, seg) {
                (Place::Top, Seg::Key(key)) => match key.as_str() {
                    "steps" => Place::Steps(Vec::new()),
                    "inputs" => Place::Inputs,
                    "env" => Place::Env,
                    _ => Place::Other,
                },
                (Place::Steps(mut chain), Seg::Key(id)) => {
                    chain.push(id.clone());
                    Place::Step(chain)
                }
                (Place::Step(chain), Seg::Key(key)) => match key.as_str() {
                    "steps" => Place::Steps(chain),
                    "ready" => Place::Ready(chain),
                    "project" => Place::Project(chain),
                    "depends_on" => Place::DependsOn(chain),
                    "artifacts" => Place::Artifacts(chain),
                    "env" => Place::Env,
                    _ => Place::Other,
                },
                (Place::Project(chain), Seg::Key(key)) if key == "devcontainer" => {
                    Place::Folder(chain)
                }
                (Place::DependsOn(chain), Seg::Key(_)) => Place::Dependency(chain),
                (Place::Artifacts(chain), Seg::Item) => Place::Artifact(chain),
                (Place::Inputs, Seg::Key(id)) => Place::Input(id.clone()),
                (Place::Input(id), Seg::Key(key)) if key == "options" => Place::Options(id),
                (Place::Options(id), Seg::Item) => Place::Option(id),
                _ => Place::Other,
            };
        }
        place
    }
}

/// A step or input mentioned in the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// The step with these ids from the top down.
    Step(Vec<String>),
    Input(String),
}

/// Where a name is written: the name's own text (quotes excluded).
pub type Occurrence = Range<usize>;

impl<'a> Analysis<'a> {
    pub fn new(text: &'a str) -> Self {
        let root = yaml::load(text).root;
        let (steps, inputs) = root.as_ref().map(workflow::spans_of).unwrap_or_default();
        Self {
            text,
            root,
            steps,
            inputs,
        }
    }

    /// The spans from the top down to the step with ids `chain`.
    pub fn chain(&self, chain: &[String]) -> Option<Vec<&StepSpan>> {
        let mut level = self.steps.as_slice();
        let mut spans = Vec::with_capacity(chain.len());
        for id in chain {
            let span = level.iter().find(|span| span.id == *id)?;
            spans.push(span);
            level = &span.steps;
        }
        Some(spans)
    }

    /// The steps of the `steps` mapping holding the step at `chain` (the
    /// top level for a top-level step).
    pub fn siblings(&self, chain: &[String]) -> &[StepSpan] {
        match chain.split_last() {
            None | Some((_, [])) => &self.steps,
            Some((_, parents)) => self
                .chain(parents)
                .and_then(|spans| spans.last().map(|span| span.steps.as_slice()))
                .unwrap_or(&[]),
        }
    }

    /// The YAML node of the step at `chain`.
    pub fn step_node(&self, chain: &[String]) -> Option<&Node> {
        let mut node = self.root.as_ref()?;
        for id in chain {
            node = node.get("steps")?.get(id)?;
        }
        Some(node)
    }

    /// The YAML node of input `id`'s settings.
    pub fn input_node(&self, id: &str) -> Option<&Node> {
        self.root.as_ref()?.get("inputs")?.get(id)
    }

    /// The keys present in the mapping at `path` (items: the one around
    /// `offset`).
    pub fn present_keys(&self, path: &[Seg], offset: usize) -> Vec<String> {
        let mut node = self.root.as_ref();
        for seg in path {
            node = match (node, seg) {
                (Some(node), Seg::Key(key)) => node.get(key),
                (Some(node), Seg::Item) => node.as_list().and_then(|items| {
                    items
                        .iter()
                        .find(|item| item.start <= offset && offset <= item.end)
                        .or(items.last())
                }),
                (None, _) => None,
            };
        }
        node.and_then(Node::as_map)
            .map(|entries| entries.iter().map(|entry| entry.key.clone()).collect())
            .unwrap_or_default()
    }

    /// The steps `${steps.<id>.…}` in the step at `chain` may read,
    /// nearest first: what its `depends_on` lists, then what each
    /// enclosing group's lists; with the target's chain.
    pub fn scope(&self, chain: &[String]) -> Vec<(String, Vec<String>)> {
        let Some(spans) = self.chain(chain) else {
            return Vec::new();
        };
        let mut scope: Vec<(String, Vec<String>)> = Vec::new();
        for depth in (0..spans.len()).rev() {
            for (id, _) in &spans[depth].depends_on {
                if scope.iter().any(|(known, _)| known == id) {
                    continue;
                }
                let mut target = chain[..depth].to_vec();
                target.push(id.clone());
                if self.chain(&target).is_some() {
                    scope.push((id.clone(), target));
                }
            }
        }
        scope
    }

    /// The range of a key or scalar's text starting at `start` that reads
    /// `name` (quotes excluded).
    pub fn name_at(&self, start: usize, name: &str) -> Occurrence {
        let start = match self.text[start..].chars().next() {
            Some('\'' | '"') => start + 1,
            _ => start,
        };
        start..(start + name.len()).min(self.text.len())
    }

    /// Where `target` is declared: its key.
    pub fn declaration(&self, target: &Target) -> Option<Occurrence> {
        match target {
            Target::Step(chain) => {
                let span = *self.chain(chain)?.last()?;
                Some(self.name_at(span.start, &span.id))
            }
            Target::Input(id) => {
                let span = self.inputs.iter().find(|span| span.id == *id)?;
                Some(self.name_at(span.start, id))
            }
        }
    }

    /// Every mention of `target` other than its declaration: `depends_on`
    /// entries naming the step, and `${steps.<id>.…}` that read it (from
    /// the steps whose scope holds it); `${inputs.<id>}` for an input.
    pub fn references(&self, target: &Target) -> Vec<Occurrence> {
        let mut found = Vec::new();
        match target {
            Target::Step(chain) => {
                let Some((id, _)) = chain.split_last() else {
                    return found;
                };
                for sibling in self.siblings(chain) {
                    for (listed, at) in &sibling.depends_on {
                        if listed == id {
                            found.push(self.name_at(*at, id));
                        }
                    }
                }
                self.walk(
                    &self.steps,
                    &mut Vec::new(),
                    &mut |this, step_chain, span| {
                        for (name, at) in this.variables_in(span, "${steps.") {
                            if name == *id
                                && this
                                    .scope(step_chain)
                                    .iter()
                                    .any(|(scoped, target)| scoped == &name && target == chain)
                            {
                                found.push(at);
                            }
                        }
                    },
                );
            }
            Target::Input(id) => {
                let needle = format!("${{inputs.{id}}}");
                let mut from = 0;
                while let Some(at) = self.text[from..].find(&needle).map(|at| from + at) {
                    let start = at + "${inputs.".len();
                    found.push(start..start + id.len());
                    from = at + needle.len();
                }
            }
        }
        found.sort_by_key(|range| range.start);
        found.dedup();
        found
    }

    /// Calls `visit` for every step (groups included) with its chain.
    fn walk(
        &self,
        steps: &[StepSpan],
        chain: &mut Vec<String>,
        visit: &mut impl FnMut(&Self, &[String], &StepSpan),
    ) {
        for span in steps {
            chain.push(span.id.clone());
            visit(self, chain, span);
            self.walk(&span.steps, chain, visit);
            chain.pop();
        }
    }

    /// The names after `prefix` (`${steps.` or `${inputs.`) written in a
    /// step's own text (its nested steps' left out), and where each is.
    pub fn variables_in(&self, span: &StepSpan, prefix: &str) -> Vec<(String, Occurrence)> {
        let own = |at: usize| {
            !span
                .steps
                .iter()
                .any(|child| child.start <= at && at < child.end)
        };
        let region = &self.text[span.start..span.end.min(self.text.len())];
        let mut found = Vec::new();
        let mut from = 0;
        while let Some(at) = region[from..].find(prefix).map(|at| from + at) {
            let start = span.start + at + prefix.len();
            let name_end = self.text[start..]
                .find(['.', '}', ' ', '\'', '"', '\n'])
                .map_or(self.text.len(), |end| start + end);
            if own(start) && name_end > start {
                found.push((self.text[start..name_end].to_owned(), start..name_end));
            }
            from = at + prefix.len();
        }
        found
    }

    /// The step chain whose text holds `offset` (innermost).
    pub fn step_chain_at(&self, offset: usize) -> Vec<String> {
        workflow::step_at(&self.steps, offset)
            .iter()
            .map(|span| span.id.clone())
            .collect()
    }

    /// The `${…}` variable around `offset`: its text inside the braces and
    /// where that is.
    pub fn variable_at(&self, offset: usize) -> Option<(String, Range<usize>)> {
        let line_start = self.text[..offset].rfind('\n').map_or(0, |at| at + 1);
        let open = self.text[line_start..offset].rfind("${")? + line_start;
        let inside = open + 2;
        let close = self.text[inside..]
            .find(['}', '\n'])
            .map_or(self.text.len(), |at| inside + at);
        if self.text[open..offset].contains('}') || close < offset {
            return None;
        }
        Some((self.text[inside..close].to_owned(), inside..close))
    }

    /// What a `${…}` variable's text names, with the range of the name.
    pub fn variable_target(
        &self,
        variable: &str,
        start: usize,
        chain: &[String],
    ) -> Option<(Target, Occurrence)> {
        if let Some(rest) = variable.strip_prefix("steps.") {
            let id = rest.split('.').next()?;
            let (_, target) = self
                .scope(chain)
                .into_iter()
                .find(|(scoped, _)| scoped == id)?;
            let at = start + "steps.".len();
            return Some((Target::Step(target), at..at + id.len()));
        }
        let id = variable.strip_prefix("inputs.")?;
        self.inputs.iter().any(|span| span.id == id).then(|| {
            let at = start + "inputs.".len();
            (Target::Input(id.to_owned()), at..at + id.len())
        })
    }

    /// The artifact labels of the step at `chain`, in order.
    pub fn artifact_labels(&self, chain: &[String]) -> Vec<Option<String>> {
        self.step_node(chain)
            .and_then(|node| node.get("artifacts"))
            .and_then(Node::as_list)
            .map(|items| {
                items
                    .iter()
                    .map(|item| item.get("label").and_then(Node::text).map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Whether the step at `chain` is a group (it has `steps`).
    pub fn is_group(&self, chain: &[String]) -> bool {
        self.step_node(chain).is_some_and(|node| {
            node.get("steps")
                .is_some_and(|steps| matches!(steps.data, Data::Map(_)))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE: &str = "inputs:
  env: {default: dev}
steps:
  db: {run: d}
  api:
    depends_on: [db]
    env: {E: '${inputs.env}'}
    steps:
      build: {run: 'b ${steps.db.status}'}
      serve: {depends_on: [build], run: 's ${steps.build.exit_code} ${steps.db.status}'}
  report: {depends_on: {api: completed, db: ready}, run: 'r ${steps.db.exit_code} ${inputs.env}'}
";

    fn chain(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|id| (*id).to_owned()).collect()
    }

    #[test]
    fn places_follow_the_path() {
        let key = |key: &str| Seg::Key(key.into());
        assert_eq!(Place::of(&[]), Place::Top);
        assert_eq!(
            Place::of(&[key("steps"), key("api"), key("steps"), key("build")]),
            Place::Step(chain(&["api", "build"]))
        );
        assert_eq!(
            Place::of(&[key("steps"), key("a"), key("project"), key("devcontainer")]),
            Place::Folder(chain(&["a"]))
        );
        assert_eq!(
            Place::of(&[key("steps"), key("a"), key("artifacts"), Seg::Item]),
            Place::Artifact(chain(&["a"]))
        );
        assert_eq!(
            Place::of(&[key("inputs"), key("e"), key("options"), Seg::Item]),
            Place::Option("e".into())
        );
        assert_eq!(Place::of(&[key("x-shared"), key("run")]), Place::Other);
    }

    #[test]
    fn scopes_reach_dependencies_and_enclosing_groups() {
        let analysis = Analysis::new(FILE);
        let ids = |scope: Vec<(String, Vec<String>)>| -> Vec<String> {
            scope.into_iter().map(|(id, _)| id).collect()
        };
        assert_eq!(
            ids(analysis.scope(&chain(&["api", "serve"]))),
            ["build", "db"]
        );
        assert_eq!(
            analysis.scope(&chain(&["api", "build"])),
            [("db".into(), chain(&["db"]))]
        );
        assert_eq!(ids(analysis.scope(&chain(&["report"]))), ["api", "db"]);
        assert_eq!(analysis.siblings(&chain(&["api", "serve"])).len(), 2);
        assert!(analysis.is_group(&chain(&["api"])));
    }

    #[test]
    fn references_find_dependencies_and_variables_that_read_the_step() {
        let analysis = Analysis::new(FILE);
        let texts = |ranges: Vec<Occurrence>| -> Vec<(usize, &str)> {
            ranges
                .into_iter()
                .map(|range| (FILE[..range.start].matches('\n').count() + 1, &FILE[range]))
                .collect()
        };
        let db = Target::Step(chain(&["db"]));
        assert_eq!(
            analysis.declaration(&db).map(|range| &FILE[range]),
            Some("db")
        );
        assert_eq!(
            texts(analysis.references(&db)),
            [(6, "db"), (9, "db"), (10, "db"), (11, "db"), (11, "db")]
        );
        let build = Target::Step(chain(&["api", "build"]));
        assert_eq!(
            texts(analysis.references(&build)),
            [(10, "build"), (10, "build")]
        );
        let env = Target::Input("env".into());
        assert_eq!(texts(analysis.references(&env)), [(7, "env"), (11, "env")]);
        // A variable names its target through the scope.
        let at = FILE.find("${steps.build").unwrap() + 2;
        let (variable, range) = analysis.variable_at(at + 3).unwrap();
        assert_eq!(variable, "steps.build.exit_code");
        assert_eq!(
            analysis.variable_target(&variable, range.start, &chain(&["api", "serve"])),
            Some((build, at + 6..at + 11))
        );
    }
}
