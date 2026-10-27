//! Descriptions of keys and values from the format's JSON Schema
//! (`workflow::SCHEMA`), so hovers and completions say what the schema
//! says.

use std::sync::OnceLock;

use serde_json::Value;

use crate::analysis::Place;

fn schema() -> &'static Value {
    static SCHEMA: OnceLock<Value> = OnceLock::new();
    SCHEMA.get_or_init(|| serde_json::from_str(workflow::SCHEMA).expect("the schema is JSON"))
}

/// The schema's description of `node`, following a `$ref` (the property's
/// own description wins).
fn description_of(node: &Value) -> Option<String> {
    if let Some(text) = node.get("description").and_then(Value::as_str) {
        return Some(text.to_owned());
    }
    let pointer = node.get("$ref")?.as_str()?.strip_prefix('#')?;
    description_of(schema().pointer(pointer)?)
}

/// Where the properties of a place are described.
fn properties(place: &Place) -> Option<&'static str> {
    Some(match place {
        Place::Top => "/properties",
        Place::Step(_) => "/definitions/step/properties",
        Place::Ready(_) => "/definitions/ready/properties",
        Place::Input(_) => "/definitions/input/properties",
        Place::Project(_) => "/definitions/project/anyOf/1/properties",
        Place::Dependency(_) => {
            "/definitions/dependsOn/anyOf/2/additionalProperties/anyOf/2/properties"
        }
        Place::Option(_) => "/definitions/input/properties/options/items/anyOf/1/properties",
        Place::Healthcheck(_) => "/definitions/healthcheck/properties",
        Place::Restart(_) => "/definitions/restart/anyOf/1/properties",
        _ => return None,
    })
}

/// What key `key` of the mapping at `place` is.
pub fn key(place: &Place, key: &str) -> Option<String> {
    let described = match place {
        Place::Folder(_) | Place::Project(_) => match key {
            "local" => Some("A folder on this machine.".to_owned()),
            "wsl" => Some("The WSL distro the folder is in (with `path`), e.g. Ubuntu.".to_owned()),
            "path" => Some("A Linux path inside the WSL distro named by `wsl`.".to_owned()),
            _ => None,
        },
        Place::Artifact(_) => match key {
            "label" => description_of(&schema()["definitions"]["artifactLabel"]),
            "path" => Some("A file, relative to the step's cwd (else its project).".to_owned()),
            "url" => Some("An http(s) link.".to_owned()),
            _ => None,
        },
        _ => None,
    };
    described.or_else(|| {
        let properties = schema().pointer(properties(place)?)?;
        description_of(properties.get(key)?)
    })
}

/// What a dependency condition means.
pub fn condition(name: &str) -> &'static str {
    match name {
        "completed" => {
            "It must have finished: ready or failed (a report after an evaluation). A stopped step blocks."
        }
        _ => "It must be ready (the default). If it fails, this step is blocked.",
    }
}

/// Whether a key is planned or from the old format (the schema rejects it
/// with `not: {}`): not offered.
pub fn hidden(place: &Place, key: &str) -> bool {
    properties(place)
        .and_then(|pointer| schema().pointer(pointer))
        .and_then(|properties| properties.get(key))
        .is_some_and(|property| property.get("not").is_some())
}
