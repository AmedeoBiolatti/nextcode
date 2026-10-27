//! The typescript grammar as a library the editor loads on demand.

grammar_abi::export! {
    // The TypeScript queries extend the JavaScript ones, like the grammars do.
    (
        "typescript",
        tree_sitter_typescript::LANGUAGE_TYPESCRIPT,
        [
            tree_sitter_javascript::HIGHLIGHT_QUERY,
            tree_sitter_typescript::HIGHLIGHTS_QUERY,
        ]
        .join("\n")
    ),
    (
        "tsx",
        tree_sitter_typescript::LANGUAGE_TSX,
        [
            tree_sitter_javascript::HIGHLIGHT_QUERY,
            tree_sitter_javascript::JSX_HIGHLIGHT_QUERY,
            tree_sitter_typescript::HIGHLIGHTS_QUERY,
        ]
        .join("\n")
    ),
}
