//! The sequel grammar as a library the editor loads on demand.

grammar_abi::export! {
    // Written for Neovim: `#match?` uses Lua's `%d`, which Rust regex reads literally.
    (
        "sql",
        tree_sitter_sequel::LANGUAGE,
        tree_sitter_sequel::HIGHLIGHTS_QUERY.replace("%d", "\\\\d")
    ),
}
