//! Where a workflow's steps go in its graph, like Azure DevOps' pipeline
//! stages, laid out the Sugiyama way:
//!
//! 1. **Columns**: one per dependency depth (the longest chain of `needs`
//!    and `after` from a step without either). An edge spanning several
//!    columns gets a *dummy* point in each column it crosses, so it runs
//!    between the cards there instead of through them.
//! 2. **Order** in each column: median and barycenter sweeps (down and
//!    up the columns, from file order and from it upside down), each
//!    refined by swapping neighbours and moving single items (sifting)
//!    while that removes crossings, counting crossings after every sweep
//!    and keeping the best order found; then, while crossings remain, an
//!    exhaustive search with pruning for fewer, within a budget (exact for
//!    small workflows).
//! 3. **Heights**: Brandes–Köpf: steps are aligned with a median
//!    neighbour into blocks (long edges first, so they stay straight),
//!    blocks packed as tightly as their columns allow, in all four
//!    directions, and the two middle results averaged; then each step
//!    moves to the median of its neighbouring steps where its column
//!    leaves room, so a chain stays on one row and a fan is centered.
//!
//! Edges run from the right side of a step to the left side of each step
//! that needs it or runs after it, through their dummies; a side with
//! several edges spreads them over distinct ports. Pure geometry in
//! logical pixels; deterministic, and polynomial but for the bounded
//! search (workflows are small).

use std::collections::{HashMap, HashSet};

use crate::Workflow;

/// Card size and spacing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Metrics {
    pub card_width: f32,
    pub card_height: f32,
    /// Horizontal space between columns (where edges run).
    pub column_gap: f32,
    pub row_gap: f32,
    /// Between an edge passing a column and the cards or edges next to it.
    pub edge_gap: f32,
    /// Around the whole graph.
    pub margin: f32,
}

impl Default for Metrics {
    fn default() -> Self {
        Self {
            card_width: 220.,
            card_height: 76.,
            column_gap: 64.,
            row_gap: 20.,
            edge_gap: 14.,
            margin: 24.,
        }
    }
}

/// What the layout needs to know about a step.
#[derive(Debug, Clone, Copy)]
pub struct NodeSpec<'a> {
    pub id: &'a str,
    pub needs: &'a [String],
    pub after: &'a [String],
}

/// A step's place: its column (depth), row in the column, and top-left
/// corner.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Node {
    pub column: usize,
    pub row: usize,
    pub x: f32,
    pub y: f32,
}

/// `from` (a need, or a step `to` runs after) → `to`.
#[derive(Debug, Clone, PartialEq)]
pub struct Edge {
    pub from: usize,
    pub to: usize,
    /// An `after` edge (`to` starts once `from` finished, ready or not).
    pub after: bool,
    /// Where it runs, left to right: its port on `from`'s right side, two
    /// points per column it crosses (where it enters and leaves the
    /// column, at one height, between the cards there), and its port on
    /// `to`'s left side. Drawn with horizontal tangents at every point.
    pub points: Vec<(f32, f32)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GraphLayout {
    /// By step index.
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    /// Step indices per column, top to bottom.
    pub columns: Vec<Vec<usize>>,
    pub width: f32,
    pub height: f32,
    pub metrics: Metrics,
}

impl GraphLayout {
    /// The step whose card contains `(x, y)` (edges only run between
    /// cards).
    pub fn step_at(&self, x: f32, y: f32) -> Option<usize> {
        let metrics = &self.metrics;
        self.nodes.iter().position(|node| {
            x >= node.x
                && x <= node.x + metrics.card_width
                && y >= node.y
                && y <= node.y + metrics.card_height
        })
    }
}

/// Sweeps per ordering heuristic, at most.
const MAX_SWEEPS: usize = 24;
/// Sweeps in a row without fewer crossings before a heuristic gives up.
const PATIENCE: usize = 6;
/// Column orders the exhaustive search tries, at most, once the sweeps
/// leave crossings.
const EXACT_BUDGET: usize = 50_000;
/// Rounds (left to right, then back) moving steps towards their
/// neighbours after Brandes–Köpf.
const STRAIGHTEN_ROUNDS: usize = 4;
/// The share of a card's side its ports spread over, at most.
const PORT_SPAN: f32 = 0.6;

/// The steps in columns by depth (long edges cut by dummies), in file
/// order, and the workflow's edges; a duplicate id, an unknown need or a
/// cycle is an error.
fn layered(nodes: &[NodeSpec]) -> Result<(Layered, Vec<Route>), String> {
    let mut index: HashMap<&str, usize> = HashMap::new();
    for (at, node) in nodes.iter().enumerate() {
        if index.insert(node.id, at).is_some() {
            return Err(format!("duplicate step id `{}`", node.id));
        }
    }
    // Per step: what it depends on, and whether through `after`.
    let mut needs: Vec<Vec<(usize, bool)>> = Vec::with_capacity(nodes.len());
    for node in nodes {
        let mut resolved: Vec<(usize, bool)> =
            Vec::with_capacity(node.needs.len() + node.after.len());
        let listed = node
            .needs
            .iter()
            .map(|id| (id, false))
            .chain(node.after.iter().map(|id| (id, true)));
        for (need, after) in listed {
            let at = *index.get(need.as_str()).ok_or_else(|| {
                if after {
                    format!("`{}` runs after unknown step `{need}`", node.id)
                } else {
                    format!("`{}` needs unknown step `{need}`", node.id)
                }
            })?;
            if !resolved.iter().any(|&(listed, _)| listed == at) {
                resolved.push((at, after));
            }
        }
        needs.push(resolved);
    }
    let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); nodes.len()];
    for (at, node_needs) in needs.iter().enumerate() {
        for &(need, _) in node_needs {
            dependents[need].push(at);
        }
    }

    // Longest path from the roots, in topological (Kahn) order.
    let mut waiting: Vec<usize> = needs.iter().map(Vec::len).collect();
    let mut ready: Vec<usize> = (0..nodes.len()).filter(|&at| waiting[at] == 0).collect();
    let mut depth = vec![0usize; nodes.len()];
    let mut next = 0;
    while next < ready.len() {
        let at = ready[next];
        next += 1;
        for &dependent in &dependents[at] {
            depth[dependent] = depth[dependent].max(depth[at] + 1);
            waiting[dependent] -= 1;
            if waiting[dependent] == 0 {
                ready.push(dependent);
            }
        }
    }
    if ready.len() < nodes.len() {
        let stuck: Vec<&str> = (0..nodes.len())
            .filter(|&at| waiting[at] > 0)
            .map(|at| nodes[at].id)
            .collect();
        return Err(format!("dependency cycle among {}", stuck.join(", ")));
    }

    Ok(Layered::new(&depth, &needs))
}

/// Lays out `nodes`; a duplicate id, an unknown need or a cycle is an
/// error (the file's validation reports those too).
pub fn layout(nodes: &[NodeSpec], metrics: Metrics) -> Result<GraphLayout, String> {
    let (mut graph, routes) = layered(nodes)?;
    graph.order();
    let pos = graph.positions();
    let center = graph.heights(&pos, &metrics);

    let column_pitch = metrics.card_width + metrics.column_gap;
    let column_x = |column: usize| metrics.margin + column as f32 * column_pitch;
    let columns: Vec<Vec<usize>> = graph
        .layers
        .iter()
        .map(|layer| {
            layer
                .iter()
                .copied()
                .filter(|&item| item < nodes.len())
                .collect()
        })
        .collect();
    let mut placed = vec![
        Node {
            column: 0,
            row: 0,
            x: 0.,
            y: 0.,
        };
        nodes.len()
    ];
    for (column, members) in columns.iter().enumerate() {
        for (row, &at) in members.iter().enumerate() {
            placed[at] = Node {
                column,
                row,
                x: column_x(column),
                y: center[at] - metrics.card_height / 2.,
            };
        }
    }
    // Per step, the port of each edge on its right (out) and left (in)
    // side, by the neighbouring item.
    let side_ports = |neighbors: &[Vec<usize>]| -> Vec<Vec<(usize, f32)>> {
        (0..nodes.len())
            .map(|at| ports(center[at], &neighbors[at], &center, &pos, &metrics))
            .collect()
    };
    let (out_ports, in_ports) = (side_ports(&graph.succs), side_ports(&graph.preds));
    let port = |ports: &[(usize, f32)], neighbor: usize| {
        ports
            .iter()
            .find(|&&(item, _)| item == neighbor)
            .map(|&(_, y)| y)
            .expect("every edge has a port")
    };
    let edges = routes
        .into_iter()
        .map(|route| {
            let first = route.dummies.first().copied().unwrap_or(route.to);
            let last = route.dummies.last().copied().unwrap_or(route.from);
            let (from, to) = (placed[route.from], placed[route.to]);
            let mut points = Vec::with_capacity(2 + 2 * route.dummies.len());
            points.push((
                from.x + metrics.card_width,
                port(&out_ports[route.from], first),
            ));
            for &dummy in &route.dummies {
                let x = column_x(graph.column[dummy]);
                points.push((x, center[dummy]));
                points.push((x + metrics.card_width, center[dummy]));
            }
            points.push((to.x, port(&in_ports[route.to], last)));
            Edge {
                from: route.from,
                to: route.to,
                after: route.after,
                points,
            }
        })
        .collect();
    let column_count = graph.layers.len();
    let bottom = (0..graph.column.len())
        .map(|item| center[item] + graph.half(item, &metrics))
        .fold(metrics.margin, f32::max);
    let width = if column_count == 0 {
        0.
    } else {
        column_count as f32 * metrics.card_width + (column_count - 1) as f32 * metrics.column_gap
    };
    Ok(GraphLayout {
        nodes: placed,
        edges,
        columns,
        width: 2. * metrics.margin + width,
        height: bottom + metrics.margin,
        metrics,
    })
}

/// An edge of the workflow and the dummies it passes, left to right.
struct Route {
    from: usize,
    to: usize,
    after: bool,
    dummies: Vec<usize>,
}

#[derive(Clone, Copy)]
enum Heuristic {
    Median,
    Barycenter,
}

impl Heuristic {
    /// Where an item wants to be given its `neighbors` in the adjacent
    /// column; `None` without any (it keeps its place).
    fn key(self, neighbors: &[usize], pos: &[usize]) -> Option<f32> {
        if neighbors.is_empty() {
            return None;
        }
        let mut at: Vec<f32> = neighbors.iter().map(|&n| pos[n] as f32).collect();
        let count = at.len();
        Some(match self {
            Self::Barycenter => at.iter().sum::<f32>() / count as f32,
            Self::Median => {
                at.sort_by(f32::total_cmp);
                let middle = count / 2;
                if count % 2 == 1 {
                    at[middle]
                } else {
                    // Weighted towards the side whose neighbours are
                    // packed closer (Gansner et al.).
                    let left = at[middle - 1] - at[0];
                    let right = at[count - 1] - at[middle];
                    if left + right == 0. {
                        (at[middle - 1] + at[middle]) / 2.
                    } else {
                        (at[middle - 1] * right + at[middle] * left) / (left + right)
                    }
                }
            }
        })
    }
}

/// The graph with long edges cut into one segment per column gap: items
/// are the steps, then the dummies.
struct Layered {
    steps: usize,
    /// By item.
    column: Vec<usize>,
    preds: Vec<Vec<usize>>,
    succs: Vec<Vec<usize>>,
    /// Items per column, top to bottom.
    layers: Vec<Vec<usize>>,
}

impl Layered {
    /// Steps in file order, then dummies in edge order, per column; the
    /// routes in edge order (by step, in the order of its `needs`, then
    /// `after`).
    fn new(depth: &[usize], needs: &[Vec<(usize, bool)>]) -> (Self, Vec<Route>) {
        let steps = depth.len();
        let mut graph = Self {
            steps,
            column: depth.to_vec(),
            preds: vec![Vec::new(); steps],
            succs: vec![Vec::new(); steps],
            layers: Vec::new(),
        };
        let mut routes = Vec::new();
        for (to, node_needs) in needs.iter().enumerate() {
            for &(from, after) in node_needs {
                let mut last = from;
                let mut dummies = Vec::new();
                for column in depth[from] + 1..depth[to] {
                    let dummy = graph.column.len();
                    graph.column.push(column);
                    graph.preds.push(vec![last]);
                    graph.succs.push(Vec::new());
                    graph.succs[last].push(dummy);
                    dummies.push(dummy);
                    last = dummy;
                }
                graph.succs[last].push(to);
                graph.preds[to].push(last);
                routes.push(Route {
                    from,
                    to,
                    after,
                    dummies,
                });
            }
        }
        let columns = depth.iter().max().map_or(0, |deepest| deepest + 1);
        graph.layers = vec![Vec::new(); columns];
        for (item, &column) in graph.column.iter().enumerate() {
            graph.layers[column].push(item);
        }
        (graph, routes)
    }

    fn is_dummy(&self, item: usize) -> bool {
        item >= self.steps
    }

    /// Half an item's height: a card's, or nothing for a dummy.
    fn half(&self, item: usize, metrics: &Metrics) -> f32 {
        if self.is_dummy(item) {
            0.
        } else {
            metrics.card_height / 2.
        }
    }

    /// The least distance between the centers of `upper` and `lower`,
    /// neighbours in a column.
    fn separation(&self, upper: usize, lower: usize, metrics: &Metrics) -> f32 {
        let gap = if self.is_dummy(upper) || self.is_dummy(lower) {
            metrics.edge_gap
        } else {
            metrics.row_gap
        };
        self.half(upper, metrics) + self.half(lower, metrics) + gap
    }

    /// Each item's row in its column.
    fn positions(&self) -> Vec<usize> {
        let mut pos = vec![0; self.column.len()];
        for layer in &self.layers {
            for (at, &item) in layer.iter().enumerate() {
                pos[item] = at;
            }
        }
        pos
    }

    /// Pairs of segments that cross, over all column gaps.
    fn crossings(&self, pos: &[usize]) -> usize {
        (0..self.layers.len())
            .map(|column| self.crossings_after(column, pos))
            .sum()
    }

    /// Pairs of segments that cross between `column` and the next one.
    fn crossings_after(&self, column: usize, pos: &[usize]) -> usize {
        let segments: Vec<(usize, usize)> = self.layers[column]
            .iter()
            .flat_map(|&from| self.succs[from].iter().map(move |&to| (pos[from], pos[to])))
            .collect();
        let mut count = 0;
        for (at, a) in segments.iter().enumerate() {
            count += segments[at + 1..]
                .iter()
                .filter(|b| (a.0 < b.0 && a.1 > b.1) || (a.0 > b.0 && a.1 < b.1))
                .count();
        }
        count
    }

    /// Orders the columns for few crossings: from file order and from it
    /// upside down, sweeps first down then up the columns or the other way
    /// round, with each heuristic, every sweep refined by swapping and
    /// sifting; then, while crossings remain, an exhaustive search for
    /// fewer within [`EXACT_BUDGET`] (exact for small workflows). The best
    /// order seen wins (the first on a tie, file order included).
    fn order(&mut self) {
        let file_order = self.layers.clone();
        let upside_down: Vec<Vec<usize>> = file_order
            .iter()
            .map(|layer| layer.iter().rev().copied().collect())
            .collect();
        let mut best = (self.crossings(&self.positions()), file_order.clone());
        for start in [&file_order, &upside_down] {
            for first_down in [true, false] {
                for heuristic in [Heuristic::Median, Heuristic::Barycenter] {
                    if best.0 == 0 {
                        break;
                    }
                    self.layers = start.clone();
                    let mut pos = self.positions();
                    let mut stale = 0;
                    for sweep in 0..MAX_SWEEPS {
                        if best.0 == 0 || stale == PATIENCE {
                            break;
                        }
                        if (sweep % 2 == 0) == first_down {
                            for column in 1..self.layers.len() {
                                reorder(&mut self.layers[column], &self.preds, &mut pos, heuristic);
                            }
                        } else {
                            for column in (0..self.layers.len().saturating_sub(1)).rev() {
                                reorder(&mut self.layers[column], &self.succs, &mut pos, heuristic);
                            }
                        }
                        self.transpose(&mut pos);
                        self.sift(&mut pos);
                        let crossings = self.crossings(&pos);
                        if crossings < best.0 {
                            best = (crossings, self.layers.clone());
                            stale = 0;
                        } else {
                            stale += 1;
                        }
                    }
                }
            }
        }
        if best.0 > 0 {
            self.layers = best.1.clone();
            let mut pos = self.positions();
            let mut budget = EXACT_BUDGET;
            self.search(0, 0, &mut pos, &mut budget, &mut best);
        }
        self.layers = best.1;
    }

    /// Tries the orders of `column` and, for each that crosses less than
    /// `best` so far (`crossed` before it), of the columns after it,
    /// while `budget` (orders tried) lasts.
    fn search(
        &mut self,
        column: usize,
        crossed: usize,
        pos: &mut [usize],
        budget: &mut usize,
        best: &mut (usize, Vec<Vec<usize>>),
    ) {
        if column == self.layers.len() {
            if crossed < best.0 {
                *best = (crossed, self.layers.clone());
            }
            return;
        }
        let mut order = self.layers[column].clone();
        order.sort_unstable();
        loop {
            if *budget == 0 || best.0 == 0 {
                return;
            }
            *budget -= 1;
            for (row, &item) in order.iter().enumerate() {
                pos[item] = row;
            }
            self.layers[column].clone_from(&order);
            let crossed = crossed
                + column
                    .checked_sub(1)
                    .map_or(0, |left| self.crossings_after(left, pos));
            if crossed < best.0 {
                self.search(column + 1, crossed, pos, budget, best);
            }
            if !next_permutation(&mut order) {
                return;
            }
        }
    }

    /// Crossings among the edges of `upper` and `lower` (in one column)
    /// with `upper` above.
    fn crossing(&self, upper: usize, lower: usize, pos: &[usize]) -> usize {
        [&self.preds, &self.succs]
            .iter()
            .map(|side| {
                side[upper]
                    .iter()
                    .map(|&a| side[lower].iter().filter(|&&b| pos[a] > pos[b]).count())
                    .sum::<usize>()
            })
            .sum()
    }

    /// Swaps neighbours in a column while that removes crossings.
    fn transpose(&mut self, pos: &mut [usize]) {
        let mut improved = true;
        while improved {
            improved = false;
            for column in 0..self.layers.len() {
                for at in 1..self.layers[column].len() {
                    let (upper, lower) = (self.layers[column][at - 1], self.layers[column][at]);
                    if self.crossing(lower, upper, pos) < self.crossing(upper, lower, pos) {
                        self.layers[column].swap(at - 1, at);
                        pos[lower] = at - 1;
                        pos[upper] = at;
                        improved = true;
                    }
                }
            }
        }
    }

    /// Moves each item to the row of its column where its edges cross the
    /// fewest others, while that removes crossings (sifting).
    fn sift(&mut self, pos: &mut [usize]) {
        let mut improved = true;
        while improved {
            improved = false;
            for column in 0..self.layers.len() {
                for item in self.layers[column].clone() {
                    let others: Vec<usize> = self.layers[column]
                        .iter()
                        .copied()
                        .filter(|&other| other != item)
                        .collect();
                    // Crossings of `item`'s edges at row `row`: with the
                    // edges of the items above it, then below it.
                    let mut cost: usize = others
                        .iter()
                        .map(|&other| self.crossing(item, other, pos))
                        .sum();
                    let (mut best, mut best_row) = (usize::MAX, 0);
                    let mut current = cost;
                    for row in 0..=others.len() {
                        if row == pos[item] {
                            current = cost;
                        }
                        if cost < best {
                            (best, best_row) = (cost, row);
                        }
                        if let Some(&other) = others.get(row) {
                            cost = cost + self.crossing(other, item, pos)
                                - self.crossing(item, other, pos);
                        }
                    }
                    if best < current {
                        let layer = &mut self.layers[column];
                        layer.retain(|&other| other != item);
                        layer.insert(best_row, item);
                        for (row, &moved) in layer.iter().enumerate() {
                            pos[moved] = row;
                        }
                        improved = true;
                    }
                }
            }
        }
    }

    /// Segments that are not between two dummies but cross such an inner
    /// segment (Brandes–Köpf's type 1 conflicts), as (left, right) items:
    /// they are never aligned, so long edges stay straight.
    fn conflicts(&self, pos: &[usize]) -> HashSet<(usize, usize)> {
        let inner = |item: usize| {
            if self.is_dummy(item) {
                self.preds[item]
                    .iter()
                    .copied()
                    .find(|&pred| self.is_dummy(pred))
            } else {
                None
            }
        };
        let mut marked = HashSet::new();
        for pair in self.layers.windows(2) {
            let (left, right) = (&pair[0], &pair[1]);
            let (mut low, mut scanned) = (0, 0);
            for (at, &item) in right.iter().enumerate() {
                let inner_pred = inner(item);
                if at + 1 < right.len() && inner_pred.is_none() {
                    continue;
                }
                let high = inner_pred.map_or(left.len().saturating_sub(1), |pred| pos[pred]);
                for &later in &right[scanned..=at] {
                    for &pred in &self.preds[later] {
                        if (pos[pred] < low || pos[pred] > high) && inner(later) != Some(pred) {
                            marked.insert((pred, later));
                        }
                    }
                }
                scanned = at + 1;
                low = high;
            }
        }
        marked
    }

    /// Each item's vertical center, the topmost card or dummy at
    /// `metrics.margin`.
    fn heights(&self, pos: &[usize], metrics: &Metrics) -> Vec<f32> {
        let count = self.column.len();
        if count == 0 {
            return Vec::new();
        }
        let conflicts = self.conflicts(pos);
        // (packed towards the bottom, centers) per direction.
        let candidates: Vec<(bool, Vec<f32>)> =
            [(true, false), (true, true), (false, false), (false, true)]
                .into_iter()
                .map(|(rightwards, upwards)| {
                    (
                        upwards,
                        self.brandes_kopf(rightwards, upwards, &conflicts, metrics),
                    )
                })
                .collect();
        let extent = |center: &[f32]| {
            (0..count).fold((f32::INFINITY, f32::NEG_INFINITY), |(top, bottom), item| {
                let half = self.half(item, metrics);
                (
                    top.min(center[item] - half),
                    bottom.max(center[item] + half),
                )
            })
        };
        let extents: Vec<(f32, f32)> = candidates
            .iter()
            .map(|(_, center)| extent(center))
            .collect();
        let narrowest = (0..extents.len())
            .min_by(|&a, &b| {
                let size = |(top, bottom): (f32, f32)| bottom - top;
                size(extents[a]).total_cmp(&size(extents[b]))
            })
            .expect("four candidates");
        let (top, bottom) = extents[narrowest];
        let shifts: Vec<f32> = candidates
            .iter()
            .zip(&extents)
            .map(|((upwards, _), extent)| {
                if *upwards {
                    bottom - extent.1
                } else {
                    top - extent.0
                }
            })
            .collect();
        let mut center: Vec<f32> = (0..count)
            .map(|item| {
                let mut four: Vec<f32> = candidates
                    .iter()
                    .zip(&shifts)
                    .map(|((_, center), shift)| center[item] + shift)
                    .collect();
                four.sort_by(f32::total_cmp);
                (four[1] + four[2]) / 2.
            })
            .collect();
        self.straighten(&mut center, metrics);
        let (top, _) = extent(&center);
        for y in &mut center {
            *y += metrics.margin - top;
        }
        center
    }

    /// One Brandes–Köpf candidate: blocks aligned along the columns from
    /// the left (`rightwards`) or the right, in each column from the top
    /// or the bottom (`upwards`: packed towards the bottom).
    fn brandes_kopf(
        &self,
        rightwards: bool,
        upwards: bool,
        conflicts: &HashSet<(usize, usize)>,
        metrics: &Metrics,
    ) -> Vec<f32> {
        let count = self.column.len();
        let layers: Vec<Vec<usize>> = self
            .layers
            .iter()
            .map(|layer| {
                if upwards {
                    layer.iter().rev().copied().collect()
                } else {
                    layer.clone()
                }
            })
            .collect();
        let mut at = vec![0; count];
        for layer in &layers {
            for (row, &item) in layer.iter().enumerate() {
                at[item] = row;
            }
        }
        let upper = if rightwards { &self.preds } else { &self.succs };
        // Vertical alignment: `align` links each block into a cycle,
        // `root` is its first item.
        let mut root: Vec<usize> = (0..count).collect();
        let mut align = root.clone();
        let mut sweep: Vec<&Vec<usize>> = layers.iter().collect();
        if !rightwards {
            sweep.reverse();
        }
        for layer in sweep {
            let mut taken: Option<usize> = None;
            for &item in layer {
                let mut neighbors = upper[item].clone();
                if neighbors.is_empty() {
                    continue;
                }
                neighbors.sort_by_key(|&n| at[n]);
                let degree = neighbors.len();
                for median in [(degree - 1) / 2, degree / 2] {
                    if align[item] != item {
                        break;
                    }
                    let neighbor = neighbors[median];
                    let segment = if rightwards {
                        (neighbor, item)
                    } else {
                        (item, neighbor)
                    };
                    if !conflicts.contains(&segment)
                        && taken.is_none_or(|taken| taken < at[neighbor])
                    {
                        align[neighbor] = item;
                        root[item] = root[neighbor];
                        align[item] = root[item];
                        taken = Some(at[neighbor]);
                    }
                }
            }
        }
        // Compaction: each block as high as the blocks above it allow.
        let mut packing = Packing {
            graph: self,
            layers: &layers,
            at: &at,
            root: &root,
            align: &align,
            metrics,
            y: vec![None; count],
        };
        for layer in &layers {
            for &item in layer {
                if root[item] == item {
                    packing.place(item);
                }
            }
        }
        (0..count)
            .map(|item| {
                let y = packing.y[root[item]].expect("every block is placed");
                if upwards { -y } else { y }
            })
            .collect()
    }

    /// Moves each step to the median of its neighbours where its column
    /// leaves room: of the steps it shares an edge with, or of its long
    /// edges' dummies when it has none (so a chain ends up on one row, and
    /// a long edge bends rather than a step-to-step one). Between two
    /// medians on one side (a fan) it goes midway; one on each side (a
    /// chain bending) leaves it anywhere between. Never grows the graph:
    /// the targets lie among the neighbours.
    fn straighten(&self, center: &mut [f32], metrics: &Metrics) {
        let pos = self.positions();
        for round in 0..2 * STRAIGHTEN_ROUNDS {
            let mut columns: Vec<&Vec<usize>> = self.layers.iter().collect();
            if round % 2 == 1 {
                columns.reverse();
            }
            for layer in columns {
                for &item in layer.iter().filter(|&&item| !self.is_dummy(item)) {
                    // (item, on the left) per neighbour.
                    let neighbors = || {
                        let left = self.preds[item].iter().map(|&n| (n, true));
                        left.chain(self.succs[item].iter().map(|&n| (n, false)))
                    };
                    let steps = neighbors().any(|(n, _)| !self.is_dummy(n));
                    let mut near: Vec<(f32, bool)> = neighbors()
                        .filter(|&(n, _)| !steps || !self.is_dummy(n))
                        .map(|(n, left)| (center[n], left))
                        .collect();
                    if near.is_empty() {
                        continue;
                    }
                    near.sort_by(|a, b| a.0.total_cmp(&b.0));
                    let middle = near.len() / 2;
                    let target = if near.len() % 2 == 1 {
                        near[middle].0
                    } else {
                        let (low, high) = (near[middle - 1], near[middle]);
                        if low.1 == high.1 {
                            (low.0 + high.0) / 2.
                        } else {
                            center[item].clamp(low.0, high.0)
                        }
                    };
                    let row = pos[item];
                    let above = row
                        .checked_sub(1)
                        .map(|row| layer[row])
                        .map_or(f32::NEG_INFINITY, |above| {
                            center[above] + self.separation(above, item, metrics)
                        });
                    let below = layer.get(row + 1).map_or(f32::INFINITY, |&below| {
                        center[below] - self.separation(item, below, metrics)
                    });
                    if above <= below {
                        center[item] = target.clamp(above, below);
                    }
                }
            }
        }
    }
}

/// Brandes–Köpf's horizontal compaction (here: vertical), as a longest
/// path over the blocks.
struct Packing<'a> {
    graph: &'a Layered,
    layers: &'a [Vec<usize>],
    at: &'a [usize],
    root: &'a [usize],
    align: &'a [usize],
    metrics: &'a Metrics,
    /// By block root.
    y: Vec<Option<f32>>,
}

impl Packing<'_> {
    fn place(&mut self, block: usize) {
        if self.y[block].is_some() {
            return;
        }
        let mut y = 0f32;
        self.y[block] = Some(y);
        let mut item = block;
        loop {
            if let Some(row) = self.at[item].checked_sub(1) {
                let above = self.layers[self.graph.column[item]][row];
                let above_block = self.root[above];
                self.place(above_block);
                let least = self.y[above_block].expect("just placed")
                    + self.graph.separation(above, item, self.metrics);
                y = y.max(least);
                self.y[block] = Some(y);
            }
            item = self.align[item];
            if item == block {
                break;
            }
        }
    }
}

/// The next permutation of `items` in lexicographic order; `false` after
/// the last.
fn next_permutation(items: &mut [usize]) -> bool {
    let Some(at) = (1..items.len()).rev().find(|&at| items[at - 1] < items[at]) else {
        return false;
    };
    let swap = (at..items.len())
        .rev()
        .find(|&later| items[later] > items[at - 1])
        .expect("items[at] is larger");
    items.swap(at - 1, swap);
    items[at..].reverse();
    true
}

/// Sorts a column by `heuristic`'s key, items without neighbours on that
/// side staying where they are.
fn reorder(layer: &mut [usize], neighbors: &[Vec<usize>], pos: &mut [usize], heuristic: Heuristic) {
    let keys: Vec<Option<f32>> = layer
        .iter()
        .map(|&item| heuristic.key(&neighbors[item], pos))
        .collect();
    let mut movable: Vec<(f32, usize)> = layer
        .iter()
        .zip(&keys)
        .filter_map(|(&item, key)| key.map(|key| (key, item)))
        .collect();
    // Stable: ties keep their order.
    movable.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut movable = movable.into_iter();
    for (slot, key) in layer.iter_mut().zip(&keys) {
        if key.is_some() {
            *slot = movable.next().expect("a movable item per free slot").1;
        }
    }
    for (row, &item) in layer.iter().enumerate() {
        pos[item] = row;
    }
}

/// The ports on one side of a card centered at `center`, as (neighbouring
/// item, height): one edge in the middle; several spread evenly in the
/// order of their neighbours, the group shifted (within [`PORT_SPAN`])
/// towards them, so an edge to a neighbour level with the card stays
/// straight.
fn ports(
    center: f32,
    neighbors: &[usize],
    centers: &[f32],
    pos: &[usize],
    metrics: &Metrics,
) -> Vec<(usize, f32)> {
    let mut sorted = neighbors.to_vec();
    sorted.sort_by_key(|&n| pos[n]);
    let count = sorted.len();
    if count <= 1 {
        return sorted.into_iter().map(|n| (n, center)).collect();
    }
    let room = metrics.card_height * PORT_SPAN;
    let pitch = (metrics.card_height / 6.).min(room / (count - 1) as f32);
    let spread = pitch * (count - 1) as f32;
    let at = |k: usize| center - spread / 2. + pitch * k as f32;
    // The shift closest to none among those minimizing the total distance
    // to the neighbours (a median).
    let mut wanted: Vec<f32> = sorted
        .iter()
        .enumerate()
        .map(|(k, &n)| centers[n] - at(k))
        .collect();
    wanted.sort_by(f32::total_cmp);
    let (low, high) = (wanted[(count - 1) / 2], wanted[count / 2]);
    let shift = if low > 0. {
        low
    } else if high < 0. {
        high
    } else {
        0.
    };
    let limit = (room - spread) / 2.;
    let shift = shift.clamp(-limit, limit);
    sorted
        .into_iter()
        .enumerate()
        .map(|(k, n)| (n, at(k) + shift))
        .collect()
}

/// [`layout`] for a parsed workflow (which is valid, so this succeeds).
pub fn layout_workflow(workflow: &Workflow, metrics: Metrics) -> Result<GraphLayout, String> {
    let specs: Vec<NodeSpec> = workflow
        .steps
        .iter()
        .map(|step| NodeSpec {
            id: &step.id,
            needs: &step.needs,
            after: &step.after,
        })
        .collect();
    layout(&specs, metrics)
}

/// How a graph of some size shows in a viewport: drawn at `offset` (the
/// graph's top-left corner, viewport coordinates) and `scale`d. A point
/// `p` of the layout lands at `offset + p * scale`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct View {
    pub scale: f32,
    pub offset: (f32, f32),
}

/// Smallest zoom (fitting and zooming); a larger graph is panned.
pub const MIN_SCALE: f32 = 0.4;
/// Largest zoom by hand; fitting never enlarges past 100 %.
pub const MAX_SCALE: f32 = 2.0;
/// Space kept around a fitted graph.
pub const FIT_MARGIN: f32 = 8.;

impl View {
    /// The whole `content` (layout width, height) in `viewport` with
    /// [`FIT_MARGIN`] around it: at most 100 %, at least [`MIN_SCALE`],
    /// centered (top-left along an axis that still does not fit).
    pub fn fit(content: (f32, f32), viewport: (f32, f32)) -> Self {
        let room = |view: f32| (view - 2. * FIT_MARGIN).max(1.);
        let wanted = |content: f32, view: f32| {
            if content > 0. {
                room(view) / content
            } else {
                1.
            }
        };
        let scale = wanted(content.0, viewport.0)
            .min(wanted(content.1, viewport.1))
            .clamp(MIN_SCALE, 1.);
        let place = |content: f32, view: f32| ((view - content * scale) / 2.).max(0.);
        Self {
            scale,
            offset: (place(content.0, viewport.0), place(content.1, viewport.1)),
        }
    }

    /// Zoomed to `scale` (clamped to [`MIN_SCALE`]..=[`MAX_SCALE`]) with
    /// the layout point under `anchor` (viewport coordinates) kept there.
    pub fn zoom_at(self, scale: f32, anchor: (f32, f32)) -> Self {
        let scale = scale.clamp(MIN_SCALE, MAX_SCALE);
        let keep = |offset: f32, anchor: f32| {
            let point = (anchor - offset) / self.scale;
            anchor - point * scale
        };
        Self {
            scale,
            offset: (keep(self.offset.0, anchor.0), keep(self.offset.1, anchor.1)),
        }
    }

    /// Moved so the graph stays in `viewport`: a graph smaller than the
    /// viewport along an axis stays fully inside, a larger one keeps
    /// covering it.
    pub fn clamped(self, content: (f32, f32), viewport: (f32, f32)) -> Self {
        let clamp = |offset: f32, content: f32, view: f32| {
            let free = view - content * self.scale;
            if free >= 0. {
                offset.clamp(0., free)
            } else {
                offset.clamp(free, 0.)
            }
        };
        Self {
            scale: self.scale,
            offset: (
                clamp(self.offset.0, content.0, viewport.0),
                clamp(self.offset.1, content.1, viewport.1),
            ),
        }
    }

    /// The layout point shown at viewport point `at` (hit testing).
    pub fn to_layout(self, at: (f32, f32)) -> (f32, f32) {
        (
            (at.0 - self.offset.0) / self.scale,
            (at.1 - self.offset.1) / self.scale,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const M: Metrics = Metrics {
        card_width: 100.,
        card_height: 40.,
        column_gap: 50.,
        row_gap: 10.,
        edge_gap: 8.,
        margin: 5.,
    };

    /// A login chain, a fan, and a long edge from the first step to the
    /// last.
    const AURA: [(&str, &str); 6] = [
        ("vpn", ""),
        ("login", "vpn"),
        ("approve", "login"),
        ("mcp", "approve"),
        ("ui", "approve"),
        ("done", "mcp ui vpn"),
    ];
    /// Long edges over fans in and out, listed in an unhelpful order.
    const TANGLE: [(&str, &str); 10] = [
        ("a", ""),
        ("b", ""),
        ("c", "a"),
        ("d", "b"),
        ("e", "a b"),
        ("f", "c d"),
        ("g", "c"),
        ("h", "e a"),
        ("i", "f g h b"),
        ("j", "d i"),
    ];
    /// K2,2 (one crossing is unavoidable) next to a matching listed
    /// backwards (none is).
    const K22: [(&str, &str); 10] = [
        ("a", ""),
        ("b", ""),
        ("x", "a b"),
        ("y", "a b"),
        ("p", ""),
        ("q", ""),
        ("r", ""),
        ("s", "r"),
        ("t", "q"),
        ("u", "p"),
    ];
    /// A fan out and back in, and an edge from the start past all of it.
    const LONG_FAN: [(&str, &str); 7] = [
        ("s", ""),
        ("a1", "s"),
        ("a2", "s"),
        ("a3", "s"),
        ("t", "a1 a2 a3"),
        ("u", "t"),
        ("end", "s u"),
    ];

    /// `("id", "need need ~after")` pairs, as specs for `f`.
    fn with_specs<R>(steps: &[(&str, &str)], f: impl FnOnce(&[NodeSpec]) -> R) -> R {
        let lists: Vec<(Vec<String>, Vec<String>)> = steps
            .iter()
            .map(|(_, needs)| {
                let (after, needs): (Vec<&str>, Vec<&str>) =
                    needs.split_whitespace().partition(|id| id.starts_with('~'));
                (
                    needs.into_iter().map(str::to_owned).collect(),
                    after
                        .into_iter()
                        .map(|id| id.trim_start_matches('~').to_owned())
                        .collect(),
                )
            })
            .collect();
        let specs: Vec<NodeSpec> = steps
            .iter()
            .zip(&lists)
            .map(|((id, _), (needs, after))| NodeSpec { id, needs, after })
            .collect();
        f(&specs)
    }

    fn graph(steps: &[(&str, &str)]) -> Result<GraphLayout, String> {
        with_specs(steps, |specs| layout(specs, M))
    }

    fn ids<'a>(steps: &[(&'a str, &str)], layout: &GraphLayout) -> Vec<Vec<&'a str>> {
        layout
            .columns
            .iter()
            .map(|column| column.iter().map(|&at| steps[at].0).collect())
            .collect()
    }

    fn near(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-3
    }

    type Point = (f32, f32);

    /// Whether segments `a`–`b` and `c`–`d` cross (touching does not).
    fn cross(a: Point, b: Point, c: Point, d: Point) -> bool {
        let side =
            |p: Point, q: Point, r: Point| (q.0 - p.0) * (r.1 - p.1) - (q.1 - p.1) * (r.0 - p.0);
        side(c, d, a) * side(c, d, b) < 0. && side(a, b, c) * side(a, b, d) < 0.
    }

    /// Crossings as drawn: between consecutive points an edge is monotone
    /// in x over one column gap (or level through a column), so two cross
    /// exactly when the straight segments do.
    fn crossings(layout: &GraphLayout) -> usize {
        let mut count = 0;
        for (at, edge) in layout.edges.iter().enumerate() {
            for other in &layout.edges[at + 1..] {
                for a in edge.points.windows(2) {
                    for b in other.points.windows(2) {
                        if cross(a[0], a[1], b[0], b[1]) {
                            count += 1;
                        }
                    }
                }
            }
        }
        count
    }

    /// What every layout promises: edges, drawn with horizontal tangents
    /// at their points, never enter a card; they leave their step's right
    /// side and enter the left side of the one needing it at a port of
    /// their own; cards neither overlap nor leave the graph.
    fn assert_clean(layout: &GraphLayout) {
        let m = layout.metrics;
        let inside = |(x, y): Point| {
            layout.nodes.iter().position(|node| {
                x > node.x + 0.01
                    && x < node.x + m.card_width - 0.01
                    && y > node.y + 0.01
                    && y < node.y + m.card_height - 0.01
            })
        };
        let mut ports: Vec<(bool, usize, f32)> = Vec::new();
        for edge in &layout.edges {
            let (from, to) = (layout.nodes[edge.from], layout.nodes[edge.to]);
            let (start, end) = (edge.points[0], edge.points[edge.points.len() - 1]);
            assert!(
                near(start.0, from.x + m.card_width)
                    && start.1 > from.y
                    && start.1 < from.y + m.card_height,
                "{edge:?}"
            );
            assert!(
                near(end.0, to.x) && end.1 > to.y && end.1 < to.y + m.card_height,
                "{edge:?}"
            );
            for (out, step, y) in [(true, edge.from, start.1), (false, edge.to, end.1)] {
                assert!(
                    !ports
                        .iter()
                        .any(|&(o, s, at)| o == out && s == step && near(at, y)),
                    "a port of its own: {edge:?}"
                );
                ports.push((out, step, y));
            }
            for pair in edge.points.windows(2) {
                let (a, b) = (pair[0], pair[1]);
                assert!(b.0 > a.0, "left to right: {edge:?}");
                let handle = (b.0 - a.0) / 2.;
                for k in 0..=64 {
                    let t = k as f32 / 64.;
                    let u = 1. - t;
                    let bezier = |p0: f32, p1: f32, p2: f32, p3: f32| {
                        u * u * u * p0 + 3. * u * u * t * p1 + 3. * u * t * t * p2 + t * t * t * p3
                    };
                    let point = (
                        bezier(a.0, a.0 + handle, b.0 - handle, b.0),
                        bezier(a.1, a.1, b.1, b.1),
                    );
                    assert_eq!(inside(point), None, "{edge:?} enters a card at {point:?}");
                }
            }
        }
        for column in &layout.columns {
            for pair in column.windows(2) {
                let (upper, lower) = (layout.nodes[pair[0]], layout.nodes[pair[1]]);
                assert!(lower.y >= upper.y + m.card_height + m.row_gap - 0.01);
            }
        }
        for node in &layout.nodes {
            assert!(node.y >= m.margin - 0.01);
            assert!(node.y + m.card_height <= layout.height - m.margin + 0.01);
        }
    }

    /// The fewest crossings any order of the columns (dummies included)
    /// has, by trying them all.
    fn fewest_crossings(graph: &mut Layered) -> usize {
        fn permutations(items: &[usize]) -> Vec<Vec<usize>> {
            if items.len() <= 1 {
                return vec![items.to_vec()];
            }
            let mut all = Vec::new();
            for at in 0..items.len() {
                let mut rest = items.to_vec();
                let first = rest.remove(at);
                for mut tail in permutations(&rest) {
                    tail.insert(0, first);
                    all.push(tail);
                }
            }
            all
        }
        fn search(graph: &mut Layered, options: &[Vec<Vec<usize>>], column: usize) -> usize {
            if column == options.len() {
                return graph.crossings(&graph.positions());
            }
            let mut best = usize::MAX;
            for order in &options[column] {
                graph.layers[column] = order.clone();
                best = best.min(search(graph, options, column + 1));
            }
            best
        }
        let options: Vec<Vec<Vec<usize>>> = graph
            .layers
            .iter()
            .map(|layer| permutations(layer))
            .collect();
        search(graph, &options, 0)
    }

    #[test]
    fn ordering_finds_the_fewest_crossings() {
        for (name, steps, fewest) in [
            ("K2,2", &K22[..], 1),
            ("AURA", &AURA[..], 0),
            ("long fan", &LONG_FAN[..], 0),
            ("tangle", &TANGLE[..], 2),
        ] {
            let (file_order, found, best) = with_specs(steps, |specs| {
                let (mut graph, _) = layered(specs).unwrap();
                let file_order = graph.crossings(&graph.positions());
                graph.order();
                let found = graph.crossings(&graph.positions());
                (file_order, found, fewest_crossings(&mut graph))
            });
            eprintln!("{name}: {file_order} crossings in file order, {found} ordered");
            assert_eq!((found, best), (fewest, fewest), "{name}");
            let layout = graph(steps).unwrap();
            assert_eq!(crossings(&layout), fewest, "{name} as drawn");
            assert_clean(&layout);
        }
    }

    #[test]
    fn chains_stay_on_one_row_and_long_edges_go_around() {
        let layout = graph(&AURA).unwrap();
        assert_eq!(
            ids(&AURA, &layout),
            [
                vec!["vpn"],
                vec!["login"],
                vec!["approve"],
                vec!["mcp", "ui"],
                vec!["done"]
            ]
        );
        let y = |at: usize| layout.nodes[at].y;
        assert_eq!((y(0), y(1)), (y(2), y(2)), "vpn → login → approve level");
        assert!(
            near(y(2), (y(3) + y(4)) / 2.),
            "the fan centered on approve"
        );
        assert!(
            near(y(5), (y(3) + y(4)) / 2.),
            "done centered on the steps it joins"
        );
        let edge = |from: usize, to: usize| {
            layout
                .edges
                .iter()
                .find(|edge| (edge.from, edge.to) == (from, to))
                .unwrap()
        };
        for (from, to) in [(0, 1), (1, 2)] {
            let points = &edge(from, to).points;
            assert_eq!(points.len(), 2);
            assert_eq!(points[0].1, points[1].1, "straight");
        }
        // vpn → done passes columns 1 to 3 on one level, below the cards.
        let long = &edge(0, 5).points;
        assert_eq!(long.len(), 2 + 2 * 3);
        let lane = long[1].1;
        assert!(long[1..7].iter().all(|point| point.1 == lane));
        assert!(lane >= y(4) + 40. + 8. - 0.01, "{long:?}");
        assert!(
            layout.height < 2. * 5. + 3. * 40. + 2. * 10.,
            "less than three rows tall: {}",
            layout.height
        );
        assert_clean(&layout);

        // A chain, and a step joining its end with a long edge from
        // another root: the chain and the join stay level.
        let steps = [
            ("a", ""),
            ("b", "a"),
            ("c", "b"),
            ("d", "c"),
            ("x", ""),
            ("y", "x d"),
        ];
        let layout = graph(&steps).unwrap();
        let rows: Vec<f32> = [0, 1, 2, 3, 5].map(|at| layout.nodes[at].y).to_vec();
        assert!(rows.iter().all(|&row| row == rows[0]), "{rows:?}");
        assert_clean(&layout);
    }

    #[test]
    fn large_workflows_stay_clean() {
        // 90 steps, each needing up to three of the twelve before it.
        let mut seed = 0x2545_f491_u32;
        let mut random = |below: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed as usize % below
        };
        let ids: Vec<String> = (0..90).map(|at| format!("s{at}")).collect();
        let needs: Vec<String> = (0..90)
            .map(|at| {
                let count = if at == 0 { 0 } else { random(4) };
                (0..count)
                    .map(|_| ids[at - 1 - random(at.min(12))].as_str())
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect();
        let steps: Vec<(&str, &str)> = ids
            .iter()
            .zip(&needs)
            .map(|(id, needs)| (id.as_str(), needs.as_str()))
            .collect();
        let started = std::time::Instant::now();
        let layout = graph(&steps).unwrap();
        eprintln!(
            "90 steps, {} edges: {} crossings, {:?}",
            layout.edges.len(),
            crossings(&layout),
            started.elapsed()
        );
        let ordered = with_specs(&steps, |specs| {
            let (mut graph, _) = layered(specs).unwrap();
            graph.order();
            graph.crossings(&graph.positions())
        });
        assert_eq!(crossings(&layout), ordered, "drawn as ordered");
        assert_clean(&layout);
    }

    #[test]
    fn layouts_are_deterministic() {
        for steps in [&TANGLE[..], &AURA[..], &K22[..], &LONG_FAN[..]] {
            let first = graph(steps).unwrap();
            for _ in 0..3 {
                assert_eq!(graph(steps).unwrap(), first);
            }
        }
    }

    #[test]
    fn a_diamond_takes_three_columns_with_the_middle_centered() {
        let steps = [
            ("check", ""),
            ("api", "check"),
            ("web", "check"),
            ("join", "api web"),
        ];
        let layout = graph(&steps).unwrap();
        assert_eq!(
            ids(&steps, &layout),
            [vec!["check"], vec!["api", "web"], vec!["join"]]
        );
        // Columns 150 apart; the lone steps centered on the pair.
        let at = |i: usize| (layout.nodes[i].x, layout.nodes[i].y);
        assert_eq!(at(0), (5., 5. + 25.));
        assert_eq!(at(1), (155., 5.));
        assert_eq!(at(2), (155., 55.));
        assert_eq!(at(3), (305., 30.));
        assert_eq!(
            (layout.width, layout.height),
            (2. * 5. + 400., 2. * 5. + 90.)
        );
        // A lone edge leaves the right middle; two entering a side get a
        // port each, evenly around the middle.
        assert_eq!(layout.edges.len(), 4);
        let edge = layout
            .edges
            .iter()
            .find(|e| e.from == 2 && e.to == 3)
            .unwrap();
        assert_eq!(edge.points[0], (255., 75.));
        let end = edge.points[1];
        assert!(end.0 == 305. && near(end.1, 50. + 40. / 12.), "{end:?}");
        assert_eq!(layout.step_at(160., 60.), Some(2));
        assert_eq!(layout.step_at(140., 60.), None, "in the gap");
        assert_clean(&layout);
    }

    #[test]
    fn depth_is_the_longest_chain_of_needs() {
        // `late` needs a root and a step two deep: it goes after the latter.
        let steps = [("a", ""), ("b", "a"), ("c", "b"), ("late", "a c")];
        let layout = graph(&steps).unwrap();
        let columns: Vec<usize> = layout.nodes.iter().map(|node| node.column).collect();
        assert_eq!(columns, [0, 1, 2, 3]);
        assert_clean(&layout);
    }

    #[test]
    fn fan_out_and_fan_in() {
        let fan_out = [("root", ""), ("x", "root"), ("y", "root"), ("z", "root")];
        let layout = graph(&fan_out).unwrap();
        assert_eq!(ids(&fan_out, &layout), [vec!["root"], vec!["x", "y", "z"]]);
        assert_eq!(
            layout.nodes[0].y, layout.nodes[2].y,
            "centered on the middle one"
        );
        assert_eq!(layout.height, 2. * 5. + 3. * 40. + 2. * 10., "no taller");
        assert_clean(&layout);
        let fan_in = [("x", ""), ("y", ""), ("z", ""), ("all", "x y z")];
        let layout = graph(&fan_in).unwrap();
        assert_eq!(ids(&fan_in, &layout), [vec!["x", "y", "z"], vec!["all"]]);
        let ends: Vec<Point> = layout
            .edges
            .iter()
            .map(|edge| edge.points[edge.points.len() - 1])
            .collect();
        // Three ports a sixth of the card apart, in the order of the
        // steps they come from.
        let middle = 5. + 50. + 20.;
        let pitch = 40. / 6.;
        assert_eq!(ends.len(), 3);
        for (end, want) in ends.iter().zip([middle - pitch, middle, middle + pitch]) {
            assert!(end.0 == 155. && near(end.1, want), "{ends:?}");
        }
        assert_clean(&layout);
    }

    #[test]
    fn ordering_uncrosses_edges() {
        // In file order `b2` (needing `a2`, below `a1`) sits above `b1`.
        let steps = [("a1", ""), ("a2", ""), ("b2", "a2"), ("b1", "a1")];
        let layout = graph(&steps).unwrap();
        assert_eq!(ids(&steps, &layout), [vec!["a1", "a2"], vec!["b1", "b2"]]);
    }

    #[test]
    fn disconnected_steps_and_empty_workflows() {
        let steps = [("a", ""), ("b", "a"), ("lonely", "")];
        let layout = graph(&steps).unwrap();
        assert_eq!(ids(&steps, &layout), [vec!["a", "lonely"], vec!["b"]]);
        assert_eq!(layout.edges.len(), 1);
        let empty = graph(&[]).unwrap();
        assert!(empty.nodes.is_empty() && empty.columns.is_empty());
        assert_eq!((empty.width, empty.height), (10., 10.));
    }

    #[test]
    fn invalid_graphs_are_errors_not_panics() {
        let cycle = graph(&[("a", "c"), ("b", "a"), ("c", "b"), ("free", "")]).unwrap_err();
        assert_eq!(cycle, "dependency cycle among a, b, c");
        assert!(graph(&[("a", "a")]).unwrap_err().contains("cycle"));
        assert_eq!(
            graph(&[("a", "nope")]).unwrap_err(),
            "`a` needs unknown step `nope`"
        );
        assert_eq!(
            graph(&[("a", ""), ("a", "")]).unwrap_err(),
            "duplicate step id `a`"
        );
        // A need listed twice draws one edge.
        assert_eq!(graph(&[("a", ""), ("b", "a a")]).unwrap().edges.len(), 1);
    }

    #[test]
    fn after_edges_count_for_depth_and_are_marked() {
        let steps = [("eval", ""), ("report", "~eval"), ("notify", "report")];
        let layout = graph(&steps).unwrap();
        assert_eq!(
            ids(&steps, &layout),
            [vec!["eval"], vec!["report"], vec!["notify"]]
        );
        let kinds: Vec<(usize, usize, bool)> = layout
            .edges
            .iter()
            .map(|edge| (edge.from, edge.to, edge.after))
            .collect();
        assert_eq!(kinds, [(0, 1, true), (1, 2, false)]);
        assert_eq!(
            graph(&[("a", "~nope")]).unwrap_err(),
            "`a` runs after unknown step `nope`"
        );
        assert!(
            graph(&[("a", "~b"), ("b", "a")])
                .unwrap_err()
                .contains("cycle")
        );
    }

    #[test]
    fn fitting_scales_down_to_the_viewport_and_centers() {
        // 5 columns, 1180 × 300, in an 860 × 700 pane: width decides.
        let view = View::fit((1180., 300.), (860., 700.));
        let scale = (860. - 2. * FIT_MARGIN) / 1180.;
        assert!((view.scale - scale).abs() < 1e-6);
        assert!((view.offset.0 - FIT_MARGIN).abs() < 1e-3);
        assert!((view.offset.1 - (700. - 300. * scale) / 2.).abs() < 1e-3);
        // Small graphs are not enlarged, just centered.
        let small = View::fit((200., 100.), (800., 600.));
        assert_eq!(
            small,
            View {
                scale: 1.,
                offset: (300., 250.)
            }
        );
        // Huge graphs stop at the smallest zoom, top-left along the axis
        // that still overflows.
        let huge = View::fit((5000., 100.), (800., 600.));
        assert_eq!(huge.scale, MIN_SCALE);
        assert_eq!(huge.offset, (0., 280.));
        // Degenerate sizes do not divide by zero.
        assert_eq!(View::fit((0., 0.), (0., 0.)).scale, 1.);
    }

    #[test]
    fn zooming_keeps_the_point_under_the_pointer() {
        let view = View {
            scale: 0.5,
            offset: (10., 20.),
        };
        let pointer = (210., 120.);
        let under = view.to_layout(pointer);
        assert_eq!(under, (400., 200.));
        let zoomed = view.zoom_at(1., pointer);
        assert_eq!(zoomed.scale, 1.);
        assert_eq!(zoomed.to_layout(pointer), under);
        assert_eq!(zoomed.offset, (-190., -80.));
        // Clamped to the zoom range, still anchored.
        let far = view.zoom_at(10., pointer);
        assert_eq!(far.scale, MAX_SCALE);
        assert_eq!(far.to_layout(pointer), under);
        assert_eq!(view.zoom_at(0.01, pointer).scale, MIN_SCALE);
    }

    #[test]
    fn panning_keeps_the_graph_in_view() {
        let view = |x: f32, y: f32| View {
            scale: 1.,
            offset: (x, y),
        };
        // Wider than the viewport: it may not leave a gap on either side.
        assert_eq!(
            view(50., 0.).clamped((1000., 100.), (600., 400.)).offset,
            (0., 0.)
        );
        assert_eq!(
            view(-900., 0.)
                .clamped((1000., 100.), (600., 400.))
                .offset
                .0,
            -400.
        );
        // Smaller: it stays fully inside.
        assert_eq!(
            view(0., 350.).clamped((1000., 100.), (600., 400.)).offset.1,
            300.
        );
        assert_eq!(
            view(0., -5.).clamped((1000., 100.), (600., 400.)).offset.1,
            0.
        );
        // At half size the 1000-wide graph fits in 600.
        let half = View {
            scale: 0.5,
            offset: (-50., 0.),
        };
        assert_eq!(half.clamped((1000., 100.), (600., 400.)).offset.0, 0.);
    }
}
