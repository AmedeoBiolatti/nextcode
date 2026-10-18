//! The css grammar as a library the editor loads on demand.

grammar_abi::export! {
    ("css", tree_sitter_css::LANGUAGE, tree_sitter_css::HIGHLIGHTS_QUERY),
}
