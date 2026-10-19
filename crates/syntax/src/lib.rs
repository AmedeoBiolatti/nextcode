//! Incremental tree-sitter parsing and highlight spans for a `text::Buffer`.
//!
//! Grammars are not linked in: each lives in a library next to the editor
//! (`nc_grammar_<crate>.dll`/`.so`, see `crates/grammar_abi`) loaded the
//! first time its language is used, which keeps ~14 MB of parse tables out
//! of the executable (`grammars`).

mod grammars;

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

    /// Whether folding follows indentation instead of the syntax tree
    /// (languages whose blocks have no bracketed or indented-body nodes).
    pub fn folds_by_indentation(self) -> bool {
        matches!(self, Self::Bash | Self::Dockerfile | Self::Sql | Self::Yaml)
    }

    /// The grammar library holding this language (`nc_grammar_<name>`) and
    /// the entry's name in it.
    fn grammar(self) -> (&'static str, &'static str) {
        match self {
            Self::Bash => ("bash", "bash"),
            Self::C => ("c", "c"),
            Self::Cpp => ("cpp", "cpp"),
            Self::Css => ("css", "css"),
            Self::Dockerfile => ("containerfile", "containerfile"),
            Self::Go => ("go", "go"),
            Self::Html => ("html", "html"),
            Self::JavaScript => ("javascript", "javascript"),
            Self::Json => ("json", "json"),
            Self::Markdown => ("md", "markdown"),
            Self::Python => ("python", "python"),
            Self::Rust => ("rust", "rust"),
            Self::Sql => ("sequel", "sql"),
            Self::Toml => ("toml", "toml"),
            Self::Tsx => ("typescript", "tsx"),
            Self::TypeScript => ("typescript", "typescript"),
            Self::Yaml => ("yaml", "yaml"),
        }
    }

    /// Our refinements of the upstream highlight query (`queries/`), in
    /// order; for the same node a later pattern wins.
    fn refinements(self) -> &'static [&'static str] {
        match self {
            Self::Bash => &[BASH],
            Self::C => &[C],
            // The C++ queries extend the C ones, like the grammar does.
            Self::Cpp => &[C, CPP],
            Self::Css => &[CSS],
            Self::Dockerfile | Self::Sql => &[],
            Self::Go => &[GO],
            Self::Html => &[HTML],
            Self::JavaScript => &[ECMA, JAVASCRIPT],
            Self::Json => &[JSON],
            Self::Markdown => &[MARKDOWN],
            Self::Python => &[PYTHON],
            Self::Rust => &[RUST],
            Self::Toml => &[TOML],
            Self::Tsx | Self::TypeScript => &[ECMA, TYPESCRIPT],
            Self::Yaml => &[YAML],
        }
    }

    /// The grammar and compiled highlight query, loaded on first use;
    /// `None` when the grammar library is missing or broken (see
    /// [`problems`]).
    fn config(self) -> Option<&'static LanguageConfig> {
        macro_rules! config {
            ($language:ident) => {{
                static CONFIG: LazyLock<Option<LanguageConfig>> =
                    LazyLock::new(|| LanguageConfig::load(Language::$language));
                CONFIG.as_ref()
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
    /// Set once a parser refused the grammar (reported through
    /// [`problems`] once).
    rejected: std::sync::OnceLock<()>,
}

impl LanguageConfig {
    fn load(language: Language) -> Option<Self> {
        let (library, entry) = language.grammar();
        let result = grammars::entries(library)
            .and_then(|entries| {
                entries
                    .iter()
                    .find(|loaded| loaded.name == entry)
                    .ok_or_else(|| format!("the {library} grammar library has no {entry} grammar"))
            })
            .and_then(|loaded| {
                let grammar: Grammar = loaded.language.into();
                // Upstream queries first, then ours (`queries/`), which
                // refine them: for the same node the later pattern wins
                // (see `Syntax::highlights`).
                let query = std::iter::once(loaded.query.as_str())
                    .chain(language.refinements().iter().copied())
                    .collect::<Vec<_>>()
                    .join("\n");
                let query = Query::new(&grammar, &query)
                    .map_err(|err| format!("{} highlight query: {err}", language.name()))?;
                let highlights = query
                    .capture_names()
                    .iter()
                    .map(|name| Highlight::from_capture(name))
                    .collect();
                Ok(Self {
                    grammar,
                    query,
                    highlights,
                    rejected: std::sync::OnceLock::new(),
                })
            });
        match result {
            Ok(config) => Some(config),
            Err(error) => {
                grammars::report(language, error);
                None
            }
        }
    }
}

/// Languages whose grammar could not be loaded so far, with the reason
/// (no highlighting, folding or structure for them).
pub fn problems() -> Vec<(Language, String)> {
    grammars::problems()
}

pub struct Syntax {
    language: Language,
    config: &'static LanguageConfig,
    parser: Parser,
    tree: Option<Tree>,
}

impl Syntax {
    /// `None` when the language's grammar library is unavailable (see
    /// [`problems`]): the file is then plain text.
    pub fn new(language: Language, buffer: &Buffer) -> Option<Self> {
        let config = language.config()?;
        let mut parser = Parser::new();
        if let Err(error) = parser.set_language(&config.grammar) {
            // The library was built against another tree-sitter: plain
            // text, like a missing library (reported once per language).
            config.rejected.get_or_init(|| {
                grammars::report(language, format!("grammar library not usable: {error}"));
            });
            return None;
        }
        let mut syntax = Self {
            language,
            config,
            parser,
            tree: None,
        };
        syntax.reparse(buffer);
        Some(syntax)
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
        let config = self.config;
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
        sweep_spans(
            captures
                .into_iter()
                .map(|(node, _, highlight)| (node, highlight)),
            range,
        )
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

    /// Foldable line ranges `(header, last hidden line)` intersecting
    /// `lines`, sorted by header line, the largest per header line. Only
    /// nodes intersecting `lines` are visited.
    ///
    /// Foldable: bracketed nodes (`{}`, `[]`, `()`, whose closing line
    /// stays shown when the closer starts it), multi-line comments, Python
    /// blocks (from the line of their `:`), HTML and JSX elements,
    /// Markdown sections and fenced code, TOML tables.
    pub fn fold_ranges(&self, lines: Range<usize>) -> Vec<(usize, usize)> {
        let mut ranges: Vec<(usize, usize)> = self
            .scopes(lines.clone())
            .into_iter()
            .filter(|scope| scope.header < lines.end)
            .map(|scope| (scope.header, scope.last))
            .collect();
        ranges.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
        ranges.dedup_by_key(|range| range.0);
        ranges
    }

    /// The foldable regions of `fold_ranges` with the line each one's
    /// construct opens on (`def`/`class`/`fn` of a multi-line signature,
    /// after decorators), for those whose opening line intersects `lines`.
    /// Unsorted; several may share a header line.
    pub fn scopes(&self, lines: Range<usize>) -> Vec<Scope> {
        let Some(tree) = &self.tree else {
            return Vec::new();
        };
        let mut scopes = Vec::new();
        let mut stack = vec![tree.root_node()];
        let mut cursor = tree.walk();
        while let Some(node) = stack.pop() {
            if node.end_position().row < lines.start {
                continue;
            }
            // Checked before the start: a scope opens before its body.
            if let Some(scope) = self.fold_range(node)
                && scope.opening < lines.end
                && scope.last >= lines.start
            {
                scopes.push(scope);
            }
            if node.start_position().row < lines.end {
                stack.extend(node.children(&mut cursor));
            }
        }
        scopes
    }

    fn fold_range(&self, node: tree_sitter::Node<'_>) -> Option<Scope> {
        let start = node.start_position().row;
        let end_position = node.end_position();
        // A node ending at a line start (its line break included) ends on
        // the line before.
        let end = if end_position.column == 0 {
            end_position.row.saturating_sub(1)
        } else {
            end_position.row
        };
        // (A one-line Python block still folds under its header line.)
        if end <= start && node.kind() != "block" {
            return None;
        }
        // The closing line stays shown when the closer starts it.
        let closing_kept = |last: tree_sitter::Node<'_>| match last.prev_sibling() {
            Some(previous) if previous.end_position().row < last.start_position().row => end - 1,
            _ => end,
        };
        let kind = node.kind();
        let python_block = self.language == Language::Python && kind == "block";
        let range = match (self.language, kind) {
            (Language::Python, "block") => {
                // The `:` before the block; comments between them are
                // extras among the siblings.
                let mut colon = node.prev_sibling();
                while let Some(extra) = colon.filter(|sibling| sibling.is_extra()) {
                    colon = extra.prev_sibling();
                }
                let header = colon.map_or(start, |colon| colon.start_position().row);
                (header, end)
            }
            (Language::Html, "element" | "script_element" | "style_element")
            | (Language::JavaScript | Language::Tsx, "jsx_element") => {
                (start, closing_kept(node.child(node.child_count() - 1)?))
            }
            (Language::Markdown, "section" | "fenced_code_block")
            | (Language::Toml, "table" | "table_array_element") => (start, end),
            _ if kind.contains("comment") => (start, end),
            _ => {
                let count = node.child_count();
                let first = node.child(0)?;
                let last = node.child(count.checked_sub(1)?)?;
                let closer = match first.kind() {
                    "{" => "}",
                    "[" => "]",
                    "(" => ")",
                    _ => return None,
                };
                if count < 2 || last.kind() != closer || last.is_missing() {
                    return None;
                }
                (start, closing_kept(last))
            }
        };
        if range.1 <= range.0 {
            return None;
        }
        // A Python block, or a construct's `body`, opens with the
        // construct (its first token after decorators and comments).
        let opening = node
            .parent()
            .filter(|parent| {
                python_block && parent.kind() != "module"
                    || parent
                        .child_by_field_name("body")
                        .is_some_and(|body| body.id() == node.id())
            })
            .and_then(|parent| {
                let mut cursor = parent.walk();
                parent
                    .children(&mut cursor)
                    .find(|child| !child.is_extra() && child.kind() != "decorator")
                    .map(|child| child.start_position().row)
            })
            .map_or(range.0, |row| row.min(range.0));
        Some(Scope {
            opening,
            header: range.0,
            last: range.1,
        })
    }
}

/// A foldable region (see `Syntax::fold_ranges`) and where its construct
/// opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Scope {
    /// First line of the construct: `def`/`fn`/`class`/`if`… (the first of
    /// a multi-line signature, after decorators).
    pub opening: usize,
    /// The line that stays shown when folded.
    pub header: usize,
    /// Last hidden line.
    pub last: usize,
}

/// Non-overlapping spans within `range` from `captures` (node ranges with
/// their highlight, sorted by start, each outer node before the nodes it
/// contains): one sweep over the captures with a stack of the open nodes,
/// so the innermost paints each byte; no buffer the size of `range`.
fn sweep_spans(
    captures: impl IntoIterator<Item = (Range<usize>, Highlight)>,
    range: Range<usize>,
) -> Vec<HighlightSpan> {
    fn push(spans: &mut Vec<HighlightSpan>, range: Range<usize>, highlight: Highlight) {
        if range.start >= range.end {
            return;
        }
        match spans.last_mut() {
            Some(last) if last.highlight == highlight && last.range.end == range.start => {
                last.range.end = range.end;
            }
            _ => spans.push(HighlightSpan { range, highlight }),
        }
    }

    /// Paints `*at..to` with the innermost open node, closing the ones
    /// that end on the way.
    fn paint(
        spans: &mut Vec<HighlightSpan>,
        open: &mut Vec<(usize, Highlight)>,
        at: &mut usize,
        to: usize,
    ) {
        while *at < to {
            while open.last().is_some_and(|&(end, _)| end <= *at) {
                open.pop();
            }
            let Some(&(end, highlight)) = open.last() else {
                *at = to;
                return;
            };
            let stop = end.min(to);
            push(spans, *at..stop, highlight);
            *at = stop;
        }
    }

    let mut spans = Vec::new();
    // Open nodes, outermost first: nested, so their ends never increase.
    let mut open: Vec<(usize, Highlight)> = Vec::new();
    let mut at = range.start;
    for (node, highlight) in captures {
        let start = node.start.max(range.start);
        let end = node.end.min(range.end);
        if start >= end {
            continue;
        }
        paint(&mut spans, &mut open, &mut at, start);
        open.push((end, highlight));
    }
    paint(&mut spans, &mut open, &mut at, range.end);
    spans
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
        let syntax = Syntax::new(language, &buffer)
            .unwrap_or_else(|| panic!("{}: {:?}", language.name(), problems()));
        let spans = highlighted(&syntax, &buffer);
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
            let syntax = Syntax::new(language, &Buffer::new("x"))
                .unwrap_or_else(|| panic!("{}: {:?}", language.name(), problems()));
            assert!(syntax.tree.is_some(), "{}", language.name());
        }
        assert!(problems().is_empty(), "{:?}", problems());
    }

    #[test]
    fn every_capture_maps_to_a_scope_or_is_deliberately_unstyled() {
        for language in Language::ALL {
            let config = language
                .config()
                .unwrap_or_else(|| panic!("{}: {:?}", language.name(), problems()));
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
        let syntax = Syntax::new(Language::Rust, &buffer).expect("rust grammar");
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
    fn sweep_paints_the_innermost_node_and_merges_equal_neighbours() {
        let (a, b, c) = (h("keyword"), h("string"), h("comment"));
        let captures = [(0..10, a), (2..4, b), (3..4, c), (6..8, a), (12..14, b)];
        let spans = |range: Range<usize>| -> Vec<(Range<usize>, Highlight)> {
            sweep_spans(captures.iter().cloned(), range)
                .into_iter()
                .map(|span| (span.range, span.highlight))
                .collect()
        };
        assert_eq!(
            spans(0..20),
            [(0..2, a), (2..3, b), (3..4, c), (4..10, a), (12..14, b)]
        );
        // Clipped to the range, including nodes starting before it.
        assert_eq!(spans(1..9), [(1..2, a), (2..3, b), (3..4, c), (4..9, a)]);
        assert_eq!(spans(13..13), []);
        assert_eq!(spans(10..12), []);
    }

    #[test]
    fn incremental_reparse_matches_a_fresh_parse() {
        let mut buffer = Buffer::new("fn a() {}\nfn b() {}\n");
        let mut syntax = Syntax::new(Language::Rust, &buffer).expect("rust grammar");
        buffer.drain_changes();

        // Turn `b` into a struct and add a comment on the first line, across lines.
        buffer.edit(10..19, "struct B;");
        buffer.edit(9..9, " // c");
        for change in buffer.drain_changes() {
            syntax.edit(&change);
        }
        syntax.reparse(&buffer);

        let fresh = Syntax::new(Language::Rust, &buffer).expect("rust grammar");
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
        let syntax = Syntax::new(Language::Rust, &buffer).expect("rust grammar");
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

    fn folds(language: Language, source: &str) -> Vec<(usize, usize)> {
        let buffer = Buffer::new(source);
        Syntax::new(language, &buffer)
            .expect("grammar")
            .fold_ranges(0..buffer.line_count())
    }

    #[test]
    fn python_folds_def_and_class_bodies_from_the_colon_line() {
        let source = "\
class A:
    def f(self):
        x = 1
        return x

    def g(
        self,
    ):
        pass
        pass
y = 2
";
        assert_eq!(
            folds(Language::Python, source),
            // class body, f body, g's parameters (`)` line kept), g body.
            [(0, 9), (1, 3), (5, 6), (7, 9)]
        );
        // Asked for the `def f` line only: its body starts on the next line.
        let buffer = Buffer::new(source);
        let syntax = Syntax::new(Language::Python, &buffer).expect("python grammar");
        assert_eq!(syntax.fold_ranges(1..2), [(0, 9), (1, 3)]);
        assert_eq!(
            folds(Language::Python, "def f():\n    pass\nx = 1\n"),
            [(0, 1)]
        );
    }

    #[test]
    fn python_bodies_starting_with_a_comment_or_docstring_fold_from_the_colon() {
        let source = "\
class Inventory:
    \"\"\"Keeps track of stock.\"\"\"

    def receive(self, count):
        # Receive the items
        if count > 0:
            # positive
            return count
        return 0

    def ship(self, count):
        \"\"\"Ship them.\"\"\"
        return count
";
        assert_eq!(
            folds(Language::Python, source),
            [(0, 12), (3, 8), (5, 7), (10, 12)]
        );
    }

    fn openings(language: Language, source: &str) -> Vec<(usize, usize, usize)> {
        let buffer = Buffer::new(source);
        let mut scopes: Vec<_> = Syntax::new(language, &buffer)
            .expect("grammar")
            .scopes(0..buffer.line_count())
            .into_iter()
            .map(|s| (s.opening, s.header, s.last))
            .collect();
        scopes.sort();
        scopes
    }

    #[test]
    fn scopes_open_on_the_constructs_first_line() {
        let python = "\
class A:
    @property
    def g(
        self,
    ):
        # first a comment
        return 1

    def h(self):
        \"\"\"Doc.\"\"\"
        if x:
            pass
        else:
            # comment
            pass
";
        assert_eq!(
            openings(Language::Python, python),
            [
                (0, 0, 14),
                // `g`: parameters, then the body from `def` (not `@property`).
                (2, 2, 3),
                (2, 4, 6),
                (8, 8, 14),
                (10, 10, 11),
                (12, 12, 14)
            ]
        );
        let rust = "\
impl A {
    #[inline]
    fn g(
        a: u32,
    ) -> u32 {
        foo(
            a,
        )
    }
}
";
        assert_eq!(
            openings(Language::Rust, rust),
            // impl, parameters, body (from `fn`), call arguments (own line).
            [(0, 0, 8), (2, 2, 3), (2, 4, 7), (5, 5, 6)]
        );
        let js = "\
function f(a) {
  return g(
    a,
  );
}
";
        assert_eq!(openings(Language::JavaScript, js), [(0, 0, 3), (1, 1, 2)]);
    }

    #[test]
    fn rust_folds_fn_and_impl_blocks_keeping_the_closing_line() {
        let source = "\
impl A {
    fn f() {
        g();
    }
    /* one
       two */
}
fn h() { 1 }
";
        assert_eq!(folds(Language::Rust, source), [(0, 5), (1, 2), (4, 5)]);
    }

    #[test]
    fn json_folds_objects_and_arrays() {
        let source = "{\n  \"a\": {\n    \"b\": [\n      1,\n      2\n    ]\n  }\n}\n";
        assert_eq!(folds(Language::Json, source), [(0, 6), (1, 5), (2, 4)]);
        let buffer = Buffer::new(source);
        let syntax = Syntax::new(Language::Json, &buffer).expect("json grammar");
        // Only ranges touching the requested lines.
        assert_eq!(syntax.fold_ranges(4..5), [(0, 6), (1, 5), (2, 4)]);
        assert_eq!(syntax.fold_ranges(7..8), []);
    }

    #[test]
    fn markup_folds_elements_and_sections() {
        let html = "<div>\n  <p>a</p>\n  <p>b</p>\n</div>\n";
        assert_eq!(folds(Language::Html, html), [(0, 2)]);
        let markdown = "# A\n\ntext\n\n## B\n\nmore\n";
        assert_eq!(folds(Language::Markdown, markdown), [(0, 6), (4, 6)]);
    }
}
