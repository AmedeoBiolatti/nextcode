//! Highlights and the picker, painted over everything else in the window:
//! outlines with a soft glow, tints, underlines, numbered badges, note
//! callouts kept inside the window, and the picker's hover outline.

use std::{collections::HashMap, f32::consts::PI, sync::Arc, time::Duration};

use gpui::{
    AnyElement, App, AvailableSpace, Bounds, BoxShadow, CursorStyle, DispatchPhase, FontWeight,
    Hitbox, HitboxBehavior, Hsla, InteractiveElement, IntoElement, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, ParentElement, Pixels, Point, SharedString, Size, Styled,
    TextRun, Window, WindowId, div, point, px, size,
};

use super::{
    Frame, HighlightId, PickText, Report, control,
    highlight::{self, Highlight, Style},
    picker,
    target::Target,
};
use crate::theme;

/// What became of a highlight in the last frame.
#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    Shown {
        rects: Vec<Bounds<Pixels>>,
        /// Part of it is scrolled away or clipped.
        clipped: bool,
        /// The text's characters were not reported: the whole element.
        approximate: bool,
    },
    /// Drawn, but nothing of it shows (scrolled away), or its owner has not
    /// drawn it (yet).
    NotVisible {
        reason: String,
        ancestor: Option<SharedString>,
    },
    NotFound {
        reason: String,
        /// The deepest part of the path that exists, and its children.
        ancestor: Option<SharedString>,
        children: Vec<SharedString>,
        suggestions: Vec<SharedString>,
    },
}

impl Status {
    pub fn name(&self) -> &'static str {
        match self {
            Status::Shown { .. } => "shown",
            Status::NotVisible { .. } => "not_visible",
            Status::NotFound { .. } => "not_found",
        }
    }
}

/// How long a new highlight pulses.
const PULSE: Duration = Duration::from_millis(1800);
const MARGIN: f32 = 8.;
const CALLOUT_MAX_WIDTH: f32 = 300.;
const NOTE_SIZE: f32 = 13.;

pub struct Overlay {
    shapes: Option<AnyElement>,
    callouts: Vec<AnyElement>,
    picker: Option<Hitbox>,
}

impl Overlay {
    pub fn paint(&mut self, window: &mut Window, cx: &mut App) {
        if let Some(shapes) = &mut self.shapes {
            shapes.paint(window, cx);
        }
        for callout in &mut self.callouts {
            callout.paint(window, cx);
        }
        if let Some(hitbox) = &self.picker {
            window.set_cursor_style(CursorStyle::Crosshair, hitbox);
            window.on_mouse_event(|_: &MouseMoveEvent, phase, window, cx| {
                if phase == DispatchPhase::Capture {
                    window.refresh();
                    cx.stop_propagation();
                }
            });
            window.on_mouse_event(|event: &MouseDownEvent, phase, window, cx| {
                if phase != DispatchPhase::Capture {
                    return;
                }
                cx.stop_propagation();
                match event.button {
                    MouseButton::Left => {
                        picker::pick(window, event.position, event.modifiers.shift, cx)
                    }
                    _ => picker::stop(window),
                }
            });
            window.on_mouse_event(|_: &MouseUpEvent, phase, _, cx| {
                if phase == DispatchPhase::Capture {
                    cx.stop_propagation();
                }
            });
        }
    }
}

/// Resolves the highlights against the window's frame (completed only
/// when there are highlights or the picker runs) and lays out what to
/// paint.
pub fn prepaint(
    id: WindowId,
    reports: Vec<Report>,
    picks: Vec<PickText>,
    window: &mut Window,
    cx: &mut App,
) -> Option<Overlay> {
    let picking = picker::active(id);
    picker::set_text_picks(id, if picking { picks.clone() } else { Vec::new() });
    let highlights = highlight::of(id);
    let toast = picker::toast(id);
    let frame = if highlights.is_empty() && !picking {
        Arc::default()
    } else {
        super::frame(id)
    };
    let frame = &frame;
    let statuses: Vec<(HighlightId, Status)> = highlights
        .iter()
        .map(|h| (h.id, resolve(frame, &reports, h)))
        .collect();
    control::frame_done(id, &statuses, window, cx);
    if highlights.is_empty() && !picking && toast.is_none() {
        return None;
    }

    let viewport = window.viewport_size();
    let accent = theme::accent();
    let mut shapes: Vec<AnyElement> = Vec::new();
    let mut callouts: Vec<AnyElement> = Vec::new();
    let mut taken: Vec<Bounds<Pixels>> = Vec::new();
    let mut animating = false;
    // Shapes and badges first, so that no callout covers another target.
    let mut notes = Vec::new();
    for (h, (_, status)) in highlights.iter().zip(&statuses) {
        let Status::Shown {
            rects, approximate, ..
        } = status
        else {
            continue;
        };
        let color = h.color.unwrap_or(accent);
        let age = h.created.elapsed();
        let pulse = if h.pulse && age < PULSE {
            animating = true;
            (age.as_secs_f32() * 2. * PI / 0.9).sin().abs()
        } else {
            0.
        };
        let rects: Vec<_> = rects.iter().map(|r| clamp_rect(*r, viewport)).collect();
        for rect in &rects {
            shapes.push(shape(h.style, *rect, color, pulse));
        }
        taken.extend(rects.iter().copied());
        let Some(anchor) = rects.iter().copied().reduce(|a, b| a.union(&b)) else {
            continue;
        };
        // Characters sit among other characters: badges and notes go where
        // no text is (the gutter side, the free end of the row).
        let free = (!matches!(h.target, Target::Element { .. }) && !approximate)
            .then(|| free_space(frame, rects[0]))
            .flatten();
        if let Some(badge) = h.badge {
            let at = free
                .and_then(|(left, _)| text_badge_origin(rects[0], left, viewport))
                .unwrap_or_else(|| badge_origin(rects[0], viewport));
            taken.push(Bounds::new(at, size(px(BADGE), px(BADGE))));
            shapes.push(badge_element(badge, at, color));
        }
        if let Some(note) = &h.note {
            notes.push((
                h,
                note,
                color,
                anchor,
                free.map(|(_, right)| (rects[0], right)),
            ));
        }
    }
    for (h, note, color, anchor, row) in notes {
        let mut element = callout(h, note, color, window);
        let measured = element.layout_as_root(AvailableSpace::min_size(), window, cx);
        let origin = row
            .and_then(|(row, right)| text_callout_origin(row, right, measured, viewport, &taken))
            .unwrap_or_else(|| place_callout(anchor, measured, viewport, &taken));
        taken.push(Bounds::new(origin, measured));
        element.prepaint_at(origin, window, cx);
        callouts.push(element);
    }

    let mut picker_hitbox = None;
    if picking {
        let pointer = window.mouse_position();
        let text = picker::text_hit(&picks, pointer);
        let (outline, tip): (Vec<Bounds<Pixels>>, Option<(SharedString, SharedString)>) = match text
        {
            Some(text) => (
                text.rects.clone(),
                Some((text.reference.clone().into(), text.path.clone())),
            ),
            None => match picker::hit(&frame.nodes, pointer) {
                Some(index) => {
                    let node = &frame.nodes[index];
                    let detail = node
                        .label
                        .clone()
                        .or_else(|| node.text.clone())
                        .unwrap_or_default();
                    (
                        node.visible.into_iter().collect(),
                        Some((node.path.clone(), detail)),
                    )
                }
                None => (Vec::new(), None),
            },
        };
        for rect in &outline {
            shapes.push(shape(Style::Outline, *rect, accent, 0.));
        }
        let anchor = outline
            .iter()
            .copied()
            .reduce(|a, b| a.union(&b))
            .unwrap_or_else(|| Bounds::new(pointer, size(px(1.), px(1.))));
        let mut element = picker_tip(tip, window);
        let measured = element.layout_as_root(AvailableSpace::min_size(), window, cx);
        let origin = place_callout(anchor, measured, viewport, &[]);
        element.prepaint_at(origin, window, cx);
        callouts.push(element);
        picker_hitbox = Some(window.insert_hitbox(
            Bounds::new(Point::default(), viewport),
            HitboxBehavior::BlockMouseExceptScroll,
        ));
    }
    if let Some(toast) = toast {
        let mut element = toast_element(toast);
        let measured = element.layout_as_root(AvailableSpace::min_size(), window, cx);
        let origin = point(
            ((viewport.width - measured.width) / 2.).max(px(MARGIN)),
            viewport.height - measured.height - px(48.),
        );
        element.prepaint_at(origin, window, cx);
        callouts.push(element);
    }
    if animating {
        window.request_animation_frame();
    }

    let mut root = div()
        .absolute()
        .top_0()
        .left_0()
        .w(viewport.width)
        .h(viewport.height)
        .children(shapes)
        .into_any_element();
    root.prepaint_as_root(Point::default(), viewport.into(), window, cx);
    Some(Overlay {
        shapes: Some(root),
        callouts,
        picker: picker_hitbox,
    })
}

/// Where highlight `h` shows in `frame`, or why not.
pub fn resolve(frame: &Frame, reports: &[Report], h: &Highlight) -> Status {
    let reported: Vec<&Report> = reports.iter().filter(|r| r.id == h.id).collect();
    if !reported.is_empty() {
        let rects: Vec<_> = reported
            .iter()
            .flat_map(|r| r.rects.iter().copied())
            .collect();
        let clipped = reported.iter().any(|r| r.clipped);
        if rects.is_empty() {
            return Status::NotVisible {
                reason: "the range is scrolled out of view".into(),
                ancestor: h.target.path().and_then(|path| ancestor_of(frame, path)),
            };
        }
        return Status::Shown {
            rects,
            clipped,
            approximate: false,
        };
    }
    match &h.target {
        Target::Element { path } | Target::Text { path, .. } => {
            let Some(index) = frame.find(path) else {
                return not_found(frame, path);
            };
            let node = &frame.nodes[index];
            let Some(visible) = node.visible else {
                return Status::NotVisible {
                    reason: "drawn, but scrolled out of view or clipped".into(),
                    ancestor: frame
                        .visible_ancestor(index)
                        .map(|i| frame.nodes[i].path.clone()),
                };
            };
            let approximate = matches!(h.target, Target::Text { .. });
            if let Target::Text { select, .. } = &h.target
                && let Some(text) = &node.text
                && let Err(reason) = highlight::resolve_text(text, select)
            {
                return Status::NotFound {
                    reason,
                    ancestor: Some(node.path.clone()),
                    children: Vec::new(),
                    suggestions: Vec::new(),
                };
            }
            Status::Shown {
                rects: vec![visible],
                clipped: visible != node.bounds,
                approximate,
            }
        }
        Target::Code(code) => Status::NotVisible {
            reason: format!(
                "{} is not on screen in this window (open it, or pass \"reveal\": true)",
                code.file
            ),
            ancestor: None,
        },
        Target::Terminal(_) => Status::NotVisible {
            reason: "that terminal text is not on screen (open the terminal panel)".into(),
            ancestor: None,
        },
    }
}

fn ancestor_of(frame: &Frame, path: &str) -> Option<SharedString> {
    let index = frame.find(path)?;
    if frame.nodes[index].visible.is_some() {
        return Some(frame.nodes[index].path.clone());
    }
    frame
        .visible_ancestor(index)
        .map(|i| frame.nodes[i].path.clone())
}

/// An unknown path: the deepest existing prefix, its children, and close
/// matches.
pub fn not_found(frame: &Frame, path: &str) -> Status {
    let mut ancestor = None;
    let mut end = path.len();
    while let Some(slash) = path[..end].rfind('/') {
        if let Some(index) = frame.find(&path[..slash]) {
            ancestor = Some(index);
            break;
        }
        end = slash;
    }
    let children = match ancestor {
        Some(parent) => {
            let mut children: Vec<SharedString> = Vec::new();
            for node in frame.nodes.iter().filter(|n| n.parent == Some(parent)) {
                if !children.contains(&node.id) {
                    children.push(node.id.clone());
                }
            }
            children.truncate(40);
            children
        }
        None => Vec::new(),
    };
    Status::NotFound {
        reason: "no element has this path in the window's last frame".into(),
        ancestor: ancestor.map(|i| frame.nodes[i].path.clone()),
        children,
        suggestions: suggest(path, frame.nodes.iter().map(|n| n.path.as_ref()), 5)
            .into_iter()
            .map(SharedString::from)
            .collect(),
    }
}

/// The paths closest to `query`: those ending in the same segment first,
/// then those whose last key matches it (`tab:commit` for `commit`), then
/// any within half the query's length in edits; edit distance within each.
pub fn suggest<'a>(query: &str, paths: impl Iterator<Item = &'a str>, count: usize) -> Vec<String> {
    let query = query.to_lowercase();
    let last = query.rsplit('/').next().unwrap_or_default().to_owned();
    let limit = (query.chars().count() / 2).max(3);
    let mut scored: HashMap<&str, (u8, usize)> = HashMap::new();
    for path in paths {
        if scored.contains_key(path) {
            continue;
        }
        let lower = path.to_lowercase();
        let segment = lower.rsplit('/').next().unwrap_or_default();
        let bucket = if last.is_empty() {
            2
        } else if segment == last {
            0
        } else if segment.rsplit(':').next() == Some(last.as_str()) {
            1
        } else {
            2
        };
        // Too different in length to be within the limit.
        if bucket == 2 && lower.len().abs_diff(query.len()) > limit {
            continue;
        }
        let distance = levenshtein(&query, &lower);
        if bucket < 2 || distance <= limit {
            scored.insert(path, (bucket, distance));
        }
    }
    let mut scored: Vec<_> = scored.into_iter().collect();
    scored.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(b.0)));
    scored
        .into_iter()
        .take(count)
        .map(|(path, _)| path.to_owned())
        .collect()
}

fn levenshtein(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut previous = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let current = row[j + 1];
            row[j + 1] = (previous + usize::from(ca != *cb))
                .min(row[j] + 1)
                .min(current + 1);
            previous = current;
        }
    }
    row[b.len()]
}

fn clamp_rect(rect: Bounds<Pixels>, viewport: Size<Pixels>) -> Bounds<Pixels> {
    rect.intersect(&Bounds::new(Point::default(), viewport))
}

fn shape(style: Style, rect: Bounds<Pixels>, color: Hsla, pulse: f32) -> AnyElement {
    let at = |rect: Bounds<Pixels>| {
        div()
            .absolute()
            .left(rect.origin.x)
            .top(rect.origin.y)
            .w(rect.size.width)
            .h(rect.size.height)
    };
    match style {
        Style::Outline => {
            let pad = px(3.);
            let rect = Bounds::new(
                rect.origin - point(pad, pad),
                rect.size + size(pad * 2., pad * 2.),
            );
            at(rect)
                .border_2()
                .border_color(color)
                .rounded(px(5.))
                .shadow(vec![
                    BoxShadow {
                        color: color.opacity(0.55),
                        offset: Point::default(),
                        blur_radius: px(10.),
                        spread_radius: px(1. + 5. * pulse),
                    },
                    BoxShadow {
                        color: color.opacity(0.10 + 0.15 * pulse),
                        offset: Point::default(),
                        blur_radius: px(2.),
                        spread_radius: px(0.),
                    },
                ])
                .into_any_element()
        }
        Style::Fill => at(rect)
            // Painted over the text: light enough to read it through.
            .bg(color.opacity(0.16 + 0.12 * pulse))
            .border_1()
            .border_color(color.opacity(0.6))
            .rounded(px(2.))
            .into_any_element(),
        Style::Underline => {
            let height = px(2.5);
            at(Bounds::new(
                point(rect.origin.x, rect.origin.y + rect.size.height - height),
                size(rect.size.width, height),
            ))
            .bg(color)
            .shadow(vec![BoxShadow {
                color: color.opacity(0.5 * pulse),
                offset: Point::default(),
                blur_radius: px(6.),
                spread_radius: px(2. * pulse),
            }])
            .into_any_element()
        }
    }
}

const BADGE: f32 = 22.;

/// The badge sits on the target's top-left corner, or left of targets too
/// small for it to leave visible (checkboxes, icons); inside the window.
pub fn badge_origin(target: Bounds<Pixels>, viewport: Size<Pixels>) -> Point<Pixels> {
    let half = px(BADGE / 2.);
    let small = target.size.width < px(BADGE * 2.) || target.size.height < px(BADGE);
    let beside = target.origin.x - px(BADGE + 6.);
    let (x, y) = if small && beside >= px(2.) {
        (beside, target.center().y - half)
    } else {
        (target.origin.x - half, target.origin.y - half)
    };
    point(
        x.min(viewport.width - px(BADGE + 2.)).max(px(2.)),
        y.min(viewport.height - px(BADGE + 2.)).max(px(2.)),
    )
}

/// Around characters on `row` (window coordinates): the left edge of the
/// element showing them (a badge goes left of it, on the gutter side) and
/// the right end of the free space after them (up to the next thing drawn
/// beside that element on the row, like a minimap; things drawn inside it,
/// like inlay hints, are part of its content).
fn free_space(frame: &Frame, row: Bounds<Pixels>) -> Option<(Pixels, Pixels)> {
    let container = picker::hit(&frame.nodes, row.center())?;
    let node = &frame.nodes[container];
    let visible = node.visible?;
    let on_row = |b: &Bounds<Pixels>| b.top() < row.bottom() && b.bottom() > row.top();
    let right = frame
        .nodes
        .iter()
        .enumerate()
        .filter(|(i, n)| *i != container && n.parent == node.parent)
        .filter_map(|(_, n)| n.visible)
        .filter(|b| b.left() < visible.left() || b.right() > visible.right())
        .filter(|b| on_row(b) && b.left() >= row.right())
        .map(|b| b.left())
        .fold(visible.right(), Pixels::min);
    Some((visible.left(), right))
}

/// A badge for characters on `row`: just left of `left` (the edge of the
/// element holding them), centered on the row; `None` without room there.
pub fn text_badge_origin(
    row: Bounds<Pixels>,
    left: Pixels,
    viewport: Size<Pixels>,
) -> Option<Point<Pixels>> {
    let x = left - px(BADGE + 3.);
    (x >= px(2.)).then(|| {
        point(
            x,
            (row.center().y - px(BADGE / 2.))
                .min(viewport.height - px(BADGE + 2.))
                .max(px(2.)),
        )
    })
}

/// A note for characters on `row`: on the same row, at the right end of the
/// free space (`right`), where lines of code rarely reach; `None` when it
/// does not fit after the characters or covers something taken.
pub fn text_callout_origin(
    row: Bounds<Pixels>,
    right: Pixels,
    size: Size<Pixels>,
    viewport: Size<Pixels>,
    taken: &[Bounds<Pixels>],
) -> Option<Point<Pixels>> {
    let margin = px(MARGIN);
    let x = right.min(viewport.width) - margin - size.width;
    if x < row.right() + px(16.) {
        return None;
    }
    let y = (row.center().y - size.height / 2.)
        .min(viewport.height - margin - size.height)
        .max(margin);
    let origin = point(x, y);
    let bounds = Bounds::new(origin, size);
    (!taken.iter().any(|t| t.intersects(&bounds))).then_some(origin)
}

fn badge_element(number: u32, at: Point<Pixels>, color: Hsla) -> AnyElement {
    div()
        .absolute()
        .left(at.x)
        .top(at.y)
        .size(px(BADGE))
        .rounded_full()
        .bg(color)
        .border_2()
        .border_color(theme::background())
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(12.))
        .font_weight(FontWeight::BOLD)
        .text_color(theme::on_accent())
        .shadow(vec![BoxShadow {
            color: gpui::black().opacity(0.35),
            offset: point(px(0.), px(1.)),
            blur_radius: px(4.),
            spread_radius: px(0.),
        }])
        .child(number.to_string())
        .into_any_element()
}

/// Where a callout of `size` goes next to `target`: below, above, right,
/// then left of it, the first that fits in the window without covering
/// `taken`; else below, moved inside the window.
pub fn place_callout(
    target: Bounds<Pixels>,
    size: Size<Pixels>,
    viewport: Size<Pixels>,
    taken: &[Bounds<Pixels>],
) -> Point<Pixels> {
    let gap = px(10.);
    let margin = px(MARGIN);
    let candidates = [
        point(target.origin.x, target.bottom() + gap),
        point(target.origin.x, target.origin.y - gap - size.height),
        point(target.right() + gap, target.origin.y),
        point(target.origin.x - gap - size.width, target.origin.y),
    ];
    // Moved inside the window, a candidate must still leave the target
    // uncovered.
    let clamp = |origin: Point<Pixels>| {
        point(
            origin
                .x
                .min(viewport.width - margin - size.width)
                .max(margin),
            origin
                .y
                .min(viewport.height - margin - size.height)
                .max(margin),
        )
    };
    let usable = |origin: Point<Pixels>| !Bounds::new(origin, size).intersects(&target);
    let clear = |origin: Point<Pixels>| {
        let bounds = Bounds::new(origin, size);
        !taken.iter().any(|t| t.intersects(&bounds))
    };
    let fitting: Vec<_> = candidates
        .iter()
        .copied()
        .map(clamp)
        .filter(|origin| usable(*origin))
        .collect();
    fitting
        .iter()
        .copied()
        .find(|origin| clear(*origin))
        .or(fitting.first().copied())
        .unwrap_or_else(|| clamp(candidates[0]))
}

fn note_width(text: &str, window: &Window) -> Pixels {
    let font = window.text_style().font();
    let longest = text
        .lines()
        .map(|line| {
            let run = TextRun {
                len: line.len(),
                font: font.clone(),
                color: theme::text(),
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            window
                .text_system()
                .shape_line(line.to_owned().into(), px(NOTE_SIZE), &[run], None)
                .width
        })
        .fold(px(0.), Pixels::max);
    (longest + px(2.)).min(px(CALLOUT_MAX_WIDTH))
}

fn callout(h: &Highlight, note: &SharedString, color: Hsla, window: &Window) -> AnyElement {
    let id = h.id;
    let window_id = h.window;
    let width = note_width(note, window);
    div()
        .id(("pinpoint-callout", id as usize))
        .occlude()
        .flex()
        .items_start()
        .gap(px(8.))
        .px(px(10.))
        .py(px(7.))
        .rounded(px(7.))
        .bg(theme::panel())
        .border_1()
        .border_color(color)
        .shadow(vec![BoxShadow {
            color: gpui::black().opacity(0.35),
            offset: point(px(0.), px(3.)),
            blur_radius: px(12.),
            spread_radius: px(0.),
        }])
        .text_size(px(NOTE_SIZE))
        .text_color(theme::text())
        .when_some(h.badge, |this, badge| {
            this.child(
                div()
                    .flex_none()
                    .size(px(18.))
                    .rounded_full()
                    .bg(color)
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(11.))
                    .font_weight(FontWeight::BOLD)
                    .text_color(theme::on_accent())
                    .child(badge.to_string()),
            )
        })
        .child(div().w(width).child(note.clone()))
        .child(
            div()
                .id(("pinpoint-close", id as usize))
                .flex_none()
                .px(px(3.))
                .rounded(px(3.))
                .text_color(theme::muted())
                .cursor_pointer()
                .hover(|this| this.bg(theme::hover_row()).text_color(theme::text()))
                .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                    cx.stop_propagation();
                    highlight::clear(Some(window_id), Some(&[id]));
                    window.refresh();
                })
                .child("×"),
        )
        .into_any_element()
}

fn picker_tip(tip: Option<(SharedString, SharedString)>, window: &Window) -> AnyElement {
    let hint: SharedString = "Click to pin · Shift+click pins and keeps picking · Esc exits".into();
    let (title, detail) =
        tip.unwrap_or_else(|| ("Pick an element".into(), SharedString::default()));
    let width = note_width(&title, window)
        .max(note_width(&hint, window))
        .max(note_width(&detail, window));
    div()
        .flex()
        .flex_col()
        .gap(px(2.))
        .px(px(10.))
        .py(px(6.))
        .rounded(px(6.))
        .bg(theme::panel())
        .border_1()
        .border_color(theme::accent())
        .text_size(px(NOTE_SIZE))
        .text_color(theme::text())
        .child(div().w(width).text_color(theme::accent_text()).child(title))
        .when(!detail.is_empty(), |this| {
            this.child(div().w(width).child(detail))
        })
        .child(
            div()
                .w(width)
                .text_size(px(11.))
                .text_color(theme::muted())
                .child(hint),
        )
        .into_any_element()
}

fn toast_element(text: SharedString) -> AnyElement {
    div()
        .px(px(14.))
        .py(px(8.))
        .rounded(px(8.))
        .bg(theme::panel())
        .border_1()
        .border_color(theme::accent())
        .text_size(px(NOTE_SIZE))
        .text_color(theme::text())
        .shadow(vec![BoxShadow {
            color: gpui::black().opacity(0.35),
            offset: point(px(0.), px(3.)),
            blur_radius: px(12.),
            spread_radius: px(0.),
        }])
        .child(text)
        .into_any_element()
}

use gpui::prelude::FluentBuilder as _;

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f32, y: f32, w: f32, h: f32) -> Bounds<Pixels> {
        Bounds::new(point(px(x), px(y)), size(px(w), px(h)))
    }

    #[test]
    fn callouts_stay_in_the_window() {
        let viewport = size(px(800.), px(600.));
        let note = size(px(200.), px(40.));
        // Room below: below, left-aligned.
        assert_eq!(
            place_callout(rect(100., 100., 50., 20.), note, viewport, &[]),
            point(px(100.), px(130.))
        );
        // At the bottom: above.
        assert_eq!(
            place_callout(rect(100., 560., 50., 20.), note, viewport, &[]),
            point(px(100.), px(510.))
        );
        // At the right edge: moved left to fit.
        assert_eq!(
            place_callout(rect(750., 100., 40., 20.), note, viewport, &[]),
            point(px(592.), px(130.))
        );
        // A target taller than the window: inside anyway.
        let origin = place_callout(rect(-50., -50., 900., 700.), note, viewport, &[]);
        assert!(origin.x >= px(8.) && origin.x + note.width <= px(792.));
        assert!(origin.y >= px(8.) && origin.y + note.height <= px(592.));
        // Below is taken by another callout: above instead.
        assert_eq!(
            place_callout(
                rect(100., 100., 50., 20.),
                note,
                viewport,
                &[rect(90., 125., 300., 60.)]
            ),
            point(px(100.), px(50.))
        );
    }

    #[test]
    fn badges_stay_in_the_window() {
        let viewport = size(px(800.), px(600.));
        // On the corner of targets large enough to stay visible.
        assert_eq!(
            badge_origin(rect(100., 100., 80., 30.), viewport),
            point(px(89.), px(89.))
        );
        // Beside small ones (a checkbox), centered.
        assert_eq!(
            badge_origin(rect(100., 100., 10., 10.), viewport),
            point(px(72.), px(94.))
        );
        // No room beside: the corner, inside the window.
        assert_eq!(
            badge_origin(rect(0., 0., 10., 10.), viewport),
            point(px(2.), px(2.))
        );
        assert_eq!(
            badge_origin(rect(799., 599., 10., 10.), viewport),
            point(px(771.), px(576.))
        );
    }

    #[test]
    fn code_notes_and_badges_avoid_the_code() {
        let node = |path: &str, parent: Option<usize>, b: Bounds<Pixels>| crate::pinpoint::Node {
            path: path.to_owned().into(),
            id: path.rsplit('/').next().unwrap().to_owned().into(),
            parent,
            depth: 0,
            label: None,
            text: None,
            bounds: b,
            visible: Some(b),
            interactive: true,
        };
        // An editor: gutter, code, minimap on the right.
        let frame = Frame {
            nodes: vec![
                node("editor:a.py", None, rect(300., 100., 1000., 600.)),
                node("editor:a.py/gutter", Some(0), rect(300., 100., 70., 600.)),
                node("editor:a.py/text", Some(0), rect(370., 100., 830., 600.)),
                node(
                    "editor:a.py/minimap",
                    Some(0),
                    rect(1200., 100., 100., 600.),
                ),
                // An inlay hint after the characters: part of the code.
                node("editor:a.py/inlay:1:1", Some(0), rect(470., 100., 60., 20.)),
            ],
        };
        let viewport = size(px(1440.), px(900.));
        // `area` on line 1.
        let row = rect(410., 100., 32., 20.);
        let (left, right) = free_space(&frame, row).unwrap();
        assert_eq!((left, right), (px(370.), px(1200.)));
        // The badge sits in the gutter, on the row: not on the code.
        let badge = text_badge_origin(row, left, viewport).unwrap();
        assert_eq!(badge, point(px(345.), px(99.)));
        assert!(badge.x + px(BADGE) <= px(370.));
        // The note on the same row, at the free end before the minimap,
        // not below (over line 2).
        let note = size(px(200.), px(34.));
        let origin = text_callout_origin(row, right, note, viewport, &[row]).unwrap();
        assert_eq!(origin, point(px(992.), px(93.)));
        // No room after the characters, or something there: the usual
        // placement takes over.
        assert_eq!(
            text_callout_origin(rect(370., 100., 700., 20.), right, note, viewport, &[]),
            None
        );
        assert_eq!(
            text_callout_origin(row, right, note, viewport, &[rect(1000., 90., 50., 40.)]),
            None
        );
        assert_eq!(text_badge_origin(row, px(10.), viewport), None);
    }

    #[test]
    fn suggestions() {
        let paths = [
            "workspace/left-dock/panel:commit/commit",
            "workspace/left-dock/panel:commit/amend",
            "workspace/left-dock/dock-tabs/tab:commit",
            "workspace/title-bar/run",
        ];
        let found = suggest("workspace/left-dock/commit", paths.into_iter(), 3);
        assert_eq!(found[0], "workspace/left-dock/panel:commit/commit");
        assert!(suggest("zzzz", paths.into_iter(), 3).is_empty());
        assert_eq!(levenshtein("kitten", "sitting"), 3);
    }
}
