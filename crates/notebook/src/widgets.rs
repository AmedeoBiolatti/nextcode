//! ipywidgets models over the Jupyter comm protocol (target
//! `jupyter.widget`, protocol 2.1): the kernel opens a comm per model
//! (`comm_open` with `{state, buffer_paths}`), and both sides send
//! `comm_msg`s with `method` `update` (state changes), `echo_update` (the
//! kernel echoing a frontend change) or `custom` (events such as a button
//! click); `comm_close` ends a model.
//!
//! [`WidgetStore`] is the frontend's side, without I/O: it folds kernel
//! comm messages into models, applies local changes and returns the comm
//! messages to send, routes outputs captured by Output widgets, and reads
//! and writes the notebook's saved widget state (`metadata.widgets`,
//! `application/vnd.jupyter.widget-state+json`) so a reopened notebook
//! shows its widgets' last values.

use std::collections::{HashMap, HashSet, VecDeque};

use serde_json::{Map, Value, json};

use crate::{
    Output, ipynb,
    outputs::{self, Applied, OutputState},
};

pub const TARGET: &str = "jupyter.widget";
/// The key of the saved state in the notebook's `metadata.widgets`.
pub const STATE_MIME: &str = "application/vnd.jupyter.widget-state+json";
/// How many of our own comm message ids are remembered (outputs they
/// cause are not a running cell's).
const OWN_MESSAGES: usize = 256;

/// A widget model: its state as the kernel synchronises it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct WidgetModel {
    /// `_model_name`, e.g. `IntSliderModel`.
    pub name: String,
    /// `_model_module`, e.g. `@jupyter-widgets/controls`.
    pub module: String,
    pub module_version: String,
    pub state: Map<String, Value>,
    /// Binary state values by path (base64): Image's `value`.
    pub buffers: Vec<(Vec<Value>, String)>,
    /// Output widgets: their outputs (the `outputs` state, plus what the
    /// frontend captured), folded like a cell's.
    pub outputs: Vec<Output>,
    output_state: OutputState,
    /// When its `msg_id` was set (Output widgets): the latest capturing
    /// widget wins.
    capture_serial: u64,
    /// A comm is open for it (else it shows the saved state, disabled).
    pub live: bool,
}

impl WidgetModel {
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.state.get(key)
    }

    pub fn str(&self, key: &str) -> &str {
        self.state.get(key).and_then(Value::as_str).unwrap_or("")
    }

    pub fn f64(&self, key: &str) -> Option<f64> {
        self.state.get(key).and_then(Value::as_f64)
    }

    pub fn bool(&self, key: &str) -> bool {
        self.state
            .get(key)
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }

    /// The base64 buffer at `path` (one key deep).
    pub fn buffer(&self, key: &str) -> Option<&str> {
        self.buffers
            .iter()
            .find(|(path, _)| path.len() == 1 && path[0] == key)
            .map(|(_, data)| data.as_str())
    }

    /// A reference (`"IPY_MODEL_<id>"`) in `key` as the model id.
    pub fn reference(&self, key: &str) -> Option<&str> {
        self.state.get(key).and_then(model_reference)
    }

    /// The model ids of a list of references (`children`).
    pub fn references(&self, key: &str) -> Vec<&str> {
        self.state
            .get(key)
            .and_then(Value::as_array)
            .map(|items| items.iter().filter_map(model_reference).collect())
            .unwrap_or_default()
    }

    fn is_output(&self) -> bool {
        self.name == "OutputModel"
    }

    /// Applies `state` (with binary values at `buffer_paths`).
    fn apply_state(
        &mut self,
        state: &Map<String, Value>,
        buffer_paths: &[Value],
        buffers: &[String],
    ) {
        for (key, value) in state {
            // A key set again replaces the binary values under it.
            self.buffers
                .retain(|(path, _)| path.first().and_then(Value::as_str) != Some(key));
            if key == "outputs" && self.is_output() {
                self.outputs = value
                    .as_array()
                    .map(|outputs| outputs.iter().filter_map(ipynb::parse_output).collect())
                    .unwrap_or_default();
                self.output_state = OutputState::default();
            }
            self.state.insert(key.clone(), value.clone());
        }
        for (path, data) in buffer_paths.iter().zip(buffers) {
            if let Some(path) = path.as_array() {
                self.buffers.retain(|(existing, _)| existing != path);
                self.buffers.push((path.clone(), data.clone()));
            }
        }
    }
}

/// `"IPY_MODEL_<id>"` as `<id>`.
pub fn model_reference(value: &Value) -> Option<&str> {
    value.as_str()?.strip_prefix("IPY_MODEL_")
}

/// A comm message for the kernel.
#[derive(Debug, Clone, PartialEq)]
pub struct CommMessage {
    pub comm_id: String,
    pub data: Value,
    /// Base64.
    pub buffers: Vec<String>,
    /// The message's header id: the kernel's echo and the outputs it
    /// causes carry it as their parent.
    pub msg_id: String,
}

/// The frontend's widget models of one notebook.
#[derive(Debug, Default)]
pub struct WidgetStore {
    models: HashMap<String, WidgetModel>,
    /// (model, key) → the id of our latest update of it, until its echo.
    expected_echo: HashMap<(String, String), String>,
    /// Our recent comm message ids.
    own: VecDeque<String>,
    own_set: HashSet<String>,
    session: String,
    counter: u64,
    capture_counter: u64,
    /// Bumped by every change (views redraw, saving rewrites the state).
    version: u64,
    /// The kernel or the user changed something since the store was
    /// loaded from the file.
    touched: bool,
}

impl WidgetStore {
    pub fn new() -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos() as u64);
        Self {
            session: format!("{:x}", nanos ^ u64::from(std::process::id()) << 32),
            ..Self::default()
        }
    }

    pub fn get(&self, id: &str) -> Option<&WidgetModel> {
        self.models.get(id)
    }

    pub fn version(&self) -> u64 {
        self.version
    }

    pub fn is_empty(&self) -> bool {
        self.models.is_empty()
    }

    /// Changed by the kernel or the user since it was loaded.
    pub fn touched(&self) -> bool {
        self.touched
    }

    fn changed(&mut self) {
        self.version += 1;
        self.touched = true;
    }

    /// Folds a kernel comm message (`comm_open`, `comm_msg`, `comm_close`;
    /// `buffers` base64; `parent`: the message's parent id) in. Returns
    /// whether a model changed.
    pub fn apply(
        &mut self,
        msg_type: &str,
        content: &Value,
        buffers: &[String],
        parent: Option<&str>,
    ) -> bool {
        let Some(comm_id) = content.get("comm_id").and_then(Value::as_str) else {
            return false;
        };
        let data = content.get("data").unwrap_or(&Value::Null);
        let empty = Map::new();
        let state = data
            .get("state")
            .and_then(Value::as_object)
            .unwrap_or(&empty);
        let buffer_paths = data
            .get("buffer_paths")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        match msg_type {
            "comm_open" => {
                if content.get("target_name").and_then(Value::as_str) != Some(TARGET) {
                    return false;
                }
                let mut model = WidgetModel {
                    name: state
                        .get("_model_name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    module: state
                        .get("_model_module")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    module_version: state
                        .get("_model_module_version")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    live: true,
                    ..WidgetModel::default()
                };
                model.apply_state(state, buffer_paths, buffers);
                self.set_capture(&mut model, state);
                self.models.insert(comm_id.to_owned(), model);
            }
            "comm_msg" => {
                let method = data.get("method").and_then(Value::as_str).unwrap_or("");
                if !matches!(method, "update" | "echo_update") {
                    // `custom` from the kernel: no built-in view uses it.
                    return false;
                }
                let mut state = state.clone();
                if method == "echo_update" {
                    // Our own change coming back (already applied), or an
                    // older value while a newer change of ours is on its
                    // way: dropped. Echoes of other frontends' changes
                    // apply.
                    state.retain(|key, _| {
                        let slot = (comm_id.to_owned(), key.clone());
                        match self.expected_echo.get(&slot) {
                            Some(expected) => {
                                if parent == Some(expected.as_str()) {
                                    self.expected_echo.remove(&slot);
                                }
                                false
                            }
                            None => true,
                        }
                    });
                    if state.is_empty() {
                        return false;
                    }
                }
                let mut capture = self.capture_counter;
                let Some(model) = self.models.get_mut(comm_id) else {
                    return false;
                };
                model.apply_state(&state, buffer_paths, buffers);
                if model.is_output() && state.contains_key("msg_id") {
                    capture += 1;
                    model.capture_serial = capture;
                    self.capture_counter = capture;
                }
            }
            "comm_close" => {
                if self.models.remove(comm_id).is_none() {
                    return false;
                }
                self.expected_echo.retain(|(model, _), _| model != comm_id);
            }
            _ => return false,
        }
        self.changed();
        true
    }

    fn set_capture(&mut self, model: &mut WidgetModel, state: &Map<String, Value>) {
        if model.is_output() && state.contains_key("msg_id") {
            self.capture_counter += 1;
            model.capture_serial = self.capture_counter;
        }
    }

    /// The Output widget capturing outputs whose parent message is
    /// `parent` (its `msg_id` state), if any: the one that started
    /// capturing last.
    pub fn capturing(&self, parent: &str) -> Option<&str> {
        if parent.is_empty() {
            return None;
        }
        self.models
            .iter()
            .filter(|(_, model)| model.is_output() && model.str("msg_id") == parent)
            .max_by_key(|(_, model)| model.capture_serial)
            .map(|(id, _)| id.as_str())
    }

    /// Folds an output message into Output widget `id`. Returns whether it
    /// changed.
    pub fn capture(&mut self, id: &str, msg_type: &str, content: &Value) -> bool {
        let Some(model) = self.models.get_mut(id) else {
            return false;
        };
        let applied = outputs::apply(
            &mut model.outputs,
            &mut model.output_state,
            msg_type,
            content,
        );
        let changed = applied == Applied::Changed;
        if changed {
            self.changed();
        }
        changed
    }

    /// Whether `parent` is one of our comm messages (outputs it caused
    /// belong to no cell).
    pub fn is_own(&self, parent: &str) -> bool {
        self.own_set.contains(parent)
    }

    fn next_msg_id(&mut self) -> String {
        self.counter += 1;
        let id = format!("nc-{}-{}", self.session, self.counter);
        self.own.push_back(id.clone());
        self.own_set.insert(id.clone());
        while self.own.len() > OWN_MESSAGES {
            if let Some(old) = self.own.pop_front() {
                self.own_set.remove(&old);
            }
        }
        id
    }

    /// Sets `key` of model `id` to `value` locally; the `update` message
    /// for the kernel when `send` and the model is live (a slider drag
    /// sends only now and then, then at its end).
    pub fn update(&mut self, id: &str, key: &str, value: Value, send: bool) -> Option<CommMessage> {
        let model = self.models.get_mut(id)?;
        if model.state.get(key) != Some(&value) {
            model.state.insert(key.to_owned(), value.clone());
            self.changed();
        }
        let model = self.models.get(id)?;
        if !send || !model.live {
            return None;
        }
        let msg_id = self.next_msg_id();
        self.expected_echo
            .insert((id.to_owned(), key.to_owned()), msg_id.clone());
        Some(CommMessage {
            comm_id: id.to_owned(),
            data: json!({
                "method": "update",
                "state": { key: value },
                "buffer_paths": [],
            }),
            buffers: Vec::new(),
            msg_id,
        })
    }

    /// A `custom` message to model `id` (a Button's `{"event": "click"}`).
    pub fn custom(&mut self, id: &str, content: Value) -> Option<CommMessage> {
        if !self.models.get(id)?.live {
            return None;
        }
        let msg_id = self.next_msg_id();
        Some(CommMessage {
            comm_id: id.to_owned(),
            data: json!({"method": "custom", "content": content}),
            buffers: Vec::new(),
            msg_id,
        })
    }

    /// The kernel went away (restart, death, shutdown): the models stay
    /// as saved state, without comms.
    pub fn kernel_gone(&mut self) {
        for model in self.models.values_mut() {
            model.live = false;
        }
        self.expected_echo.clear();
        self.version += 1;
    }

    /// Models no longer reachable from `roots` (the outputs' widget views)
    /// that have no comm are dropped (saved state of outputs cleared or
    /// replaced).
    pub fn retain_reachable<'a>(&mut self, roots: impl IntoIterator<Item = &'a str>) {
        let keep = self.reachable(roots);
        let before = self.models.len();
        self.models
            .retain(|id, model| model.live || keep.contains(id.as_str()));
        if self.models.len() != before {
            self.version += 1;
        }
    }

    /// Model ids reachable from `roots` through `IPY_MODEL_` references.
    fn reachable<'a>(&self, roots: impl IntoIterator<Item = &'a str>) -> HashSet<String> {
        let mut seen = HashSet::new();
        let mut stack: Vec<String> = roots.into_iter().map(str::to_owned).collect();
        while let Some(id) = stack.pop() {
            let Some(model) = self.models.get(&id) else {
                continue;
            };
            if !seen.insert(id) {
                continue;
            }
            let mut references = Vec::new();
            for value in model.state.values() {
                collect_references(value, &mut references);
            }
            for output in &model.outputs {
                if let Some(id) = view_model_id(output) {
                    references.push(id.to_owned());
                }
            }
            stack.extend(references);
        }
        seen
    }

    /// The saved-state value for `metadata.widgets` (Jupyter's
    /// `application/vnd.jupyter.widget-state+json`): the models reachable
    /// from `roots`; `None` when there are none.
    pub fn to_metadata<'a>(&self, roots: impl IntoIterator<Item = &'a str>) -> Option<Value> {
        let reachable = self.reachable(roots);
        if reachable.is_empty() {
            return None;
        }
        let mut state = Map::new();
        for id in reachable {
            let model = &self.models[&id];
            let mut model_state = model.state.clone();
            if model.is_output() {
                model_state.insert(
                    "outputs".into(),
                    Value::Array(model.outputs.iter().map(ipynb::output_json).collect()),
                );
            }
            let mut entry = Map::new();
            entry.insert("model_name".into(), model.name.clone().into());
            entry.insert("model_module".into(), model.module.clone().into());
            entry.insert(
                "model_module_version".into(),
                model.module_version.clone().into(),
            );
            entry.insert("state".into(), Value::Object(model_state));
            if !model.buffers.is_empty() {
                entry.insert(
                    "buffers".into(),
                    Value::Array(
                        model
                            .buffers
                            .iter()
                            .map(|(path, data)| {
                                json!({"encoding": "base64", "path": path, "data": data})
                            })
                            .collect(),
                    ),
                );
            }
            state.insert(id, Value::Object(entry));
        }
        Some(json!({
            STATE_MIME: {
                "version_major": 2,
                "version_minor": 0,
                "state": state,
            }
        }))
    }

    /// Loads saved state (`metadata.widgets`) as models without comms,
    /// replacing those without comms. Not a change ([`Self::touched`]).
    pub fn load_metadata(&mut self, widgets: Option<&Value>) {
        self.models.retain(|_, model| model.live);
        if let Some(state) = widgets
            .and_then(|widgets| widgets.get(STATE_MIME))
            .and_then(|saved| saved.get("state"))
            .and_then(Value::as_object)
        {
            for (id, entry) in state {
                if self.models.contains_key(id) {
                    continue;
                }
                let text = |key: &str| {
                    entry
                        .get(key)
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned()
                };
                let mut model = WidgetModel {
                    name: text("model_name"),
                    module: text("model_module"),
                    module_version: text("model_module_version"),
                    ..WidgetModel::default()
                };
                let empty = Map::new();
                let saved = entry
                    .get("state")
                    .and_then(Value::as_object)
                    .unwrap_or(&empty);
                let (paths, data): (Vec<Value>, Vec<String>) = entry
                    .get("buffers")
                    .and_then(Value::as_array)
                    .map(|buffers| {
                        buffers
                            .iter()
                            .filter(|buffer| {
                                buffer.get("encoding").and_then(Value::as_str) == Some("base64")
                            })
                            .filter_map(|buffer| {
                                Some((
                                    buffer.get("path")?.clone(),
                                    buffer.get("data")?.as_str()?.to_owned(),
                                ))
                            })
                            .unzip()
                    })
                    .unwrap_or_default();
                model.apply_state(saved, &paths, &data);
                self.models.insert(id.clone(), model);
            }
        }
        self.version += 1;
    }
}

fn collect_references(value: &Value, into: &mut Vec<String>) {
    match value {
        Value::String(_) => {
            if let Some(id) = model_reference(value) {
                into.push(id.to_owned());
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_references(item, into);
            }
        }
        Value::Object(map) => {
            for item in map.values() {
                collect_references(item, into);
            }
        }
        _ => {}
    }
}

/// The model an output's widget view shows.
pub fn view_model_id(output: &Output) -> Option<&str> {
    match output {
        Output::DisplayData { data, .. } | Output::ExecuteResult { data, .. } => {
            data.get(outputs::WIDGET_MIME)?.get("model_id")?.as_str()
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open(store: &mut WidgetStore, id: &str, name: &str, state: Value) {
        let mut state = state.as_object().cloned().unwrap_or_default();
        state.insert("_model_name".into(), name.into());
        state.insert("_model_module".into(), "@jupyter-widgets/controls".into());
        state.insert("_model_module_version".into(), "2.0.0".into());
        assert!(store.apply(
            "comm_open",
            &json!({"comm_id": id, "target_name": TARGET, "data": {"state": state, "buffer_paths": []}}),
            &[],
            Some("exec-1"),
        ));
    }

    fn kernel_msg(
        store: &mut WidgetStore,
        id: &str,
        method: &str,
        state: Value,
        parent: &str,
    ) -> bool {
        store.apply(
            "comm_msg",
            &json!({"comm_id": id, "data": {"method": method, "state": state, "buffer_paths": []}}),
            &[],
            Some(parent),
        )
    }

    #[test]
    fn a_slider_drag_sends_updates_and_ignores_its_own_echoes() {
        let mut store = WidgetStore::new();
        open(
            &mut store,
            "s",
            "IntSliderModel",
            json!({"value": 5, "min": 0, "max": 10}),
        );
        assert_eq!(store.get("s").unwrap().f64("value"), Some(5.));

        // Dragging: local values at once, messages only when asked.
        assert_eq!(store.update("s", "value", json!(6), false), None);
        let first = store.update("s", "value", json!(7), true).expect("sent");
        assert_eq!(first.comm_id, "s");
        assert_eq!(
            first.data,
            json!({"method": "update", "state": {"value": 7}, "buffer_paths": []})
        );
        let second = store.update("s", "value", json!(8), true).expect("sent");
        assert_ne!(first.msg_id, second.msg_id);
        assert!(store.is_own(&first.msg_id));

        // The echo of the first (older) change does not move the slider
        // back; the echo of the latest is dropped too.
        assert!(!kernel_msg(
            &mut store,
            "s",
            "echo_update",
            json!({"value": 7}),
            &first.msg_id
        ));
        assert_eq!(store.get("s").unwrap().f64("value"), Some(8.));
        assert!(!kernel_msg(
            &mut store,
            "s",
            "echo_update",
            json!({"value": 8}),
            &second.msg_id
        ));
        assert_eq!(store.get("s").unwrap().f64("value"), Some(8.));

        // Another frontend's echo, and kernel-side updates, apply.
        assert!(kernel_msg(
            &mut store,
            "s",
            "echo_update",
            json!({"value": 2}),
            "other"
        ));
        assert_eq!(store.get("s").unwrap().f64("value"), Some(2.));
        assert!(kernel_msg(
            &mut store,
            "s",
            "update",
            json!({"value": 3, "max": 20}),
            "exec-2"
        ));
        let model = store.get("s").unwrap();
        assert_eq!(
            (model.f64("value"), model.f64("max")),
            (Some(3.), Some(20.))
        );
    }

    #[test]
    fn a_button_click_is_a_custom_message() {
        let mut store = WidgetStore::new();
        open(
            &mut store,
            "b",
            "ButtonModel",
            json!({"description": "Click"}),
        );
        let click = store.custom("b", json!({"event": "click"})).unwrap();
        assert_eq!(
            click.data,
            json!({"method": "custom", "content": {"event": "click"}})
        );
        // The handler updates a label: a plain kernel update.
        open(&mut store, "l", "LabelModel", json!({"value": "0"}));
        assert!(kernel_msg(
            &mut store,
            "l",
            "update",
            json!({"value": "1"}),
            &click.msg_id
        ));
        assert_eq!(store.get("l").unwrap().str("value"), "1");
        // Kernel-side `custom` messages change nothing.
        assert!(!store.apply(
            "comm_msg",
            &json!({"comm_id": "b", "data": {"method": "custom", "content": {}}}),
            &[],
            None
        ));
    }

    #[test]
    fn output_widgets_capture_outputs_of_their_msg_id() {
        let mut store = WidgetStore::new();
        open(
            &mut store,
            "o",
            "OutputModel",
            json!({"msg_id": "", "outputs": []}),
        );
        assert_eq!(store.capturing("exec-1"), None);
        kernel_msg(
            &mut store,
            "o",
            "update",
            json!({"msg_id": "exec-1"}),
            "exec-1",
        );
        assert_eq!(store.capturing("exec-1"), Some("o"));
        assert!(store.capture("o", "stream", &json!({"name": "stdout", "text": "a"})));
        assert!(store.capture("o", "stream", &json!({"name": "stdout", "text": "b\n"})));
        // A nested Output started later wins.
        open(&mut store, "inner", "OutputModel", json!({"msg_id": ""}));
        kernel_msg(
            &mut store,
            "inner",
            "update",
            json!({"msg_id": "exec-1"}),
            "exec-1",
        );
        assert_eq!(store.capturing("exec-1"), Some("inner"));
        kernel_msg(
            &mut store,
            "inner",
            "update",
            json!({"msg_id": ""}),
            "exec-1",
        );
        assert_eq!(store.capturing("exec-1"), Some("o"));
        // clear_output(wait=True) then new output: replaced.
        store.capture("o", "clear_output", &json!({"wait": true}));
        store.capture(
            "o",
            "display_data",
            &json!({"data": {"text/plain": "42"}, "metadata": {}}),
        );
        let outputs = &store.get("o").unwrap().outputs;
        assert_eq!(outputs.len(), 1);
        assert!(
            matches!(&outputs[0], Output::DisplayData { data, .. } if data["text/plain"] == "42")
        );
        // The kernel setting `outputs` (append_stdout) replaces them.
        kernel_msg(
            &mut store,
            "o",
            "update",
            json!({"outputs": [{"output_type": "stream", "name": "stdout", "text": "x"}]}),
            "exec-2",
        );
        assert_eq!(
            store.get("o").unwrap().outputs,
            [Output::Stream {
                name: "stdout".into(),
                text: "x".into()
            }]
        );
    }

    #[test]
    fn interact_is_a_box_of_controls_and_an_output() {
        // What `interact(f, x=10)` opens and sends, in order.
        let mut store = WidgetStore::new();
        open(
            &mut store,
            "layout",
            "LayoutModel",
            json!({"width": "300px"}),
        );
        open(
            &mut store,
            "x",
            "IntSliderModel",
            json!({"value": 10, "min": -10, "max": 30, "description": "x", "layout": "IPY_MODEL_layout"}),
        );
        open(&mut store, "out", "OutputModel", json!({"msg_id": ""}));
        open(
            &mut store,
            "box",
            "VBoxModel",
            json!({"children": ["IPY_MODEL_x", "IPY_MODEL_out"], "_dom_classes": ["widget-interact"]}),
        );
        assert_eq!(
            store.get("box").unwrap().references("children"),
            ["x", "out"]
        );
        assert_eq!(store.get("x").unwrap().reference("layout"), Some("layout"));
        // The slider moves: the kernel captures f's output for our message.
        let message = store.update("x", "value", json!(11), true).unwrap();
        kernel_msg(
            &mut store,
            "x",
            "echo_update",
            json!({"value": 11}),
            &message.msg_id,
        );
        kernel_msg(
            &mut store,
            "out",
            "update",
            json!({"msg_id": message.msg_id}),
            &message.msg_id,
        );
        let target = store.capturing(&message.msg_id).unwrap().to_owned();
        store.capture(&target, "clear_output", &json!({"wait": true}));
        store.capture(
            &target,
            "execute_result",
            &json!({"data": {"text/plain": "22"}, "metadata": {}, "execution_count": null}),
        );
        kernel_msg(
            &mut store,
            "out",
            "update",
            json!({"msg_id": ""}),
            &message.msg_id,
        );
        assert_eq!(store.get("out").unwrap().outputs.len(), 1);
        assert_eq!(store.get("x").unwrap().f64("value"), Some(11.));
    }

    #[test]
    fn saved_state_round_trips_and_shows_static_models() {
        let mut store = WidgetStore::new();
        open(&mut store, "lay", "LayoutModel", json!({}));
        open(
            &mut store,
            "s",
            "FloatSliderModel",
            json!({"value": 1.5, "layout": "IPY_MODEL_lay"}),
        );
        open(&mut store, "stray", "IntSliderModel", json!({"value": 1}));
        open(&mut store, "o", "OutputModel", json!({"msg_id": ""}));
        store.capture("o", "stream", &json!({"name": "stdout", "text": "hi\n"}));
        assert!(store.apply(
            "comm_open",
            &json!({"comm_id": "img", "target_name": TARGET, "data": {
                "state": {"_model_name": "ImageModel", "_model_module": "@jupyter-widgets/controls",
                          "_model_module_version": "2.0.0", "format": "png", "value": {}},
                "buffer_paths": [["value"]]}}),
            &["iVBORw0KGgo=".to_owned()],
            None,
        ));
        assert_eq!(
            store.get("img").unwrap().buffer("value"),
            Some("iVBORw0KGgo=")
        );
        store.update("s", "value", json!(2.5), true);

        let saved = store.to_metadata(["s", "o", "img"]).unwrap();
        let models = &saved[STATE_MIME]["state"];
        let mut ids: Vec<&String> = models.as_object().unwrap().keys().collect();
        ids.sort();
        assert_eq!(
            ids,
            ["img", "lay", "o", "s"],
            "unreferenced models are not saved"
        );
        assert_eq!(models["s"]["model_name"], "FloatSliderModel");
        assert_eq!(models["s"]["state"]["value"], 2.5);
        assert_eq!(models["o"]["state"]["outputs"][0]["text"], json!(["hi\n"]));
        assert_eq!(
            models["img"]["buffers"],
            json!([{"encoding": "base64", "path": ["value"], "data": "iVBORw0KGgo="}])
        );

        let mut reopened = WidgetStore::new();
        reopened.load_metadata(Some(&saved));
        assert!(!reopened.touched());
        let slider = reopened.get("s").unwrap();
        assert!(!slider.live);
        assert_eq!(slider.f64("value"), Some(2.5));
        assert_eq!(reopened.get("o").unwrap().outputs.len(), 1);
        assert_eq!(
            reopened.get("img").unwrap().buffer("value"),
            Some("iVBORw0KGgo=")
        );
        // Static models send nothing.
        assert_eq!(reopened.update("s", "value", json!(3), true), None);
        assert_eq!(reopened.custom("s", json!({})), None);
        // Outputs cleared: the saved models go with them.
        reopened.retain_reachable(["o"]);
        assert!(reopened.get("s").is_none());
        assert!(reopened.get("o").is_some());
    }

    #[test]
    fn closing_and_kernel_restarts() {
        let mut store = WidgetStore::new();
        open(&mut store, "a", "CheckboxModel", json!({"value": true}));
        open(&mut store, "b", "CheckboxModel", json!({"value": false}));
        assert!(store.apply(
            "comm_close",
            &json!({"comm_id": "a", "data": {}}),
            &[],
            None
        ));
        assert!(store.get("a").is_none());
        store.kernel_gone();
        assert!(!store.get("b").unwrap().live);
        assert!(!store.get("b").unwrap().bool("value"));
        // Other comm targets are not widgets.
        assert!(!store.apply(
            "comm_open",
            &json!({"comm_id": "c", "target_name": "jupyter.widget.control", "data": {}}),
            &[],
            None
        ));
    }
}
