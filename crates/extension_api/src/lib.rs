//! Language-neutral NextCode extension protocol. All ranges use UTF-8 byte offsets.

use serde::{Deserialize, Serialize};

/// Per-panel limits the host enforces on every update (the same for every API version).
pub mod limits {
    pub const WIDGETS: usize = 256;
    pub const DEPTH: usize = 12;
    /// Ids, labels, cells, series names and point labels together.
    pub const TEXT_BYTES: usize = 256 * 1024;
    pub const CHART_VALUES: usize = 16_384;
    pub const SERIES: usize = 12;
    pub const STATE_BYTES: usize = 64 * 1024;
    pub const TEXT_INPUT_BYTES: usize = 4096;
    pub const OPTIONS: usize = 32;
    pub const TABLE_COLUMNS: usize = 16;
    pub const TABLE_CELLS: usize = 4096;
    pub const CHART_CONTROLS: usize = 4;
    pub const PALETTE: u8 = 12;
}

/// The newest protocol version. Hosts also accept packages written for 2 and 3.
pub const API_VERSION: u32 = 4;
/// The oldest protocol version a host accepts.
pub const MIN_API_VERSION: u32 = 2;
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
/// A process whose stdout is the whole response (`"response": "stdout"`) keeps up to the response limit.
pub const MAX_PROCESS_RESPONSE_BYTES: usize = MAX_RESPONSE_BYTES;
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
    Change {
        value: InputValue,
    },
    ChartSelect {
        series: u32,
        index: u32,
    },
    ChartZoom {
        start: f64,
        end: f64,
    },
    /// A selectable table's row was clicked or activated (API 4).
    SelectRow {
        index: u32,
    },
    /// A checkable table's row was ticked or unticked (API 4).
    CheckRow {
        index: u32,
        value: bool,
    },
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

fn is_false(value: &bool) -> bool {
    !*value
}
fn default_gap() -> u16 {
    8
}
fn default_grid_min() -> u16 {
    320
}
fn default_grid_gap() -> u16 {
    12
}
fn default_chart_height() -> u16 {
    220
}
fn show_legend() -> bool {
    true
}
fn is_true(value: &bool) -> bool {
    *value
}

/// How a row places children of different heights (API 4). Omitted: `end` (fields line up with buttons), or
/// `start` when the row holds a growing column (a sidebar beside its content).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Align {
    Start,
    Center,
    End,
}

/// The widget tree of a panel. Optional fields may be omitted from the JSON; the Rust builders below
/// (`Widget::row`, `Widget::chart`, …) fill them in.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Widget {
    Row {
        id: String,
        children: Vec<Widget>,
        #[serde(default = "default_gap")]
        gap: u16,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        align: Option<Align>,
    },
    Column {
        id: String,
        children: Vec<Widget>,
        #[serde(default = "default_gap")]
        gap: u16,
        /// A fixed width in px (120..=1600, API 4).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        width: Option<u16>,
        /// Take the width a row has left (API 4); its contents fill it.
        #[serde(default, skip_serializing_if = "is_false")]
        grow: bool,
    },
    /// As many equal columns as fit, each at least `min_width` px; children fill them in order (API 4).
    Grid {
        id: String,
        children: Vec<Widget>,
        #[serde(default = "default_grid_min")]
        min_width: u16,
        #[serde(default = "default_grid_gap")]
        gap: u16,
        /// A fixed number of columns (1..=12) instead of as many as fit.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        columns: Option<u8>,
    },
    /// A titled group that folds away. Clicking its header sends `change` with the new `open` (API 4).
    Section {
        id: String,
        title: String,
        #[serde(default = "show_legend", skip_serializing_if = "is_true")]
        open: bool,
        children: Vec<Widget>,
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
    /// A headline number with its name and an optional line under it (API 4).
    Stat {
        id: String,
        label: String,
        value: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        detail: String,
        /// A palette index 0..11 drawn as a bar beside the value.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        color: Option<u8>,
    },
    Button {
        id: String,
        text: String,
        #[serde(default)]
        disabled: bool,
    },
    TextInput {
        id: String,
        #[serde(default)]
        label: String,
        #[serde(default)]
        label_inline: bool,
        #[serde(default)]
        value: String,
        #[serde(default)]
        placeholder: String,
        #[serde(default)]
        disabled: bool,
    },
    Checkbox {
        id: String,
        #[serde(default)]
        label: String,
        value: bool,
        #[serde(default)]
        disabled: bool,
        #[serde(default)]
        color: Option<u8>,
    },
    Select {
        id: String,
        #[serde(default)]
        label: String,
        #[serde(default)]
        label_inline: bool,
        value: String,
        options: Vec<SelectOption>,
        #[serde(default)]
        disabled: bool,
    },
    /// A tab strip; picking a tab sends `change` with its value, as a select does (API 4). The tabs hold no
    /// children: the extension draws the page under them.
    Tabs {
        id: String,
        value: String,
        options: Vec<SelectOption>,
        #[serde(default, skip_serializing_if = "is_false")]
        disabled: bool,
    },
    Progress {
        id: String,
        #[serde(default)]
        label: String,
        value: f64,
    },
    Table {
        id: String,
        columns: Vec<String>,
        rows: Vec<Vec<String>>,
        /// px per column; 0 shares what is left (API 4). Empty: columns share the width by content.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        widths: Vec<u16>,
        /// Clicking a row (or Enter on it) sends `select-row` (API 4).
        #[serde(default, skip_serializing_if = "is_false")]
        selectable: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        selected: Option<u32>,
        /// 120..=1200 px; omitted: 320.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_height: Option<u16>,
        /// A checkbox at the start of each row; ticking one sends `check-row` (API 4).
        #[serde(default, skip_serializing_if = "is_false")]
        check: bool,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        checked: Vec<u32>,
        /// Rows whose box can't be ticked.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        disabled: Vec<u32>,
        /// One palette index (or null) per row, drawn as a swatch: the table doubles as a chart legend.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        swatches: Vec<Option<u8>>,
    },
    Chart {
        id: String,
        #[serde(default)]
        title: String,
        chart: ChartSpec,
        #[serde(default = "default_chart_height")]
        height: u16,
        #[serde(default = "show_legend")]
        legend: bool,
        /// Width in px (180..=1600); omitted fills the available width (at least 180 px).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        width: Option<u16>,
        /// Up to four small selects, buttons or checkboxes in the chart's title row (API 4).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        controls: Vec<Widget>,
        /// Charts with the same group share their hover line and zoom (API 4).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sync: Option<String>,
    },
}

impl Widget {
    pub fn id(&self) -> &str {
        match self {
            Self::Row { id, .. }
            | Self::Column { id, .. }
            | Self::Grid { id, .. }
            | Self::Section { id, .. }
            | Self::Scroll { id, .. }
            | Self::Space { id, .. }
            | Self::Label { id, .. }
            | Self::Markdown { id, .. }
            | Self::Stat { id, .. }
            | Self::Button { id, .. }
            | Self::TextInput { id, .. }
            | Self::Checkbox { id, .. }
            | Self::Select { id, .. }
            | Self::Tabs { id, .. }
            | Self::Progress { id, .. }
            | Self::Table { id, .. }
            | Self::Chart { id, .. } => id,
        }
    }

    /// The widgets drawn inside this one: a container's children, a scroll's child, a chart's controls.
    pub fn children(&self) -> &[Widget] {
        match self {
            Self::Row { children, .. }
            | Self::Column { children, .. }
            | Self::Grid { children, .. }
            | Self::Section { children, .. } => children,
            Self::Scroll { child, .. } => std::slice::from_ref(child.as_ref()),
            Self::Chart { controls, .. } => controls,
            _ => &[],
        }
    }

    // --- builders: every optional field at its default; chain the setters below for the rest.

    pub fn row(id: impl Into<String>, children: impl IntoIterator<Item = Widget>) -> Self {
        Self::Row {
            id: id.into(),
            children: children.into_iter().collect(),
            gap: default_gap(),
            align: None,
        }
    }
    pub fn column(id: impl Into<String>, children: impl IntoIterator<Item = Widget>) -> Self {
        Self::Column {
            id: id.into(),
            children: children.into_iter().collect(),
            gap: default_gap(),
            width: None,
            grow: false,
        }
    }
    pub fn grid(
        id: impl Into<String>,
        min_width: u16,
        children: impl IntoIterator<Item = Widget>,
    ) -> Self {
        Self::Grid {
            id: id.into(),
            children: children.into_iter().collect(),
            min_width,
            gap: default_grid_gap(),
            columns: None,
        }
    }
    pub fn section(
        id: impl Into<String>,
        title: impl Into<String>,
        open: bool,
        children: impl IntoIterator<Item = Widget>,
    ) -> Self {
        Self::Section {
            id: id.into(),
            title: title.into(),
            open,
            children: children.into_iter().collect(),
        }
    }
    pub fn scroll(id: impl Into<String>, height: u16, child: Widget) -> Self {
        Self::Scroll {
            id: id.into(),
            child: Box::new(child),
            height,
        }
    }
    pub fn space(id: impl Into<String>, size: u16) -> Self {
        Self::Space {
            id: id.into(),
            size,
        }
    }
    pub fn label(id: impl Into<String>, text: impl Into<String>) -> Self {
        Self::Label {
            id: id.into(),
            text: text.into(),
        }
    }
    pub fn markdown(id: impl Into<String>, text: impl Into<String>) -> Self {
        Self::Markdown {
            id: id.into(),
            text: text.into(),
        }
    }
    pub fn stat(id: impl Into<String>, label: impl Into<String>, value: impl Into<String>) -> Self {
        Self::Stat {
            id: id.into(),
            label: label.into(),
            value: value.into(),
            detail: String::new(),
            color: None,
        }
    }
    pub fn button(id: impl Into<String>, text: impl Into<String>) -> Self {
        Self::Button {
            id: id.into(),
            text: text.into(),
            disabled: false,
        }
    }
    pub fn text_input(
        id: impl Into<String>,
        label: impl Into<String>,
        value: impl Into<String>,
    ) -> Self {
        Self::TextInput {
            id: id.into(),
            label: label.into(),
            label_inline: false,
            value: value.into(),
            placeholder: String::new(),
            disabled: false,
        }
    }
    pub fn checkbox(id: impl Into<String>, label: impl Into<String>, value: bool) -> Self {
        Self::Checkbox {
            id: id.into(),
            label: label.into(),
            value,
            disabled: false,
            color: None,
        }
    }
    pub fn select(
        id: impl Into<String>,
        label: impl Into<String>,
        value: impl Into<String>,
        options: impl IntoIterator<Item = SelectOption>,
    ) -> Self {
        Self::Select {
            id: id.into(),
            label: label.into(),
            label_inline: false,
            value: value.into(),
            options: options.into_iter().collect(),
            disabled: false,
        }
    }
    pub fn tabs(
        id: impl Into<String>,
        value: impl Into<String>,
        options: impl IntoIterator<Item = SelectOption>,
    ) -> Self {
        Self::Tabs {
            id: id.into(),
            value: value.into(),
            options: options.into_iter().collect(),
            disabled: false,
        }
    }
    pub fn progress(id: impl Into<String>, label: impl Into<String>, value: f64) -> Self {
        Self::Progress {
            id: id.into(),
            label: label.into(),
            value,
        }
    }
    pub fn table(id: impl Into<String>, columns: Vec<String>, rows: Vec<Vec<String>>) -> Self {
        Self::Table {
            id: id.into(),
            columns,
            rows,
            widths: Vec::new(),
            selectable: false,
            selected: None,
            max_height: None,
            check: false,
            checked: Vec::new(),
            disabled: Vec::new(),
            swatches: Vec::new(),
        }
    }
    pub fn chart(id: impl Into<String>, title: impl Into<String>, chart: ChartSpec) -> Self {
        Self::Chart {
            id: id.into(),
            title: title.into(),
            chart,
            height: default_chart_height(),
            legend: true,
            width: None,
            controls: Vec::new(),
            sync: None,
        }
    }

    // --- setters: each applies to the widgets that have that field and leaves the others unchanged.

    pub fn gap(mut self, value: u16) -> Self {
        if let Self::Row { gap, .. } | Self::Column { gap, .. } | Self::Grid { gap, .. } = &mut self
        {
            *gap = value;
        }
        self
    }
    /// A grid's fixed column count.
    pub fn columns(mut self, value: u8) -> Self {
        if let Self::Grid { columns, .. } = &mut self {
            *columns = Some(value);
        }
        self
    }
    pub fn align(mut self, value: Align) -> Self {
        if let Self::Row { align, .. } = &mut self {
            *align = Some(value);
        }
        self
    }
    /// A column's fixed width, or a chart's.
    pub fn width(mut self, value: u16) -> Self {
        match &mut self {
            Self::Column { width, .. } | Self::Chart { width, .. } => *width = Some(value),
            _ => {}
        }
        self
    }
    pub fn grow(mut self) -> Self {
        if let Self::Column { grow, .. } = &mut self {
            *grow = true;
        }
        self
    }
    /// A chart's height in px.
    pub fn height(mut self, value: u16) -> Self {
        if let Self::Chart { height, .. } = &mut self {
            *height = value;
        }
        self
    }
    pub fn disabled(mut self, value: bool) -> Self {
        match &mut self {
            Self::Button { disabled, .. }
            | Self::TextInput { disabled, .. }
            | Self::Checkbox { disabled, .. }
            | Self::Select { disabled, .. }
            | Self::Tabs { disabled, .. } => *disabled = value,
            _ => {}
        }
        self
    }
    /// A palette index 0..11: a checkbox's swatch or a stat's bar.
    pub fn color(mut self, value: u8) -> Self {
        match &mut self {
            Self::Checkbox { color, .. } | Self::Stat { color, .. } => *color = Some(value),
            _ => {}
        }
        self
    }
    pub fn detail(mut self, value: impl Into<String>) -> Self {
        if let Self::Stat { detail, .. } = &mut self {
            *detail = value.into();
        }
        self
    }
    pub fn placeholder(mut self, value: impl Into<String>) -> Self {
        if let Self::TextInput { placeholder, .. } = &mut self {
            *placeholder = value.into();
        }
        self
    }
    /// `Label: value` inside a text input or select's field instead of a label above it.
    pub fn label_inline(mut self) -> Self {
        match &mut self {
            Self::TextInput { label_inline, .. } | Self::Select { label_inline, .. } => {
                *label_inline = true
            }
            _ => {}
        }
        self
    }
    pub fn legend(mut self, value: bool) -> Self {
        if let Self::Chart { legend, .. } = &mut self {
            *legend = value;
        }
        self
    }
    pub fn controls(mut self, value: impl IntoIterator<Item = Widget>) -> Self {
        if let Self::Chart { controls, .. } = &mut self {
            *controls = value.into_iter().collect();
        }
        self
    }
    pub fn sync(mut self, group: impl Into<String>) -> Self {
        if let Self::Chart { sync, .. } = &mut self {
            *sync = Some(group.into());
        }
        self
    }
    /// A table's column widths in px (0 shares the rest).
    pub fn widths(mut self, value: impl IntoIterator<Item = u16>) -> Self {
        if let Self::Table { widths, .. } = &mut self {
            *widths = value.into_iter().collect();
        }
        self
    }
    /// A table whose rows send `select-row`, with `selected` highlighted.
    pub fn selectable(mut self, value: Option<u32>) -> Self {
        if let Self::Table {
            selectable,
            selected,
            ..
        } = &mut self
        {
            *selectable = true;
            *selected = value;
        }
        self
    }
    pub fn max_height(mut self, value: u16) -> Self {
        if let Self::Table { max_height, .. } = &mut self {
            *max_height = Some(value);
        }
        self
    }
    /// A table with a checkbox a row: the rows ticked, those that can't be, and a swatch a row (or none).
    pub fn checks(
        mut self,
        ticked: impl IntoIterator<Item = u32>,
        locked: impl IntoIterator<Item = u32>,
        colors: impl IntoIterator<Item = Option<u8>>,
    ) -> Self {
        if let Self::Table {
            check,
            checked,
            disabled,
            swatches,
            ..
        } = &mut self
        {
            *check = true;
            *checked = ticked.into_iter().collect();
            *disabled = locked.into_iter().collect();
            *swatches = colors.into_iter().collect();
        }
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectOption {
    pub value: String,
    pub label: String,
}
impl SelectOption {
    pub fn new(value: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            label: label.into(),
        }
    }
}

/// An axis scale (API 4). `log` takes positive values only.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Scale {
    #[default]
    Linear,
    Log,
}

/// How an axis writes its numbers (API 4): `date` is Unix seconds, `duration` seconds, `percent` a fraction.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AxisFormat {
    #[default]
    Number,
    Date,
    Duration,
    Percent,
}

fn is_default<T: Default + PartialEq>(value: &T) -> bool {
    *value == T::default()
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum ChartSpec {
    Line {
        series: Vec<ChartSeries>,
        #[serde(default)]
        x_label: String,
        #[serde(default)]
        y_label: String,
        #[serde(default, skip_serializing_if = "is_default")]
        x_scale: Scale,
        #[serde(default, skip_serializing_if = "is_default")]
        y_scale: Scale,
        #[serde(default, skip_serializing_if = "is_default")]
        x_format: AxisFormat,
        #[serde(default, skip_serializing_if = "is_default")]
        y_format: AxisFormat,
    },
    Scatter {
        series: Vec<ChartSeries>,
        #[serde(default)]
        x_label: String,
        #[serde(default)]
        y_label: String,
        #[serde(default, skip_serializing_if = "is_default")]
        x_scale: Scale,
        #[serde(default, skip_serializing_if = "is_default")]
        y_scale: Scale,
        #[serde(default, skip_serializing_if = "is_default")]
        x_format: AxisFormat,
        #[serde(default, skip_serializing_if = "is_default")]
        y_format: AxisFormat,
    },
    Bar {
        labels: Vec<String>,
        series: Vec<BarSeries>,
        #[serde(default)]
        x_label: String,
        #[serde(default)]
        y_label: String,
        #[serde(default, skip_serializing_if = "is_default")]
        y_format: AxisFormat,
    },
    Histogram {
        values: Vec<f64>,
        bins: u16,
        #[serde(default)]
        x_label: String,
        #[serde(default)]
        y_label: String,
    },
}

impl ChartSpec {
    pub fn line(series: impl IntoIterator<Item = ChartSeries>) -> Self {
        Self::Line {
            series: series.into_iter().collect(),
            x_label: String::new(),
            y_label: String::new(),
            x_scale: Scale::Linear,
            y_scale: Scale::Linear,
            x_format: AxisFormat::Number,
            y_format: AxisFormat::Number,
        }
    }
    pub fn scatter(series: impl IntoIterator<Item = ChartSeries>) -> Self {
        match Self::line(series) {
            Self::Line {
                series,
                x_label,
                y_label,
                x_scale,
                y_scale,
                x_format,
                y_format,
            } => Self::Scatter {
                series,
                x_label,
                y_label,
                x_scale,
                y_scale,
                x_format,
                y_format,
            },
            _ => unreachable!(),
        }
    }
    pub fn bar(labels: Vec<String>, series: impl IntoIterator<Item = BarSeries>) -> Self {
        Self::Bar {
            labels,
            series: series.into_iter().collect(),
            x_label: String::new(),
            y_label: String::new(),
            y_format: AxisFormat::Number,
        }
    }
    pub fn histogram(values: Vec<f64>, bins: u16) -> Self {
        Self::Histogram {
            values,
            bins,
            x_label: String::new(),
            y_label: String::new(),
        }
    }
    pub fn labels(mut self, x: impl Into<String>, y: impl Into<String>) -> Self {
        match &mut self {
            Self::Line {
                x_label, y_label, ..
            }
            | Self::Scatter {
                x_label, y_label, ..
            }
            | Self::Bar {
                x_label, y_label, ..
            }
            | Self::Histogram {
                x_label, y_label, ..
            } => {
                *x_label = x.into();
                *y_label = y.into();
            }
        }
        self
    }
    pub fn scales(mut self, x: Scale, y: Scale) -> Self {
        if let Self::Line {
            x_scale, y_scale, ..
        }
        | Self::Scatter {
            x_scale, y_scale, ..
        } = &mut self
        {
            *x_scale = x;
            *y_scale = y;
        }
        self
    }
    pub fn formats(mut self, x: AxisFormat, y: AxisFormat) -> Self {
        match &mut self {
            Self::Line {
                x_format, y_format, ..
            }
            | Self::Scatter {
                x_format, y_format, ..
            } => {
                *x_format = x;
                *y_format = y;
            }
            Self::Bar { y_format, .. } => *y_format = y,
            Self::Histogram { .. } => {}
        }
        self
    }
}

/// How a line or scatter series is drawn (API 4). Omitted: `line` in a line chart, `points` in a scatter chart.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SeriesStyle {
    Line,
    /// Holds each value until the next point's x.
    Step,
    Dashed,
    Points,
    /// A line with the area under it tinted.
    Area,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChartSeries {
    /// Palette index 0..11; omitted uses this series' position.
    #[serde(default)]
    pub color: Option<u8>,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub style: Option<SeriesStyle>,
    pub points: Vec<ChartPoint>,
}
impl ChartSeries {
    pub fn new(name: impl Into<String>, points: impl IntoIterator<Item = ChartPoint>) -> Self {
        Self {
            color: None,
            name: name.into(),
            style: None,
            points: points.into_iter().collect(),
        }
    }
    pub fn color(mut self, color: u8) -> Self {
        self.color = Some(color);
        self
    }
    pub fn style(mut self, style: SeriesStyle) -> Self {
        self.style = Some(style);
        self
    }
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChartPoint {
    pub x: f64,
    pub y: f64,
    /// Shown on hover instead of the point's index (API 4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}
impl ChartPoint {
    pub fn new(x: f64, y: f64) -> Self {
        Self { x, y, label: None }
    }
    pub fn labelled(x: f64, y: f64, label: impl Into<String>) -> Self {
        Self {
            x,
            y,
            label: Some(label.into()),
        }
    }
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
impl BarSeries {
    pub fn new(name: impl Into<String>, values: Vec<f64>) -> Self {
        Self {
            color: None,
            name: name.into(),
            values,
        }
    }
    pub fn color(mut self, color: u8) -> Self {
        self.color = Some(color);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn optional_fields_default_and_api3_json_still_parses() {
        let api3: Widget = serde_json::from_value(serde_json::json!({
            "type": "chart", "id": "c", "title": "t", "height": 140, "legend": false,
            "chart": {"kind": "line", "x_label": "", "y_label": "",
                      "series": [{"name": "a", "color": 2, "points": [{"x": 1, "y": 2}]}]}
        }))
        .unwrap();
        assert_eq!(
            api3,
            Widget::chart(
                "c",
                "t",
                ChartSpec::line([ChartSeries::new("a", [ChartPoint::new(1., 2.)]).color(2)])
            )
            .height(140)
            .legend(false)
        );
        let short: Widget = serde_json::from_value(serde_json::json!({
            "type": "row", "id": "r", "children": [{"type": "button", "id": "b", "text": "Go"}]
        }))
        .unwrap();
        assert_eq!(short, Widget::row("r", [Widget::button("b", "Go")]));
        // Defaults are left out when written.
        assert_eq!(
            serde_json::to_value(Widget::column("c", [])).unwrap(),
            serde_json::json!({"type": "column", "id": "c", "children": [], "gap": 8})
        );
    }

    #[test]
    fn api4_widgets_and_events_round_trip() {
        let table = Widget::table("t", vec!["a".into()], vec![vec!["1".into()]])
            .widths([0])
            .selectable(Some(0))
            .checks([0], [], [Some(3)]);
        let json = serde_json::to_value(&table).unwrap();
        assert_eq!(json["check"], true);
        assert_eq!(json["swatches"], serde_json::json!([3]));
        assert_eq!(serde_json::from_value::<Widget>(json).unwrap(), table);
        let event: UiEventKind = serde_json::from_value(
            serde_json::json!({"kind": "check-row", "index": 2, "value": true}),
        )
        .unwrap();
        assert_eq!(
            event,
            UiEventKind::CheckRow {
                index: 2,
                value: true
            }
        );
        let tabs = Widget::tabs(
            "nav",
            "b",
            [SelectOption::new("a", "A"), SelectOption::new("b", "B")],
        );
        assert_eq!(
            serde_json::from_value::<Widget>(serde_json::to_value(&tabs).unwrap()).unwrap(),
            tabs
        );
        assert_eq!(
            Widget::section("s", "S", true, [Widget::label("l", "x")])
                .children()
                .len(),
            1
        );
    }
}
