//! Incremental tree-sitter parsing and highlight spans for a `text::Buffer`.

use std::{ops::Range, path::Path, sync::LazyLock};

use text::{Buffer, Change};
use tree_sitter::{
    InputEdit, Language as Grammar, Parser, Point, Query, QueryCursor, StreamingIterator, Tree,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Language {
    C,
    Cpp,
    Python,
    Rust,
    Sql,
}

impl Language {
    pub const ALL: [Self; 5] = [Self::C, Self::Cpp, Self::Python, Self::Rust, Self::Sql];

    /// Detects the language from the file extension. `.h` is treated as C++,
    /// whose grammar also accepts most C headers.
    pub fn from_path(path: &Path) -> Option<Self> {
        let extension = path.extension()?.to_str()?.to_ascii_lowercase();
        Some(match extension.as_str() {
            "c" => Self::C,
            "h" | "cc" | "cpp" | "cxx" | "c++" | "hh" | "hpp" | "hxx" | "h++" | "ipp" | "inl"
            | "tpp" => Self::Cpp,
            "py" | "pyi" | "pyw" => Self::Python,
            "rs" => Self::Rust,
            "sql" => Self::Sql,
            _ => return None,
        })
    }

    /// Identifier used as the LSP `languageId` and as the settings key.
    pub fn id(self) -> &'static str {
        match self {
            Self::C => "c",
            Self::Cpp => "cpp",
            Self::Python => "python",
            Self::Rust => "rust",
            Self::Sql => "sql",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::C => "C",
            Self::Cpp => "C++",
            Self::Python => "Python",
            Self::Rust => "Rust",
            Self::Sql => "SQL",
        }
    }

    fn config(self) -> &'static LanguageConfig {
        static C: LazyLock<LanguageConfig> = LazyLock::new(|| LanguageConfig::new(Language::C));
        static CPP: LazyLock<LanguageConfig> = LazyLock::new(|| LanguageConfig::new(Language::Cpp));
        static PYTHON: LazyLock<LanguageConfig> =
            LazyLock::new(|| LanguageConfig::new(Language::Python));
        static RUST: LazyLock<LanguageConfig> =
            LazyLock::new(|| LanguageConfig::new(Language::Rust));
        static SQL: LazyLock<LanguageConfig> = LazyLock::new(|| LanguageConfig::new(Language::Sql));
        match self {
            Self::C => &C,
            Self::Cpp => &CPP,
            Self::Python => &PYTHON,
            Self::Rust => &RUST,
            Self::Sql => &SQL,
        }
    }
}

/// Highlight categories; the UI maps them to colors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Highlight {
    Keyword,
    String,
    Escape,
    Comment,
    DocComment,
    Number,
    Constant,
    Function,
    Macro,
    Type,
    Property,
    Attribute,
}

impl Highlight {
    /// Maps a capture name, falling back to its parent (`function.method` → `function`).
    fn from_capture(name: &str) -> Option<Self> {
        let mut name = name;
        loop {
            let highlight = match name {
                "keyword" | "conditional" | "repeat" | "storageclass" | "include"
                | "type.qualifier" | "variable.builtin" => Self::Keyword,
                "string" => Self::String,
                "escape" => Self::Escape,
                "comment" => Self::Comment,
                "comment.documentation" => Self::DocComment,
                "number" | "float" | "boolean" | "constant.builtin" => Self::Number,
                "constant" => Self::Constant,
                "function" => Self::Function,
                "function.macro" | "function.special" => Self::Macro,
                "type" | "constructor" => Self::Type,
                "property" | "field" => Self::Property,
                "attribute" | "label" => Self::Attribute,
                _ => {
                    name = &name[..name.rfind('.')?];
                    continue;
                }
            };
            return Some(highlight);
        }
    }
}

/// A highlighted byte range; spans from `Syntax::highlights` never overlap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HighlightSpan {
    pub range: Range<usize>,
    pub highlight: Highlight,
}

struct LanguageConfig {
    grammar: Grammar,
    query: Query,
    /// Highlight per query capture index.
    highlights: Vec<Option<Highlight>>,
}

impl LanguageConfig {
    fn new(language: Language) -> Self {
        let (grammar, query): (Grammar, String) = match language {
            Language::C => (
                tree_sitter_c::LANGUAGE.into(),
                tree_sitter_c::HIGHLIGHT_QUERY.into(),
            ),
            // The C++ query extends the C one, like the grammar does.
            Language::Cpp => (
                tree_sitter_cpp::LANGUAGE.into(),
                format!(
                    "{}\n{}",
                    tree_sitter_c::HIGHLIGHT_QUERY,
                    tree_sitter_cpp::HIGHLIGHT_QUERY
                ),
            ),
            Language::Python => (
                tree_sitter_python::LANGUAGE.into(),
                tree_sitter_python::HIGHLIGHTS_QUERY.into(),
            ),
            Language::Rust => (
                tree_sitter_rust::LANGUAGE.into(),
                tree_sitter_rust::HIGHLIGHTS_QUERY.into(),
            ),
            // Written for Neovim: `#match?` uses Lua's `%d`, which Rust regex reads literally.
            Language::Sql => (
                tree_sitter_sequel::LANGUAGE.into(),
                tree_sitter_sequel::HIGHLIGHTS_QUERY.replace("%d", "\\\\d"),
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

    fn highlighted(syntax: &Syntax, buffer: &Buffer) -> Vec<(String, Highlight)> {
        syntax
            .highlights(buffer, 0..buffer.len())
            .into_iter()
            .map(|span| (buffer.slice(span.range), span.highlight))
            .collect()
    }

    #[test]
    fn every_language_query_compiles_and_parses() {
        for language in Language::ALL {
            let syntax = Syntax::new(language, &Buffer::new("x"));
            assert!(syntax.tree.is_some(), "{}", language.name());
        }
    }

    #[test]
    fn python_highlights_prefer_later_patterns_and_inner_nodes() {
        let buffer = Buffer::new("MAX = 1\ndef f(x):\n    return \"a\\n\"  # done\n");
        let syntax = Syntax::new(Language::Python, &buffer);
        let spans = highlighted(&syntax, &buffer);
        for expected in [
            // `@constant` (by #match?) comes after `@variable` and `@constructor`.
            ("MAX", Highlight::Constant),
            ("1", Highlight::Number),
            ("def", Highlight::Keyword),
            ("f", Highlight::Function),
            ("return", Highlight::Keyword),
            // The escape sequence splits the enclosing string.
            ("\"a", Highlight::String),
            ("\\n", Highlight::Escape),
            ("# done", Highlight::Comment),
        ] {
            assert!(
                spans.contains(&(expected.0.to_owned(), expected.1)),
                "missing {expected:?} in {spans:?}"
            );
        }
    }

    #[test]
    fn each_language_highlights_its_core_constructs() {
        let cases = [
            (
                Language::C,
                "int main(void) { return 0; } // x",
                [
                    ("return", Highlight::Keyword),
                    ("main", Highlight::Function),
                    ("// x", Highlight::Comment),
                ],
            ),
            (
                Language::Cpp,
                "class A { void f() { this->g(); } };",
                [
                    ("class", Highlight::Keyword),
                    ("this", Highlight::Keyword),
                    ("g", Highlight::Function),
                ],
            ),
            (
                Language::Rust,
                "/// d\n#[test]\nfn t() { s.m(\"x\"); }",
                [
                    ("/// d\n", Highlight::DocComment),
                    ("fn", Highlight::Keyword),
                    ("m", Highlight::Function),
                ],
            ),
            (
                Language::Python,
                "@dec\nclass A: pass",
                [
                    ("class", Highlight::Keyword),
                    ("pass", Highlight::Keyword),
                    ("@dec", Highlight::Function),
                ],
            ),
            (
                Language::Sql,
                "SELECT id FROM users WHERE id = 1; -- c",
                [
                    ("SELECT", Highlight::Keyword),
                    ("1", Highlight::Number),
                    ("-- c", Highlight::Comment),
                ],
            ),
        ];
        for (language, source, expected) in cases {
            let buffer = Buffer::new(source);
            let spans = highlighted(&Syntax::new(language, &buffer), &buffer);
            for (text, highlight) in expected {
                assert!(
                    spans.contains(&(text.to_owned(), highlight)),
                    "{}: missing {text:?} as {highlight:?} in {spans:?}",
                    language.name()
                );
            }
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
                highlight: Highlight::Keyword
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
}
