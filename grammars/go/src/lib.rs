//! The go grammar as a library the editor loads on demand.

grammar_abi::export! {
    ("go", tree_sitter_go::LANGUAGE, tree_sitter_go::HIGHLIGHTS_QUERY),
}
