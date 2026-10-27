//! The yaml grammar as a library the editor loads on demand.

grammar_abi::export! {
    ("yaml", tree_sitter_yaml::LANGUAGE, tree_sitter_yaml::HIGHLIGHTS_QUERY),
}
