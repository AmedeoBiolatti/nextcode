//! Language-neutral NextCode extension protocol. All ranges use UTF-8 byte offsets.

use serde::{Deserialize, Serialize};

pub const API_VERSION: u32 = 2;
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
        value: String,
        placeholder: String,
        disabled: bool,
    },
    Checkbox {
        id: String,
        label: String,
        value: bool,
        disabled: bool,
    },
    Select {
        id: String,
        label: String,
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
    },
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
    pub name: String,
    pub values: Vec<f64>,
}
