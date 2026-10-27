//! The containerfile grammar as a library the editor loads on demand.

grammar_abi::export! {
    (
        "containerfile",
        tree_sitter_containerfile::LANGUAGE,
        tree_sitter_containerfile::HIGHLIGHTS_QUERY
    ),
}
