//! The rust grammar as a library the editor loads on demand.

grammar_abi::export! {
    ("rust", tree_sitter_rust::LANGUAGE, tree_sitter_rust::HIGHLIGHTS_QUERY),
}
