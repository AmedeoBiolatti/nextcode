//! The human picker ("Pinpoint: Pick Element"): the pointer outlines the
//! element (or the characters, where a text owner reports them) under it; a
//! click copies a reference and adds it to the pins agents read.

use std::{
    cell::RefCell,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use gpui::{
    App, Bounds, ClipboardItem, KeyBinding, Pixels, Point, SharedString, Window, WindowId, actions,
};
use serde_json::{Value, json};

use super::{Frame, Node, PickText, highlight, target::UI_SCHEME};

actions!(
    pinpoint,
    [
        /// Outlines the element under the pointer; a click pins it.
        PickElement,
        /// Removes every highlight of the window.
        ClearHighlights
    ]
);

/// "P" for pinpoint; free in every context.
pub const PICK_KEY: &str = "secondary-alt-shift-p";

/// Pins kept for agents (oldest dropped).
const MAX_PINS: usize = 200;

/// What a person picked.
#[derive(Clone, Debug)]
pub struct Picked {
    pub seq: u64,
    pub window: WindowId,
    /// `element`, `text` or `code`.
    pub kind: &'static str,
    pub path: SharedString,
    pub label: Option<SharedString>,
    pub text: Option<SharedString>,
    pub reference: String,
    pub bounds: Option<Bounds<Pixels>>,
    /// Unix milliseconds.
    pub time: u64,
}

impl Picked {
    pub fn to_json(&self) -> Value {
        let mut value = json!({
            "seq": self.seq,
            "window": self.window.as_u64(),
            "kind": self.kind,
            "path": self.path.as_ref(),
            "reference": self.reference,
            "time": self.time,
        });
        if let Some(label) = &self.label {
            value["label"] = label.as_ref().into();
        }
        if let Some(text) = &self.text {
            value["text"] = text.as_ref().into();
        }
        // What to send back to highlight exactly this.
        let target = match self.kind {
            "element" => None,
            _ => super::target::parse_reference(&self.reference).ok(),
        };
        if let Some(target) = target {
            value["target"] = target.to_json();
        }
        if let Some(bounds) = self.bounds {
            value["bounds"] = super::control::bounds_json(bounds);
        }
        value
    }
}

type Reply = Box<dyn FnOnce(Value) + Send>;

struct Waiter {
    id: u64,
    /// Answered by the first pin after this sequence number.
    since: u64,
    reply: Reply,
}

#[derive(Default)]
struct State {
    /// Windows in pick mode.
    active: Vec<WindowId>,
    pins: Vec<Picked>,
    next_seq: u64,
    waiters: Vec<Waiter>,
    next_waiter: u64,
    /// The characters text owners reported under the pointer last frame.
    text_picks: Vec<(WindowId, Vec<PickText>)>,
    /// A short notice after a pick.
    toasts: Vec<(WindowId, SharedString, Instant)>,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State {
        next_seq: 1,
        next_waiter: 1,
        ..State::default()
    });
}

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([KeyBinding::new(PICK_KEY, PickElement, None)]);
}

pub fn active(window: WindowId) -> bool {
    STATE.with_borrow(|state| state.active.contains(&window))
}

pub fn start(window: &mut Window) {
    let id = window.window_handle().window_id();
    STATE.with_borrow_mut(|state| {
        if !state.active.contains(&id) {
            state.active.push(id);
        }
    });
    window.refresh();
}

pub fn stop(window: &mut Window) {
    let id = window.window_handle().window_id();
    STATE.with_borrow_mut(|state| state.active.retain(|w| *w != id));
    window.refresh();
}

pub fn forget(window: WindowId) {
    STATE.with_borrow_mut(|state| {
        state.active.retain(|w| *w != window);
        state.text_picks.retain(|(w, _)| *w != window);
        state.toasts.retain(|(w, _, _)| *w != window);
    });
}

/// Keeps last frame's text picks for the next click.
pub fn set_text_picks(window: WindowId, picks: Vec<PickText>) {
    STATE.with_borrow_mut(|state| {
        state.text_picks.retain(|(w, _)| *w != window);
        if !picks.is_empty() {
            state.text_picks.push((window, picks));
        }
    });
}

/// The innermost node under `point`: the last drawn whose visible part
/// holds it (children are drawn after their parents, popovers last).
pub fn hit(nodes: &[Node], point: Point<Pixels>) -> Option<usize> {
    nodes
        .iter()
        .rposition(|node| node.visible.is_some_and(|visible| visible.contains(&point)))
}

/// The characters under `point` that owners reported, last reported first.
pub fn text_hit(picks: &[PickText], point: Point<Pixels>) -> Option<&PickText> {
    picks
        .iter()
        .rev()
        .find(|pick| pick.rects.iter().any(|rect| rect.contains(&point)))
}

/// Picks what is under `point` in `window`. `keep`: stay in pick mode.
pub fn pick(window: &mut Window, point: Point<Pixels>, keep: bool, cx: &mut App) {
    let id = window.window_handle().window_id();
    let frame = super::frame(id);
    let text = STATE.with_borrow(|state| {
        let picks = &state.text_picks.iter().find(|(w, _)| *w == id)?.1;
        text_hit(picks, point).cloned()
    });
    let picked = match text {
        Some(text) => picked_text(&frame, text),
        None => match hit(&frame.nodes, point) {
            Some(index) => picked_element(&frame.nodes[index]),
            None => return,
        },
    };
    cx.write_to_clipboard(ClipboardItem::new_string(picked.reference.clone()));
    let notice: SharedString = format!("Pinned and copied: {}", picked.reference).into();
    let waiters = STATE.with_borrow_mut(|state| {
        let mut picked = picked;
        picked.seq = state.next_seq;
        picked.window = id;
        state.next_seq += 1;
        if !keep {
            state.active.retain(|w| *w != id);
        }
        state.toasts.retain(|(w, _, _)| *w != id);
        state.toasts.push((id, notice, Instant::now() + TOAST_TIME));
        let value = picked.to_json();
        let seq = picked.seq;
        state.pins.push(picked);
        if state.pins.len() > MAX_PINS {
            state.pins.remove(0);
        }
        let (ready, waiting) = std::mem::take(&mut state.waiters)
            .into_iter()
            .partition::<Vec<_>, _>(|waiter| waiter.since < seq);
        state.waiters = waiting;
        (ready, value)
    });
    let (ready, value) = waiters;
    for waiter in ready {
        (waiter.reply)(json!({ "ok": true, "pin": value.clone() }));
    }
    // Repaint once the notice is over.
    let handle = window.window_handle();
    cx.spawn(async move |cx| {
        cx.background_executor().timer(TOAST_TIME).await;
        handle.update(cx, |_, window, _| window.refresh()).ok();
    })
    .detach();
    window.refresh();
}

const TOAST_TIME: Duration = Duration::from_millis(2800);

/// The notice to show in `window`, if any.
pub fn toast(window: WindowId) -> Option<SharedString> {
    let now = Instant::now();
    STATE.with_borrow_mut(|state| {
        state.toasts.retain(|(_, _, until)| *until > now);
        state
            .toasts
            .iter()
            .find(|(w, _, _)| *w == window)
            .map(|(_, text, _)| text.clone())
    })
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

pub fn picked_element(node: &Node) -> Picked {
    Picked {
        seq: 0,
        window: WindowId::from(0u64),
        kind: "element",
        path: node.path.clone(),
        label: node.label.clone(),
        text: node.text.clone(),
        reference: format!("{UI_SCHEME}{}", node.path),
        bounds: Some(node.bounds),
        time: now_ms(),
    }
}

fn picked_text(frame: &Frame, pick: PickText) -> Picked {
    let node = frame.find(&pick.path).map(|index| &frame.nodes[index]);
    let code = !pick.reference.starts_with(UI_SCHEME);
    let bounds = pick.rects.iter().copied().reduce(|a, b| a.union(&b));
    Picked {
        seq: 0,
        window: WindowId::from(0u64),
        kind: if code { "code" } else { "text" },
        path: pick.path,
        label: node.and_then(|node| node.label.clone()),
        text: None,
        reference: pick.reference,
        bounds,
        time: now_ms(),
    }
}

/// The pins (oldest first); `clear` empties the list.
pub fn pins(clear: bool) -> Vec<Value> {
    STATE.with_borrow_mut(|state| {
        let pins = state.pins.iter().map(Picked::to_json).collect();
        if clear {
            state.pins.clear();
        }
        pins
    })
}

/// The newest pin's sequence number (0: none yet).
pub fn last_seq() -> u64 {
    STATE.with_borrow(|state| state.next_seq - 1)
}

/// Answers `reply` with the first pin after `since` (now, when it exists
/// already); returns the waiter's id for [`expire`].
pub fn wait(since: u64, reply: Reply) -> Option<u64> {
    STATE.with_borrow_mut(|state| {
        if let Some(pin) = state.pins.iter().find(|pin| pin.seq > since) {
            reply(json!({ "ok": true, "pin": pin.to_json() }));
            return None;
        }
        let id = state.next_waiter;
        state.next_waiter += 1;
        state.waiters.push(Waiter { id, since, reply });
        Some(id)
    })
}

/// Times out waiter `id` if it is still waiting.
pub fn expire(id: u64, timeout: Duration) {
    let waiter = STATE.with_borrow_mut(|state| {
        let index = state.waiters.iter().position(|w| w.id == id)?;
        Some(state.waiters.remove(index))
    });
    if let Some(waiter) = waiter {
        (waiter.reply)(json!({
            "ok": false,
            "error": format!("no pick within {}s", timeout.as_secs_f32()),
            "hint": "ask the user to press the Pick Element shortcut (or send {\"cmd\":\"pick\"}) and click the element, then wait again",
            "timeout": true,
        }));
    }
}

/// Escape leaves pick mode (before anything else sees it); otherwise, when
/// no binding used it (no popup to close), it dismisses the highlights.
pub fn watch_keys(cx: &mut App) {
    cx.intercept_keystrokes(|event, window, cx| {
        if is_escape(&event.keystroke) && active(window.window_handle().window_id()) {
            stop(window);
            cx.stop_propagation();
        }
    })
    .detach();
    cx.observe_keystrokes(|event, window, _| {
        let id = window.window_handle().window_id();
        if event.action.is_none()
            && is_escape(&event.keystroke)
            && !active(id)
            && !highlight::clear(Some(id), None).is_empty()
        {
            window.refresh();
        }
    })
    .detach();
}

fn is_escape(keystroke: &gpui::Keystroke) -> bool {
    keystroke.key == "escape" && !keystroke.modifiers.modified()
}

#[cfg(test)]
mod tests {
    use gpui::{point, px, size};

    use super::*;

    fn node(path: &str, x: f32, y: f32, w: f32, h: f32, visible: bool) -> Node {
        let bounds = Bounds::new(point(px(x), px(y)), size(px(w), px(h)));
        Node {
            path: path.to_owned().into(),
            id: path.rsplit('/').next().unwrap().to_owned().into(),
            parent: None,
            depth: 0,
            label: None,
            text: None,
            bounds,
            visible: visible.then_some(bounds),
            interactive: true,
        }
    }

    #[test]
    fn innermost_hit() {
        let nodes = vec![
            node("workspace", 0., 0., 800., 600., true),
            node("workspace/left-dock", 0., 0., 200., 600., true),
            node(
                "workspace/left-dock/panel:files/row:src/a.rs",
                0.,
                40.,
                200.,
                20.,
                true,
            ),
            // Scrolled away: never hit.
            node(
                "workspace/left-dock/panel:files/row:src/z.rs",
                0.,
                60.,
                200.,
                20.,
                false,
            ),
            node("workspace/editor-area", 200., 0., 600., 600., true),
            // A popover drawn last over the dock.
            node(
                "workspace/editor-area/completion",
                150.,
                50.,
                120.,
                80.,
                true,
            ),
        ];
        let at = |x: f32, y: f32| hit(&nodes, point(px(x), px(y))).map(|i| nodes[i].path.as_ref());
        assert_eq!(
            at(10., 45.),
            Some("workspace/left-dock/panel:files/row:src/a.rs")
        );
        assert_eq!(at(10., 65.), Some("workspace/left-dock"));
        assert_eq!(at(160., 55.), Some("workspace/editor-area/completion"));
        assert_eq!(at(400., 300.), Some("workspace/editor-area"));
        assert_eq!(at(900., 300.), None);
    }

    #[test]
    fn text_hits_prefer_last_reported() {
        let rect = |x: f32| Bounds::new(point(px(x), px(0.)), size(px(10.), px(10.)));
        let picks = vec![
            PickText {
                reference: "a.rs:1:1-1:1".into(),
                path: "editor:a.rs/text".into(),
                rects: vec![rect(0.)],
            },
            PickText {
                reference: "b.rs:1:1-1:1".into(),
                path: "completion".into(),
                rects: vec![rect(0.), rect(20.)],
            },
        ];
        let at = |x: f32| text_hit(&picks, point(px(x), px(5.))).map(|p| p.reference.as_str());
        assert_eq!(at(5.), Some("b.rs:1:1-1:1"));
        assert_eq!(at(25.), Some("b.rs:1:1-1:1"));
        assert_eq!(at(15.), None);
    }
}
