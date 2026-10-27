//! Step-result variables: `${steps.<id>.status}`, `${steps.<id>.exit_code}`,
//! `${steps.<id>.artifacts.<label>}` and `${steps.<id>.artifacts.<index>}`
//! in a step's `run`, `check`, `cwd`, `env` values and artifact paths.
//! They name steps the step lists in `needs` or `after`, and are replaced
//! when the step starts (other `${…}` variables are left for the editor,
//! which expands them where the step runs).

/// What a `${steps.…}` variable reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepField {
    /// `ready`, `failed` or `stopped`.
    Status,
    /// The command's exit code; empty when it has none.
    ExitCode,
    /// An artifact's resolved path or URL.
    Artifact(ArtifactRef),
}

/// An artifact by its `label`, or by its 0-based position in `artifacts`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArtifactRef {
    Label(String),
    Index(usize),
}

/// One `${steps.…}` variable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepRef<'a> {
    /// The whole variable, `${…}` included.
    pub text: &'a str,
    /// The step it names.
    pub step: &'a str,
    pub field: StepField,
}

const PREFIX: &str = "${steps.";
const FIELDS: &str = "status, exit_code, artifacts.<label or index>";

/// Every `${steps.…}` variable in `text`, in order. A step id may contain
/// dots: the longest of `ids` that fits is taken. A variable that does not
/// parse is an error naming it.
pub fn step_refs<'a>(text: &'a str, ids: &[&str]) -> Vec<Result<StepRef<'a>, String>> {
    let mut found = Vec::new();
    let mut from = 0;
    while let Some(start) = text[from..].find(PREFIX).map(|at| from + at) {
        let Some(end) = text[start..].find('}').map(|at| start + at) else {
            found.push(Err(format!("unclosed `{}`", &text[start..])));
            break;
        };
        from = end + 1;
        found.push(parse_ref(&text[start..=end], ids));
    }
    found
}

fn parse_ref<'a>(whole: &'a str, ids: &[&str]) -> Result<StepRef<'a>, String> {
    let rest = &whole[PREFIX.len()..whole.len() - 1];
    let step = ids
        .iter()
        .filter(|id| {
            rest.len() > id.len() && rest.starts_with(**id) && rest[id.len()..].starts_with('.')
        })
        .max_by_key(|id| id.len())
        .map(|id| &rest[..id.len()])
        .unwrap_or_else(|| rest.split('.').next().unwrap_or(rest));
    let field = rest
        .get(step.len()..)
        .and_then(|field| field.strip_prefix('.'))
        .unwrap_or("");
    let field = match field {
        "status" => StepField::Status,
        "exit_code" => StepField::ExitCode,
        other => match other.strip_prefix("artifacts.") {
            Some(name) if !name.is_empty() => StepField::Artifact(match name.parse() {
                Ok(index) if name.bytes().all(|byte| byte.is_ascii_digit()) => {
                    ArtifactRef::Index(index)
                }
                _ => ArtifactRef::Label(name.to_owned()),
            }),
            _ => {
                return Err(format!(
                    "`{whole}`: unknown field `{other}` (known: {FIELDS})"
                ));
            }
        },
    };
    Ok(StepRef {
        text: whole,
        step,
        field,
    })
}

/// `text` with every `${steps.…}` variable replaced by what `value` says
/// for it; other variables stay.
pub fn substitute(
    text: &str,
    ids: &[&str],
    mut value: impl FnMut(&StepRef) -> Result<String, String>,
) -> Result<String, String> {
    let mut out = String::with_capacity(text.len());
    let mut rest = 0;
    for found in step_refs(text, ids) {
        let found = found?;
        // The variables come in order and each occurs at or after `rest`.
        let at = rest + text[rest..].find(found.text).unwrap_or(0);
        out.push_str(&text[rest..at]);
        out.push_str(&value(&found)?);
        rest = at + found.text.len();
    }
    out.push_str(&text[rest..]);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variables_parse_with_dotted_ids_labels_and_indices() {
        let ids = ["eval", "eval.gpu"];
        let refs: Vec<_> = step_refs(
            "a ${steps.eval.status} ${steps.eval.gpu.exit_code} ${steps.eval.artifacts.report.html} \
             ${steps.eval.artifacts.1} ${workspaceFolder}",
            &ids,
        )
        .into_iter()
        .map(Result::unwrap)
        .map(|found| (found.step, found.field))
        .collect();
        assert_eq!(
            refs,
            [
                ("eval", StepField::Status),
                ("eval.gpu", StepField::ExitCode),
                (
                    "eval",
                    StepField::Artifact(ArtifactRef::Label("report.html".into()))
                ),
                ("eval", StepField::Artifact(ArtifactRef::Index(1))),
            ]
        );
        let unknown = step_refs("${steps.x.colour}", &ids);
        assert_eq!(
            unknown[0].as_ref().unwrap_err(),
            "`${steps.x.colour}`: unknown field `colour` (known: status, exit_code, \
             artifacts.<label or index>)"
        );
        assert!(step_refs("${steps.x}", &ids)[0].is_err());
        assert!(step_refs("${steps.x.status", &ids)[0].is_err());
        // An id not among `ids` is the text up to the first dot.
        assert_eq!(
            step_refs("${steps.other.status}", &ids)[0]
                .as_ref()
                .unwrap()
                .step,
            "other"
        );
    }

    #[test]
    fn substitution_replaces_only_step_variables() {
        let text = "report --code ${steps.eval.exit_code} --in ${steps.eval.artifacts.0} ${env:X}";
        let done = substitute(text, &["eval"], |found| {
            Ok(match found.field {
                StepField::ExitCode => "1".into(),
                _ => "/tmp/out.json".into(),
            })
        })
        .unwrap();
        assert_eq!(done, "report --code 1 --in /tmp/out.json ${env:X}");
        assert_eq!(
            substitute("plain", &[], |_| Ok(String::new())).unwrap(),
            "plain"
        );
        assert!(substitute("${steps.eval.status}", &["eval"], |_| Err("no".into())).is_err());
    }
}
