//! The bash grammar as a library the editor loads on demand.

grammar_abi::export! {
    ("bash", tree_sitter_bash::LANGUAGE, tree_sitter_bash::HIGHLIGHT_QUERY),
}
