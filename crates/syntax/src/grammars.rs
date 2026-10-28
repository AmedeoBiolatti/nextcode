//! Loading grammar libraries (`grammars/`, built with
//! `crates/grammar_abi`): `nc_grammar_<name>.dll`/`.so`/`.dylib` from the
//! `NEXT_CODE_GRAMMARS` directory, else next to the executable, else in its
//! `deps` directory (cargo's output for a library built as a dependency,
//! which is where `cargo run` and the test binaries find them).

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Mutex, OnceLock},
};

use grammar_abi::{ABI, ABI_SYMBOL, Entry, LanguageFn, SYMBOL};

use crate::Language;

/// One grammar from a library.
pub(crate) struct Loaded {
    pub name: String,
    pub language: LanguageFn,
    /// The upstream highlight query.
    pub query: String,
}

/// A library's grammars, or why it could not be loaded.
type Library = Result<&'static [Loaded], String>;
/// Libraries loaded (or failed) so far, by name.
static LIBRARIES: OnceLock<Mutex<HashMap<&'static str, Library>>> = OnceLock::new();
/// Languages whose grammar failed to load, with the reason.
static PROBLEMS: Mutex<Vec<(Language, String)>> = Mutex::new(Vec::new());

/// The grammars of library `name`, loading it on first use.
pub(crate) fn entries(name: &'static str) -> Result<&'static [Loaded], String> {
    let mut libraries = LIBRARIES
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    libraries.entry(name).or_insert_with(|| load(name)).clone()
}

/// Records that `language` is unavailable (printed once, for the console
/// and headless uses).
pub(crate) fn report(language: Language, error: String) {
    eprintln!("next_code: no {} syntax: {error}", language.name());
    PROBLEMS
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .push((language, error));
}

pub(crate) fn problems() -> Vec<(Language, String)> {
    PROBLEMS
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .clone()
}

/// The file name of library `name` on this platform.
pub fn file_name(name: &str) -> String {
    format!(
        "{}nc_grammar_{name}{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    )
}

/// Where libraries are looked for, in order.
pub fn search_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(dir) = std::env::var_os("NEXT_CODE_GRAMMARS") {
        dirs.push(PathBuf::from(dir));
    }
    if let Some(exe_dir) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.to_path_buf()))
    {
        dirs.push(exe_dir.join("deps"));
        dirs.push(exe_dir);
    }
    dirs
}

fn load(name: &str) -> Library {
    let file = file_name(name);
    let path = search_dirs()
        .into_iter()
        .map(|dir| dir.join(&file))
        .find(|path| path.is_file())
        .ok_or_else(|| format!("{file} is not next to the editor (nor in its deps directory)"))?;
    // SAFETY: the library is one of ours (`grammars/`), whose only
    // initialization is Rust's runtime.
    let library = unsafe { libloading::Library::new(&path) }
        .map_err(|err| format!("cannot load {}: {err}", path.display()))?;
    let loaded = {
        // SAFETY: the symbols have the signatures `grammar_abi` declares.
        let abi: libloading::Symbol<unsafe extern "C" fn() -> u32> =
            unsafe { library.get(ABI_SYMBOL) }
                .map_err(|err| format!("{} is not a grammar library: {err}", path.display()))?;
        let version = unsafe { abi() };
        if version != ABI {
            return Err(format!(
                "{} is grammar ABI {version}, this editor expects {ABI}: rebuild it",
                path.display()
            ));
        }
        let table: libloading::Symbol<unsafe extern "C" fn(*mut usize) -> *const Entry> =
            unsafe { library.get(SYMBOL) }
                .map_err(|err| format!("{} is not a grammar library: {err}", path.display()))?;
        let mut count = 0usize;
        let entries = unsafe { table(&mut count) };
        if entries.is_null() {
            return Err(format!("{} has no grammars", path.display()));
        }
        // SAFETY: the library returned `count` entries that live as long
        // as it does, and it is never unloaded (below).
        let entries = unsafe { std::slice::from_raw_parts(entries, count) };
        let text = |ptr: *const u8, len: usize| -> Result<String, String> {
            // SAFETY: as above.
            let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
            String::from_utf8(bytes.to_vec()).map_err(|err| format!("{}: {err}", path.display()))
        };
        entries
            .iter()
            .map(|entry| {
                Ok(Loaded {
                    name: text(entry.name, entry.name_len)?,
                    // SAFETY: the function is the grammar's own.
                    language: unsafe { LanguageFn::from_raw(entry.language) },
                    query: text(entry.query, entry.query_len)?,
                })
            })
            .collect::<Result<Vec<_>, String>>()?
    };
    // The parse tables tree-sitter uses live in the library: it stays
    // loaded for the rest of the process.
    std::mem::forget(library);
    Ok(Box::leak(loaded.into_boxed_slice()))
}
