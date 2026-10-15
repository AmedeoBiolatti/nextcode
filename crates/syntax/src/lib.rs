//! Incremental tree-sitter parsing and highlight spans for a `text::Buffer`.

use std::{ops::Range, path::Path, sync::LazyLock};

use text::{Buffer, Change};
use tree_sitter::{
    InputEdit, Language as Grammar, Parser, Point, Query, QueryCursor, StreamingIterator, Tree,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Language {
    Bash,
    C,
    Cpp,
    Css,
    Dockerfile,
    Go,
    Html,
    JavaScript,
    Json,
    Markdown,
    Python,
    Rust,
    Sql,
    Toml,
    Tsx,
    TypeScript,
    Yaml,
}

impl Language {
    pub const ALL: [Self; 17] = [
        Self::Bash,
        Self::C,
        Self::Cpp,
        Self::Css,
        Self::Dockerfile,
        Self::Go,
        Self::Html,
        Self::JavaScript,
        Self::Json,
        Self::Markdown,
        Self::Python,
        Self::Rust,
        Self::Sql,
        Self::Toml,
        Self::Tsx,
        Self::TypeScript,
        Self::Yaml,
    ];

    /// Detects the language from the file name, then the extension. `.h` is
    /// treated as C++, whose grammar also accepts most C headers.
    pub fn from_path(path: &Path) -> Option<Self> {
        let name = path.file_name()?.to_str()?;
        match name {
            "Dockerfile" | "Containerfile" => return Some(Self::Dockerfile),
            ".bashrc" | ".bash_profile" | ".bash_aliases" | ".bash_logout" | ".profile"
            | ".zshrc" | ".zshenv" | ".zprofile" => return Some(Self::Bash),
            "Cargo.lock" | "uv.lock" | "poetry.lock" | "Pipfile" => return Some(Self::Toml),
            ".clang-format" | ".clang-tidy" => return Some(Self::Yaml),
            _ if name.starts_with("Dockerfile.") || name.starts_with("Containerfile.") => {
                return Some(Self::Dockerfile);
            }
            _ => {}
        }
        let extension = path.extension()?.to_str()?.to_ascii_lowercase();
        Some(match extension.as_str() {
            "sh" | "bash" | "zsh" => Self::Bash,
            "c" => Self::C,
            "h" | "cc" | "cpp" | "cxx" | "c++" | "hh" | "hpp" | "hxx" | "h++" | "ipp" | "inl"
            | "tpp" => Self::Cpp,
            "css" => Self::Css,
            "dockerfile" | "containerfile" => Self::Dockerfile,
            "go" => Self::Go,
            "html" | "htm" | "xhtml" => Self::Html,
            "js" | "mjs" | "cjs" | "jsx" => Self::JavaScript,
            "json" | "jsonc" => Self::Json,
            "md" | "markdown" => Self::Markdown,
            "py" | "pyi" | "pyw" => Self::Python,
            "rs" => Self::Rust,
            "sql" => Self::Sql,
            "toml" => Self::Toml,
            "tsx" => Self::Tsx,
            "ts" | "mts" | "cts" => Self::TypeScript,
            "yaml" | "yml" => Self::Yaml,
            _ => return None,
        })
    }

    /// Identifier used as the LSP `languageId` and as the settings key.
    pub fn id(self) -> &'static str {
        match self {
            Self::Bash => "shellscript",
            Self::C => "c",
            Self::Cpp => "cpp",
            Self::Css => "css",
            Self::Dockerfile => "dockerfile",
            Self::Go => "go",
            Self::Html => "html",
            Self::JavaScript => "javascript",
            Self::Json => "json",
            Self::Markdown => "markdown",
            Self::Python => "python",
            Self::Rust => "rust",
            Self::Sql => "sql",
            Self::Toml => "toml",
            Self::Tsx => "typescriptreact",
            Self::TypeScript => "typescript",
            Self::Yaml => "yaml",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Bash => "Bash",
            Self::C => "C",
            Self::Cpp => "C++",
            Self::Css => "CSS",
            Self::Dockerfile => "Dockerfile",
            Self::Go => "Go",
            Self::Html => "HTML",
            Self::JavaScript => "JavaScript",
            Self::Json => "JSON",
            Self::Markdown => "Markdown",
            Self::Python => "Python",
            Self::Rust => "Rust",
            Self::Sql => "SQL",
            Self::Toml => "TOML",
            Self::Tsx => "TSX",
            Self::TypeScript => "TypeScript",
            Self::Yaml => "YAML",
        }
    }

    /// Line comment token, if the language has one (`#`, `//`, `--`).
    pub fn line_comment(self) -> Option<&'static str> {
        match self {
            Self::Bash | Self::Dockerfile | Self::Python | Self::Toml | Self::Yaml => Some("#"),
            Self::C
            | Self::Cpp
            | Self::Go
            | Self::JavaScript
            | Self::Json
            | Self::Rust
            | Self::Tsx
            | Self::TypeScript => Some("//"),
            Self::Sql => Some("--"),
            Self::Css | Self::Html | Self::Markdown => None,
        }
    }

    /// Block comment delimiters, if the language has them.
    pub fn block_comment(self) -> Option<(&'static str, &'static str)> {
        match self {
            Self::C
            | Self::Cpp
            | Self::Css
            | Self::Go
            | Self::JavaScript
            | Self::Json
            | Self::Rust
            | Self::Sql
            | Self::Tsx
            | Self::TypeScript => Some(("/*", "*/")),
            Self::Html | Self::Markdown => Some(("<!--", "-->")),
            Self::Bash | Self::Dockerfile | Self::Python | Self::Toml | Self::Yaml => None,
        }
    }

    fn config(self) -> &'static LanguageConfig {
        macro_rules! config {
            ($language:ident) => {{
                static CONFIG: LazyLock<LanguageConfig> =
                    LazyLock::new(|| LanguageConfig::new(Language::$language));
                &CONFIG
            }};
        }
        match self {
            Self::Bash => config!(Bash),
            Self::C => config!(C),
            Self::Cpp => config!(Cpp),
            Self::Css => config!(Css),
            Self::Dockerfile => config!(Dockerfile),
            Self::Go => config!(Go),
            Self::Html => config!(Html),
            Self::JavaScript => config!(JavaScript),
            Self::Json => config!(Json),
            Self::Markdown => config!(Markdown),
            Self::Python => config!(Python),
            Self::Rust => config!(Rust),
            Self::Sql => config!(Sql),
            Self::Toml => config!(Toml),
            Self::Tsx => config!(Tsx),
            Self::TypeScript => config!(TypeScript),
            Self::Yaml => config!(Yaml),
        }
    }
}

/// Highlight scopes: dotted names, most general part first. Themes style a
/// scope by its longest prefix they define (`function.method` falls back
/// to `function`). Language servers' semantic tokens map onto the same
/// scopes.
pub const SCOPES: &[&str] = &[
    "attribute",
    "boolean",
    "comment",
    "comment.doc",
    "constant",
    "constant.builtin",
    "constructor",
    "embedded",
    "function",
    "function.builtin",
    "function.macro",
    "function.method",
    "keyword",
    "keyword.control",
    "keyword.directive",
    "keyword.function",
    "keyword.import",
    "keyword.operator",
    "keyword.return",
    "keyword.storage",
    "keyword.type",
    "label",
    "markup.bold",
    "markup.heading",
    "markup.italic",
    "markup.link",
    "markup.list",
    "markup.quote",
    "markup.raw",
    "module",
    "number",
    "operator",
    "property",
    "punctuation",
    "punctuation.bracket",
    "punctuation.delimiter",
    "punctuation.special",
    "string",
    "string.escape",
    "string.regex",
    "string.special",
    "tag",
    "type",
    "type.builtin",
    "type.class",
    "type.enum",
    "type.interface",
    "type.parameter",
    "type.struct",
    "variable",
    "variable.builtin",
    "variable.member",
    "variable.parameter",
    "variant",
];

/// One of [`SCOPES`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Highlight(u16);

impl Highlight {
    /// The scope with exactly this name.
    pub fn from_scope(scope: &str) -> Option<Self> {
        SCOPES
            .iter()
            .position(|known| *known == scope)
            .map(|index| Self(index as u16))
    }

    pub fn scope(self) -> &'static str {
        SCOPES[self.0 as usize]
    }

    pub fn all() -> impl Iterator<Item = Self> {
        (0..SCOPES.len() as u16).map(Self)
    }

    /// Position in [`SCOPES`], for tables indexed by highlight.
    pub fn index(self) -> usize {
        self.0 as usize
    }

    /// Maps a query capture name: renamed captures first, then the name
    /// itself, then its parents (`function.method.call` → `function.method`).
    /// Unstyled and private (`@_name`) captures map to `None`.
    fn from_capture(name: &str) -> Option<Self> {
        if UNSTYLED_CAPTURES.contains(&name) || name.starts_with('_') {
            return None;
        }
        let mut name = name;
        loop {
            let scope = match name {
                // Neovim's names, current and pre-0.10 (tree-sitter-sequel, tree-sitter-md).
                "conditional"
                | "repeat"
                | "exception"
                | "keyword.conditional"
                | "keyword.repeat"
                | "keyword.exception" => "keyword.control",
                "include" => "keyword.import",
                "storageclass" | "type.qualifier" | "keyword.modifier" => "keyword.storage",
                "namespace" => "module",
                "float" => "number",
                "field" => "property",
                "parameter" => "variable.parameter",
                "method" => "function.method",
                "text.title" => "markup.heading",
                "text.literal" => "markup.raw",
                "text.uri" | "text.reference" => "markup.link",
                "text.emphasis" => "markup.italic",
                "text.strong" => "markup.bold",
                // tree-sitter's own names.
                "comment.documentation" => "comment.doc",
                "escape" => "string.escape",
                "delimiter" => "punctuation.delimiter",
                // C: function-like macro definitions.
                "function.special" => "function.macro",
                // JSON object keys.
                "string.special.key" => "property",
                other => other,
            };
            if let Some(highlight) = Self::from_scope(scope) {
                return Some(highlight);
            }
            name = &name[..name.rfind('.')?];
        }
    }
}

/// Captures that queries use deliberately without a style.
const UNSTYLED_CAPTURES: &[&str] = &["conceal", "none", "nospell", "spell"];

/// A highlighted byte range; spans from `Syntax::highlights` never overlap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HighlightSpan {
    pub range: Range<usize>,
    pub highlight: Highlight,
}

// Our refinements of the grammars' bundled highlight queries.
const BASH: &str = include_str!("../queries/bash.scm");
const C: &str = include_str!("../queries/c.scm");
const CPP: &str = include_str!("../queries/cpp.scm");
const CSS: &str = include_str!("../queries/css.scm");
const ECMA: &str = include_str!("../queries/ecma.scm");
const GO: &str = include_str!("../queries/go.scm");
const HTML: &str = include_str!("../queries/html.scm");
const JAVASCRIPT: &str = include_str!("../queries/javascript.scm");
const JSON: &str = include_str!("../queries/json.scm");
const MARKDOWN: &str = include_str!("../queries/markdown.scm");
const PYTHON: &str = include_str!("../queries/python.scm");
const RUST: &str = include_str!("../queries/rust.scm");
const TOML: &str = include_str!("../queries/toml.scm");
const TYPESCRIPT: &str = include_str!("../queries/typescript.scm");
const YAML: &str = include_str!("../queries/yaml.scm");

struct LanguageConfig {
    grammar: Grammar,
    query: Query,
    /// Highlight per query capture index.
    highlights: Vec<Option<Highlight>>,
}

impl LanguageConfig {
    fn new(language: Language) -> Self {
        // Upstream queries first, then ours (`queries/`), which refine them:
        // for the same node the later pattern wins (see `Syntax::highlights`).
        let (grammar, query): (Grammar, String) = match language {
            Language::Bash => (
                tree_sitter_bash::LANGUAGE.into(),
                [tree_sitter_bash::HIGHLIGHT_QUERY, BASH].join("\n"),
            ),
            Language::C => (
                tree_sitter_c::LANGUAGE.into(),
                [tree_sitter_c::HIGHLIGHT_QUERY, C].join("\n"),
            ),
            // The C++ queries extend the C ones, like the grammar does.
            Language::Cpp => (
                tree_sitter_cpp::LANGUAGE.into(),
                [
                    tree_sitter_c::HIGHLIGHT_QUERY,
                    tree_sitter_cpp::HIGHLIGHT_QUERY,
                    C,
                    CPP,
                ]
                .join("\n"),
            ),
            Language::Css => (
                tree_sitter_css::LANGUAGE.into(),
                [tree_sitter_css::HIGHLIGHTS_QUERY, CSS].join("\n"),
            ),
            Language::Dockerfile => (
                tree_sitter_containerfile::LANGUAGE.into(),
                tree_sitter_containerfile::HIGHLIGHTS_QUERY.into(),
            ),
            Language::Go => (
                tree_sitter_go::LANGUAGE.into(),
                [tree_sitter_go::HIGHLIGHTS_QUERY, GO].join("\n"),
            ),
            Language::Html => (
                tree_sitter_html::LANGUAGE.into(),
                [tree_sitter_html::HIGHLIGHTS_QUERY, HTML].join("\n"),
            ),
            Language::JavaScript => (
                tree_sitter_javascript::LANGUAGE.into(),
                [
                    tree_sitter_javascript::HIGHLIGHT_QUERY,
                    tree_sitter_javascript::JSX_HIGHLIGHT_QUERY,
                    ECMA,
                    JAVASCRIPT,
                ]
                .join("\n"),
            ),
            Language::Json => (
                tree_sitter_json::LANGUAGE.into(),
                [tree_sitter_json::HIGHLIGHTS_QUERY, JSON].join("\n"),
            ),
            // The block grammar only: inline markup needs a second parse.
            Language::Markdown => (
                tree_sitter_md::LANGUAGE.into(),
                [tree_sitter_md::HIGHLIGHT_QUERY_BLOCK, MARKDOWN].join("\n"),
            ),
            Language::Python => (
                tree_sitter_python::LANGUAGE.into(),
                [tree_sitter_python::HIGHLIGHTS_QUERY, PYTHON].join("\n"),
            ),
            Language::Rust => (
                tree_sitter_rust::LANGUAGE.into(),
                [tree_sitter_rust::HIGHLIGHTS_QUERY, RUST].join("\n"),
            ),
            // Written for Neovim: `#match?` uses Lua's `%d`, which Rust regex reads literally.
            Language::Sql => (
                tree_sitter_sequel::LANGUAGE.into(),
                tree_sitter_sequel::HIGHLIGHTS_QUERY.replace("%d", "\\\\d"),
            ),
            // Upstream captures the whole `key = value` pair as `@property`,
            // coloring the whitespace in it; capture the key instead.
            Language::Toml => (
                tree_sitter_toml_ng::LANGUAGE.into(),
                format!(
                    "{}\n{TOML}",
                    tree_sitter_toml_ng::HIGHLIGHTS_QUERY.replace(
                        "(pair\n  (bare_key)) @property",
                        "(pair\n  (bare_key) @property)"
                    )
                ),
            ),
            // The TypeScript queries extend the JavaScript ones, like the grammars do.
            Language::Tsx => (
                tree_sitter_typescript::LANGUAGE_TSX.into(),
                [
                    tree_sitter_javascript::HIGHLIGHT_QUERY,
                    tree_sitter_javascript::JSX_HIGHLIGHT_QUERY,
                    tree_sitter_typescript::HIGHLIGHTS_QUERY,
                    ECMA,
                    TYPESCRIPT,
                ]
                .join("\n"),
            ),
            Language::TypeScript => (
                tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
                [
                    tree_sitter_javascript::HIGHLIGHT_QUERY,
                    tree_sitter_typescript::HIGHLIGHTS_QUERY,
                    ECMA,
                    TYPESCRIPT,
                ]
                .join("\n"),
            ),
            Language::Yaml => (
                tree_sitter_yaml::LANGUAGE.into(),
                [tree_sitter_yaml::HIGHLIGHTS_QUERY, YAML].join("\n"),
            ),
        };
        let query = Query::new(&grammar, &query)
            .unwrap_or_else(|err| panic!("{} highlight query: {err}", language.name()));
        let highlights = query
            .capture_names()
            .iter()
            .map(|name| Highlight::from_capture(name))
            .collect();
        Self {
            grammar,
            query,
            highlights,
        }
    }
}

pub struct Syntax {
    language: Language,
    parser: Parser,
    tree: Option<Tree>,
}

impl Syntax {
    pub fn new(language: Language, buffer: &Buffer) -> Self {
        let mut parser = Parser::new();
        parser
            .set_language(&language.config().grammar)
            .expect("grammar ABI is supported by the tree-sitter runtime");
        let mut syntax = Self {
            language,
            parser,
            tree: None,
        };
        syntax.reparse(buffer);
        syntax
    }

    pub fn language(&self) -> Language {
        self.language
    }

    /// Tells the tree about an edit; call `reparse` after applying all of them.
    pub fn edit(&mut self, change: &Change) {
        if let Some(tree) = &mut self.tree {
            tree.edit(&InputEdit {
                start_byte: change.start,
                old_end_byte: change.old_end,
                new_end_byte: change.new_end,
                start_position: point(change.start_point),
                old_end_position: point(change.old_end_point),
                new_end_position: point(change.new_end_point),
            });
        }
    }

    /// Reparses, reusing unchanged subtrees from the edited previous tree.
    pub fn reparse(&mut self, buffer: &Buffer) {
        self.tree = self.parser.parse_with_options(
            &mut |offset, _| buffer.chunk_at(offset).as_bytes(),
            self.tree.as_ref(),
            None,
        );
    }

    /// Non-overlapping highlight spans intersecting `range`, sorted by start.
    ///
    /// Inner nodes override enclosing ones. For the same node the latest
    /// colored pattern wins: the bundled queries list generic patterns
    /// (`(identifier) @variable`) before specific ones, and C++ appends to C.
    pub fn highlights(&self, buffer: &Buffer, range: Range<usize>) -> Vec<HighlightSpan> {
        let Some(tree) = &self.tree else {
            return Vec::new();
        };
        let config = self.language.config();
        let mut cursor = QueryCursor::new();
        cursor.set_byte_range(range.clone());
        let text = |node: tree_sitter::Node| buffer.chunks(node.byte_range()).map(str::as_bytes);
        let mut matches = cursor.matches(&config.query, tree.root_node(), text);

        let mut captures = Vec::new();
        while let Some(m) = matches.next() {
            for capture in m.captures() {
                if let Some(highlight) = config.highlights[capture.index as usize] {
                    captures.push((capture.node.byte_range(), m.pattern_index, highlight));
                }
            }
        }
        // Outer nodes first so inner ones paint over them; latest pattern first per node.
        captures.sort_by(|(a, a_pattern, _), (b, b_pattern, _)| {
            a.start
                .cmp(&b.start)
                .then(b.end.cmp(&a.end))
                .then(b_pattern.cmp(a_pattern))
        });
        captures.dedup_by(|(later, ..), (earlier, ..)| later == earlier);

        let mut layer = vec![None; range.len()];
        for (node, _, highlight) in captures {
            let start = node.start.max(range.start) - range.start;
            let end = node.end.min(range.end).saturating_sub(range.start);
            if start < end {
                layer[start..end].fill(Some(highlight));
            }
        }

        let mut spans: Vec<HighlightSpan> = Vec::new();
        for (i, highlight) in layer.into_iter().enumerate() {
            let Some(highlight) = highlight else { continue };
            let offset = range.start + i;
            match spans.last_mut() {
                Some(last) if last.highlight == highlight && last.range.end == offset => {
                    last.range.end += 1;
                }
                _ => spans.push(HighlightSpan {
                    range: offset..offset + 1,
                    highlight,
                }),
            }
        }
        spans
    }

    /// The bracket matching the `(`, `)`, `[`, `]`, `{` or `}` token at
    /// `offset`, found among the token's siblings in the tree. `None` when
    /// the character there is no bracket token (e.g. inside a string or
    /// comment, or prose); `Some(None)` when the token has no partner.
    pub fn bracket_partner(&self, offset: usize) -> Option<Option<usize>> {
        let tree = self.tree.as_ref()?;
        let node = tree
            .root_node()
            .descendant_for_byte_range(offset, offset + 1)?;
        let (open, close, forward) = match node.kind() {
            "(" => ("(", ")", true),
            "[" => ("[", "]", true),
            "{" => ("{", "}", true),
            ")" => ("(", ")", false),
            "]" => ("[", "]", false),
            "}" => ("{", "}", false),
            _ => return None,
        };
        if node.is_named() || node.start_byte() != offset || node.end_byte() != offset + 1 {
            return None;
        }
        let (same, other) = if forward {
            (open, close)
        } else {
            (close, open)
        };
        fn step(node: tree_sitter::Node<'_>, forward: bool) -> Option<tree_sitter::Node<'_>> {
            if forward {
                node.next_sibling()
            } else {
                node.prev_sibling()
            }
        }
        let mut depth = 0usize;
        let mut sibling = step(node, forward);
        while let Some(candidate) = sibling {
            let kind = candidate.kind();
            if kind == same && !candidate.is_missing() {
                depth += 1;
            } else if kind == other {
                if depth == 0 {
                    return Some((!candidate.is_missing()).then(|| candidate.start_byte()));
                }
                depth -= 1;
            }
            sibling = step(candidate, forward);
        }
        Some(None)
    }
}

fn point(point: text::Point) -> Point {
    Point {
        row: point.row,
        column: point.column,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(scope: &str) -> Highlight {
        Highlight::from_scope(scope).unwrap_or_else(|| panic!("unknown scope {scope:?}"))
    }

    fn highlighted(syntax: &Syntax, buffer: &Buffer) -> Vec<(String, &'static str)> {
        syntax
            .highlights(buffer, 0..buffer.len())
            .into_iter()
            .map(|span| (buffer.slice(span.range), span.highlight.scope()))
            .collect()
    }

    fn assert_highlights(language: Language, source: &str, expected: &[(&str, &str)]) {
        let buffer = Buffer::new(source);
        let spans = highlighted(&Syntax::new(language, &buffer), &buffer);
        for &(text, scope) in expected {
            h(scope);
            assert!(
                spans.contains(&(text.to_owned(), scope)),
                "{}: missing {text:?} as {scope:?} in {spans:?}",
                language.name()
            );
        }
    }

    #[test]
    fn every_language_query_compiles_and_parses() {
        for language in Language::ALL {
            let syntax = Syntax::new(language, &Buffer::new("x"));
            assert!(syntax.tree.is_some(), "{}", language.name());
        }
    }

    #[test]
    fn every_capture_maps_to_a_scope_or_is_deliberately_unstyled() {
        for language in Language::ALL {
            let config = language.config();
            for (name, highlight) in config.query.capture_names().iter().zip(&config.highlights) {
                assert!(
                    highlight.is_some() || UNSTYLED_CAPTURES.contains(name),
                    "{}: capture @{name} has no scope",
                    language.name()
                );
            }
        }
    }

    #[test]
    fn captures_fall_back_to_their_parents_and_skip_unstyled_ones() {
        assert_eq!(
            Highlight::from_capture("function.method.call"),
            Some(h("function.method"))
        );
        assert_eq!(
            Highlight::from_capture("text.title"),
            Some(h("markup.heading"))
        );
        assert_eq!(
            Highlight::from_capture("string.special.key"),
            Some(h("property"))
        );
        assert_eq!(Highlight::from_capture("none"), None);
        assert_eq!(Highlight::from_capture("spell"), None);
        assert_eq!(Highlight::from_capture("_name"), None);
    }

    #[test]
    fn python_highlights_prefer_later_patterns_and_inner_nodes() {
        assert_highlights(
            Language::Python,
            "MAX = 1\ndef f(x, *rest, key=None):\n    return \"a\\n\"  # done\n",
            &[
                // `@constant` (by #match?) comes after `@variable` and `@constructor`.
                ("MAX", "constant"),
                ("1", "number"),
                ("def", "keyword.function"),
                ("f", "function"),
                ("x", "variable.parameter"),
                ("rest", "variable.parameter"),
                ("key", "variable.parameter"),
                ("None", "constant.builtin"),
                ("return", "keyword.return"),
                // The escape sequence splits the enclosing string.
                ("\"a", "string"),
                ("\\n", "string.escape"),
                ("# done", "comment"),
            ],
        );
    }

    /// A language, a source and the `(text, scope)` spans it must contain.
    type Case = (
        Language,
        &'static str,
        &'static [(&'static str, &'static str)],
    );

    #[test]
    fn each_language_highlights_its_core_constructs() {
        let cases: &[Case] = &[
            (
                Language::Bash,
                "#!/bin/bash\nfoo() { echo \"$HOME\"; }\nfunction bar { ls -l; }\nfoo\n",
                &[
                    ("#!/bin/bash", "keyword.directive"),
                    ("foo", "function"),
                    ("echo", "function.builtin"),
                    ("$", "punctuation.special"),
                    ("HOME", "variable"),
                    ("function", "keyword.function"),
                    ("ls", "function"),
                ],
            ),
            (
                Language::C,
                "#include <stdio.h>\n#define N 1\nint main(int argc, char **argv) { if (argc) return 0; } // x",
                &[
                    ("#include", "keyword.import"),
                    ("<stdio.h>", "string"),
                    ("#define", "keyword.directive"),
                    ("N", "constant"),
                    ("int", "type.builtin"),
                    ("main", "function"),
                    ("argc", "variable.parameter"),
                    ("argv", "variable.parameter"),
                    ("if", "keyword.control"),
                    ("return", "keyword.return"),
                    ("// x", "comment"),
                ],
            ),
            (
                Language::Cpp,
                "namespace ns { class A { void f() { this->g(); } }; }",
                &[
                    ("namespace", "keyword.type"),
                    ("ns", "module"),
                    ("class", "keyword.type"),
                    ("void", "type.builtin"),
                    ("this", "variable.builtin"),
                    ("g", "function"),
                ],
            ),
            (
                Language::Css,
                "@import \"a.css\";\n.btn > a:hover { color: red; margin: 0 4px; }",
                &[
                    ("@import", "keyword.import"),
                    ("btn", "property"),
                    ("a", "tag"),
                    ("hover", "attribute"),
                    ("color", "property"),
                    ("red", "constant"),
                    ("4", "number"),
                    ("px", "type"),
                ],
            ),
            (
                Language::Dockerfile,
                "FROM rust:1 AS build\nRUN cargo build\n# c",
                &[
                    ("FROM", "keyword"),
                    ("AS", "keyword"),
                    ("RUN", "keyword"),
                    ("# c", "comment"),
                ],
            ),
            (
                Language::Go,
                "package main\nimport \"fmt\"\nfunc add(a int, b int) int { return a + b }\nfunc main() { fmt.Println(add(1, 2)) }\n",
                &[
                    ("package", "keyword.import"),
                    ("main", "module"),
                    ("\"fmt\"", "string"),
                    ("func", "keyword.function"),
                    ("add", "function"),
                    ("a", "variable.parameter"),
                    ("int", "type.builtin"),
                    ("return", "keyword.return"),
                    ("Println", "function.method"),
                    ("1", "number"),
                ],
            ),
            (
                Language::Html,
                "<!DOCTYPE html>\n<a href=\"x\">&amp;</a><!-- c -->",
                &[
                    ("<!", "punctuation.bracket"),
                    ("DOCTYPE html", "keyword.directive"),
                    ("<", "punctuation.bracket"),
                    ("a", "tag"),
                    ("href", "attribute"),
                    ("\"x\"", "string"),
                    ("&amp;", "string.escape"),
                    ("<!-- c -->", "comment"),
                ],
            ),
            (
                Language::JavaScript,
                "import { a as b } from \"m\";\nconst x = /re/g;\nfunction f(p, { q }) { return new Foo(p) ?? q; }\n// c",
                &[
                    ("import", "keyword.import"),
                    ("from", "keyword.import"),
                    ("\"m\"", "string"),
                    ("const", "keyword.storage"),
                    ("/re/g", "string.regex"),
                    ("function", "keyword.function"),
                    ("f", "function"),
                    ("p", "variable.parameter"),
                    ("q", "variable.parameter"),
                    ("return", "keyword.return"),
                    ("new", "keyword.operator"),
                    ("Foo", "constructor"),
                    ("// c", "comment"),
                ],
            ),
            (
                Language::Json,
                "{\"key\": \"value\", \"n\": 1, \"b\": true, \"z\": null}",
                &[
                    ("\"key\"", "property"),
                    ("\"value\"", "string"),
                    ("1", "number"),
                    ("true", "boolean"),
                    ("null", "constant.builtin"),
                    ("{", "punctuation.bracket"),
                ],
            ),
            (
                Language::Markdown,
                "# Title\n\n- item\n\n> quote\n\n```rs\nfn x() {}\n```\n",
                &[
                    ("#", "markup.heading"),
                    ("Title", "markup.heading"),
                    ("- ", "markup.list"),
                    ("> quote\n", "markup.quote"),
                    ("rs", "label"),
                    ("\nfn x() {}\n", "markup.raw"),
                ],
            ),
            (
                Language::Python,
                "import os\n@dec\nclass A:\n    \"\"\"Doc.\"\"\"\n    def m(self):\n        return True and self.x\n",
                &[
                    ("import", "keyword.import"),
                    ("@dec", "attribute"),
                    ("class", "keyword.type"),
                    ("A", "type"),
                    ("\"\"\"Doc.\"\"\"", "comment.doc"),
                    ("m", "function.method"),
                    ("self", "variable.builtin"),
                    ("True", "boolean"),
                    ("and", "keyword.operator"),
                    ("x", "property"),
                ],
            ),
            (
                Language::Rust,
                "/// d\n#[test]\nfn t<'a>(x: &'a str) { 'outer: loop { break 'outer; } println!(\"{}\", x); s.m(1); }",
                &[
                    ("/// d\n", "comment.doc"),
                    ("#[test]", "attribute"),
                    ("fn", "keyword.function"),
                    ("t", "function"),
                    ("'a", "label"),
                    ("x", "variable.parameter"),
                    ("str", "type.builtin"),
                    ("'outer", "label"),
                    ("loop", "keyword.control"),
                    ("println!", "function.macro"),
                    ("m", "function.method"),
                    ("1", "number"),
                ],
            ),
            (
                Language::Sql,
                "SELECT id FROM users WHERE id = 1; -- c",
                &[("SELECT", "keyword"), ("1", "number"), ("-- c", "comment")],
            ),
            (
                Language::Toml,
                "[package]\nname = \"x\" # c\n[dependencies.serde]\nversion = 1\nenabled = true\ndate = 1979-05-27\n",
                &[
                    ("package", "type"),
                    ("name", "property"),
                    ("\"x\"", "string"),
                    ("# c", "comment"),
                    ("serde", "type"),
                    ("version", "property"),
                    ("1", "number"),
                    ("true", "boolean"),
                    ("1979-05-27", "string.special"),
                ],
            ),
            (
                Language::Tsx,
                "const el = <div className=\"a\">{x}</div>;",
                &[
                    ("const", "keyword.storage"),
                    ("div", "tag"),
                    ("className", "attribute"),
                ],
            ),
            (
                Language::TypeScript,
                "interface P { n: number }\nfunction f(p: P, s?: string): void {}\nlet x = y as P;",
                &[
                    ("interface", "keyword.type"),
                    ("P", "type"),
                    ("n", "property"),
                    ("number", "type.builtin"),
                    ("function", "keyword.function"),
                    ("p", "variable.parameter"),
                    ("s", "variable.parameter"),
                    ("as", "keyword.operator"),
                ],
            ),
            (
                Language::Yaml,
                "key: value\nlist:\n  - \"quoted\"\nanchor: &a 1\nflag: true\nnothing: null # c\n",
                &[
                    ("key", "property"),
                    ("value", "string"),
                    (":", "punctuation.delimiter"),
                    ("\"quoted\"", "string"),
                    ("a", "label"),
                    ("1", "number"),
                    ("true", "boolean"),
                    ("null", "constant.builtin"),
                    ("# c", "comment"),
                ],
            ),
        ];
        for &(language, source, expected) in cases {
            assert_highlights(language, source, expected);
        }
        let covered: Vec<Language> = cases.iter().map(|case| case.0).collect();
        for language in Language::ALL {
            assert!(
                covered.contains(&language),
                "no case for {}",
                language.name()
            );
        }
    }

    #[test]
    fn from_path_recognizes_file_names_and_extensions() {
        for (path, language) in [
            ("src/main.rs", Language::Rust),
            ("include/a.h", Language::Cpp),
            ("Dockerfile", Language::Dockerfile),
            ("docker/Dockerfile.dev", Language::Dockerfile),
            ("api.dockerfile", Language::Dockerfile),
            ("Containerfile", Language::Dockerfile),
            ("/home/u/.bashrc", Language::Bash),
            (".bash_profile", Language::Bash),
            (".zshrc", Language::Bash),
            ("run.sh", Language::Bash),
            ("Cargo.lock", Language::Toml),
            ("uv.lock", Language::Toml),
            ("poetry.lock", Language::Toml),
            ("pyproject.toml", Language::Toml),
            ("tsconfig.jsonc", Language::Json),
            ("a.mjs", Language::JavaScript),
            ("a.cjs", Language::JavaScript),
            ("a.jsx", Language::JavaScript),
            ("a.mts", Language::TypeScript),
            ("a.cts", Language::TypeScript),
            ("App.tsx", Language::Tsx),
            ("ci.yml", Language::Yaml),
            ("README.markdown", Language::Markdown),
            ("README.MD", Language::Markdown),
            ("index.htm", Language::Html),
            ("style.css", Language::Css),
            ("main.go", Language::Go),
        ] {
            assert_eq!(
                Language::from_path(Path::new(path)),
                Some(language),
                "{path}"
            );
        }
        for path in ["Makefile", "notes.txt", "Cargo", ".gitignore"] {
            assert_eq!(Language::from_path(Path::new(path)), None, "{path}");
        }
    }

    #[test]
    fn spans_are_clipped_to_the_requested_range() {
        let buffer = Buffer::new("fn main() {}\n");
        let syntax = Syntax::new(Language::Rust, &buffer);
        let spans = syntax.highlights(&buffer, 1..buffer.len());
        assert_eq!(
            spans.first(),
            Some(&HighlightSpan {
                range: 1..2,
                highlight: h("keyword.function")
            })
        );
    }

    #[test]
    fn incremental_reparse_matches_a_fresh_parse() {
        let mut buffer = Buffer::new("fn a() {}\nfn b() {}\n");
        let mut syntax = Syntax::new(Language::Rust, &buffer);
        buffer.drain_changes();

        // Turn `b` into a struct and add a comment on the first line, across lines.
        buffer.edit(10..19, "struct B;");
        buffer.edit(9..9, " // c");
        for change in buffer.drain_changes() {
            syntax.edit(&change);
        }
        syntax.reparse(&buffer);

        let fresh = Syntax::new(Language::Rust, &buffer);
        assert_eq!(
            syntax.tree.as_ref().unwrap().root_node().to_sexp(),
            fresh.tree.as_ref().unwrap().root_node().to_sexp()
        );
        assert_eq!(highlighted(&syntax, &buffer), highlighted(&fresh, &buffer));
    }

    #[test]
    fn every_language_has_a_comment_syntax() {
        for language in Language::ALL {
            assert!(
                language.line_comment().is_some() || language.block_comment().is_some(),
                "{} has no comments",
                language.name()
            );
        }
        assert_eq!(Language::Python.line_comment(), Some("#"));
        assert_eq!(Language::Sql.line_comment(), Some("--"));
        assert_eq!(Language::Html.block_comment(), Some(("<!--", "-->")));
    }

    #[test]
    fn bracket_partners_come_from_the_tree_and_skip_strings() {
        let source = "fn f(a: [u8; 2]) { g(\"(\", (1)); }";
        let buffer = Buffer::new(source);
        let syntax = Syntax::new(Language::Rust, &buffer);
        let at = |pattern: &str, nth: usize| source.match_indices(pattern).nth(nth).unwrap().0;
        // `f(` ↔ the `)` before ` {`.
        assert_eq!(syntax.bracket_partner(at("(", 0)), Some(Some(at(")", 0))));
        assert_eq!(syntax.bracket_partner(at(")", 0)), Some(Some(at("(", 0))));
        assert_eq!(syntax.bracket_partner(at("[", 0)), Some(Some(at("]", 0))));
        assert_eq!(syntax.bracket_partner(at("{", 0)), Some(Some(at("}", 0))));
        // `g(` skips the `(` inside the string and the nested `(1)`.
        assert_eq!(
            syntax.bracket_partner(at("g(", 0) + 1),
            Some(Some(at(")", 2)))
        );
        assert_eq!(
            syntax.bracket_partner(at(")", 2)),
            Some(Some(at("g(", 0) + 1))
        );
        assert_eq!(
            syntax.bracket_partner(at("\"(", 0) + 1),
            None,
            "inside a string"
        );
        assert_eq!(syntax.bracket_partner(at("fn", 0)), None);
    }
}
