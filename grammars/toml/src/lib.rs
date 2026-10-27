//! The toml grammar as a library the editor loads on demand.

grammar_abi::export! {
    // Upstream captures the whole `key = value` pair as `@property`,
    // coloring the whitespace in it; capture the key instead.
    (
        "toml",
        tree_sitter_toml_ng::LANGUAGE,
        tree_sitter_toml_ng::HIGHLIGHTS_QUERY.replace(
            "(pair\n  (bare_key)) @property",
            "(pair\n  (bare_key) @property)"
        )
    ),
}
