//! The cpp grammar as a library the editor loads on demand.

grammar_abi::export! {
    // The C++ queries extend the C ones, like the grammar does.
    (
        "cpp",
        tree_sitter_cpp::LANGUAGE,
        [tree_sitter_c::HIGHLIGHT_QUERY, tree_sitter_cpp::HIGHLIGHT_QUERY].join("\n")
    ),
}
