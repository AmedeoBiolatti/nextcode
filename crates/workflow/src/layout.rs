//! Where a workflow's steps go in its graph, like Azure DevOps' pipeline
//! stages: one column per dependency depth (the longest chain of `needs`
//! from a step without needs), steps in a column ordered by a few
//! barycenter sweeps so edges cross less, columns centered on the tallest
//! one, and edges from the right side of a step to the left side of each
//! step that needs it. Pure geometry in logical pixels.

use std::collections::HashMap;

use crate::Workflow;

/// Card size and spacing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Metrics {
    pub card_width: f32,
    pub card_height: f32,
    /// Horizontal space between columns (where edges run).
    pub column_gap: f32,
    pub row_gap: f32,
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
            margin: 24.,
        }
    }
}

/// What the layout needs to know about a step.
#[derive(Debug, Clone, Copy)]
pub struct NodeSpec<'a> {
    pub id: &'a str,
    pub needs: &'a [String],
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

/// `from` (a need) → `to` (the step needing it), between the middle of
/// `from`'s right side and the middle of `to`'s left side.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Edge {
    pub from: usize,
    pub to: usize,
    pub start: (f32, f32),
    pub end: (f32, f32),
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
    /// The step whose card contains `(x, y)`.
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

/// Barycenter sweeps (each one down then up the columns).
const SWEEPS: usize = 4;

/// Lays out `nodes`; a duplicate id, an unknown need or a cycle is an
/// error (the file's validation reports those too).
pub fn layout(nodes: &[NodeSpec], metrics: Metrics) -> Result<GraphLayout, String> {
    let mut index: HashMap<&str, usize> = HashMap::new();
    for (at, node) in nodes.iter().enumerate() {
        if index.insert(node.id, at).is_some() {
            return Err(format!("duplicate step id `{}`", node.id));
        }
    }
    let mut needs: Vec<Vec<usize>> = Vec::with_capacity(nodes.len());
    for node in nodes {
        let mut resolved = Vec::with_capacity(node.needs.len());
        for need in node.needs {
            let at = *index
                .get(need.as_str())
                .ok_or_else(|| format!("`{}` needs unknown step `{need}`", node.id))?;
            if !resolved.contains(&at) {
                resolved.push(at);
            }
        }
        needs.push(resolved);
    }
    let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); nodes.len()];
    for (at, node_needs) in needs.iter().enumerate() {
        for &need in node_needs {
            dependents[need].push(at);
        }
    }

    // Longest path from the roots, in topological (Kahn) order.
    let mut waiting: Vec<usize> = needs.iter().map(Vec::len).collect();
    let mut ready: Vec<usize> = (0..nodes.len()).filter(|&at| waiting[at] == 0).collect();
    let mut depth = vec![0usize; nodes.len()];
    let mut done = 0;
    let mut next = 0;
    while next < ready.len() {
        let at = ready[next];
        next += 1;
        done += 1;
        for &dependent in &dependents[at] {
            depth[dependent] = depth[dependent].max(depth[at] + 1);
            waiting[dependent] -= 1;
            if waiting[dependent] == 0 {
                ready.push(dependent);
            }
        }
    }
    if done < nodes.len() {
        let stuck: Vec<&str> = (0..nodes.len())
            .filter(|&at| waiting[at] > 0)
            .map(|at| nodes[at].id)
            .collect();
        return Err(format!("dependency cycle among {}", stuck.join(", ")));
    }

    let column_count = depth.iter().max().map_or(0, |deepest| deepest + 1);
    // File order to start with.
    let mut columns: Vec<Vec<usize>> = vec![Vec::new(); column_count];
    for (at, &column) in depth.iter().enumerate() {
        columns[column].push(at);
    }
    let tallest = columns.iter().map(Vec::len).max().unwrap_or(0);
    // A step's vertical center in rows, columns centered on the tallest.
    let centers = |columns: &[Vec<usize>]| -> Vec<f32> {
        let mut center = vec![0.; nodes.len()];
        for column in columns {
            let offset = (tallest - column.len()) as f32 / 2.;
            for (row, &at) in column.iter().enumerate() {
                center[at] = offset + row as f32;
            }
        }
        center
    };
    let reorder = |column: &mut Vec<usize>, neighbors: &[Vec<usize>], center: &[f32]| {
        let mut keyed: Vec<(f32, usize)> = column
            .iter()
            .map(|&at| {
                let near = &neighbors[at];
                let key = if near.is_empty() {
                    center[at]
                } else {
                    near.iter().map(|&n| center[n]).sum::<f32>() / near.len() as f32
                };
                (key, at)
            })
            .collect();
        // Stable: ties keep their order.
        keyed.sort_by(|a, b| a.0.total_cmp(&b.0));
        *column = keyed.into_iter().map(|(_, at)| at).collect();
    };
    for _ in 0..SWEEPS {
        for column in 1..column_count {
            let center = centers(&columns);
            reorder(&mut columns[column], &needs, &center);
        }
        for column in (0..column_count.saturating_sub(1)).rev() {
            let center = centers(&columns);
            reorder(&mut columns[column], &dependents, &center);
        }
    }

    let center = centers(&columns);
    let row_pitch = metrics.card_height + metrics.row_gap;
    let column_pitch = metrics.card_width + metrics.column_gap;
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
                x: metrics.margin + column as f32 * column_pitch,
                y: metrics.margin + center[at] * row_pitch,
            };
        }
    }
    let mut edges = Vec::new();
    for (to, node_needs) in needs.iter().enumerate() {
        for &from in node_needs {
            let (a, b) = (placed[from], placed[to]);
            edges.push(Edge {
                from,
                to,
                start: (a.x + metrics.card_width, a.y + metrics.card_height / 2.),
                end: (b.x, b.y + metrics.card_height / 2.),
            });
        }
    }
    let span = |count: usize, size: f32, gap: f32| {
        if count == 0 {
            0.
        } else {
            count as f32 * size + (count - 1) as f32 * gap
        }
    };
    Ok(GraphLayout {
        nodes: placed,
        edges,
        width: 2. * metrics.margin + span(column_count, metrics.card_width, metrics.column_gap),
        height: 2. * metrics.margin + span(tallest, metrics.card_height, metrics.row_gap),
        columns,
        metrics,
    })
}

/// [`layout`] for a parsed workflow (which is valid, so this succeeds).
pub fn layout_workflow(workflow: &Workflow, metrics: Metrics) -> Result<GraphLayout, String> {
    let specs: Vec<NodeSpec> = workflow
        .steps
        .iter()
        .map(|step| NodeSpec {
            id: &step.id,
            needs: &step.needs,
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
        margin: 5.,
    };

    /// `("id", "need need")` pairs.
    fn graph(steps: &[(&str, &str)]) -> Result<GraphLayout, String> {
        let needs: Vec<Vec<String>> = steps
            .iter()
            .map(|(_, needs)| needs.split_whitespace().map(str::to_owned).collect())
            .collect();
        let specs: Vec<NodeSpec> = steps
            .iter()
            .zip(&needs)
            .map(|((id, _), needs)| NodeSpec { id, needs })
            .collect();
        layout(&specs, M)
    }

    fn ids<'a>(steps: &[(&'a str, &str)], layout: &GraphLayout) -> Vec<Vec<&'a str>> {
        layout
            .columns
            .iter()
            .map(|column| column.iter().map(|&at| steps[at].0).collect())
            .collect()
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
        // Edges leave the right middle of the need, enter the left middle.
        assert_eq!(layout.edges.len(), 4);
        let edge = layout
            .edges
            .iter()
            .find(|e| e.from == 2 && e.to == 3)
            .unwrap();
        assert_eq!(edge.start, (255., 75.));
        assert_eq!(edge.end, (305., 50.));
        assert_eq!(layout.step_at(160., 60.), Some(2));
        assert_eq!(layout.step_at(140., 60.), None, "in the gap");
    }

    #[test]
    fn depth_is_the_longest_chain_of_needs() {
        // `late` needs a root and a step two deep: it goes after the latter.
        let steps = [("a", ""), ("b", "a"), ("c", "b"), ("late", "a c")];
        let layout = graph(&steps).unwrap();
        let columns: Vec<usize> = layout.nodes.iter().map(|node| node.column).collect();
        assert_eq!(columns, [0, 1, 2, 3]);
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
        let fan_in = [("x", ""), ("y", ""), ("z", ""), ("all", "x y z")];
        let layout = graph(&fan_in).unwrap();
        assert_eq!(ids(&fan_in, &layout), [vec!["x", "y", "z"], vec!["all"]]);
        assert_eq!(layout.edges.len(), 3);
        assert!(
            layout
                .edges
                .iter()
                .all(|edge| edge.end == (155., 5. + 50. + 20.))
        );
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
