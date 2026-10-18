//! The javascript grammar as a library the editor loads on demand.

grammar_abi::export! {
    (
        "javascript",
        tree_sitter_javascript::LANGUAGE,
        [
            tree_sitter_javascript::HIGHLIGHT_QUERY,
            tree_sitter_javascript::JSX_HIGHLIGHT_QUERY,
        ]
        .join("\n")
    ),
}
