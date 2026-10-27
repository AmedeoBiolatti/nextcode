//! A workflow's steps as one flat list in file order, groups before their
//! steps (pre-order), each with its qualified id (`group/step`), its
//! parent and children, its `depends_on` resolved among its siblings
//! (split by condition), and what it inherits from its groups and the
//! workflow (`project`, `cwd`, `env` merged with its own values winning,
//! and `env_file`s, outer ones first).
//! A run schedules the steps that do something (leaves); a group's state
//! follows from its steps'.

use std::{collections::BTreeMap, ops::Range};

use crate::{
    file::{Condition, Step, Workflow},
    policy::EnvFile,
};

#[derive(Debug, Clone, PartialEq)]
pub struct StepNode {
    /// Qualified id: the ids from the top level down, joined by `/`.
    pub path: String,
    /// As written, plus what it inherits from its groups; `steps` empty
    /// (a group's steps are its `children`).
    pub step: Step,
    pub parent: Option<usize>,
    /// Its position in its parent's (or the top level's) `steps`.
    pub index: usize,
    /// A group's steps (empty for a step that does something).
    pub children: Vec<usize>,
    /// The siblings its `depends_on` wants ready (`needs`) and finished,
    /// ready or failed (`after`: `condition: completed`).
    pub needs: Vec<usize>,
    pub after: Vec<usize>,
    /// 0 at the top level.
    pub depth: usize,
}

impl StepNode {
    pub fn is_group(&self) -> bool {
        !self.children.is_empty()
    }

    /// The siblings it waits for: the ready ones, then the completed ones.
    pub fn upstream(&self) -> impl Iterator<Item = usize> + '_ {
        self.needs.iter().chain(&self.after).copied()
    }
}

/// The workflow's steps, flattened (see the module).
pub fn flatten(workflow: &Workflow) -> Vec<StepNode> {
    flatten_steps(&workflow.steps, &workflow.env, &workflow.env_file)
}

/// [`flatten`] for a list of top-level steps and the workflow's `env`
/// and `env_file` (ids not found among the siblings are left out of
/// `needs` and `after`; validation reports them).
pub fn flatten_steps(
    steps: &[Step],
    env: &BTreeMap<String, String>,
    env_file: &[EnvFile],
) -> Vec<StepNode> {
    let mut nodes = Vec::new();
    add(&mut nodes, steps, None, env, env_file);
    nodes
}

/// Adds `steps` (children of `parent`) and what they hold; returns their
/// indices.
fn add(
    nodes: &mut Vec<StepNode>,
    steps: &[Step],
    parent: Option<usize>,
    env: &BTreeMap<String, String>,
    env_file: &[EnvFile],
) -> Vec<usize> {
    let mut indices = Vec::with_capacity(steps.len());
    for (index, step) in steps.iter().enumerate() {
        let mut effective = step.shallow();
        let (path, depth) = match parent {
            Some(parent) => {
                let from = &nodes[parent];
                if effective.project.is_none() {
                    effective.project.clone_from(&from.step.project);
                }
                if effective.cwd.is_none() {
                    effective.cwd.clone_from(&from.step.cwd);
                }
                for (name, value) in &from.step.env {
                    effective
                        .env
                        .entry(name.clone())
                        .or_insert_with(|| value.clone());
                }
                let mut files = from.step.env_file.clone();
                files.append(&mut effective.env_file);
                effective.env_file = files;
                (format!("{}/{}", from.path, step.id), from.depth + 1)
            }
            None => {
                for (name, value) in env {
                    effective
                        .env
                        .entry(name.clone())
                        .or_insert_with(|| value.clone());
                }
                let mut files = env_file.to_vec();
                files.append(&mut effective.env_file);
                effective.env_file = files;
                (step.id.clone(), 0)
            }
        };
        let at = nodes.len();
        nodes.push(StepNode {
            path,
            step: effective,
            parent,
            index,
            children: Vec::new(),
            needs: Vec::new(),
            after: Vec::new(),
            depth,
        });
        indices.push(at);
        if !step.steps.is_empty() {
            nodes[at].children = add(nodes, &step.steps, Some(at), env, env_file);
        }
    }
    for &at in &indices {
        let resolve = |condition: Condition| -> Vec<usize> {
            nodes[at]
                .step
                .depending(condition)
                .filter_map(|id| {
                    indices
                        .iter()
                        .copied()
                        .find(|&sibling| nodes[sibling].step.id == id)
                })
                .collect()
        };
        let needs = resolve(Condition::Ready);
        let after = resolve(Condition::Completed);
        nodes[at].needs = needs;
        nodes[at].after = after;
    }
    indices
}

/// The node with qualified id `path`.
pub fn find(nodes: &[StepNode], path: &str) -> Option<usize> {
    nodes.iter().position(|node| node.path == path)
}

/// Node `at` and everything inside it (pre-order keeps them together).
pub fn subtree(nodes: &[StepNode], at: usize) -> Range<usize> {
    let depth = nodes[at].depth;
    let end = nodes[at + 1..]
        .iter()
        .position(|node| node.depth <= depth)
        .map_or(nodes.len(), |offset| at + 1 + offset);
    at..end
}

/// The steps `${steps.<id>.…}` in node `at` may read, nearest first: the
/// siblings its `depends_on` lists, then those each enclosing group
/// lists; by their id at that level.
pub fn scope(nodes: &[StepNode], at: usize) -> Vec<(&str, usize)> {
    let mut scope = Vec::new();
    let mut node = Some(at);
    while let Some(current) = node {
        for upstream in nodes[current].upstream() {
            scope.push((nodes[upstream].step.id.as_str(), upstream));
        }
        node = nodes[current].parent;
    }
    scope
}

/// What must be ready before it starts (its own, then each enclosing
/// group's), and what must have finished.
pub fn gates(nodes: &[StepNode], at: usize) -> (Vec<usize>, Vec<usize>) {
    let (mut needs, mut after) = (Vec::new(), Vec::new());
    let mut node = Some(at);
    while let Some(current) = node {
        needs.extend_from_slice(&nodes[current].needs);
        after.extend_from_slice(&nodes[current].after);
        node = nodes[current].parent;
    }
    (needs, after)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_flatten_in_order_with_qualified_ids_and_inherit_defaults() {
        let workflow = crate::parse(
            r#"
env: {W: top, B: workflow}
steps:
  db: {run: d}
  api:
    depends_on: db
    project: {local: C:/api}
    env: {A: "1", B: group}
    cwd: srv
    steps:
      build: {run: b, env: {B: own}}
      serve: {depends_on: build, run: s, cwd: .}
      more:
        depends_on: {serve: completed}
        project: C:/other
        steps:
          x: {run: x}
  web: {depends_on: {api: completed}, run: w}
"#,
            "f",
        )
        .unwrap();
        let nodes = flatten(&workflow);
        let paths: Vec<&str> = nodes.iter().map(|node| node.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "db",
                "api",
                "api/build",
                "api/serve",
                "api/more",
                "api/more/x",
                "web"
            ]
        );
        assert_eq!(nodes[1].children, [2, 3, 4]);
        assert_eq!(nodes[4].children, [5]);
        assert_eq!(
            (nodes[5].parent, nodes[5].depth, nodes[5].index),
            (Some(4), 2, 0)
        );
        assert_eq!(nodes[3].needs, [2]);
        assert_eq!(nodes[4].after, [3]);
        assert_eq!(nodes[6].after, [1]);
        assert!(nodes[1].is_group() && !nodes[2].is_group());
        assert!(nodes[1].step.steps.is_empty(), "children are nodes");
        let build = &nodes[2].step;
        assert_eq!(
            build.project,
            Some(crate::ProjectSpec::Local("C:/api".into()))
        );
        assert_eq!(build.cwd.as_deref(), Some("srv"));
        assert_eq!(build.env["A"], "1");
        assert_eq!(build.env["B"], "own", "the step's own value wins");
        assert_eq!(nodes[3].step.cwd.as_deref(), Some("."));
        assert_eq!(
            nodes[5].step.project,
            Some(crate::ProjectSpec::Typed("C:/other".into())),
            "the nearest group's project"
        );
        assert_eq!(nodes[5].step.env["B"], "group");
        assert_eq!(nodes[5].step.env["W"], "top", "the workflow's env");
        assert_eq!(nodes[0].step.env["B"], "workflow");
        assert_eq!(nodes[5].step.cwd.as_deref(), Some("srv"));
        assert_eq!(subtree(&nodes, 1), 1..6);
        assert_eq!(subtree(&nodes, 4), 4..6);
        assert_eq!(subtree(&nodes, 6), 6..7);
        assert_eq!(find(&nodes, "api/more/x"), Some(5));
        assert_eq!(find(&nodes, "x"), None);
        // `x` reads `serve` (its group's `after`), then `db` (the outer
        // group's `needs`).
        assert_eq!(scope(&nodes, 5), [("serve", 3), ("db", 0)]);
        assert_eq!(gates(&nodes, 5), (vec![0], vec![3]));
        assert_eq!(gates(&nodes, 3), (vec![2, 0], vec![]));
    }
}
