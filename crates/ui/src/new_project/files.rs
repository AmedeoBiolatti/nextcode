//! Project scaffolding, with an optional uv initializer supplied by the caller.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Template {
    Empty,
    Python,
    Rust,
    C,
    Cpp,
}

impl Template {
    pub const ALL: [Self; 5] = [Self::Empty, Self::Python, Self::Rust, Self::C, Self::Cpp];

    pub fn label(self) -> &'static str {
        match self {
            Self::Empty => "Empty",
            Self::Python => "Python",
            Self::Rust => "Rust",
            Self::C => "C",
            Self::Cpp => "C++",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Empty => "An empty folder for any language or toolchain.",
            Self::Python => "main.py, pyproject.toml and .gitignore. Run with python3 main.py.",
            Self::Rust => {
                "A Cargo application with src/main.rs and .gitignore. Run with cargo run."
            }
            Self::C => "A C17 application with CMake and Build and Run configurations.",
            Self::Cpp => "A C++20 application with CMake and Build and Run configurations.",
        }
    }
}

pub fn check_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("Enter a project name.".into());
    }
    if name == "."
        || name == ".."
        || name.starts_with(char::is_whitespace)
        || name.ends_with(char::is_whitespace)
        || name.ends_with('.')
        || name.len() > 255
        || name
            .chars()
            .any(|c| c.is_control() || "/\\<>:\"|?*".contains(c))
    {
        return Err(
            "Use a folder name without slashes, reserved characters, or surrounding spaces.".into(),
        );
    }
    let stem = name
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    if ["CON", "PRN", "AUX", "NUL"].contains(&stem.as_str())
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && matches!(stem.as_bytes()[3], b'1'..=b'9'))
    {
        return Err("That name is reserved. Choose another project name.".into());
    }
    Ok(())
}

// Keep package names portable, valid as Rust crate names, and short enough
// for generated artifact filenames even when the folder name is long.
fn package_name(name: &str) -> String {
    let suffix: String = name
        .chars()
        .take(48)
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    let reserved = matches!(
        suffix.as_str(),
        "abstract"
            | "as"
            | "async"
            | "await"
            | "become"
            | "box"
            | "break"
            | "const"
            | "continue"
            | "crate"
            | "do"
            | "dyn"
            | "else"
            | "enum"
            | "extern"
            | "false"
            | "final"
            | "fn"
            | "for"
            | "gen"
            | "if"
            | "impl"
            | "in"
            | "let"
            | "loop"
            | "macro"
            | "match"
            | "mod"
            | "move"
            | "mut"
            | "override"
            | "priv"
            | "pub"
            | "ref"
            | "return"
            | "self"
            | "static"
            | "struct"
            | "super"
            | "trait"
            | "true"
            | "try"
            | "type"
            | "typeof"
            | "union"
            | "unsafe"
            | "unsized"
            | "use"
            | "virtual"
            | "where"
            | "while"
            | "yield"
            | "test"
            | "std"
            | "core"
            | "alloc"
            | "proc_macro"
    );
    if reserved || !suffix.starts_with(|c: char| c.is_ascii_alphabetic()) {
        format!("project_{suffix}")
    } else {
        suffix
    }
}

fn python_package_name(name: &str) -> String {
    // Python distribution names must end with a letter or digit, unlike Rust
    // crate names. Unicode and punctuation can leave a trailing underscore.
    package_name(name).trim_end_matches('_').to_owned()
}

pub fn destination(parent: &Path, name: &str) -> Result<PathBuf, String> {
    check_name(name)?;
    if !parent.is_absolute() {
        return Err("Choose an absolute parent folder path.".into());
    }
    Ok(parent.join(name))
}

pub fn create(parent: &Path, name: &str, template: Template) -> Result<PathBuf, String> {
    create_inner(
        parent,
        name,
        template,
        None::<fn(&Path, &str) -> Result<(), String>>,
    )
}

/// Runs uv in an exclusively created folder before publishing the project.
/// Keeping the runner outside this module shares the editor's subprocess
/// timeout and process-tree handling and lets filesystem tests inject failures.
pub fn create_with_uv(
    parent: &Path,
    name: &str,
    initialize: impl FnOnce(&Path, &str) -> Result<(), String>,
) -> Result<PathBuf, String> {
    create_inner(parent, name, Template::Python, Some(initialize))
}

fn create_inner(
    parent: &Path,
    name: &str,
    template: Template,
    initialize: Option<impl FnOnce(&Path, &str) -> Result<(), String>>,
) -> Result<PathBuf, String> {
    let root = destination(parent, name)?;
    if !parent.is_dir() {
        return Err("The parent folder does not exist or is not a directory.".into());
    }
    // create_dir is exclusive, including when a symlink or empty folder exists.
    fs::create_dir(&root).map_err(|error| {
        if error.kind() == io::ErrorKind::AlreadyExists {
            format!(
                "{} already exists. Choose another name or use Open Project.",
                root.display()
            )
        } else {
            format!("Could not create {}: {error}", root.display())
        }
    })?;
    let mut files: Vec<PathBuf> = Vec::new();
    let mut directories = Vec::new();
    let result = (|| -> io::Result<()> {
        let using_uv = initialize.is_some();
        if let Some(initialize) = initialize {
            let result = initialize(&root, &python_package_name(name));
            // uv init --app --no-package --vcs none creates only these files.
            // Record partial output even on failure, so retries work without
            // ever recursively deleting unknown files in the new folder.
            for relative in ["pyproject.toml", "main.py", "README.md", ".python-version"] {
                let path = root.join(relative);
                if fs::symlink_metadata(&path).is_ok_and(|meta| meta.file_type().is_file()) {
                    files.push(path);
                }
            }
            result.map_err(io::Error::other)?;
            for relative in ["pyproject.toml", "main.py"] {
                if !root.join(relative).is_file() {
                    return Err(io::Error::other(format!(
                        "uv init did not create {relative}"
                    )));
                }
            }
        }
        let contents = match template {
            Template::Empty => Vec::new(),
            Template::Python if using_uv => vec![
                (".gitignore", ".venv/\n__pycache__/\n*.py[cod]\n".to_owned()),
            ],
            Template::Python => vec![
                ("main.py", "def main():\n    print(\"Hello, world!\")\n\n\nif __name__ == \"__main__\":\n    main()\n".to_owned()),
                ("pyproject.toml", format!("[project]\nname = \"{}\"\nversion = \"0.1.0\"\nrequires-python = \">=3.10\"\ndependencies = []\n", python_package_name(name))),
                (".gitignore", ".venv/\n__pycache__/\n*.py[cod]\n".to_owned()),
            ],
            Template::Rust => {
                let src = root.join("src");
                fs::create_dir(&src)?;
                directories.push(src);
                vec![
                    ("Cargo.toml", format!("[package]\nname = \"{}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n", package_name(name))),
                    ("src/main.rs", "fn main() {\n    println!(\"Hello, world!\");\n}\n".to_owned()),
                    (".gitignore", "/target/\n".to_owned()),
                ]
            }
            Template::C | Template::Cpp => {
                for relative in ["src", "cmake", ".next_code"] {
                    let path = root.join(relative);
                    fs::create_dir(&path)?;
                    directories.push(path);
                }
                let (language, language_name, source, standard, main) = match template {
                    Template::C => ("C", "C", "main.c", "17", include_str!("../../assets/projects/cmake/main.c")),
                    _ => ("CXX", "C++", "main.cpp", "20", include_str!("../../assets/projects/cmake/main.cpp")),
                };
                let cmake = include_str!("../../assets/projects/cmake/CMakeLists.txt")
                    .replace("@PROJECT_NAME@", &package_name(name))
                    .replace("@LANGUAGE@", language)
                    .replace("@SOURCE@", source)
                    .replace("@STANDARD@", standard);
                let readme = include_str!("../../assets/projects/cmake/README.md")
                    .replace("@PROJECT_NAME@", name)
                    .replace("@LANGUAGE_NAME@", language_name)
                    .replace("@STANDARD@", standard);
                vec![
                    ("CMakeLists.txt", cmake),
                    (if template == Template::C { "src/main.c" } else { "src/main.cpp" }, main.to_owned()),
                    ("cmake/Run.cmake", include_str!("../../assets/projects/cmake/Run.cmake").to_owned()),
                    (".next_code/run.json", include_str!("../../assets/projects/cmake/run.json")
                        .replace("@EXE_SUFFIX@", if cfg!(windows) { ".exe" } else { "" })),
                    (".clangd", "CompileFlags:\n  CompilationDatabase: build\n".to_owned()),
                    (".gitignore", "/build/\n/CMakeUserPresets.json\n".to_owned()),
                    ("README.md", readme),
                ]
            }
        };
        for (relative, contents) in contents {
            use io::Write;
            let path = root.join(relative);
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)?;
            files.push(path);
            file.write_all(contents.as_bytes())?;
        }
        Ok(())
    })();
    if let Err(error) = result {
        // Remove only entries this operation created; never recursively erase
        // a folder that another process might have populated in the meantime.
        for file in files.iter().rev() {
            let _ = fs::remove_file(file);
        }
        for directory in directories.iter().rev() {
            let _ = fs::remove_dir(directory);
        }
        let remaining = if fs::remove_dir(&root).is_err() {
            " Any remaining files were kept in that folder."
        } else {
            ""
        };
        return Err(format!(
            "Could not create the starter files in {}: {error}{remaining}",
            root.display()
        ));
    }
    Ok(root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "nextcode-project-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn rejects_invalid_names_and_relative_parents() {
        for name in [
            "",
            ".",
            "..",
            "../outside",
            "a/b",
            "a\\b",
            " leading",
            "trailing ",
            "trailing.",
            "a\n",
            "NUL.txt",
            "com1",
            "a:b",
        ] {
            assert!(check_name(name).is_err(), "{name:?}");
        }
        for name in ["my-project", "My Project", "café", "中文", "123", "async"] {
            check_name(name).unwrap();
        }
        assert!(destination(Path::new("relative"), "project").is_err());
    }

    #[test]
    fn creates_each_template_without_external_tools() {
        let scratch = Scratch::new();
        let empty = create(&scratch.0, "empty", Template::Empty).unwrap();
        assert_eq!(fs::read_dir(empty).unwrap().count(), 0);
        let python = create(&scratch.0, "My Python", Template::Python).unwrap();
        assert!(
            fs::read_to_string(python.join("main.py"))
                .unwrap()
                .contains("def main():")
        );
        assert!(
            fs::read_to_string(python.join("pyproject.toml"))
                .unwrap()
                .contains("my_python")
        );
        let rust = create(&scratch.0, "123 café", Template::Rust).unwrap();
        assert!(
            fs::read_to_string(rust.join("Cargo.toml"))
                .unwrap()
                .contains("project_123_caf_")
        );
        assert!(rust.join("src/main.rs").is_file());
        assert!(rust.join(".gitignore").is_file());
    }

    #[test]
    fn c_and_cpp_starters_have_build_run_and_language_server_configuration() {
        let scratch = Scratch::new();
        for (template, source, standard) in [
            (Template::C, "main.c", "C_STANDARD 17"),
            (Template::Cpp, "main.cpp", "CXX_STANDARD 20"),
        ] {
            let root = create(&scratch.0, template.label(), template).unwrap();
            assert!(root.join("src").join(source).is_file());
            let cmake = fs::read_to_string(root.join("CMakeLists.txt")).unwrap();
            assert!(cmake.contains(standard));
            assert!(cmake.contains("CMAKE_EXPORT_COMPILE_COMMANDS ON"));
            assert!(!cmake.contains("@PROJECT_NAME@"));
            assert!(root.join("cmake/Run.cmake").is_file());
            assert!(root.join(".next_code/run.json").is_file());
            assert!(
                fs::read_to_string(root.join(".clangd"))
                    .unwrap()
                    .contains("CompilationDatabase: build")
            );
            assert!(!root.join("pyproject.toml").exists());
            assert!(!root.join("Cargo.toml").exists());
        }
    }

    #[test]
    fn never_reuses_existing_folders_or_files() {
        let scratch = Scratch::new();
        let existing = scratch.0.join("existing");
        fs::create_dir(&existing).unwrap();
        fs::write(existing.join("main.py"), "keep me").unwrap();
        assert!(create(&scratch.0, "existing", Template::Python).is_err());
        assert_eq!(
            fs::read_to_string(existing.join("main.py")).unwrap(),
            "keep me"
        );
        assert!(!existing.join("pyproject.toml").exists());
        fs::write(scratch.0.join("file"), "keep file").unwrap();
        assert!(create(&scratch.0, "file", Template::Rust).is_err());
        assert!(create(&scratch.0.join("missing"), "new", Template::Empty).is_err());
        assert!(!scratch.0.join("missing").exists());
    }

    #[test]
    fn uv_initialization_is_optional_and_never_runs_on_existing_projects() {
        let scratch = Scratch::new();
        let existing = create(&scratch.0, "plain", Template::Python).unwrap();
        assert!(!existing.join(".python-version").exists());
        assert!(create_with_uv(&scratch.0, "plain", |_, _| panic!("must not run uv")).is_err());
        let root = create_with_uv(&scratch.0, "My UV App", |root, name| {
            assert_eq!(name, "my_uv_app");
            fs::write(
                root.join("pyproject.toml"),
                "[project]\nname = \"my-uv-app\"\n",
            )
            .unwrap();
            fs::write(root.join("main.py"), "print('uv starter')\n").unwrap();
            fs::write(root.join(".python-version"), "3.14\n").unwrap();
            Ok(())
        })
        .unwrap();
        assert_eq!(
            fs::read_to_string(root.join("main.py")).unwrap(),
            "print('uv starter')\n"
        );
        assert!(root.join(".gitignore").is_file());
    }

    #[test]
    fn python_package_names_end_in_a_letter_or_digit() {
        for (folder, expected) in [
            ("café", "caf"),
            ("中文", "project"),
            ("app-", "app"),
            ("123", "project_123"),
        ] {
            assert_eq!(python_package_name(folder), expected);
        }
    }

    #[test]
    fn uv_failure_rolls_back_partial_files_and_allows_retry() {
        let scratch = Scratch::new();
        let error = create_with_uv(&scratch.0, "retry", |root, _| {
            fs::write(root.join("pyproject.toml"), "partial").unwrap();
            fs::write(root.join(".python-version"), "3.14").unwrap();
            Err("uv init failed: no Python interpreter".into())
        })
        .unwrap_err();
        assert!(error.contains("no Python interpreter"));
        assert!(!scratch.0.join("retry").exists());
        create(&scratch.0, "retry", Template::Python).unwrap();
        assert!(
            create_with_uv(&scratch.0, "missing-uv", |_, _| Err("uv not found".into())).is_err()
        );
        assert!(!scratch.0.join("missing-uv").exists());
    }

    #[test]
    fn uv_failure_keeps_unexpected_files() {
        let scratch = Scratch::new();
        let error = create_with_uv(&scratch.0, "keep", |root, _| {
            fs::write(root.join("user-file.txt"), "keep me").unwrap();
            Err("uv init failed".into())
        })
        .unwrap_err();
        assert!(error.contains("remaining files were kept"));
        assert_eq!(
            fs::read_to_string(scratch.0.join("keep/user-file.txt")).unwrap(),
            "keep me"
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_existing_symlinks_including_dangling_ones() {
        use std::os::unix::fs::symlink;
        let scratch = Scratch::new();
        symlink(scratch.0.join("absent"), scratch.0.join("link")).unwrap();
        assert!(create(&scratch.0, "link", Template::Python).is_err());
        assert!(!scratch.0.join("absent").exists());
    }
}
