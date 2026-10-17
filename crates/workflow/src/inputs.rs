//! Workflow inputs: the parameters a run asks for, declared in the file's
//! top-level `inputs` (`string`, `number`, `boolean` or `choice`, each
//! with a default), and `${inputs.<id>}` in steps' `run`, `check`, `cwd`,
//! `env` values, artifact paths and `project`. Values are fixed when a run
//! starts: [`resolve`] checks given values (filling in defaults),
//! [`prefill`] picks what a form starts with, and
//! [`Workflow::with_inputs`](crate::Workflow::with_inputs) puts them into
//! the steps.

use serde_json::{Map, Value, json};

/// The keys an input may have (its id is its key in `inputs`).
pub const INPUT_KEYS: &[&str] = &[
    "label",
    "type",
    "description",
    "default",
    "required",
    "placeholder",
    "options",
    "min",
    "max",
    "integer",
];

/// One declared input.
#[derive(Debug, Clone, PartialEq)]
pub struct Input {
    /// Unique among the inputs; `${inputs.<id>}` names it.
    pub id: String,
    pub label: Option<String>,
    /// Helper text under the field.
    pub description: Option<String>,
    pub kind: InputKind,
    /// What a run gets when it is not given a value, and what the form
    /// starts with (declared, else empty, `false` or the first option).
    pub default: InputValue,
    /// A string must not be empty, a number must be given.
    pub required: bool,
    /// Shown in an empty textbox.
    pub placeholder: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum InputKind {
    /// A textbox.
    String,
    /// A textbox for a number, within `min` and `max`, whole with
    /// `integer`.
    Number {
        min: Option<f64>,
        max: Option<f64>,
        integer: bool,
    },
    /// A checkbox.
    Boolean,
    /// A dropdown of these.
    Choice(Vec<Choice>),
}

/// One option of a `choice` input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    /// What `${inputs.<id>}` expands to.
    pub value: String,
    /// What the dropdown shows (default: the value).
    pub label: Option<String>,
}

impl Choice {
    pub fn label(&self) -> &str {
        self.label.as_deref().unwrap_or(&self.value)
    }
}

/// An input's value for a run.
#[derive(Debug, Clone, PartialEq)]
pub enum InputValue {
    /// A string, or a choice's value.
    Text(String),
    /// `None`: left empty (allowed when not `required`).
    Number(Option<f64>),
    Bool(bool),
}

/// `4` for whole numbers, else the shortest form that reads back (`0.5`).
fn number_text(number: f64) -> String {
    if number.fract() == 0.0 && number.abs() < 1e15 {
        (number as i64).to_string()
    } else {
        number.to_string()
    }
}

impl InputValue {
    /// As `${inputs.<id>}` expands it: the text, a number in its canonical
    /// form (empty when left empty), `true` or `false`.
    pub fn text(&self) -> String {
        match self {
            Self::Text(text) => text.clone(),
            Self::Number(number) => number.map(number_text).unwrap_or_default(),
            Self::Bool(flag) => flag.to_string(),
        }
    }

    /// A string, a number (`null` when empty) or a boolean.
    pub fn to_json(&self) -> Value {
        match self {
            Self::Text(text) => json!(text),
            Self::Number(None) => Value::Null,
            Self::Number(Some(number)) if number.fract() == 0.0 && number.abs() < 1e15 => {
                json!(*number as i64)
            }
            Self::Number(Some(number)) => json!(number),
            Self::Bool(flag) => json!(flag),
        }
    }
}

impl Input {
    /// The label, else the id.
    pub fn label(&self) -> &str {
        self.label.as_deref().unwrap_or(&self.id)
    }

    pub fn type_name(&self) -> &'static str {
        match self.kind {
            InputKind::String => "string",
            InputKind::Number { .. } => "number",
            InputKind::Boolean => "boolean",
            InputKind::Choice(_) => "choice",
        }
    }

    /// A value given as text (a textbox; a boolean as `true`/`false`, a
    /// choice by its value). Unchecked: see [`Input::check`].
    pub fn parse_text(&self, text: &str) -> Result<InputValue, String> {
        match &self.kind {
            InputKind::String | InputKind::Choice(_) => Ok(InputValue::Text(text.to_owned())),
            InputKind::Number { .. } => {
                let text = text.trim();
                if text.is_empty() {
                    return Ok(InputValue::Number(None));
                }
                match text.parse::<f64>() {
                    Ok(number) if number.is_finite() => Ok(InputValue::Number(Some(number))),
                    _ => Err(format!("`{text}` is not a number")),
                }
            }
            InputKind::Boolean => match text.trim() {
                "true" => Ok(InputValue::Bool(true)),
                "false" => Ok(InputValue::Bool(false)),
                other => Err(format!("`{other}` is not true or false")),
            },
        }
    }

    /// A value given as JSON: of the input's type (`null` leaves a number
    /// empty), or a string [`Input::parse_text`] reads for a number or a
    /// boolean. Unchecked: see [`Input::check`].
    pub fn from_json(&self, value: &Value) -> Result<InputValue, String> {
        match (&self.kind, value) {
            (InputKind::String | InputKind::Choice(_), Value::String(text)) => {
                Ok(InputValue::Text(text.clone()))
            }
            (InputKind::String | InputKind::Choice(_), _) => Err("must be a string".into()),
            (InputKind::Number { .. }, Value::Number(number)) => number
                .as_f64()
                .map(|number| InputValue::Number(Some(number)))
                .ok_or_else(|| "must be a number".into()),
            (InputKind::Number { .. }, Value::Null) => Ok(InputValue::Number(None)),
            (InputKind::Number { .. } | InputKind::Boolean, Value::String(text)) => {
                self.parse_text(text)
            }
            (InputKind::Number { .. }, _) => Err("must be a number".into()),
            (InputKind::Boolean, Value::Bool(flag)) => Ok(InputValue::Bool(*flag)),
            (InputKind::Boolean, _) => Err("must be true or false".into()),
        }
    }

    /// Whether `value` is fine for this input: given when `required`,
    /// within `min`/`max`, whole with `integer`, one of the options, no
    /// `${` (values are put in before the step's own variables).
    pub fn check(&self, value: &InputValue) -> Result<(), String> {
        match (&self.kind, value) {
            (InputKind::String, InputValue::Text(text)) => {
                if self.required && text.trim().is_empty() {
                    return Err("required".into());
                }
                no_variables(text)
            }
            (InputKind::Choice(options), InputValue::Text(text)) => {
                if options.iter().any(|option| option.value == *text) {
                    Ok(())
                } else {
                    Err(format!(
                        "`{text}` is not one of {}",
                        options
                            .iter()
                            .map(|option| option.value.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ))
                }
            }
            (InputKind::Number { .. }, InputValue::Number(None)) => {
                if self.required {
                    Err("required".into())
                } else {
                    Ok(())
                }
            }
            (InputKind::Number { min, max, integer }, InputValue::Number(Some(number))) => {
                if *integer && number.fract() != 0.0 {
                    return Err("must be a whole number".into());
                }
                if let Some(min) = min
                    && number < min
                {
                    return Err(format!("must be ≥ {}", number_text(*min)));
                }
                if let Some(max) = max
                    && number > max
                {
                    return Err(format!("must be ≤ {}", number_text(*max)));
                }
                Ok(())
            }
            (InputKind::Boolean, InputValue::Bool(_)) => Ok(()),
            _ => Err(format!("must be a {}", self.type_name())),
        }
    }

    /// [`Input::from_json`], then [`Input::check`].
    pub fn accept_json(&self, value: &Value) -> Result<InputValue, String> {
        let value = self.from_json(value)?;
        self.check(&value)?;
        Ok(value)
    }
}

fn no_variables(text: &str) -> Result<(), String> {
    if text.contains("${") {
        Err("must not contain `${`".into())
    } else {
        Ok(())
    }
}

/// A run's inputs, in the order they are declared.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct InputValues(Vec<(String, InputValue)>);

impl InputValues {
    pub fn get(&self, id: &str) -> Option<&InputValue> {
        self.0
            .iter()
            .find(|(known, _)| known == id)
            .map(|(_, value)| value)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &InputValue)> {
        self.0.iter().map(|(id, value)| (id.as_str(), value))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// `env=dev · branch=main`.
    pub fn summary(&self) -> String {
        self.0
            .iter()
            .map(|(id, value)| format!("{id}={}", value.text()))
            .collect::<Vec<_>>()
            .join(" · ")
    }

    /// `{"<id>": <value>, …}`.
    pub fn to_json(&self) -> Value {
        Value::Object(
            self.0
                .iter()
                .map(|(id, value)| (id.clone(), value.to_json()))
                .collect(),
        )
    }
}

impl FromIterator<(String, InputValue)> for InputValues {
    fn from_iter<T: IntoIterator<Item = (String, InputValue)>>(iter: T) -> Self {
        Self(iter.into_iter().collect())
    }
}

/// Every input's default.
pub fn defaults(inputs: &[Input]) -> InputValues {
    inputs
        .iter()
        .map(|input| (input.id.clone(), input.default.clone()))
        .collect()
}

/// A run's values from `given` (JSON by id), defaults for the others;
/// else every problem (`input `workers`: must be ≤ 64`), unknown ids and
/// missing required values included.
pub fn resolve(inputs: &[Input], given: &Map<String, Value>) -> Result<InputValues, Vec<String>> {
    let mut problems: Vec<String> = given
        .keys()
        .filter(|id| !inputs.iter().any(|input| input.id == **id))
        .map(|id| match inputs.len() {
            0 => format!("unknown input `{id}` (the workflow has no inputs)"),
            _ => format!(
                "unknown input `{id}` (inputs: {})",
                inputs
                    .iter()
                    .map(|input| input.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        })
        .collect();
    let mut values = Vec::with_capacity(inputs.len());
    for input in inputs {
        let value = match given.get(&input.id) {
            Some(value) => input.accept_json(value),
            None => input.check(&input.default).map(|()| input.default.clone()),
        };
        match value {
            Ok(value) => values.push((input.id.clone(), value)),
            Err(err) => problems.push(format!("input `{}`: {err}", input.id)),
        }
    }
    if problems.is_empty() {
        Ok(InputValues(values))
    } else {
        Err(problems)
    }
}

/// What a form starts with: each remembered value that still fits the
/// input (its type, options and bounds), else the default.
pub fn prefill(inputs: &[Input], remembered: Option<&Map<String, Value>>) -> InputValues {
    inputs
        .iter()
        .map(|input| {
            let value = remembered
                .and_then(|remembered| remembered.get(&input.id))
                .and_then(|value| input.accept_json(value).ok())
                .unwrap_or_else(|| input.default.clone());
            (input.id.clone(), value)
        })
        .collect()
}

const PREFIX: &str = "${inputs.";

/// One `${inputs.<id>}`: the whole variable and the id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputRef<'a> {
    pub text: &'a str,
    pub id: &'a str,
}

/// Every `${inputs.…}` variable in `text`, in order; an unclosed or empty
/// one is an error naming it.
pub fn input_refs(text: &str) -> Vec<Result<InputRef<'_>, String>> {
    let mut found = Vec::new();
    let mut from = 0;
    while let Some(start) = text[from..].find(PREFIX).map(|at| from + at) {
        let Some(end) = text[start..].find('}').map(|at| start + at) else {
            found.push(Err(format!("unclosed `{}`", &text[start..])));
            break;
        };
        from = end + 1;
        let whole = &text[start..=end];
        let id = &whole[PREFIX.len()..whole.len() - 1];
        found.push(if id.is_empty() {
            Err(format!("`{whole}` names no input"))
        } else {
            Ok(InputRef { text: whole, id })
        });
    }
    found
}

/// `text` with each `${inputs.<id>}` replaced by its value; unknown ones
/// stay (validation reports them).
pub fn substitute(text: &str, values: &InputValues) -> String {
    if !text.contains(PREFIX) {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = 0;
    for found in input_refs(text).into_iter().flatten() {
        let at = rest + text[rest..].find(found.text).unwrap_or(0);
        out.push_str(&text[rest..at]);
        match values.get(found.id) {
            Some(value) => out.push_str(&value.text()),
            None => out.push_str(found.text),
        }
        rest = at + found.text.len();
    }
    out.push_str(&text[rest..]);
    out
}

/// A problem in an input's declaration: its message and the key it is
/// about (`None`: the whole input).
pub(crate) type InputProblem = (String, Option<&'static str>);

/// The top-level `inputs` mapping, as `(id, declaration)` in file order:
/// the inputs that parse, every id (also of those that do not, so steps'
/// references to them are not reported too), and each problem with the
/// input's index. `id` problems are about the input's key.
pub(crate) fn parse(
    declared: Vec<(String, Value)>,
) -> (Vec<Input>, Vec<String>, Vec<(usize, InputProblem)>) {
    let mut inputs: Vec<Input> = Vec::new();
    let mut ids = Vec::new();
    let mut problems = Vec::new();
    for (index, (id, value)) in declared.into_iter().enumerate() {
        ids.push(id.clone());
        match parse_input(id.clone(), value) {
            Ok(input) => inputs.push(input),
            Err((message, key)) => {
                problems.push((index, (format!("input `{id}`: {message}"), key)));
            }
        }
    }
    (inputs, ids, problems)
}

fn string(map: &mut Map<String, Value>, key: &'static str) -> Result<Option<String>, InputProblem> {
    match map.remove(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text)),
        Some(_) => Err((format!("`{key}` must be a string"), Some(key))),
    }
}

fn number(map: &mut Map<String, Value>, key: &'static str) -> Result<Option<f64>, InputProblem> {
    match map.remove(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(number)) => Ok(number.as_f64()),
        Some(_) => Err((format!("`{key}` must be a number"), Some(key))),
    }
}

fn flag(map: &mut Map<String, Value>, key: &'static str) -> Result<Option<bool>, InputProblem> {
    match map.remove(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(flag)) => Ok(Some(flag)),
        Some(_) => Err((format!("`{key}` must be true or false"), Some(key))),
    }
}

fn choices(value: Value) -> Result<Vec<Choice>, String> {
    const SHAPES: &str =
        "`options` is a list of values or `{value: ..., label: ...}`";
    let Value::Array(entries) = value else {
        return Err(SHAPES.into());
    };
    let mut options: Vec<Choice> = Vec::with_capacity(entries.len());
    for entry in entries {
        let option = match entry {
            Value::String(value) => Choice { value, label: None },
            Value::Object(mut map) => {
                if let Some(unknown) = map
                    .keys()
                    .find(|key| !["value", "label"].contains(&key.as_str()))
                {
                    return Err(format!("unknown key `{unknown}` in an option ({SHAPES})"));
                }
                let label = match map.remove("label") {
                    None => None,
                    Some(Value::String(label)) => Some(label),
                    Some(_) => return Err("an option's `label` must be a string".into()),
                };
                match map.remove("value") {
                    Some(Value::String(value)) => Choice { value, label },
                    _ => return Err(format!("an option needs a string `value` ({SHAPES})")),
                }
            }
            _ => return Err(SHAPES.into()),
        };
        no_variables(&option.value).map_err(|err| format!("option `{}` {err}", option.value))?;
        if options.iter().any(|other| other.value == option.value) {
            return Err(format!("option `{}` is listed twice", option.value));
        }
        options.push(option);
    }
    if options.is_empty() {
        return Err("a `choice` input needs at least one option".into());
    }
    Ok(options)
}

fn parse_input(id: String, value: Value) -> Result<Input, InputProblem> {
    let mut map = match value {
        Value::Object(map) => map,
        // `branch:` alone: a string input with no settings.
        Value::Null => Map::new(),
        _ => return Err(("an input is a mapping of its settings".into(), None)),
    };
    // Located at the input, like a step's unknown keys.
    if let Some(unknown) = map.keys().find(|key| !INPUT_KEYS.contains(&key.as_str())) {
        return Err((
            format!("unknown key `{unknown}` (known: {})", INPUT_KEYS.join(", ")),
            None,
        ));
    }
    if id.trim().is_empty() {
        return Err(("`id` must not be empty".into(), Some("id")));
    }
    if id
        .chars()
        .any(|c| matches!(c, '/' | '.' | '{' | '}' | '$') || c.is_whitespace())
    {
        return Err((
            "an input id must not contain `/`, `.`, `{`, `}`, `$` or spaces".into(),
            Some("id"),
        ));
    }
    let label = string(&mut map, "label")?;
    let description = string(&mut map, "description")?;
    let placeholder = string(&mut map, "placeholder")?;
    let required = flag(&mut map, "required")?;
    let type_name = string(&mut map, "type")?.unwrap_or_else(|| "string".into());
    let min = number(&mut map, "min")?;
    let max = number(&mut map, "max")?;
    let integer = flag(&mut map, "integer")?;
    let mut options = map.remove("options");
    let has_options = options.is_some();
    let only = |key: &'static str, present: bool, types: &str| {
        if present {
            Err((format!("`{key}` applies to {types} inputs"), Some(key)))
        } else {
            Ok(())
        }
    };
    let kind = match type_name.as_str() {
        "string" => InputKind::String,
        "number" => {
            if let (Some(min), Some(max)) = (min, max)
                && min > max
            {
                return Err(("`min` is greater than `max`".into(), Some("min")));
            }
            InputKind::Number {
                min,
                max,
                integer: integer.unwrap_or(false),
            }
        }
        "boolean" => InputKind::Boolean,
        "choice" => InputKind::Choice(
            choices(
                options
                    .take()
                    .ok_or(("a `choice` input needs `options`".to_owned(), Some("type")))?,
            )
            .map_err(|err| (err, Some("options")))?,
        ),
        other => {
            return Err((
                format!("unknown type `{other}` (types: string, number, boolean, choice)"),
                Some("type"),
            ));
        }
    };
    let is_number = matches!(kind, InputKind::Number { .. });
    let textbox = matches!(kind, InputKind::String | InputKind::Number { .. });
    for (key, present) in [("min", min.is_some()), ("max", max.is_some())] {
        only(key, present && !is_number, "number")?;
    }
    only("integer", integer.is_some() && !is_number, "number")?;
    only(
        "options",
        has_options && !matches!(kind, InputKind::Choice(_)),
        "choice",
    )?;
    only(
        "required",
        required.is_some() && !textbox,
        "string and number",
    )?;
    only(
        "placeholder",
        placeholder.is_some() && !textbox,
        "string and number",
    )?;
    let mut input = Input {
        id,
        label,
        description,
        default: match &kind {
            InputKind::String => InputValue::Text(String::new()),
            InputKind::Number { .. } => InputValue::Number(None),
            InputKind::Boolean => InputValue::Bool(false),
            InputKind::Choice(options) => InputValue::Text(options[0].value.clone()),
        },
        kind,
        required: required.unwrap_or(false),
        placeholder,
    };
    if let Some(value) = map.remove("default") {
        let default = input
            .from_json(&value)
            .map_err(|err| (format!("`default` {err}"), Some("default")))?;
        match input.check(&default) {
            Ok(()) => input.default = default,
            Err(_) if input.required && default.text().trim().is_empty() => {
                return Err((
                    "`default` is empty but the input is `required` (drop one)".into(),
                    Some("default"),
                ));
            }
            Err(err) => return Err((format!("`default` {err}"), Some("default"))),
        }
    }
    Ok(input)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A declaration written with its `id` (the key in a file), as
    /// `(id, settings)`.
    fn declared(json: Value) -> (String, Value) {
        match json {
            Value::Object(mut map) => match map.remove("id") {
                Some(Value::String(id)) => (id, Value::Object(map)),
                _ => ("x".into(), Value::Object(map)),
            },
            other => ("x".into(), other),
        }
    }

    fn inputs(json: Value) -> Vec<Input> {
        let Value::Array(values) = json else {
            unreachable!()
        };
        let (inputs, _, problems) = parse(values.into_iter().map(declared).collect());
        assert!(problems.is_empty(), "{problems:?}");
        inputs
    }

    fn problem(json: Value) -> String {
        let (_, _, problems) = parse(vec![declared(json)]);
        problems
            .into_iter()
            .map(|(_, (message, _))| message)
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn every_type_parses_with_its_defaults() {
        let parsed = inputs(json!([
            { "id": "env", "label": "Environment", "type": "choice",
              "options": ["dev", { "value": "prod", "label": "Production" }], "default": "prod",
              "description": "Where to deploy" },
            { "id": "branch", "type": "string", "default": "main", "placeholder": "branch name",
              "required": true },
            { "id": "workers", "type": "number", "default": 4, "min": 1, "max": 64, "integer": true },
            { "id": "clean", "type": "boolean", "default": true },
            { "id": "note" },
            { "id": "ratio", "type": "number" },
            { "id": "flag", "type": "boolean" },
            { "id": "pick", "type": "choice", "options": ["a", "b"] },
        ]));
        assert_eq!(parsed[0].label(), "Environment");
        assert_eq!(parsed[0].default, InputValue::Text("prod".into()));
        let InputKind::Choice(options) = &parsed[0].kind else {
            unreachable!()
        };
        assert_eq!(options[0].label(), "dev");
        assert_eq!(options[1].label(), "Production");
        assert!(parsed[1].required);
        assert_eq!(parsed[1].placeholder.as_deref(), Some("branch name"));
        assert_eq!(
            parsed[2].kind,
            InputKind::Number {
                min: Some(1.),
                max: Some(64.),
                integer: true
            }
        );
        assert_eq!(parsed[3].default, InputValue::Bool(true));
        // Without `type`: a string; without `default`: empty, false, the
        // first option.
        assert_eq!(parsed[4].kind, InputKind::String);
        assert_eq!(parsed[4].label(), "note");
        assert_eq!(parsed[4].default, InputValue::Text(String::new()));
        assert_eq!(parsed[5].default, InputValue::Number(None));
        assert_eq!(parsed[6].default, InputValue::Bool(false));
        assert_eq!(parsed[7].default, InputValue::Text("a".into()));
        assert_eq!(
            defaults(&parsed).summary(),
            "env=prod · branch=main · workers=4 · clean=true · note= · ratio= · flag=false · pick=a"
        );
    }

    #[test]
    fn declarations_with_problems_say_what_is_wrong() {
        for (json, expected) in [
            (json!("x"), "input `x`: an input is a mapping of its settings"),
            (
                json!({ "id": "a.b" }),
                "input `a.b`: an input id must not contain `/`, `.`, `{`, `}`, `$` or spaces",
            ),
            (json!({ "id": "a/b" }), "input `a/b`: an input id must not"),
            (
                json!({ "id": "x", "colour": 1 }),
                "input `x`: unknown key `colour` (known: label,",
            ),
            (
                json!({ "id": "x", "type": "date" }),
                "unknown type `date` (types: string, number, boolean, choice)",
            ),
            (
                json!({ "id": "x", "type": "choice" }),
                "a `choice` input needs `options`",
            ),
            (
                json!({ "id": "x", "type": "choice", "options": [] }),
                "needs at least one option",
            ),
            (
                json!({ "id": "x", "type": "choice", "options": ["a", "a"] }),
                "option `a` is listed twice",
            ),
            (
                json!({ "id": "x", "type": "choice", "options": [1] }),
                "`options` is a list of values",
            ),
            (
                json!({ "id": "env", "type": "choice", "options": ["dev", "prod"], "default": "qa" }),
                "input `env`: `default` `qa` is not one of dev, prod",
            ),
            (
                json!({ "id": "n", "type": "number", "default": "many" }),
                "input `n`: `default` `many` is not a number",
            ),
            (
                json!({ "id": "n", "type": "number", "default": 100, "max": 64 }),
                "input `n`: `default` must be ≤ 64",
            ),
            (
                json!({ "id": "n", "type": "number", "default": 0, "min": 1 }),
                "`default` must be ≥ 1",
            ),
            (
                json!({ "id": "n", "type": "number", "default": 1.5, "integer": true }),
                "`default` must be a whole number",
            ),
            (
                json!({ "id": "n", "type": "number", "min": 5, "max": 1 }),
                "`min` is greater than `max`",
            ),
            (
                json!({ "id": "b", "type": "boolean", "default": "yes" }),
                "`default` `yes` is not true or false",
            ),
            (
                json!({ "id": "s", "default": 3 }),
                "input `s`: `default` must be a string",
            ),
            (
                json!({ "id": "s", "default": "${env:X}" }),
                "`default` must not contain `${`",
            ),
            (
                json!({ "id": "s", "required": true, "default": "" }),
                "`default` is empty but the input is `required`",
            ),
            (
                json!({ "id": "s", "min": 1 }),
                "`min` applies to number inputs",
            ),
            (
                json!({ "id": "s", "options": ["a"] }),
                "`options` applies to choice inputs",
            ),
            (
                json!({ "id": "b", "type": "boolean", "required": true }),
                "`required` applies to string and number inputs",
            ),
            (
                json!({ "id": "c", "type": "choice", "options": ["a"], "placeholder": "p" }),
                "`placeholder` applies to string and number",
            ),
        ] {
            let found = problem(json.clone());
            assert!(found.contains(expected), "{json}: {found}");
        }
        let (parsed, ids, problems) = parse(vec![
            declared(json!({ "id": "a" })),
            declared(json!({ "id": "b", "type": 1 })),
            ("c".into(), Value::Null),
        ]);
        assert_eq!(ids, ["a", "b", "c"], "every id, also of inputs with problems");
        assert_eq!(parsed.len(), 2, "`c:` alone is a string input");
        assert_eq!(problems.len(), 1);
        assert_eq!(problems[0].0, 1);
        assert_eq!(problems[0].1.1, Some("type"));
    }

    #[test]
    fn values_are_coerced_and_checked() {
        let parsed = inputs(json!([
            { "id": "env", "type": "choice", "options": ["dev", "prod"] },
            { "id": "branch", "required": true },
            { "id": "workers", "type": "number", "default": 4, "min": 1, "max": 64, "integer": true },
            { "id": "ratio", "type": "number" },
            { "id": "clean", "type": "boolean" },
        ]));
        let given = |value: Value| {
            let Value::Object(map) = value else {
                unreachable!()
            };
            resolve(&parsed, &map)
        };
        let values =
            given(json!({ "branch": "dev-1", "workers": "8", "clean": "true", "ratio": 0.5 }))
                .unwrap();
        assert_eq!(
            values.summary(),
            "env=dev · branch=dev-1 · workers=8 · ratio=0.5 · clean=true"
        );
        assert_eq!(
            values.to_json(),
            json!({ "env": "dev", "branch": "dev-1", "workers": 8, "ratio": 0.5, "clean": true })
        );
        assert_eq!(
            given(json!({ "branch": "x", "ratio": null }))
                .unwrap()
                .get("ratio"),
            Some(&InputValue::Number(None))
        );
        let problems =
            given(json!({ "env": "qa", "workers": 100, "clean": 1, "colour": "red" })).unwrap_err();
        assert_eq!(
            problems,
            [
                "unknown input `colour` (inputs: env, branch, workers, ratio, clean)",
                "input `env`: `qa` is not one of dev, prod",
                "input `branch`: required",
                "input `workers`: must be ≤ 64",
                "input `clean`: must be true or false",
            ]
        );
        assert_eq!(
            given(json!({ "branch": "  ", "workers": 2.5, "ratio": "x" })).unwrap_err(),
            [
                "input `branch`: required",
                "input `workers`: must be a whole number",
                "input `ratio`: `x` is not a number",
            ]
        );
        assert_eq!(
            given(json!({ "branch": "${steps.a.status}" })).unwrap_err(),
            ["input `branch`: must not contain `${`"]
        );
        assert_eq!(
            resolve(&[], &Map::from_iter([("x".to_owned(), json!(1))])).unwrap_err(),
            ["unknown input `x` (the workflow has no inputs)"]
        );
        assert_eq!(
            parsed[2].parse_text(" 12 "),
            Ok(InputValue::Number(Some(12.)))
        );
        assert_eq!(parsed[2].parse_text(""), Ok(InputValue::Number(None)));
        assert_eq!(
            InputValue::Number(Some(1e20)).text(),
            "100000000000000000000"
        );
        assert_eq!(InputValue::Number(Some(-3.)).text(), "-3");
    }

    #[test]
    fn remembered_values_that_no_longer_fit_fall_back_to_defaults() {
        let parsed = inputs(json!([
            { "id": "env", "type": "choice", "options": ["dev", "prod"], "default": "dev" },
            { "id": "workers", "type": "number", "default": 4, "max": 8 },
            { "id": "clean", "type": "boolean" },
            { "id": "branch", "default": "main" },
        ]));
        let remembered =
            json!({ "env": "staging", "workers": 16, "clean": true, "branch": "feature" });
        let values = prefill(&parsed, remembered.as_object());
        assert_eq!(
            values.summary(),
            "env=dev · workers=4 · clean=true · branch=feature"
        );
        assert_eq!(prefill(&parsed, None), defaults(&parsed));
        // A type change: the remembered number is no boolean.
        let changed = inputs(json!([{ "id": "workers", "type": "boolean" }]));
        assert_eq!(
            prefill(&changed, remembered.as_object()).get("workers"),
            Some(&InputValue::Bool(false))
        );
    }

    #[test]
    fn references_parse_and_substitute() {
        let refs = input_refs("a ${inputs.env} ${inputs.} ${steps.x.status} ${inputs.w");
        assert_eq!(
            refs[0],
            Ok(InputRef {
                text: "${inputs.env}",
                id: "env"
            })
        );
        assert_eq!(refs[1], Err("`${inputs.}` names no input".into()));
        assert_eq!(refs[2], Err("unclosed `${inputs.w`".into()));
        let values: InputValues = [
            ("env".to_owned(), InputValue::Text("dev".into())),
            ("workers".to_owned(), InputValue::Number(Some(8.))),
        ]
        .into_iter()
        .collect();
        assert_eq!(
            substitute(
                "deploy ${inputs.env}-${inputs.workers} ${inputs.other} ${env:HOME}",
                &values
            ),
            "deploy dev-8 ${inputs.other} ${env:HOME}"
        );
        assert_eq!(substitute("plain", &values), "plain");
    }
}
