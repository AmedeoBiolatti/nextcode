//! The c grammar as a library the editor loads on demand.

grammar_abi::export! {
    ("c", tree_sitter_c::LANGUAGE, tree_sitter_c::HIGHLIGHT_QUERY),
}
