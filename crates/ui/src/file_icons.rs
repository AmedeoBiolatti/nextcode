//! File and folder icons for trees (the Files panel): a folder glyph
//! colored by role (sources, tests, build output), and for files a
//! full-color pictogram of the type (`assets/icons/files`), or a plain
//! page.

use std::borrow::Cow;

use gpui::{
    AnyElement, AssetSource, Hsla, ImageSource, Resource, SharedString, div, img, prelude::*, px,
    rgb, svg,
};

use crate::theme;

/// The icons embedded in the executable: one-color glyphs for
/// `svg().path(…)`, full-color file types for `img(…)`.
pub struct Assets;

/// Embeds `assets/icons/files/<name>.svg` as `icons/files/<name>.svg`.
macro_rules! file_type_icons {
    ($($name:literal),* $(,)?) => {
        const FILE_TYPE_ICONS: &[(&str, &[u8])] = &[$((
            concat!("icons/files/", $name, ".svg"),
            include_bytes!(concat!("../assets/icons/files/", $name, ".svg")),
        )),*];
    };
}

file_type_icons![
    "archive",
    "c",
    "config",
    "cpp",
    "csharp",
    "css",
    "database",
    "docker",
    "docker_ignore",
    "env",
    "git",
    "go",
    "h",
    "html",
    "image",
    "java",
    "javascript",
    "json",
    "kotlin",
    "license",
    "lock",
    "lua",
    "make",
    "markdown",
    "notebook",
    "pdf",
    "php",
    "powershell",
    "python",
    "react",
    "react_ts",
    "ruby",
    "rust",
    "scss",
    "shell",
    "swift",
    "table",
    "toml",
    "typescript",
    "xml",
    "yaml",
];

/// Embeds `assets/icons/<name>.svg` as `icons/<name>.svg`.
macro_rules! glyph_icons {
    ($($name:literal),* $(,)?) => {
        const ICONS: &[(&str, &[u8])] = &[$((
            concat!("icons/", $name, ".svg"),
            include_bytes!(concat!("../assets/icons/", $name, ".svg")),
        )),*];
    };
}

glyph_icons![
    "folder",
    "folder_open",
    "file",
    "folder_package",
    "folder_package_open",
    "package_dot",
    "person",
    "arrow_down",
    "arrow_up",
    "branch",
    "check",
    "chevron_down",
    "chevron_right",
    "cloud",
    "collapse_all",
    "commit",
    "compare",
    "diff",
    "expand_all",
    "fetch",
    "group_dirs",
    "history",
    "minus",
    "maximize",
    "restore",
    "plus",
    "refresh",
    "rollback",
    "star",
    "tag",
    "worktree",
    "book",
    "close",
    "code",
    "flask",
    "gear",
    "keyboard",
    "palette",
    "search",
    "window",
    "workflow",
];

fn all_icons() -> impl Iterator<Item = &'static (&'static str, &'static [u8])> {
    ICONS.iter().chain(FILE_TYPE_ICONS)
}

impl AssetSource for Assets {
    fn load(&self, path: &str) -> anyhow::Result<Option<Cow<'static, [u8]>>> {
        Ok(all_icons()
            .find(|(name, _)| *name == path)
            .map(|(_, bytes)| Cow::Borrowed(*bytes)))
    }

    fn list(&self, path: &str) -> anyhow::Result<Vec<SharedString>> {
        Ok(all_icons()
            .filter(|(name, _)| name.starts_with(path))
            .map(|(name, _)| (*name).into())
            .collect())
    }
}

/// Side of an icon.
pub const SIZE: f32 = 16.;

/// What a folder holds, by its name (PyCharm's source, test and excluded
/// roots).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FolderRole {
    Plain,
    Sources,
    Tests,
    /// Build output, caches, environments, VCS data.
    Generated,
}

fn folder_role(name: &str) -> FolderRole {
    match name {
        "src" | "lib" | "crates" | "packages" | "app" => FolderRole::Sources,
        "test" | "tests" | "spec" | "specs" | "__tests__" | "testing" => FolderRole::Tests,
        "target" | "build" | "dist" | "out" | "node_modules" | ".venv" | "venv" | "env"
        | "__pycache__" | ".git" | ".idea" | ".mypy_cache" | ".pytest_cache" | ".ruff_cache"
        | ".tox" | ".next" | ".cache" | "coverage" => FolderRole::Generated,
        _ => FolderRole::Plain,
    }
}

/// The pictogram of a file named `name` (`icons/files/<icon>.svg`), by
/// its whole name first, then its extension.
fn file_type_icon(name: &str) -> Option<&'static str> {
    let lower = name.to_ascii_lowercase();
    let by_name = match lower.as_str() {
        "dockerfile" | "containerfile" => Some("docker"),
        ".dockerignore" => Some("docker_ignore"),
        "makefile" | "gnumakefile" | "justfile" | "cmakelists.txt" => Some("make"),
        ".gitignore" | ".gitattributes" | ".gitmodules" | ".gitkeep" => Some("git"),
        ".env" | ".env.local" | ".env.example" | ".envrc" => Some("env"),
        "license" | "license.md" | "license.txt" | "copying" => Some("license"),
        "cargo.lock" | "uv.lock" | "poetry.lock" | "pipfile.lock" | "package-lock.json"
        | "yarn.lock" | "pnpm-lock.yaml" | "composer.lock" | "gemfile.lock" => Some("lock"),
        _ => None,
    };
    if by_name.is_some() {
        return by_name;
    }
    if lower.starts_with("dockerfile.") || lower.ends_with(".dockerfile") {
        return Some("docker");
    }
    let extension = lower.rsplit_once('.')?.1;
    Some(match extension {
        "py" | "pyi" | "pyw" => "python",
        "ipynb" => "notebook",
        "rs" => "rust",
        "js" | "mjs" | "cjs" => "javascript",
        "jsx" => "react",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "react_ts",
        "json" | "jsonc" | "json5" => "json",
        "toml" => "toml",
        "yaml" | "yml" => "yaml",
        "md" | "markdown" | "mdx" | "rst" => "markdown",
        "html" | "htm" => "html",
        "xml" | "xsd" | "plist" => "xml",
        "css" | "less" => "css",
        "scss" | "sass" => "scss",
        "sh" | "bash" | "zsh" | "fish" => "shell",
        "ps1" | "psm1" | "bat" | "cmd" => "powershell",
        "go" => "go",
        "c" => "c",
        "h" | "hpp" | "hh" | "hxx" => "h",
        "cpp" | "cc" | "cxx" | "c++" => "cpp",
        "cs" => "csharp",
        "java" => "java",
        "kt" | "kts" => "kotlin",
        "rb" => "ruby",
        "php" => "php",
        "swift" => "swift",
        "lua" => "lua",
        "sql" | "sqlite" | "db" => "database",
        "csv" | "tsv" => "table",
        "ini" | "cfg" | "conf" | "properties" | "editorconfig" => "config",
        "png" | "jpg" | "jpeg" | "gif" | "bmp" | "ico" | "svg" | "webp" => "image",
        "pdf" => "pdf",
        "zip" | "tar" | "gz" | "tgz" | "xz" | "bz2" | "7z" | "whl" => "archive",
        "lock" => "lock",
        _ => return None,
    })
}

/// The icon of a folder, open or closed. A Python package (it holds an
/// `__init__.py`) gets a Python-yellow dot set into a notch at the
/// folder's bottom-right corner, so it reads on any row background.
pub fn folder(name: &str, open: bool, package: bool) -> AnyElement {
    let color: Hsla = match folder_role(name) {
        FolderRole::Plain => rgb(0x8496b0).into(),
        FolderRole::Sources => rgb(0x5a95e6).into(),
        FolderRole::Tests => rgb(0x5fae68).into(),
        FolderRole::Generated => rgb(0xc08a5b).into(),
    };
    let glyph = match (package, open) {
        (false, false) => "icons/folder.svg",
        (false, true) => "icons/folder_open.svg",
        (true, false) => "icons/folder_package.svg",
        (true, true) => "icons/folder_package_open.svg",
    };
    let folder = svg()
        .flex_none()
        .size(px(SIZE))
        .path(glyph)
        .text_color(color);
    if !package {
        return folder.into_any_element();
    }
    div()
        .flex_none()
        .relative()
        .size(px(SIZE))
        .child(folder)
        .child(
            svg()
                .absolute()
                .top_0()
                .left_0()
                .size(px(SIZE))
                .path("icons/package_dot.svg")
                .text_color(rgb(PYTHON_YELLOW)),
        )
        .into_any_element()
}

/// The dot of Python packages.
const PYTHON_YELLOW: u32 = 0xf2c94c;

/// Whether a folder named `name` may be a Python package worth checking
/// for an `__init__.py`: an identifier (importable), not generated.
pub fn may_be_package(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c == '_' || c.is_alphabetic())
        && chars.all(|c| c == '_' || c.is_alphanumeric())
        && folder_role(name) != FolderRole::Generated
}

/// The one-color glyph `assets/icons/<name>.svg` at icon size. An svg
/// takes no inherited text color: without `color` it is not drawn.
pub fn icon(name: &str, color: Hsla) -> gpui::Svg {
    svg()
        .flex_none()
        .size(px(SIZE))
        .path(SharedString::from(format!("icons/{name}.svg")))
        .text_color(color)
}

/// The icon of a file: its type's pictogram, or a plain page.
pub fn file(name: &str) -> AnyElement {
    let Some(icon) = file_type_icon(name) else {
        return svg()
            .flex_none()
            .size(px(SIZE))
            .path("icons/file.svg")
            .text_color(theme::muted())
            .into_any_element();
    };
    // An embedded resource, named explicitly: a `&str` source would be
    // tried as a URI first.
    let source =
        ImageSource::Resource(Resource::Embedded(format!("icons/files/{icon}.svg").into()));
    img(source).flex_none().size(px(SIZE)).into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_are_known_by_name_then_extension() {
        assert_eq!(file_type_icon("main.py"), Some("python"));
        assert_eq!(file_type_icon("Lib.RS"), Some("rust"));
        assert_eq!(file_type_icon("Dockerfile"), Some("docker"));
        assert_eq!(file_type_icon("Dockerfile.dev"), Some("docker"));
        assert_eq!(file_type_icon(".gitignore"), Some("git"));
        assert_eq!(file_type_icon("archive.tar.gz"), Some("archive"));
        assert_eq!(file_type_icon("Cargo.lock"), Some("lock"));
        assert_eq!(file_type_icon("package-lock.json"), Some("lock"));
        assert_eq!(file_type_icon("README"), None);
        assert_eq!(file_type_icon("notes.txt"), None);
    }

    #[test]
    fn every_file_type_has_its_pictogram() {
        let samples = [
            "a.py",
            "a.ipynb",
            "a.rs",
            "a.js",
            "a.jsx",
            "a.ts",
            "a.tsx",
            "a.json",
            "a.toml",
            "a.yaml",
            "a.md",
            "a.html",
            "a.xml",
            "a.css",
            "a.scss",
            "a.sh",
            "a.ps1",
            "a.go",
            "a.c",
            "a.h",
            "a.cpp",
            "a.cs",
            "a.java",
            "a.kt",
            "a.rb",
            "a.php",
            "a.swift",
            "a.lua",
            "a.sql",
            "a.csv",
            "a.ini",
            "a.png",
            "a.pdf",
            "a.zip",
            "a.lock",
            "Dockerfile",
            ".dockerignore",
            "Makefile",
            ".gitignore",
            ".env",
            "LICENSE",
        ];
        for sample in samples {
            let icon = file_type_icon(sample).unwrap_or_else(|| panic!("{sample}"));
            let path = format!("icons/files/{icon}.svg");
            assert!(Assets.load(&path).unwrap().is_some(), "{sample}: {path}");
        }
    }

    #[test]
    fn folders_get_their_role_by_name() {
        assert_eq!(folder_role("src"), FolderRole::Sources);
        assert_eq!(folder_role("tests"), FolderRole::Tests);
        assert_eq!(folder_role("node_modules"), FolderRole::Generated);
        assert_eq!(folder_role("docs"), FolderRole::Plain);
    }

    #[test]
    fn only_importable_folders_are_checked_for_packages() {
        assert!(may_be_package("mypkg"));
        assert!(may_be_package("_private2"));
        assert!(!may_be_package("my-app"));
        assert!(!may_be_package("2fa"));
        assert!(!may_be_package(".github"));
        assert!(!may_be_package("__pycache__"));
        assert!(!may_be_package("node_modules"));
        assert!(!may_be_package(""));
    }

    #[test]
    fn every_icon_path_is_embedded() {
        for (path, bytes) in all_icons() {
            assert!(Assets.load(path).unwrap().is_some(), "{path}");
            assert!(
                std::str::from_utf8(bytes).unwrap().starts_with("<svg"),
                "{path}"
            );
        }
        assert!(Assets.load("icons/missing.svg").unwrap().is_none());
    }
}
