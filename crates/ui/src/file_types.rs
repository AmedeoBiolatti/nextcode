//! The types New File… offers (the Files tree's type selector and headless
//! `create_file`'s `type`): where a file of each type goes, the name it
//! gets from what was typed, and its starter text.
//!
//! - Most types add their extension when the typed name lacks it
//!   (`analysis` + Jupyter Notebook → `analysis.ipynb`; `tool.py` stays).
//! - Dockerfile: `Dockerfile`, or `<name>.Dockerfile`; .gitignore: the
//!   typed text is its folder. Their name may be left empty.
//! - Dev Container config: `.devcontainer/devcontainer.json` (a typed name:
//!   `.devcontainer/<name>/devcontainer.json`) in the workspace folder's
//!   root; NextCode Workflow: `.next_code/workflows/<name>.yaml` in the
//!   project root (empty: `workflow.yaml`, `workflow-2.yaml`, …).
//!
//! Without a type ("Auto") the name is used as typed; [`detect`] names the
//! type its extension implies.

/// One type of New File.
#[derive(Debug, PartialEq, Eq)]
pub struct FileType {
    /// What headless `type` and Pinpoint (`item:<id>`) call it.
    pub id: &'static str,
    pub label: &'static str,
    /// Extensions without the dot; the first is added to names lacking
    /// them all.
    extensions: &'static [&'static str],
    place: Place,
    template: Template,
}

#[derive(Debug, PartialEq, Eq)]
enum Place {
    /// `name.ext` where it was typed.
    Here,
    /// `Dockerfile`, or `<name>.Dockerfile`.
    Dockerfile,
    /// This file name; the typed text is its folder.
    Named(&'static str),
    /// `.devcontainer/[<name>/]devcontainer.json` in the folder's root.
    DevContainer,
    /// `.next_code/workflows/<name>.yaml` in the project root.
    Workflow,
}

#[derive(Debug, PartialEq, Eq)]
enum Template {
    Empty,
    /// A `#!` line for the extension's shell.
    Shebang,
    Jupyter,
    Marimo,
    Workflow,
    DevContainer,
}

const fn here(
    id: &'static str,
    label: &'static str,
    extensions: &'static [&'static str],
) -> FileType {
    FileType {
        id,
        label,
        extensions,
        place: Place::Here,
        template: Template::Empty,
    }
}

/// Every type, in the list's order.
pub const TYPES: &[FileType] = &[
    here("text", "Text", &["txt"]),
    here("python", "Python", &["py", "pyi", "pyw"]),
    FileType {
        template: Template::Jupyter,
        ..here("jupyter", "Jupyter Notebook", &["ipynb"])
    },
    FileType {
        template: Template::Marimo,
        ..here("marimo", "marimo Notebook", &["py"])
    },
    here("markdown", "Markdown", &["md", "markdown"]),
    here("json", "JSON", &["json", "jsonc"]),
    here("yaml", "YAML", &["yaml", "yml"]),
    here("toml", "TOML", &["toml"]),
    here("rust", "Rust", &["rs"]),
    here("javascript", "JavaScript", &["js", "mjs", "cjs"]),
    here("typescript", "TypeScript", &["ts", "mts", "cts"]),
    here("tsx", "TypeScript React", &["tsx"]),
    here("html", "HTML", &["html", "htm"]),
    here("css", "CSS", &["css"]),
    FileType {
        template: Template::Shebang,
        ..here("shell", "Shell script", &["sh", "bash", "zsh"])
    },
    here("powershell", "PowerShell", &["ps1", "psm1"]),
    here("c", "C", &["c"]),
    here("cpp", "C++", &["cpp", "cc", "cxx", "hpp", "hh", "hxx"]),
    here("go", "Go", &["go"]),
    here("sql", "SQL", &["sql"]),
    FileType {
        id: "dockerfile",
        label: "Dockerfile",
        extensions: &[],
        place: Place::Dockerfile,
        template: Template::Empty,
    },
    FileType {
        id: "devcontainer",
        label: "Dev Container config",
        extensions: &[],
        place: Place::DevContainer,
        template: Template::DevContainer,
    },
    FileType {
        id: "workflow",
        label: "NextCode Workflow",
        extensions: &["yaml", "yml"],
        place: Place::Workflow,
        template: Template::Workflow,
    },
    FileType {
        id: "gitignore",
        label: ".gitignore",
        extensions: &[],
        place: Place::Named(".gitignore"),
        template: Template::Empty,
    },
];

/// The type with this id; `auto` (or nothing) is no type.
pub fn by_id(id: &str) -> Result<Option<&'static FileType>, String> {
    if id == "auto" {
        return Ok(None);
    }
    TYPES
        .iter()
        .find(|file_type| file_type.id == id)
        .map(Some)
        .ok_or_else(|| {
            let ids: Vec<&str> = TYPES.iter().map(|file_type| file_type.id).collect();
            format!("Unknown file type `{id}` (auto, {})", ids.join(", "))
        })
}

/// What the type list shows for `file_type` (`None`: Auto).
pub fn label(file_type: Option<&FileType>) -> &'static str {
    file_type.map_or("Auto", |file_type| file_type.label)
}

/// The Pinpoint/headless id of `file_type` (`None`: `auto`).
pub fn id(file_type: Option<&FileType>) -> &'static str {
    file_type.map_or("auto", |file_type| file_type.id)
}

/// The list's entries matching `query` (in the label, id or an extension,
/// ignoring case): Auto first, then [`TYPES`] in order.
pub fn filter(query: &str) -> Vec<Option<&'static FileType>> {
    let query = query.trim().trim_start_matches('.').to_lowercase();
    let auto = "auto".contains(&query);
    auto.then_some(None)
        .into_iter()
        .chain(
            TYPES
                .iter()
                .filter(|file_type| {
                    file_type.label.to_lowercase().contains(&query)
                        || file_type.id.contains(&query)
                        || file_type
                            .extensions
                            .iter()
                            .any(|extension| extension.starts_with(&query))
                })
                .map(Some),
        )
        .collect()
}

/// The last part of `input` (after `/`, or `\` on Windows).
fn last_part(input: &str, windows: bool) -> &str {
    input
        .rsplit(|c| c == '/' || (windows && c == '\\'))
        .next()
        .unwrap_or(input)
}

fn is_dockerfile(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower == "dockerfile"
        || lower == "containerfile"
        || lower.starts_with("dockerfile.")
        || lower.ends_with(".dockerfile")
}

fn has_extension(name: &str, extensions: &[&str]) -> bool {
    let lower = name.to_ascii_lowercase();
    extensions.iter().any(|extension| {
        lower
            .strip_suffix(extension)
            .is_some_and(|stem| stem.len() > 1 && stem.ends_with('.'))
    })
}

/// The type a name typed without one implies, by its name or extension:
/// shown faintly beside Auto.
pub fn detect(input: &str, windows: bool) -> Option<&'static FileType> {
    let name = last_part(input.trim(), windows);
    if is_dockerfile(name) {
        return by_id("dockerfile").ok().flatten();
    }
    if name == ".gitignore" {
        return by_id("gitignore").ok().flatten();
    }
    if name == "devcontainer.json" || name == ".devcontainer.json" {
        return by_id("devcontainer").ok().flatten();
    }
    TYPES.iter().find(|file_type| {
        file_type.place == Place::Here && has_extension(name, file_type.extensions)
    })
}

/// What a new file's path is relative to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Base {
    /// The folder New File was started in.
    Dir,
    /// The root of the workspace folder holding it.
    FolderRoot,
    /// The project's (first) root.
    ProjectRoot,
}

/// Whether a file of `file_type` may be created with no name typed.
pub fn name_optional(file_type: Option<&FileType>) -> bool {
    file_type.is_some_and(|file_type| file_type.place != Place::Here)
}

/// Whether `file_type` names its file itself when none is typed and
/// [`new_path`]'s name may be taken (workflows: the caller picks a free
/// `workflow-N.yaml`).
pub fn picks_free_name(file_type: Option<&FileType>, input: &str) -> bool {
    file_type.is_some_and(|file_type| file_type.place == Place::Workflow) && input.trim().is_empty()
}

/// The `n`th free-name candidate of [`picks_free_name`] types.
pub fn numbered(file_type: &FileType, n: usize) -> String {
    debug_assert_eq!(file_type.place, Place::Workflow);
    match n {
        1 => format!("{WORKFLOW_DIR}/workflow.yaml"),
        n => format!("{WORKFLOW_DIR}/workflow-{n}.yaml"),
    }
}

/// Where workflows live in a project.
const WORKFLOW_DIR: &str = ".next_code/workflows";

/// The `/`-separated path (unchecked: see `file_ops::new_entry_parts`) a
/// file typed as `input` gets for `file_type`, and what it is relative to.
pub fn new_path(
    file_type: Option<&FileType>,
    input: &str,
    windows: bool,
) -> Result<(Base, String), String> {
    let input = input.trim();
    let Some(file_type) = file_type else {
        return Ok((Base::Dir, input.to_owned()));
    };
    let name = last_part(input, windows);
    // A trailing separator names a folder for the fixed names.
    let folder = input.trim_end_matches(|c| c == '/' || (windows && c == '\\'));
    let in_folder = |file: &str| {
        if folder.is_empty() {
            file.to_owned()
        } else {
            format!("{folder}/{file}")
        }
    };
    Ok(match &file_type.place {
        Place::Here => {
            if name.is_empty() || has_extension(name, file_type.extensions) {
                (Base::Dir, input.to_owned())
            } else {
                (Base::Dir, format!("{input}.{}", file_type.extensions[0]))
            }
        }
        Place::Dockerfile => {
            if name.is_empty() {
                (Base::Dir, in_folder("Dockerfile"))
            } else if is_dockerfile(name) {
                (Base::Dir, input.to_owned())
            } else {
                (Base::Dir, format!("{input}.Dockerfile"))
            }
        }
        Place::Named(file) => {
            if name == *file {
                (Base::Dir, input.to_owned())
            } else {
                (Base::Dir, in_folder(file))
            }
        }
        Place::DevContainer => {
            if input.is_empty() {
                (Base::FolderRoot, ".devcontainer/devcontainer.json".into())
            } else if name == "devcontainer.json" || name == ".devcontainer.json" {
                (Base::FolderRoot, input.to_owned())
            } else {
                (
                    Base::FolderRoot,
                    format!(".devcontainer/{folder}/devcontainer.json"),
                )
            }
        }
        Place::Workflow => {
            if name != input {
                return Err(
                    "A workflow name cannot contain a folder: workflows live in \
                            .next_code/workflows"
                        .into(),
                );
            }
            let file = if input.is_empty() {
                "workflow.yaml".to_owned()
            } else if has_extension(input, file_type.extensions) {
                input.to_owned()
            } else {
                format!("{input}.yaml")
            };
            (Base::ProjectRoot, format!("{WORKFLOW_DIR}/{file}"))
        }
    })
}

/// What the list shows beside the label: the extension added, or where the
/// file goes.
pub fn hint(file_type: &FileType) -> Option<String> {
    match &file_type.place {
        Place::Here => Some(format!(".{}", file_type.extensions[0])),
        Place::DevContainer => Some(".devcontainer/".into()),
        Place::Workflow => Some("workflows/".into()),
        Place::Dockerfile | Place::Named(_) => None,
    }
}

/// A file name of the type, for its icon.
pub fn icon_name(file_type: &FileType) -> String {
    match &file_type.place {
        Place::Here | Place::Workflow => format!("a.{}", file_type.extensions[0]),
        Place::Dockerfile => "Dockerfile".into(),
        Place::Named(file) => (*file).into(),
        Place::DevContainer => "devcontainer.json".into(),
    }
}

/// Whether a new file of `file_type` is a workflow (its folder gets the
/// schema its first line names).
pub fn is_workflow(file_type: &FileType) -> bool {
    file_type.place == Place::Workflow
}

/// The starter text of a new file named `name` of `file_type`; `None`:
/// empty. `folder` names the project folder (a dev container's `name`).
pub fn template(file_type: &FileType, name: &str, folder: &str) -> Option<String> {
    Some(match file_type.template {
        Template::Empty => return None,
        Template::Shebang => {
            let shell = if has_extension(name, &["zsh"]) {
                "zsh"
            } else {
                "bash"
            };
            format!("#!/usr/bin/env {shell}\n\n")
        }
        Template::Jupyter => notebook::ipynb::new_notebook().to_text(),
        Template::Marimo => notebook::marimo::new_notebook().to_text(),
        Template::Workflow => workflow::TEMPLATE.to_owned(),
        Template::DevContainer => {
            let config = serde_json::json!({
                "name": folder,
                "image": "mcr.microsoft.com/devcontainers/base:ubuntu",
            });
            format!(
                "{}\n",
                serde_json::to_string_pretty(&config).expect("JSON values serialize")
            )
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(id: &str, input: &str) -> Result<(Base, String), String> {
        new_path(by_id(id).unwrap(), input, false)
    }

    fn here(relative: &str) -> Result<(Base, String), String> {
        Ok((Base::Dir, relative.to_owned()))
    }

    #[test]
    fn types_add_their_extension_unless_the_name_has_one_of_theirs() {
        assert_eq!(path("auto", " notes "), here("notes"));
        assert_eq!(path("python", "tool"), here("tool.py"));
        assert_eq!(path("python", "tool.py"), here("tool.py"));
        assert_eq!(path("python", "stubs.PYI"), here("stubs.PYI"));
        assert_eq!(path("jupyter", "nb/analysis"), here("nb/analysis.ipynb"));
        assert_eq!(path("marimo", "app"), here("app.py"));
        assert_eq!(path("yaml", "config.local"), here("config.local.yaml"));
        assert_eq!(path("yaml", "ci.yml"), here("ci.yml"));
        assert_eq!(path("text", "notes.md"), here("notes.md.txt"));
        assert_eq!(path("shell", "run"), here("run.sh"));
        // A bare `.py` is a dotfile name, not a Python extension.
        assert_eq!(path("python", ".py"), here(".py.py"));
        // An empty name stays empty (refused as such by the name checks).
        assert_eq!(path("rust", ""), here(""));
        assert!(!name_optional(by_id("rust").unwrap()));
    }

    #[test]
    fn fixed_names_take_the_typed_text_as_their_folder() {
        assert_eq!(path("dockerfile", ""), here("Dockerfile"));
        assert_eq!(path("dockerfile", "api"), here("api.Dockerfile"));
        assert_eq!(path("dockerfile", "docker/"), here("docker/Dockerfile"));
        assert_eq!(path("dockerfile", "Dockerfile.dev"), here("Dockerfile.dev"));
        assert_eq!(path("gitignore", ""), here(".gitignore"));
        assert_eq!(path("gitignore", "build"), here("build/.gitignore"));
        assert_eq!(path("gitignore", "sub/.gitignore"), here("sub/.gitignore"));
        for id in ["dockerfile", "gitignore", "devcontainer", "workflow"] {
            assert!(name_optional(by_id(id).unwrap()), "{id}");
        }
    }

    #[test]
    fn dev_containers_and_workflows_go_to_their_folders() {
        assert_eq!(
            path("devcontainer", ""),
            Ok((Base::FolderRoot, ".devcontainer/devcontainer.json".into()))
        );
        assert_eq!(
            path("devcontainer", "gpu"),
            Ok((
                Base::FolderRoot,
                ".devcontainer/gpu/devcontainer.json".into()
            ))
        );
        assert_eq!(
            path("workflow", "deploy"),
            Ok((Base::ProjectRoot, ".next_code/workflows/deploy.yaml".into()))
        );
        assert_eq!(
            path("workflow", "ci.yml"),
            Ok((Base::ProjectRoot, ".next_code/workflows/ci.yml".into()))
        );
        assert_eq!(
            path("workflow", ""),
            Ok((
                Base::ProjectRoot,
                ".next_code/workflows/workflow.yaml".into()
            ))
        );
        assert!(path("workflow", "a/b").is_err());
        let workflow = by_id("workflow").unwrap().unwrap();
        assert!(picks_free_name(Some(workflow), " "));
        assert!(!picks_free_name(Some(workflow), "deploy"));
        assert_eq!(
            numbered(workflow, 2),
            ".next_code/workflows/workflow-2.yaml"
        );
    }

    #[test]
    fn the_list_filters_by_label_id_and_extension() {
        let ids = |query: &str| -> Vec<&str> { filter(query).into_iter().map(id).collect() };
        assert_eq!(ids("").len(), TYPES.len() + 1);
        assert_eq!(ids("")[..3], ["auto", "text", "python"]);
        assert_eq!(ids("note"), ["jupyter", "marimo"]);
        assert_eq!(ids(".ipynb"), ["jupyter"]);
        assert_eq!(ids("PY"), ["python", "jupyter", "marimo"]);
        assert_eq!(ids("work"), ["workflow"]);
        assert_eq!(ids("yml"), ["yaml", "workflow"]);
        assert!(ids("zzz").is_empty());
        assert!(by_id("nope").unwrap_err().contains("jupyter"));
    }

    #[test]
    fn names_imply_their_type() {
        let detected = |name: &str| detect(name, false).map(|file_type| file_type.id);
        assert_eq!(detected("main.py"), Some("python"));
        assert_eq!(detected("pkg/analysis.ipynb"), Some("jupyter"));
        assert_eq!(detected("Dockerfile"), Some("dockerfile"));
        assert_eq!(detected("api.dockerfile"), Some("dockerfile"));
        assert_eq!(detected(".gitignore"), Some("gitignore"));
        assert_eq!(detected("ci.yml"), Some("yaml"));
        assert_eq!(detected("README"), None);
        assert_eq!(detected("archive.zip"), None);
    }

    #[test]
    fn templates_parse_as_their_formats() {
        let template =
            |id: &str, name: &str| template(by_id(id).unwrap().unwrap(), name, "demo").unwrap();
        let ipynb = template("jupyter", "a.ipynb");
        let parsed = notebook::ipynb::parse(&ipynb).unwrap();
        assert_eq!(parsed.cells.len(), 1);
        assert_eq!(parsed.cells[0].kind, notebook::CellKind::Code);
        assert_eq!(parsed.cells[0].source, "");
        let notebook::Format::Ipynb(info) = &parsed.format else {
            panic!("{parsed:?}");
        };
        assert_eq!(info.metadata["kernelspec"]["language"], "python");
        assert_eq!(
            notebook::kind_of("a.ipynb", Some(&ipynb)),
            Some(notebook::Kind::Ipynb)
        );

        let marimo = template("marimo", "app.py");
        assert_eq!(
            notebook::kind_of("app.py", Some(&marimo)),
            Some(notebook::Kind::Marimo)
        );
        let parsed = notebook::marimo::parse(&marimo).unwrap();
        assert_eq!(parsed.cells.len(), 1);

        let workflow = template("workflow", "deploy.yaml");
        workflow::file::check(&workflow, "deploy").unwrap();

        let devcontainer = template("devcontainer", "devcontainer.json");
        let config = devcontainer::Config::parse(&devcontainer).unwrap();
        assert_eq!(config.name.as_deref(), Some("demo"));

        assert_eq!(template("shell", "run.sh"), "#!/usr/bin/env bash\n\n");
        assert_eq!(template("shell", "run.zsh"), "#!/usr/bin/env zsh\n\n");
        assert_eq!(
            super::template(by_id("python").unwrap().unwrap(), "a.py", "demo"),
            None
        );
    }
}
