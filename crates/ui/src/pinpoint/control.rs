//! Agent requests: JSON in, JSON out, from any thread, served on the UI
//! thread. Every transport (the control server and CLI, `--automation`,
//! remote agents) ends in [`dispatch`]. Commands are documented in
//! `docs/pinpoint-for-agents.md`.

use std::{
    cell::RefCell,
    collections::HashSet,
    sync::OnceLock,
    time::{Duration, Instant},
};

use futures::{StreamExt, channel::mpsc};
use gpui::{AnyWindowHandle, App, Bounds, Hsla, Pixels, Rgba, SharedString, Window, WindowId};
use serde_json::{Value, json};

use super::{
    Frame, HighlightId, frame,
    highlight::{self, Highlight, Style},
    overlay::Status,
    picker,
    target::{Target, parse_reference},
};
use crate::{project::Project, shell::Shell, theme};

type Reply = Box<dyn FnOnce(Value) + Send>;

static REQUESTS: OnceLock<mpsc::UnboundedSender<(Value, Reply)>> = OnceLock::new();

pub const COMMANDS: &[&str] = &[
    "list",
    "find",
    "highlight",
    "clear",
    "pins",
    "wait_pin",
    "pick",
    "windows",
    "context",
];

/// Runs `request` on the UI thread and answers through `reply` (from that
/// thread, or later for `wait_pin`). Callable from any thread.
pub fn dispatch(request: Value, reply: impl FnOnce(Value) + Send + 'static) {
    match REQUESTS.get() {
        Some(requests) => {
            if let Err(error) = requests.unbounded_send((request, Box::new(reply))) {
                let (_, reply) = error.into_inner();
                reply(error_reply("next_code is shutting down", Value::Null));
            }
        }
        None => reply(error_reply(
            "pinpoint is not running in this process",
            Value::Null,
        )),
    }
}

/// Starts serving requests (once, from the app's startup).
pub fn init(cx: &mut App) {
    let (sender, mut receiver) = mpsc::unbounded::<(Value, Reply)>();
    if REQUESTS.set(sender).is_err() {
        return;
    }
    cx.spawn(async move |cx| {
        while let Some((request, reply)) = receiver.next().await {
            cx.update(|cx| handle(request, reply, cx)).ok();
        }
    })
    .detach();
}

pub fn error_reply(error: impl Into<String>, extra: Value) -> Value {
    let mut reply = json!({ "ok": false, "error": error.into() });
    if let (Some(reply), Value::Object(extra)) = (reply.as_object_mut(), extra) {
        reply.extend(extra);
    }
    reply
}

pub fn bounds_json(bounds: Bounds<Pixels>) -> Value {
    let round = |p: Pixels| (f32::from(p) * 10.).round() / 10.;
    json!([
        round(bounds.origin.x),
        round(bounds.origin.y),
        round(bounds.size.width),
        round(bounds.size.height)
    ])
}

fn handle(request: Value, reply: Reply, cx: &mut App) {
    // Echo the caller's request id, for clients that pipeline.
    let id = request.get("id").cloned();
    let reply: Reply = match id {
        Some(id) => Box::new(move |mut value: Value| {
            if let Some(object) = value.as_object_mut() {
                object.insert("id".into(), id);
            }
            reply(value)
        }),
        None => reply,
    };
    let Some(cmd) = request.get("cmd").and_then(Value::as_str) else {
        return reply(error_reply(
            "the request needs \"cmd\"",
            json!({ "commands": COMMANDS }),
        ));
    };
    let cmd = cmd.replace('-', "_");
    match cmd.as_str() {
        "windows" => reply(json!({ "ok": true, "windows": windows_json(cx) })),
        "pins" => {
            let clear = request.get("clear").and_then(Value::as_bool) == Some(true);
            let mut pins = picker::pins(clear);
            add_absolute_files(&mut pins, cx);
            reply(json!({ "ok": true, "pins": pins, "last_seq": picker::last_seq() }))
        }
        "wait_pin" => wait_pin(&request, reply, cx),
        "list" | "find" | "highlight" | "clear" | "pick" | "context" => {
            let handle = match choose_window(&request, cx) {
                Ok(handle) => handle,
                Err(error) => return reply(error),
            };
            let result = handle.update(cx, |_, window, cx| match cmd.as_str() {
                "list" => reply(list(&request, window)),
                "find" => reply(find(&request, window)),
                "highlight" => highlight_cmd(&request, reply, window, cx),
                "clear" => reply(clear(&request, window, cx)),
                "context" => reply(super::context::context(&request, window, cx)),
                _ => {
                    picker::start(window);
                    reply(json!({
                        "ok": true,
                        "window": window.window_handle().window_id().as_u64(),
                        "hint": "pick mode is on: the user's next click pins an element; read it with wait_pin",
                    }))
                }
            });
            // The window closed between choosing and updating it: the
            // reply was dropped with the closure.
            result.ok();
        }
        other => reply(error_reply(
            format!("unknown command \"{other}\""),
            json!({ "commands": COMMANDS }),
        )),
    }
}

struct WindowInfo {
    handle: AnyWindowHandle,
    title: String,
    active: bool,
    size: (f32, f32),
    projects: Vec<Project>,
}

fn window_infos(cx: &mut App) -> Vec<WindowInfo> {
    let active = cx.active_window().map(|handle| handle.window_id());
    cx.windows()
        .into_iter()
        .filter_map(|handle| {
            handle
                .update(cx, |_, window, cx| {
                    let projects = window
                        .root::<Shell>()
                        .flatten()
                        .map(|shell| {
                            shell
                                .read(cx)
                                .projects(cx)
                                .into_iter()
                                .map(|(_, project)| project)
                                .collect()
                        })
                        .unwrap_or_default();
                    let size = window.viewport_size();
                    WindowInfo {
                        handle,
                        title: window.window_title(),
                        active: active == Some(handle.window_id()),
                        size: (size.width.into(), size.height.into()),
                        projects,
                    }
                })
                .ok()
        })
        .collect()
}

fn roots_of(info: &WindowInfo) -> Vec<String> {
    info.projects
        .iter()
        .flat_map(|project| project.roots().iter())
        .map(|root| root.path.to_string_lossy().into_owned())
        .collect()
}

fn windows_json(cx: &mut App) -> Vec<Value> {
    window_infos(cx)
        .iter()
        .map(|info| {
            json!({
                "window": info.handle.window_id().as_u64(),
                "title": info.title,
                "active": info.active,
                "size": [info.size.0, info.size.1],
                "roots": roots_of(info),
            })
        })
        .collect()
}

/// The project roots of every window (instance files list them).
pub fn all_roots(cx: &mut App) -> Vec<String> {
    let mut roots: Vec<String> = window_infos(cx).iter().flat_map(roots_of).collect();
    roots.dedup();
    roots
}

/// A path for comparisons: `/`-separated, without a trailing `/`, a WSL
/// UNC prefix dropped (`\\wsl.localhost\Ubuntu\home` → `/home`), and
/// lowercased when it is a Windows path.
pub fn normalize_path(path: &str) -> String {
    let mut path = path.replace('\\', "/");
    for prefix in ["//wsl.localhost/", "//wsl$/"] {
        if path.len() > prefix.len() && path[..prefix.len()].eq_ignore_ascii_case(prefix) {
            let rest = &path[prefix.len()..];
            path = match rest.find('/') {
                Some(slash) => rest[slash..].to_owned(),
                None => "/".to_owned(),
            };
            break;
        }
    }
    let windows = path.as_bytes().get(1) == Some(&b':') || path.starts_with("//");
    if windows {
        path = path.to_lowercase();
    }
    while path.len() > 1 && path.ends_with('/') {
        path.pop();
    }
    path
}

/// Whether `path` is `root` or inside it.
pub fn contains(root: &str, path: &str) -> bool {
    let (root, path) = (normalize_path(root), normalize_path(path));
    path == root
        || path
            .strip_prefix(&root)
            .is_some_and(|rest| rest.starts_with('/') || root.ends_with('/'))
}

/// Picks the window a request is about: `window` (an id), else a window
/// with one of `project_roots`, else the one whose root holds `cwd`
/// (deepest root), else the active window, else the first. `windows`:
/// (id, roots, active).
pub fn choose(windows: &[(u64, Vec<String>, bool)], request: &Value) -> Result<usize, Value> {
    if windows.is_empty() {
        return Err(error_reply("next_code has no window open", Value::Null));
    }
    if let Some(id) = request.get("window").filter(|v| !v.is_null()) {
        let Some(id) = id.as_u64() else {
            return Err(error_reply(
                "\"window\" is a window id (see \"windows\")",
                Value::Null,
            ));
        };
        return windows
            .iter()
            .position(|(window, _, _)| *window == id)
            .ok_or_else(|| {
                error_reply(
                    format!("no window {id}"),
                    json!({ "windows": windows.iter().map(|w| w.0).collect::<Vec<_>>() }),
                )
            });
    }
    if let Some(wanted) = request.get("project_roots").and_then(Value::as_array) {
        let wanted: Vec<String> = wanted
            .iter()
            .filter_map(Value::as_str)
            .map(normalize_path)
            .collect();
        if let Some(index) = windows.iter().position(|(_, roots, _)| {
            roots
                .iter()
                .any(|root| wanted.contains(&normalize_path(root)))
        }) {
            return Ok(index);
        }
    }
    if let Some(cwd) = request.get("cwd").and_then(Value::as_str) {
        let best = windows
            .iter()
            .enumerate()
            .flat_map(|(index, (_, roots, _))| {
                roots
                    .iter()
                    .filter(|root| contains(root, cwd))
                    .map(move |root| (index, normalize_path(root).len()))
            })
            .max_by_key(|(_, len)| *len);
        if let Some((index, _)) = best {
            return Ok(index);
        }
    }
    Ok(windows
        .iter()
        .position(|(_, _, active)| *active)
        .unwrap_or(0))
}

fn choose_window(request: &Value, cx: &mut App) -> Result<AnyWindowHandle, Value> {
    let infos = window_infos(cx);
    let windows: Vec<_> = infos
        .iter()
        .map(|info| {
            (
                info.handle.window_id().as_u64(),
                roots_of(info),
                info.active,
            )
        })
        .collect();
    choose(&windows, request).map(|index| infos[index].handle)
}

fn node_json(frame: &Frame, index: usize, full_text: bool) -> Value {
    let node = &frame.nodes[index];
    let mut value = json!({
        "path": node.path.as_ref(),
        "bounds": bounds_json(node.bounds),
        "visible": node.visible.is_some(),
        "interactive": node.interactive,
    });
    if let Some(label) = &node.label {
        value["label"] = label.as_ref().into();
    }
    if let Some(text) = &node.text {
        value["text"] = if full_text || text.chars().count() <= 160 {
            text.as_ref().into()
        } else {
            let short: String = text.chars().take(160).collect();
            format!("{short}…").into()
        };
    }
    value
}

fn bool_field(request: &Value, key: &str) -> bool {
    request.get(key).and_then(Value::as_bool) == Some(true)
}

fn list(request: &Value, window: &Window) -> Value {
    let id = window.window_handle().window_id();
    let frame = frame(id);
    let prefix = request
        .get("prefix")
        .and_then(Value::as_str)
        .map(|p| p.strip_prefix(super::target::UI_SCHEME).unwrap_or(p))
        .map(|p| p.trim_end_matches('/'))
        .filter(|p| !p.is_empty());
    let visible_only = bool_field(request, "visible");
    let interactive_only = bool_field(request, "interactive");
    let full_text = bool_field(request, "full_text");
    let limit = request.get("limit").and_then(Value::as_u64).unwrap_or(1000) as usize;
    let max_depth = request.get("depth").and_then(Value::as_u64);
    let base_depth = match prefix {
        Some(prefix) => match frame.find(prefix) {
            Some(index) => frame.nodes[index].depth as u64,
            None => {
                return not_found_reply(&frame, prefix, "no element has this prefix path");
            }
        },
        None => 0,
    };
    let mut seen = HashSet::new();
    let mut duplicates = Vec::new();
    let mut nodes = Vec::new();
    let mut total = 0;
    for (index, node) in frame.nodes.iter().enumerate() {
        if let Some(prefix) = prefix
            && !(node.path == prefix
                || node
                    .path
                    .strip_prefix(prefix)
                    .is_some_and(|rest| rest.starts_with('/')))
        {
            continue;
        }
        if (visible_only && node.visible.is_none())
            || (interactive_only && !node.interactive)
            || max_depth.is_some_and(|max| node.depth as u64 > base_depth + max)
        {
            continue;
        }
        if !seen.insert(node.path.clone()) {
            duplicates.push(node.path.as_ref());
            continue;
        }
        total += 1;
        if nodes.len() < limit {
            nodes.push(node_json(&frame, index, full_text));
        }
    }
    let mut reply = json!({
        "ok": true,
        "window": id.as_u64(),
        "count": nodes.len(),
        "total": total,
        "nodes": nodes,
    });
    if total > limit {
        reply["truncated"] = true.into();
        reply["hint"] =
            "narrow with \"prefix\" (a path), \"depth\", \"visible\" or raise \"limit\"".into();
    }
    if !duplicates.is_empty() {
        duplicates.dedup();
        // Paths must be unique: highlights take the last drawn one.
        reply["duplicates"] = duplicates.into();
    }
    reply
}

fn not_found_reply(frame: &Frame, path: &str, message: &str) -> Value {
    let mut extra = json!({ "path": path });
    status_details(&super::overlay::not_found(frame, path), &mut extra);
    error_reply(message, extra)
}

/// Matches of `query` in `frame`, best first: equal label/text, then a
/// label/text starting or containing it, then the id, then the path;
/// visible and interactive ones first within each.
pub fn find_nodes(frame: &Frame, query: &str) -> Vec<usize> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return Vec::new();
    }
    let mut seen = HashSet::new();
    let mut found: Vec<(u8, bool, bool, usize)> = frame
        .nodes
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, node)| seen.insert(node.path.clone()))
        .filter_map(|(index, node)| {
            let words = [node.label.as_ref(), node.text.as_ref()];
            let mut rank = None;
            for word in words.into_iter().flatten() {
                let word = word.to_lowercase();
                let r = if word == query {
                    0
                } else if word.starts_with(&query) {
                    1
                } else if word.contains(&query) {
                    2
                } else {
                    continue;
                };
                rank = Some(rank.map_or(r, |old: u8| old.min(r)));
            }
            let id = node.id.to_lowercase();
            let rank = rank.or_else(|| {
                if id == query || id.split(':').next_back() == Some(query.as_str()) {
                    Some(3)
                } else if id.contains(&query) {
                    Some(4)
                } else if node.path.to_lowercase().contains(&query) {
                    Some(5)
                } else {
                    None
                }
            })?;
            Some((rank, node.visible.is_none(), !node.interactive, index))
        })
        .collect();
    found.sort_by_key(|(rank, hidden, passive, index)| (*rank, *hidden, *passive, *index));
    found.into_iter().map(|(.., index)| index).collect()
}

fn find(request: &Value, window: &Window) -> Value {
    let id = window.window_handle().window_id();
    let Some(query) = request
        .get("query")
        .or_else(|| request.get("text"))
        .and_then(Value::as_str)
    else {
        return error_reply("find needs \"query\"", Value::Null);
    };
    let frame = frame(id);
    let limit = request.get("limit").and_then(Value::as_u64).unwrap_or(20) as usize;
    let found = find_nodes(&frame, query);
    let nodes: Vec<_> = found
        .iter()
        .take(limit)
        .map(|index| node_json(&frame, *index, false))
        .collect();
    json!({
        "ok": true,
        "window": id.as_u64(),
        "count": nodes.len(),
        "total": found.len(),
        "nodes": nodes,
    })
}

fn parse_color(value: &Value) -> Result<Option<Hsla>, String> {
    let Some(name) = value.as_str() else {
        return match value {
            Value::Null => Ok(None),
            _ => Err("\"color\" is a name or \"#rrggbb\"".into()),
        };
    };
    Ok(Some(match name {
        "accent" => theme::accent(),
        "warning" | "yellow" | "orange" => theme::warning(),
        "error" | "red" => theme::error(),
        "green" | "added" => theme::git_added(),
        "blue" | "modified" => theme::git_modified(),
        hex if hex.starts_with('#') => Rgba::try_from(hex)
            .map_err(|_| format!("bad color {hex:?}: use #rrggbb"))?
            .into(),
        other => {
            return Err(format!(
                "unknown color {other:?}: use accent, warning, error, green, blue or #rrggbb"
            ));
        }
    }))
}

/// A highlight request's targets with their own options.
struct Planned {
    target: Result<Target, Value>,
    style: Style,
    color: Option<Hsla>,
    badge: Option<u32>,
    note: Option<SharedString>,
}

/// Reads one target (object, reference string, or `find`) with options
/// falling back to the request's.
fn plan_target(
    value: &Value,
    request: &Value,
    index: usize,
    frame: &Frame,
) -> Result<Planned, String> {
    let option = |key: &str| {
        value
            .as_object()
            .and_then(|o| o.get(key))
            .filter(|v| !v.is_null())
            .or_else(|| request.get(key).filter(|v| !v.is_null()))
    };
    // A string, or `{"ref": "<string>", …options}` (the CLI's form).
    let reference = value
        .as_str()
        .or_else(|| value.get("ref").and_then(Value::as_str));
    let target = match reference {
        // A path of this frame wins over reading it as code
        // (`…/cell:3`).
        Some(text) if frame.find(text.trim()).is_some() => Ok(Target::Element {
            path: text.trim().to_owned(),
        }),
        Some(text) => Ok(parse_reference(text)?),
        None => match Target::from_json(value)? {
            Ok(target) => Ok(target),
            Err(query) => match find_nodes(frame, &query).first() {
                Some(&index) => Ok(Target::Element {
                    path: frame.nodes[index].path.to_string(),
                }),
                None => Err(error_reply(
                    format!("nothing on screen matches {query:?}"),
                    json!({ "find": query }),
                )),
            },
        },
    };
    let style = match option("style").and_then(Value::as_str) {
        Some(name) => Style::parse(name)?,
        None => match &target {
            Ok(Target::Element { .. }) | Err(_) => Style::Outline,
            Ok(_) => Style::Fill,
        },
    };
    let badge = match value
        .as_object()
        .and_then(|o| o.get("step").or_else(|| o.get("badge")))
        .filter(|v| !v.is_null())
    {
        Some(step) => Some(step.as_u64().ok_or("\"step\" is a number")? as u32),
        None if bool_field(request, "steps") => Some(index as u32 + 1),
        None => match request.get("step").or_else(|| request.get("badge")) {
            Some(Value::Null) | None => None,
            Some(step) => Some(step.as_u64().ok_or("\"step\" is a number")? as u32),
        },
    };
    let note = option("note")
        .map(|note| {
            note.as_str()
                .map(|note| SharedString::from(note.to_owned()))
                .ok_or("\"note\" is a string")
        })
        .transpose()?;
    Ok(Planned {
        target,
        style,
        color: parse_color(option("color").unwrap_or(&Value::Null))?,
        badge,
        note,
    })
}

struct Pending {
    window: WindowId,
    ids: Vec<HighlightId>,
    /// Failed targets, in request order with the shown ones.
    entries: Vec<Result<HighlightId, Value>>,
    /// Why revealing a target failed.
    reveal_errors: Vec<(HighlightId, String)>,
    /// Waits until then for code to be revealed and drawn.
    until: Instant,
    reply: Reply,
}

/// Each highlight's status in one window's last frame.
type Statuses = Vec<(HighlightId, Status)>;

thread_local! {
    static PENDING: RefCell<Vec<Pending>> = const { RefCell::new(Vec::new()) };
    /// What became of each highlight in its window's last frame.
    static LAST: RefCell<Vec<(WindowId, Statuses)>> = const { RefCell::new(Vec::new()) };
}

fn highlight_cmd(request: &Value, reply: Reply, window: &mut Window, cx: &mut App) {
    let id = window.window_handle().window_id();
    let frame = frame(id);
    let targets: Vec<Value> = match (request.get("targets"), request.get("target")) {
        (Some(Value::Array(targets)), _) => targets.clone(),
        (_, Some(target)) => vec![target.clone()],
        _ => {
            return reply(error_reply(
                "highlight needs \"targets\": [...] (or \"target\")",
                json!({ "example": { "cmd": "highlight", "targets": [{ "path": "menu-bar/run", "note": "Click Run", "step": 1 }] } }),
            ));
        }
    };
    if targets.is_empty() {
        return reply(error_reply("\"targets\" is empty", Value::Null));
    }
    let mut planned = Vec::new();
    for (index, target) in targets.iter().enumerate() {
        match plan_target(target, request, index, &frame) {
            Ok(plan) => planned.push(plan),
            Err(error) => {
                return reply(error_reply(
                    format!("target {}: {error}", index + 1),
                    json!({ "target": target }),
                ));
            }
        }
    }
    let duration = match request.get("duration") {
        None | Some(Value::Null) => None,
        Some(seconds) => match seconds.as_f64().filter(|s| *s > 0. && *s < 86_400.) {
            Some(seconds) => Some(Duration::from_secs_f64(seconds)),
            None => return reply(error_reply("\"duration\" is seconds (> 0)", Value::Null)),
        },
    };
    let reveal = bool_field(request, "reveal");
    let pulse = request.get("pulse").and_then(Value::as_bool) != Some(false);
    if bool_field(request, "clear") {
        highlight::clear(Some(id), None);
    }
    let now = Instant::now();
    let mut entries = Vec::new();
    let mut ids = Vec::new();
    let mut revealing = false;
    let mut reveal_errors = Vec::new();
    for plan in planned {
        let target = match plan.target {
            Ok(target) => target,
            Err(error) => {
                entries.push(Err(error));
                continue;
            }
        };
        let highlight_id = highlight::next_id();
        if reveal {
            match reveal_target(&target, window, cx) {
                Some(Ok(())) => revealing = true,
                Some(Err(error)) => reveal_errors.push((highlight_id, error)),
                None => {}
            }
        }
        highlight::add(Highlight {
            id: highlight_id,
            window: id,
            target,
            style: plan.style,
            color: plan.color,
            badge: plan.badge,
            note: plan.note,
            created: now,
            pulse,
            until: duration.map(|d| now + d),
        });
        ids.push(highlight_id);
        entries.push(Ok(highlight_id));
    }
    if let Some(duration) = duration {
        let handle = window.window_handle();
        cx.spawn(async move |cx| {
            cx.background_executor().timer(duration).await;
            handle.update(cx, |_, window, _| window.refresh()).ok();
        })
        .detach();
    }
    let wait = if revealing {
        Duration::from_millis(3000)
    } else {
        Duration::ZERO
    };
    PENDING.with_borrow_mut(|pending| {
        pending.push(Pending {
            window: id,
            ids,
            entries,
            reveal_errors,
            until: now + wait,
            reply,
        })
    });
    // Answer even if the window does not draw (minimized).
    let handle = window.window_handle();
    cx.spawn(async move |cx| {
        cx.background_executor()
            .timer(wait + Duration::from_millis(1500))
            .await;
        cx.update(|_| flush(id, true)).ok();
        handle.update(cx, |_, window, _| window.refresh()).ok();
    })
    .detach();
    window.refresh();
}

/// Asks the owner of code/terminal targets to show them (`None`: nothing
/// to reveal for elements).
fn reveal_target(target: &Target, window: &mut Window, cx: &mut App) -> Option<Result<(), String>> {
    match target {
        Target::Code(code) => Some(crate::pinpoint_text::reveal_code(window, cx, code)),
        Target::Terminal(terminal) => {
            Some(crate::pinpoint_text::reveal_terminal(window, cx, terminal))
        }
        _ => None,
    }
}

/// Called after every frame of `window` with its highlights' statuses.
pub(super) fn frame_done(
    window_id: WindowId,
    statuses: &[(HighlightId, Status)],
    window: &mut Window,
    _: &mut App,
) {
    LAST.with_borrow_mut(|last| {
        last.retain(|(w, _)| *w != window_id);
        if !statuses.is_empty() {
            last.push((window_id, statuses.to_vec()));
        }
    });
    if flush(window_id, false) {
        window.request_animation_frame();
    }
}

/// Answers pending highlight requests of `window` that are settled (all
/// shown, or their wait is over; all of them when `force`). Whether some
/// still wait.
fn flush(window: WindowId, force: bool) -> bool {
    let now = Instant::now();
    let statuses = LAST.with_borrow(|last| {
        last.iter()
            .find(|(w, _)| *w == window)
            .map(|(_, s)| s.clone())
            .unwrap_or_default()
    });
    let status_of = |id: HighlightId| statuses.iter().find(|(i, _)| *i == id).map(|(_, s)| s);
    let ready: Vec<Pending> = PENDING.with_borrow_mut(|pending| {
        let (ready, waiting) = std::mem::take(pending).into_iter().partition(|p| {
            p.window == window
                && (force
                    || now >= p.until
                    || p.ids
                        .iter()
                        .all(|id| matches!(status_of(*id), Some(Status::Shown { .. }))))
        });
        *pending = waiting;
        ready
    });
    for pending in ready {
        let mut results = Vec::new();
        let mut shown = 0;
        for entry in pending.entries {
            results.push(match entry {
                Err(error) => {
                    let mut error = error;
                    error["status"] = "not_found".into();
                    error
                }
                Ok(id) => {
                    let target = highlight::target_of(id).map(|t| t.to_json());
                    let mut value = json!({ "id": id, "target": target });
                    match status_of(id) {
                        Some(status) => {
                            shown += usize::from(matches!(status, Status::Shown { .. }));
                            value["status"] = status.name().into();
                            status_details(status, &mut value);
                        }
                        None if target.is_none() => value["status"] = "cleared".into(),
                        None => {
                            value["status"] = "pending".into();
                            value["reason"] = "the window has not drawn since (minimized?)".into();
                        }
                    }
                    if let Some((_, error)) = pending.reveal_errors.iter().find(|(i, _)| *i == id) {
                        value["reveal_error"] = error.as_str().into();
                    }
                    value
                }
            });
        }
        let total = results.len();
        let mut reply = json!({
            "ok": true,
            "window": window.as_u64(),
            "ids": pending.ids,
            "shown": shown,
            "results": results,
        });
        if shown < total {
            reply["hint"] = "targets not shown stay active and appear when drawn; see each result's reason, nearest_visible and did_you_mean".into();
        }
        (pending.reply)(reply);
    }
    PENDING.with_borrow(|pending| pending.iter().any(|p| p.window == window))
}

fn status_details(status: &Status, value: &mut Value) {
    match status {
        Status::Shown {
            rects,
            clipped,
            approximate,
        } => {
            value["rects"] = rects.iter().copied().map(bounds_json).collect();
            if *clipped {
                value["clipped"] = true.into();
            }
            if *approximate {
                value["approximate"] = true.into();
            }
        }
        Status::NotVisible { reason, ancestor } => {
            value["reason"] = reason.as_str().into();
            if let Some(ancestor) = ancestor {
                value["nearest_visible"] = ancestor.as_ref().into();
            }
        }
        Status::NotFound {
            reason,
            ancestor,
            children,
            suggestions,
        } => {
            value["reason"] = reason.as_str().into();
            if let Some(ancestor) = ancestor {
                value["deepest_existing"] = ancestor.as_ref().into();
                if !children.is_empty() {
                    value["children"] = children.iter().map(|c| c.as_ref()).collect();
                }
            }
            if !suggestions.is_empty() {
                value["did_you_mean"] = suggestions.iter().map(|s| s.as_ref()).collect();
            }
        }
    }
}

fn clear(request: &Value, window: &mut Window, cx: &mut App) -> Value {
    let ids: Option<Vec<HighlightId>> = request
        .get("ids")
        .and_then(Value::as_array)
        .map(|ids| ids.iter().filter_map(Value::as_u64).collect());
    let everywhere = bool_field(request, "all_windows");
    let window_id = window.window_handle().window_id();
    let cleared = highlight::clear(
        (!everywhere && ids.is_none()).then_some(window_id),
        ids.as_deref(),
    );
    window.refresh();
    if everywhere || ids.is_some() {
        for handle in cx.windows() {
            handle.update(cx, |_, window, _| window.refresh()).ok();
        }
    }
    json!({ "ok": true, "cleared": cleared })
}

fn wait_pin(request: &Value, reply: Reply, cx: &mut App) {
    let timeout = request
        .get("timeout")
        .and_then(Value::as_f64)
        .unwrap_or(120.)
        .clamp(0.1, 3600.);
    let timeout = Duration::from_secs_f64(timeout);
    let since = request
        .get("since")
        .and_then(Value::as_u64)
        .unwrap_or_else(picker::last_seq);
    if bool_field(request, "pick")
        && let Ok(handle) = choose_window(request, cx)
    {
        handle.update(cx, |_, window, _| picker::start(window)).ok();
    }
    let roots = absolute_roots(cx);
    let reply: Reply = Box::new(move |mut value: Value| {
        if let Some(pin) = value.get_mut("pin") {
            add_absolute_file(pin, &roots);
        }
        reply(value)
    });
    if let Some(waiter) = picker::wait(since, reply) {
        cx.spawn(async move |cx| {
            cx.background_executor().timer(timeout).await;
            cx.update(|_| picker::expire(waiter, timeout)).ok();
        })
        .detach();
    }
}

/// Per window: its project's roots as (name, absolute path).
fn absolute_roots(cx: &mut App) -> Vec<(u64, Vec<(String, String)>)> {
    window_infos(cx)
        .iter()
        .map(|info| {
            let roots = info
                .projects
                .first()
                .map(|project| {
                    project
                        .roots()
                        .iter()
                        .map(|root| (root.name.clone(), root.path.to_string_lossy().into_owned()))
                        .collect()
                })
                .unwrap_or_default();
            (info.handle.window_id().as_u64(), roots)
        })
        .collect()
}

fn add_absolute_files(pins: &mut [Value], cx: &mut App) {
    let roots = absolute_roots(cx);
    for pin in pins {
        add_absolute_file(pin, &roots);
    }
}

/// Code pins name files by key (see `file_key`); adds `file_absolute`.
fn add_absolute_file(pin: &mut Value, roots: &[(u64, Vec<(String, String)>)]) {
    let Some(file) = pin
        .get("target")
        .and_then(|t| t.get("file"))
        .and_then(Value::as_str)
    else {
        return;
    };
    let window = pin.get("window").and_then(Value::as_u64);
    let Some((_, roots)) = roots.iter().find(|(w, _)| Some(*w) == window) else {
        return;
    };
    if let Some(absolute) = absolute_file(file, roots) {
        pin["file_absolute"] = absolute.into();
    }
}

/// `key` (relative, `<root name>/relative` with several roots, or already
/// absolute) as an absolute path.
pub fn absolute_file(key: &str, roots: &[(String, String)]) -> Option<String> {
    let join = |root: &str, rest: &str| {
        let separator = if root.contains('\\') { "\\" } else { "/" };
        format!(
            "{}{separator}{}",
            root.trim_end_matches(['/', '\\']),
            rest.replace('/', separator)
        )
    };
    if key.starts_with('/') || key.as_bytes().get(1) == Some(&b':') || key.starts_with("\\\\") {
        return Some(key.to_owned());
    }
    match roots {
        [] => None,
        [(_, root)] => Some(join(root, key)),
        roots => {
            let (name, rest) = key.split_once('/')?;
            let (_, root) = roots.iter().find(|(n, _)| n == name)?;
            Some(join(root, rest))
        }
    }
}

#[cfg(test)]
mod tests {
    use gpui::{point, px, size};

    use super::*;
    use crate::pinpoint::Node;

    #[test]
    fn window_choice() {
        let windows = vec![
            (1, vec!["/home/me/api".to_owned()], false),
            (
                2,
                vec![r"C:\work\web".to_owned(), r"C:\work\web\sub".to_owned()],
                true,
            ),
            (3, vec!["/home/me".to_owned()], false),
        ];
        let pick = |request: Value| choose(&windows, &request);
        assert_eq!(pick(json!({})), Ok(1));
        assert_eq!(pick(json!({"window": 3})), Ok(2));
        assert!(
            pick(json!({"window": 9})).unwrap_err()["error"]
                .as_str()
                .unwrap()
                .contains("no window 9")
        );
        assert_eq!(pick(json!({"project_roots": ["/home/me/api/"]})), Ok(0));
        // Deepest root holding the directory.
        assert_eq!(pick(json!({"cwd": "/home/me/api/src"})), Ok(0));
        assert_eq!(pick(json!({"cwd": "/home/me/other"})), Ok(2));
        assert_eq!(pick(json!({"cwd": "c:/Work/web/sub/x"})), Ok(1));
        assert_eq!(
            pick(json!({"cwd": r"\\wsl.localhost\Ubuntu\home\me\api"})),
            Ok(0)
        );
        // Not a prefix on a segment boundary.
        assert_eq!(pick(json!({"cwd": "/home/me/apis"})), Ok(2));
        assert_eq!(pick(json!({"cwd": "/elsewhere"})), Ok(1));
        assert!(choose(&[], &json!({})).is_err());
    }

    #[test]
    fn absolute_files() {
        let one = [("api".to_owned(), "/home/me/api".to_owned())];
        assert_eq!(
            absolute_file("src/a.rs", &one).as_deref(),
            Some("/home/me/api/src/a.rs")
        );
        let two = [
            ("api".to_owned(), r"C:\w\api".to_owned()),
            ("web".to_owned(), r"C:\w\web".to_owned()),
        ];
        assert_eq!(
            absolute_file("web/src/a.ts", &two).as_deref(),
            Some(r"C:\w\web\src\a.ts")
        );
        assert_eq!(absolute_file("nope/a", &two), None);
        assert_eq!(absolute_file("/abs/x", &two).as_deref(), Some("/abs/x"));
    }

    fn node(path: &str, label: Option<&str>, visible: bool, interactive: bool) -> Node {
        let bounds = Bounds::new(point(px(0.), px(0.)), size(px(10.), px(10.)));
        Node {
            path: path.to_owned().into(),
            id: path.rsplit('/').next().unwrap().to_owned().into(),
            parent: None,
            depth: 0,
            label: label.map(|l| l.to_owned().into()),
            text: None,
            bounds,
            visible: visible.then_some(bounds),
            interactive,
        }
    }

    #[test]
    fn finding() {
        let frame = Frame {
            nodes: vec![
                node("workspace/left-dock/panel:commit", None, true, false),
                node(
                    "workspace/left-dock/panel:commit/commit-and-push",
                    Some("Commit and Push…"),
                    true,
                    true,
                ),
                node(
                    "workspace/left-dock/panel:commit/commit",
                    Some("Commit"),
                    true,
                    true,
                ),
                node(
                    "workspace/left-dock/panel:commit/hidden",
                    Some("Commit"),
                    false,
                    true,
                ),
            ],
        };
        let paths: Vec<_> = find_nodes(&frame, "commit")
            .into_iter()
            .map(|i| frame.nodes[i].path.as_ref())
            .collect();
        assert_eq!(
            paths,
            [
                "workspace/left-dock/panel:commit/commit",
                "workspace/left-dock/panel:commit/hidden",
                "workspace/left-dock/panel:commit/commit-and-push",
                "workspace/left-dock/panel:commit",
            ]
        );
        assert!(find_nodes(&frame, "  ").is_empty());
    }
}
