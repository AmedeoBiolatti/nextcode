//! The html grammar as a library the editor loads on demand.

grammar_abi::export! {
    ("html", tree_sitter_html::LANGUAGE, tree_sitter_html::HIGHLIGHTS_QUERY),
}
