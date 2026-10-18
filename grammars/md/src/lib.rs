//! The md grammar as a library the editor loads on demand.

grammar_abi::export! {
    // The block grammar only: inline markup needs a second parse.
    ("markdown", tree_sitter_md::LANGUAGE, tree_sitter_md::HIGHLIGHT_QUERY_BLOCK),
}
