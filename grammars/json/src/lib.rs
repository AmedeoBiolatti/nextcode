//! The json grammar as a library the editor loads on demand.

grammar_abi::export! {
    ("json", tree_sitter_json::LANGUAGE, tree_sitter_json::HIGHLIGHTS_QUERY),
}
