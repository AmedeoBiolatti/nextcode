//! Language-neutral NextCode extension protocol. All ranges use UTF-8 byte offsets.

use serde::{Deserialize, Serialize};

pub const API_VERSION: u32 = 3;
/// Maximum encoded request size, including document and settings.
pub const MAX_REQUEST_BYTES: usize = 16 * 1024 * 1024;
/// Maximum encoded response size, including edits, logs, and errors.
pub const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub id: String,
    pub name: String,
    pub version: String,
    pub api_version: u32,
    pub module: String,
    pub commands: Vec<Command>,
    #[serde(default)]
    pub panels: Vec<PanelContribution>,
    pub permissions: Vec<Permission>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub processes: Vec<DeclaredProcess>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Command {
    pub id: String,
    pub title: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Permission {
    ReadDocument,
    EditDocument,
    RunProcess,
}

/// A bare executable name, approved together with the permission grant.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeclaredProcess {
    pub program: String,
}

pub const DEFAULT_PROCESS_TIMEOUT_MS: u64 = 30_000;
pub const MAX_PROCESS_TIMEOUT_MS: u64 = 120_000;
/// Before JSON escaping; both streams together fit comfortably in a request.
pub const MAX_PROCESS_OUTPUT_BYTES: usize = 256 * 1024;
pub const MAX_PROCESSES: usize = 4;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessRequest {
    pub id: String,
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default = "default_process_timeout")]
    pub timeout_ms: u64,
    /// On successful, complete output, validate stdout as the response directly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response: Option<ProcessResponse>,
}

fn default_process_timeout() -> u64 {
    DEFAULT_PROCESS_TIMEOUT_MS
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProcessResponse {
    Stdout,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessExit {
    pub id: String,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub truncated: bool,
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub api_version: u32,
    pub invocation: Invocation,
    pub document: Option<Document>,
    pub settings: serde_json::Value,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Document {
    pub version: u64,
    pub path: Option<String>,
    pub text: String,
    pub selection: Selection,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Selection {
    pub start: u32,
    pub end: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Response {
    pub edits: Vec<TextEdit>,
    pub panels: Vec<PanelUpdate>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub processes: Vec<ProcessRequest>,
    pub logs: Vec<String>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TextEdit {
    pub start: u32,
    pub end: u32,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PanelContribution {
    pub id: String,
    pub title: String,
    pub placement: PanelPlacement,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PanelPlacement {
    Dock,
    Editor,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Invocation {
    Command {
        id: String,
    },
    OpenPanel {
        id: String,
    },
    Refresh {
        panel: String,
        revision: u64,
        state: serde_json::Value,
        inputs: std::collections::BTreeMap<String, InputValue>,
    },
    ProcessExit {
        panel: String,
        revision: u64,
        state: serde_json::Value,
        inputs: std::collections::BTreeMap<String, InputValue>,
        process: ProcessExit,
    },
    UiEvent {
        panel: String,
        revision: u64,
        state: serde_json::Value,
        inputs: std::collections::BTreeMap<String, InputValue>,
        event: UiEvent,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum InputValue {
    Text(String),
    Bool(bool),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiEvent {
    pub widget: String,
    pub kind: UiEventKind,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum UiEventKind {
    Click,
    Submit,
    Change { value: InputValue },
    ChartSelect { series: u32, index: u32 },
    ChartZoom { start: f64, end: f64 },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PanelUpdate {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_ms: Option<u64>,
    pub id: String,
    pub root: Widget,
    pub state: serde_json::Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Widget {
    Row {
        id: String,
        children: Vec<Widget>,
        gap: u16,
    },
    Column {
        id: String,
        children: Vec<Widget>,
        gap: u16,
    },
    Scroll {
        id: String,
        child: Box<Widget>,
        height: u16,
    },
    Space {
        id: String,
        size: u16,
    },
    Label {
        id: String,
        text: String,
    },
    Markdown {
        id: String,
        text: String,
    },
    Button {
        id: String,
        text: String,
        disabled: bool,
    },
    TextInput {
        id: String,
        label: String,
        #[serde(default)]
        label_inline: bool,
        value: String,
        placeholder: String,
        disabled: bool,
    },
    Checkbox {
        id: String,
        label: String,
        value: bool,
        disabled: bool,
        #[serde(default)]
        color: Option<u8>,
    },
    Select {
        id: String,
        label: String,
        #[serde(default)]
        label_inline: bool,
        value: String,
        options: Vec<SelectOption>,
        disabled: bool,
    },
    Progress {
        id: String,
        label: String,
        value: f64,
    },
    Table {
        id: String,
        columns: Vec<String>,
        rows: Vec<Vec<String>>,
    },
    Chart {
        id: String,
        title: String,
        chart: ChartSpec,
        height: u16,
        #[serde(default = "show_legend")]
        legend: bool,
    },
}

fn show_legend() -> bool {
    true
}

impl Widget {
    pub fn id(&self) -> &str {
        match self {
            Self::Row { id, .. }
            | Self::Column { id, .. }
            | Self::Scroll { id, .. }
            | Self::Space { id, .. }
            | Self::Label { id, .. }
            | Self::Markdown { id, .. }
            | Self::Button { id, .. }
            | Self::TextInput { id, .. }
            | Self::Checkbox { id, .. }
            | Self::Select { id, .. }
            | Self::Progress { id, .. }
            | Self::Table { id, .. }
            | Self::Chart { id, .. } => id,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectOption {
    pub value: String,
    pub label: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum ChartSpec {
    Line {
        series: Vec<ChartSeries>,
        x_label: String,
        y_label: String,
    },
    Scatter {
        series: Vec<ChartSeries>,
        x_label: String,
        y_label: String,
    },
    Bar {
        labels: Vec<String>,
        series: Vec<BarSeries>,
        x_label: String,
        y_label: String,
    },
    Histogram {
        values: Vec<f64>,
        bins: u16,
        x_label: String,
        y_label: String,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChartSeries {
    /// Palette index 0..11; omitted uses this series' position.
    #[serde(default)]
    pub color: Option<u8>,
    pub name: String,
    pub points: Vec<ChartPoint>,
}
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChartPoint {
    pub x: f64,
    pub y: f64,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BarSeries {
    /// Palette index 0..11; omitted uses this series' position.
    #[serde(default)]
    pub color: Option<u8>,
    pub name: String,
    pub values: Vec<f64>,
}
