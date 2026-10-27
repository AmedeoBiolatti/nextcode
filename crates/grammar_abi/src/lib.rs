//! The contract between the editor and its grammar libraries.
//!
//! Tree-sitter grammars are the largest part of the editor's executable
//! (their parse tables, ~14 MB), and the executable's size is paid at every
//! launch on machines whose endpoint security scans it. So each grammar
//! crate is built as a shared library (`nc_grammar_<crate>.dll`, `.so`,
//! `.dylib`; see `grammars/`) that the editor loads the first time a
//! file of that language opens. A library exports one function,
//! [`SYMBOL`], returning its [`Entry`] table: a grammar and its highlight
//! query per entry (the TypeScript library holds TypeScript and TSX).
//!
//! The editor and the libraries are built from the same source tree, so
//! the layout here is all they must agree on; [`ABI`] guards against a
//! stale library next to a newer editor.

/// Bumped when [`Entry`] or the exported functions change.
pub const ABI: u32 = 1;

/// The exported function: `fn(count: *mut usize) -> *const Entry`.
pub const SYMBOL: &[u8] = b"next_code_grammars\0";
/// The exported ABI check: `fn() -> u32`, returning [`ABI`].
pub const ABI_SYMBOL: &[u8] = b"next_code_grammars_abi\0";

/// One grammar: its name (as the editor asks for it), the function
/// tree-sitter calls for the language, and the upstream highlight query
/// (already composed, e.g. C++'s over C's) as UTF-8 bytes.
#[repr(C)]
pub struct Entry {
    pub name: *const u8,
    pub name_len: usize,
    pub language: unsafe extern "C" fn() -> *const (),
    pub query: *const u8,
    pub query_len: usize,
}

// The pointers reference data that lives as long as the library.
unsafe impl Sync for Entry {}
unsafe impl Send for Entry {}

pub use tree_sitter_language::LanguageFn;

/// Defines a grammar library's exports from `(name, LanguageFn, query)`
/// entries; `query` is any expression yielding a `String` or `&str`, built
/// on first use.
#[macro_export]
macro_rules! export {
    ($( ($name:expr, $language:expr, $query:expr) ),+ $(,)?) => {
        static ENTRIES: ::std::sync::OnceLock<Vec<$crate::Entry>> = ::std::sync::OnceLock::new();

        fn entries() -> &'static [$crate::Entry] {
            ENTRIES.get_or_init(|| {
                vec![
                    $({
                        let name: &'static str = $name;
                        let query: &'static str = ::std::boxed::Box::leak(
                            ::std::string::String::from($query).into_boxed_str(),
                        );
                        let language: $crate::LanguageFn = $language;
                        $crate::Entry {
                            name: name.as_ptr(),
                            name_len: name.len(),
                            language: language.into_raw(),
                            query: query.as_ptr(),
                            query_len: query.len(),
                        }
                    }),+
                ]
            })
        }

        /// The grammar table; `count` receives its length.
        ///
        /// # Safety
        /// `count` is null or points to a writable `usize`.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn next_code_grammars(count: *mut usize) -> *const $crate::Entry {
            let entries = entries();
            if !count.is_null() {
                // SAFETY: the caller's contract.
                unsafe { *count = entries.len() };
            }
            entries.as_ptr()
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn next_code_grammars_abi() -> u32 {
            $crate::ABI
        }
    };
}
