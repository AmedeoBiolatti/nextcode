//! Spell checking for the editor: Hunspell dictionaries (through the
//! pure-Rust `spellbook`), word lists (user, project, ignored in a file)
//! and a built-in list of programming words, over the words `tokenize`
//! finds in the parts of a document `scope` selects.
//!
//! The bundled dictionary is en_US (SCOWL, see
//! `dictionaries/README_en_US.txt`); others are read from a directory of
//! `<language>.aff`/`<language>.dic` pairs. Dictionaries load on first use
//! (en_US ~10 ms in release builds, far longer in debug ones): call
//! [`dictionary`] off the UI thread.

mod programming;
pub mod scope;
pub mod tokenize;

use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    ops::Range,
    path::Path,
    sync::{Arc, LazyLock, OnceLock},
};

use parking_lot::Mutex;
use spellbook::Dictionary;

pub use scope::{Scope, segments};

const EN_US_AFF: &str = include_str!("../dictionaries/en_US.aff");
const EN_US_DIC: &str = include_str!("../dictionaries/en_US.dic");

/// The language of the bundled dictionary.
pub const BUILT_IN: &str = "en_US";

type Slot = Arc<OnceLock<Result<Arc<Dictionary>, String>>>;

/// Loaded (or loading) dictionaries by language.
static DICTIONARIES: LazyLock<Mutex<HashMap<String, Slot>>> = LazyLock::new(Default::default);

/// The dictionary for `language`: `<dir>/<language>.aff` and `.dic` when
/// both exist, else the bundled one for [`BUILT_IN`]. Loaded once per
/// process on the calling thread (other callers wait for it).
pub fn dictionary(language: &str, dir: Option<&Path>) -> Result<Arc<Dictionary>, String> {
    let slot = DICTIONARIES
        .lock()
        .entry(language.to_owned())
        .or_default()
        .clone();
    slot.get_or_init(|| load(language, dir)).clone()
}

fn load(language: &str, dir: Option<&Path>) -> Result<Arc<Dictionary>, String> {
    let files = dir.map(|dir| {
        (
            dir.join(format!("{language}.aff")),
            dir.join(format!("{language}.dic")),
        )
    });
    let parsed = match files {
        Some((aff, dic)) if aff.is_file() && dic.is_file() => {
            let read = |path: &Path| {
                std::fs::read(path)
                    .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                    .map_err(|err| format!("{}: {err}", path.display()))
            };
            Dictionary::new(&read(&aff)?, &read(&dic)?)
        }
        _ if language == BUILT_IN => Dictionary::new(EN_US_AFF, EN_US_DIC),
        _ => {
            let place = dir.map_or_else(String::new, |dir| format!(" in {}", dir.display()));
            return Err(format!("no dictionary for {language}{place}"));
        }
    };
    parsed
        .map(Arc::new)
        .map_err(|err| format!("dictionary {language}: {err}"))
}

/// Words accepted as spelled right, compared case-insensitively: a
/// `words.txt` (one word per line; blank lines and `#` comments skipped)
/// or the words ignored in one file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WordList {
    words: HashSet<String>,
}

impl WordList {
    pub fn parse(text: &str) -> Self {
        Self {
            words: text
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty() && !line.starts_with('#'))
                .map(str::to_lowercase)
                .collect(),
        }
    }

    pub fn contains(&self, word: &str) -> bool {
        !self.words.is_empty() && self.words.contains(lowercase(word).as_ref())
    }

    pub fn insert(&mut self, word: &str) {
        self.words.insert(word.to_lowercase());
    }

    pub fn is_empty(&self) -> bool {
        self.words.is_empty()
    }
}

/// `text` (a `words.txt`) with `word` appended on its own line; unchanged
/// when the file already lists it (in any case).
pub fn add_word(text: &str, word: &str) -> String {
    if WordList::parse(text).contains(word) {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len() + word.len() + 2);
    out.push_str(text);
    if !text.is_empty() && !text.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(word);
    out.push('\n');
    out
}

fn lowercase(word: &str) -> Cow<'_, str> {
    if word.chars().any(char::is_uppercase) {
        Cow::Owned(word.to_lowercase())
    } else {
        Cow::Borrowed(word)
    }
}

/// Checks words against dictionaries and word lists.
#[derive(Clone, Default)]
pub struct Checker {
    dictionaries: Vec<Arc<Dictionary>>,
    lists: Vec<Arc<WordList>>,
}

impl Checker {
    /// Without dictionaries every word passes.
    pub fn new(dictionaries: Vec<Arc<Dictionary>>, lists: Vec<Arc<WordList>>) -> Self {
        Self {
            dictionaries,
            lists,
        }
    }

    /// Whether `word` (one of `tokenize::words`) is spelled right: known
    /// to a dictionary, to a word list or to the programming words, as is
    /// or without a possessive `'s`.
    pub fn is_correct(&self, word: &str) -> bool {
        if self.dictionaries.is_empty() {
            return true;
        }
        let word: Cow<str> = if word.contains('’') {
            Cow::Owned(word.replace('’', "'"))
        } else {
            Cow::Borrowed(word)
        };
        let base = tokenize::strip_possessive(&word);
        let known = |word: &str| {
            programming::contains(&lowercase(word))
                || self.lists.iter().any(|list| list.contains(word))
                || self
                    .dictionaries
                    .iter()
                    .any(|dictionary| dictionary.check(word))
        };
        known(&word) || (base.len() < word.len() && known(base))
    }

    /// Byte ranges (within `text`) of the misspelled words in `text`.
    pub fn check(&self, text: &str, out: &mut Vec<Range<usize>>) {
        if self.dictionaries.is_empty() {
            return;
        }
        let mut words = Vec::new();
        tokenize::words(text, &mut words);
        out.extend(
            words
                .into_iter()
                .filter(|range| !self.is_correct(&text[range.clone()])),
        );
    }

    /// Up to `limit` corrections for `word`, the first dictionary's first.
    pub fn suggest(&self, word: &str, limit: usize) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut found = Vec::new();
        for dictionary in &self.dictionaries {
            found.clear();
            dictionary.suggest(word, &mut found);
            for suggestion in found.drain(..) {
                if out.len() < limit && !out.contains(&suggestion) {
                    out.push(suggestion);
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn english(lists: Vec<Arc<WordList>>) -> Checker {
        Checker::new(vec![dictionary(BUILT_IN, None).unwrap()], lists)
    }

    fn misspelled(checker: &Checker, text: &str) -> Vec<String> {
        let mut ranges = Vec::new();
        checker.check(text, &mut ranges);
        ranges.into_iter().map(|r| text[r].to_owned()).collect()
    }

    #[test]
    fn prose_with_typos() {
        let checker = english(Vec::new());
        assert_eq!(
            misspelled(
                &checker,
                "Teh quick brown fox jumps over the lazy dog. Recieve the user's \
                 HTTPRequest and parseJsonValue; it's definately fine."
            ),
            ["Teh", "Recieve", "definately"]
        );
    }

    #[test]
    fn programming_words_and_identifier_parts_pass() {
        let checker = english(Vec::new());
        assert_eq!(
            misspelled(
                &checker,
                "async await regex stdout stderr kwargs args json yaml toml dict tuple \
                 init enum struct impl bool usize mutex serde config localhost \
                 readFileSync get_user_name xyzzyCount"
            ),
            ["xyzzy"]
        );
    }

    #[test]
    fn suggestions_rank_the_intended_word_first() {
        let checker = english(Vec::new());
        let suggestions = checker.suggest("recieve", 5);
        assert_eq!(suggestions.first().map(String::as_str), Some("receive"));
        assert!(suggestions.len() <= 5);
        // Capitalized words keep their capital.
        assert_eq!(
            checker.suggest("Definately", 5).first().map(String::as_str),
            Some("Definitely")
        );
    }

    #[test]
    fn word_lists_accept_words_in_any_case() {
        let list = Arc::new(WordList::parse(
            "# team words\nKubeflow\n\n  frobnicate  \n",
        ));
        let checker = english(vec![list.clone()]);
        assert!(checker.is_correct("kubeflow"));
        assert!(checker.is_correct("KUBEFLOW"));
        assert!(checker.is_correct("Frobnicate"));
        assert!(!checker.is_correct("frobnicat"));
        assert!(!list.contains("team"));
    }

    #[test]
    fn adding_a_word_appends_a_line_once() {
        assert_eq!(add_word("", "Foo"), "Foo\n");
        assert_eq!(add_word("bar", "Foo"), "bar\nFoo\n");
        assert_eq!(add_word("bar\n", "Foo"), "bar\nFoo\n");
        assert_eq!(add_word("bar\nfoo\n", "Foo"), "bar\nfoo\n");
    }

    #[test]
    fn missing_dictionaries_are_reported_and_extra_ones_load_from_the_directory() {
        let dir = std::env::temp_dir().join(format!("spell-dicts-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(
            dictionary("xx_NONE", Some(&dir))
                .unwrap_err()
                .contains("xx_NONE")
        );
        std::fs::write(dir.join("xx_TEST.aff"), "SET UTF-8\n").unwrap();
        std::fs::write(dir.join("xx_TEST.dic"), "2\nzorblat\nquuxing\n").unwrap();
        let test = dictionary("xx_TEST", Some(&dir)).unwrap();
        let checker = Checker::new(vec![test], Vec::new());
        assert_eq!(misspelled(&checker, "zorblat quuxing hello"), ["hello"]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn without_dictionaries_nothing_is_misspelled() {
        assert!(misspelled(&Checker::default(), "Teh recieve").is_empty());
    }
}
