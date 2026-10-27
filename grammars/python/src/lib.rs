//! The python grammar as a library the editor loads on demand.

grammar_abi::export! {
    ("python", tree_sitter_python::LANGUAGE, tree_sitter_python::HIGHLIGHTS_QUERY),
}
