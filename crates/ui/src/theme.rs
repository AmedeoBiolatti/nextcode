//! Themes: the colors of the UI, the terminal and syntax highlighting.
//!
//! Built-in themes (`Dark`, `Light`) are embedded; user themes are JSON
//! files in `<config dir>/next_code/themes/`. `"theme": "<name>"` in
//! `settings.json` picks one (default `Dark`):
//!
//! ```json
//! { "name": "Mine", "extends": "Dark",
//!   "ui": { "accent": "#3574f0" },
//!   "terminal": { "cursor": "#ced0d6", "ansi": ["#1e1f22", …16 colors] },
//!   "syntax": { "keyword": "#cf8e6d", "comment.doc": { "color": "#5f826b", "italic": true } },
//!   "languages": { "python": { "syntax": { "keyword": { "bold": true } } } },
//!   "semantic_modifiers": { "deprecated": { "strikethrough": true } } }
//! ```
//!
//! A style is a color (`#rrggbb` or `#rrggbbaa`) or an object with
//! `color`, `background`, `bold`, `italic`, `underline`, `strikethrough`.
//! A scope is styled by every prefix the theme defines, most specific
//! last (`function.method` takes what `function` sets, then what
//! `function.method` sets); a language's `syntax` layers over the global
//! one. `extends` merges key by key, the child winning.

use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::{
        Arc, LazyLock,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    time::SystemTime,
};

use gpui::{App, Hsla, Rgba, rgba};
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Deserializer, de::Error as _};
use serde_json::Value;
use syntax::{Highlight, Language, SCOPES};
use terminal::{Palette, Rgb};

use crate::settings::{self, Settings};

pub const EDITOR_FONT_SIZE: f32 = 14.;
pub const EDITOR_LINE_HEIGHT: f32 = 20.;

#[cfg(target_os = "windows")]
pub const MONO_FONT: &str = "Consolas";
#[cfg(target_os = "macos")]
pub const MONO_FONT: &str = "Menlo";
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub const MONO_FONT: &str = "DejaVu Sans Mono";

/// gpui alias for the platform UI font.
pub const UI_FONT: &str = ".SystemUIFont";

/// The theme used when none is chosen or the chosen one fails to load.
pub const DEFAULT: &str = "Dark";

/// Built-in themes; their names are reserved.
const BUILT_IN: &[(&str, &str)] = &[
    ("Dark", include_str!("../themes/dark.json")),
    ("Light", include_str!("../themes/light.json")),
];

/// `#rrggbb` or `#rrggbbaa`, kept as `0xrrggbbaa`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Color(u32);

impl Color {
    fn parse(text: &str) -> Result<Self, String> {
        let invalid = || format!("invalid color \"{text}\": expected #rrggbb or #rrggbbaa");
        let hex = text.strip_prefix('#').ok_or_else(invalid)?;
        if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(invalid());
        }
        let value = u32::from_str_radix(hex, 16).map_err(|_| invalid())?;
        match hex.len() {
            6 => Ok(Self(value << 8 | 0xff)),
            8 => Ok(Self(value)),
            _ => Err(invalid()),
        }
    }

    fn rgb(self) -> Rgb {
        let [r, g, b, _] = self.0.to_be_bytes();
        Rgb { r, g, b }
    }
}

impl From<Color> for Hsla {
    fn from(color: Color) -> Self {
        rgba(color.0).into()
    }
}

impl<'de> Deserialize<'de> for Color {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::parse(&text).map_err(D::Error::custom)
    }
}

/// How a highlight scope (or a semantic modifier) is drawn. Unset fields
/// leave the text as it would be otherwise; an unset color means the
/// text color.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct SyntaxStyle {
    pub color: Option<Hsla>,
    pub background: Option<Hsla>,
    pub bold: Option<bool>,
    pub italic: Option<bool>,
    pub underline: Option<bool>,
    pub strikethrough: Option<bool>,
}

impl SyntaxStyle {
    /// `self` layered over `base`: what `self` sets wins.
    pub fn over(self, base: Self) -> Self {
        Self {
            color: self.color.or(base.color),
            background: self.background.or(base.background),
            bold: self.bold.or(base.bold),
            italic: self.italic.or(base.italic),
            underline: self.underline.or(base.underline),
            strikethrough: self.strikethrough.or(base.strikethrough),
        }
    }
}

impl<'de> Deserialize<'de> for SyntaxStyle {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Full {
            color: Option<Color>,
            background: Option<Color>,
            bold: Option<bool>,
            italic: Option<bool>,
            underline: Option<bool>,
            strikethrough: Option<bool>,
        }
        match Value::deserialize(deserializer)? {
            Value::String(text) => Ok(Self {
                color: Some(Color::parse(&text).map_err(D::Error::custom)?.into()),
                ..Self::default()
            }),
            value @ Value::Object(_) => {
                let full = Full::deserialize(value).map_err(D::Error::custom)?;
                Ok(Self {
                    color: full.color.map(Into::into),
                    background: full.background.map(Into::into),
                    bold: full.bold,
                    italic: full.italic,
                    underline: full.underline,
                    strikethrough: full.strikethrough,
                })
            }
            _ => Err(D::Error::custom(
                "expected a color string or a style object",
            )),
        }
    }
}

/// Declares the `"ui"` colors: the resolved [`Ui`], the file's optional
/// fields, and an accessor per color reading the current theme.
macro_rules! ui_colors {
    ($($(#[$doc:meta])* $key:ident),* $(,)?) => {
        /// The UI colors of a theme.
        #[derive(Debug, Clone, PartialEq)]
        pub struct Ui {
            $($(#[$doc])* pub $key: Hsla,)*
            /// Commit graph lanes, cycled.
            pub graph_lanes: Vec<Hsla>,
        }

        #[derive(Debug, Clone, Default, Deserialize)]
        #[serde(default, deny_unknown_fields)]
        struct UiFile {
            $($key: Option<Color>,)*
            graph_lanes: Option<Vec<Color>>,
        }

        impl UiFile {
            fn over(self, base: Self) -> Self {
                Self {
                    $($key: self.$key.or(base.$key),)*
                    graph_lanes: self.graph_lanes.or(base.graph_lanes),
                }
            }

            fn resolve(self) -> Result<Ui, String> {
                let graph_lanes = self
                    .graph_lanes
                    .filter(|lanes| !lanes.is_empty())
                    .ok_or("ui.graph_lanes is not set")?;
                Ok(Ui {
                    $($key: self
                        .$key
                        .ok_or(concat!("ui.", stringify!($key), " is not set"))?
                        .into(),)*
                    graph_lanes: graph_lanes.into_iter().map(Into::into).collect(),
                })
            }
        }

        $(
            $(#[$doc])*
            pub fn $key() -> Hsla {
                CURRENT.read().ui.$key
            }
        )*
    };
}

ui_colors! {
    /// Editor and window background.
    background,
    /// Panels, menus, popovers and tab bars.
    panel,
    border,
    text,
    /// Secondary text: hints, details, inactive tabs.
    muted,
    accent,
    /// Accent-colored text: links, information, the selected tab.
    accent_text,
    /// Text on `accent`, `branch` and `tag` backgrounds.
    on_accent,
    warning,
    error,
    /// The selected row of a list.
    active_row,
    hover_row,
    /// Selected text and search matches.
    selection,
    cursor,
    /// Find matches in the editor; the current one is painted over the
    /// selection, so it should be translucent.
    find_match,
    find_match_active,
    /// The bracket pair at the cursor.
    bracket_match,
    /// Background of the `⋯` shown after a folded line.
    fold_placeholder,
    /// Diff line backgrounds.
    diff_added,
    diff_removed,
    /// Git change letters.
    git_added,
    git_modified,
    git_renamed,
    git_deleted,
    git_conflicted,
    /// History ref labels: branches (other than HEAD's, which uses `accent`) and tags.
    branch,
    tag,
}

/// The color of commit graph lane `index`.
pub fn graph_lane(index: usize) -> Hsla {
    let theme = CURRENT.read();
    theme.ui.graph_lanes[index % theme.ui.graph_lanes.len()]
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct TerminalFile {
    foreground: Option<Color>,
    background: Option<Color>,
    cursor: Option<Color>,
    ansi: Option<[Color; 16]>,
}

impl TerminalFile {
    fn over(self, base: Self) -> Self {
        Self {
            foreground: self.foreground.or(base.foreground),
            background: self.background.or(base.background),
            cursor: self.cursor.or(base.cursor),
            ansi: self.ansi.or(base.ansi),
        }
    }

    fn resolve(self) -> Result<Palette, String> {
        let get = |color: Option<Color>, key: &str| {
            color
                .map(Color::rgb)
                .ok_or_else(|| format!("terminal.{key} is not set"))
        };
        Ok(Palette {
            ansi: self.ansi.ok_or("terminal.ansi is not set")?.map(Color::rgb),
            foreground: get(self.foreground, "foreground")?,
            background: get(self.background, "background")?,
            cursor: get(self.cursor, "cursor")?,
        })
    }
}

type Styles = BTreeMap<String, SyntaxStyle>;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct LanguageFile {
    syntax: Styles,
}

/// A theme file as written.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ThemeFile {
    name: Option<String>,
    extends: Option<String>,
    ui: UiFile,
    terminal: TerminalFile,
    syntax: Styles,
    languages: BTreeMap<String, LanguageFile>,
    semantic_modifiers: Styles,
}

/// Style maps merged key by key; a style set in both merges field by field.
fn merge_styles(child: Styles, mut base: Styles) -> Styles {
    for (key, style) in child {
        let merged = base
            .get(&key)
            .map_or(style, |inherited| style.over(*inherited));
        base.insert(key, merged);
    }
    base
}

impl ThemeFile {
    /// `self` extending `base`.
    fn over(self, mut base: Self) -> Self {
        let mut languages = std::mem::take(&mut base.languages);
        for (id, language) in self.languages {
            let inherited = languages.remove(&id).unwrap_or_default();
            languages.insert(
                id,
                LanguageFile {
                    syntax: merge_styles(language.syntax, inherited.syntax),
                },
            );
        }
        Self {
            name: self.name,
            extends: self.extends,
            ui: self.ui.over(base.ui),
            terminal: self.terminal.over(base.terminal),
            syntax: merge_styles(self.syntax, base.syntax),
            languages,
            semantic_modifiers: merge_styles(self.semantic_modifiers, base.semantic_modifiers),
        }
    }
}

/// A scope in [`SCOPES`] or a dotted prefix of one.
fn known_scope(scope: &str) -> bool {
    SCOPES.iter().any(|known| {
        known
            .strip_prefix(scope)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('.'))
    })
}

/// Styles indexed by [`Highlight::index`]: each scope's defined prefixes
/// layered from the most general, over `base`.
fn style_table(styles: &Styles, base: Option<&[SyntaxStyle]>) -> Box<[SyntaxStyle]> {
    SCOPES
        .iter()
        .enumerate()
        .map(|(index, scope)| {
            let prefix_ends = scope.match_indices('.').map(|(end, _)| end);
            prefix_ends
                .chain([scope.len()])
                .filter_map(|end| styles.get(&scope[..end]))
                .fold(
                    base.map_or_else(SyntaxStyle::default, |base| base[index]),
                    |style, defined| defined.over(style),
                )
        })
        .collect()
}

/// A resolved theme.
#[derive(Debug)]
pub struct Theme {
    pub name: String,
    pub ui: Ui,
    pub terminal: Palette,
    /// Global styles by [`Highlight::index`].
    syntax: Box<[SyntaxStyle]>,
    /// Languages with their own overrides, layered over `syntax`.
    languages: Vec<(Language, Box<[SyntaxStyle]>)>,
    modifiers: HashMap<String, SyntaxStyle>,
}

impl Theme {
    /// Resolves a merged file; problems that do not stop the theme from
    /// working (unknown scopes or languages) come back as warnings.
    fn build(name: &str, file: ThemeFile) -> Result<(Self, Vec<String>), String> {
        let ui = file.ui.resolve()?;
        let terminal = file.terminal.resolve()?;
        let mut unknown_scopes: Vec<String> = file
            .syntax
            .keys()
            .filter(|scope| !known_scope(scope))
            .cloned()
            .collect();
        let mut unknown_languages = Vec::new();
        let syntax = style_table(&file.syntax, None);
        let mut languages = Vec::new();
        for (id, language) in &file.languages {
            unknown_scopes.extend(
                language
                    .syntax
                    .keys()
                    .filter(|scope| !known_scope(scope))
                    .map(|scope| format!("{id}: {scope}")),
            );
            match Language::ALL.iter().find(|known| known.id() == id) {
                Some(&known) => {
                    languages.push((known, style_table(&language.syntax, Some(&syntax))));
                }
                None => unknown_languages.push(id.clone()),
            }
        }
        let mut warnings = Vec::new();
        if !unknown_scopes.is_empty() {
            warnings.push(format!("unknown scopes: {}", unknown_scopes.join(", ")));
        }
        if !unknown_languages.is_empty() {
            warnings.push(format!(
                "unknown languages: {}",
                unknown_languages.join(", ")
            ));
        }
        let theme = Self {
            name: name.to_owned(),
            ui,
            terminal,
            syntax,
            languages,
            modifiers: file.semantic_modifiers.into_iter().collect(),
        };
        Ok((theme, warnings))
    }

    /// Styles of every highlight in `language`, by [`Highlight::index`].
    pub fn syntax_styles(&self, language: Option<Language>) -> &[SyntaxStyle] {
        language
            .and_then(|language| self.languages.iter().find(|(known, _)| *known == language))
            .map_or(&self.syntax, |(_, styles)| styles)
    }

    pub fn syntax_style(&self, language: Option<Language>, highlight: Highlight) -> SyntaxStyle {
        self.syntax_styles(language)[highlight.index()]
    }

    /// The highlight's color, or the text color.
    pub fn syntax_color(&self, language: Option<Language>, highlight: Highlight) -> Hsla {
        self.syntax_style(language, highlight)
            .color
            .unwrap_or(self.ui.text)
    }

    /// The style a semantic-token modifier (`deprecated`, `mutable`, …)
    /// layers over the token's style.
    pub fn modifier_style(&self, modifier: &str) -> Option<SyntaxStyle> {
        self.modifiers.get(modifier).copied()
    }
}

static CURRENT: LazyLock<RwLock<Arc<Theme>>> = LazyLock::new(|| RwLock::new(Arc::new(fallback())));

/// The theme in use.
pub fn current() -> Arc<Theme> {
    CURRENT.read().clone()
}

/// The built-in default theme.
fn fallback() -> Theme {
    let (theme, _) = load(DEFAULT, None, &mut Vec::new()).expect("the built-in theme is valid");
    theme
}

/// `themes/` in the platform config dir (none in unit tests, which must
/// not see the user's themes).
pub fn themes_dir() -> Option<PathBuf> {
    if cfg!(test) {
        return None;
    }
    Some(dirs::config_dir()?.join("next_code").join("themes"))
}

/// `settings.json`, where the theme is chosen (none in unit tests).
fn settings_path() -> Option<PathBuf> {
    if cfg!(test) {
        return None;
    }
    Settings::path()
}

/// User themes in `dir` as (name, file), by file name. A theme's name is
/// its `"name"`, else the file name; built-in names are reserved.
fn user_themes(dir: &Path) -> Vec<(String, PathBuf)> {
    #[derive(Deserialize)]
    struct Named {
        name: Option<String>,
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(|entry| Some(entry.ok()?.path()))
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .collect();
    files.sort();
    let mut themes: Vec<(String, PathBuf)> = Vec::new();
    for path in files {
        let named = std::fs::read_to_string(&path)
            .ok()
            .and_then(|json| serde_json::from_str::<Named>(&json).ok())
            .and_then(|named| named.name);
        let Some(name) = named.or_else(|| Some(path.file_stem()?.to_string_lossy().into_owned()))
        else {
            continue;
        };
        let reserved = BUILT_IN.iter().any(|(built_in, _)| *built_in == name);
        if !reserved && !themes.iter().any(|(known, _)| *known == name) {
            themes.push((name, path));
        }
    }
    themes
}

/// Every theme's name: built-in ones first.
pub fn names() -> Vec<String> {
    let user = themes_dir()
        .map(|dir| user_themes(&dir))
        .unwrap_or_default();
    BUILT_IN
        .iter()
        .map(|(name, _)| (*name).to_owned())
        .chain(user.into_iter().map(|(name, _)| name))
        .collect()
}

/// Loads theme `name` and those it extends from the built-ins and `dir`,
/// adding the user files read to `files` (to watch even when loading fails).
fn load(
    name: &str,
    dir: Option<&Path>,
    files: &mut Vec<PathBuf>,
) -> Result<(Theme, Vec<String>), String> {
    let user = dir.map(user_themes).unwrap_or_default();
    let mut chain: Vec<(String, ThemeFile)> = Vec::new();
    let mut next = Some(name.to_owned());
    while let Some(name) = next {
        if chain.iter().any(|(known, _)| *known == name) {
            return Err(format!("theme \"{name}\" extends itself"));
        }
        let parsed = if let Some((_, json)) = BUILT_IN.iter().find(|(known, _)| *known == name) {
            serde_json::from_str::<ThemeFile>(json).map_err(|err| format!("{name}: {err}"))
        } else if let Some((_, path)) = user.iter().find(|(known, _)| *known == name) {
            files.push(path.clone());
            std::fs::read_to_string(path)
                .map_err(|err| err.to_string())
                .and_then(|json| {
                    serde_json::from_str::<ThemeFile>(&json).map_err(|err| err.to_string())
                })
                .map_err(|err| format!("{}: {err}", path.display()))
        } else {
            Err(format!("theme \"{name}\" not found"))
        };
        let file = parsed?;
        next = file.extends.clone();
        chain.push((name, file));
    }
    let merged = chain
        .into_iter()
        .rev()
        .fold(ThemeFile::default(), |base, (_, file)| file.over(base));
    Theme::build(name, merged)
}

/// What the current theme was loaded from, to notice changes.
struct Loaded {
    stamps: Vec<(PathBuf, Option<SystemTime>)>,
    problem: Option<String>,
}

static LOADED: Mutex<Option<Loaded>> = Mutex::new(None);

/// `"semantic_highlighting"` from `settings.json` as of the last refresh.
static SEMANTIC_HIGHLIGHTING: AtomicBool = AtomicBool::new(true);
/// `"auto_close_brackets"` from `settings.json` as of the last refresh.
static AUTO_CLOSE_BRACKETS: AtomicBool = AtomicBool::new(true);
/// `"word_wrap"` per language as of the last refresh: bit 0 for plain
/// text, bit `i + 1` for `Language::ALL[i]`.
static WORD_WRAP: LazyLock<AtomicU32> =
    LazyLock::new(|| AtomicU32::new(word_wrap_mask(&crate::settings::Settings::default())));

fn word_wrap_mask(settings: &crate::settings::Settings) -> u32 {
    let languages = Language::ALL.iter().enumerate();
    languages.fold(settings.word_wrap(None) as u32, |mask, (i, &language)| {
        mask | (settings.word_wrap(Some(language)) as u32) << (i + 1)
    })
}

/// Whether editors layer language-server semantic tokens over syntax
/// highlighting; follows `settings.json` like the theme.
pub fn semantic_highlighting() -> bool {
    SEMANTIC_HIGHLIGHTING.load(Ordering::Relaxed)
}

/// Whether typing an opening bracket or quote inserts its closer.
pub fn auto_close_brackets() -> bool {
    AUTO_CLOSE_BRACKETS.load(Ordering::Relaxed)
}

/// Whether editors of `language` (`None`: plain text) start wrapped.
pub fn word_wrap(language: Option<Language>) -> bool {
    let bit = language.map_or(0, |language| {
        1 + Language::ALL
            .iter()
            .position(|&known| known == language)
            .unwrap_or(0)
    });
    WORD_WRAP.load(Ordering::Relaxed) & (1 << bit) != 0
}

fn modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
}

/// Re-reads the theme chosen in `settings.json` when that file, the themes
/// folder or a theme file in use changed, then redraws every window. Cheap
/// when nothing changed.
pub fn refresh(cx: &mut App) {
    let mut loaded = LOADED.lock();
    if let Some(loaded) = &*loaded
        && loaded
            .stamps
            .iter()
            .all(|(path, stamp)| modified(path) == *stamp)
    {
        return;
    }
    let settings = settings_path();
    let dir = themes_dir();
    let (chosen, settings_problem) = match &settings {
        Some(path) => {
            let (settings, problem) = Settings::load_from(path);
            let problem = problem.map(|problem| format!("Settings ignored: {problem}"));
            SEMANTIC_HIGHLIGHTING.store(settings.semantic_highlighting, Ordering::Relaxed);
            AUTO_CLOSE_BRACKETS.store(settings.auto_close_brackets, Ordering::Relaxed);
            WORD_WRAP.store(word_wrap_mask(&settings), Ordering::Relaxed);
            (settings.theme, problem)
        }
        None => (None, None),
    };
    let name = chosen.as_deref().unwrap_or(DEFAULT);
    let mut files = Vec::new();
    let result = load(name, dir.as_deref(), &mut files);
    let (theme, problem) = match result {
        Ok((theme, warnings)) => {
            let problem = (!warnings.is_empty())
                .then(|| format!("Theme \"{name}\": {}", warnings.join("; ")));
            (theme, settings_problem.or(problem))
        }
        Err(err) => (
            fallback(),
            Some(format!("Theme \"{name}\": {err}; using {DEFAULT}")),
        ),
    };
    let stamps = settings
        .into_iter()
        .chain(dir)
        .chain(files)
        .map(|path| {
            let stamp = modified(&path);
            (path, stamp)
        })
        .collect();
    *loaded = Some(Loaded { stamps, problem });
    drop(loaded);
    *CURRENT.write() = Arc::new(theme);
    cx.refresh_windows();
}

/// What is wrong with the current theme or its settings, if anything.
pub fn problem() -> Option<String> {
    LOADED
        .lock()
        .as_ref()
        .and_then(|loaded| loaded.problem.clone())
}

/// Saves `name` as the theme in `settings.json` and applies it.
pub fn select(name: &str, cx: &mut App) -> Result<(), String> {
    let path = settings_path().ok_or("no config directory")?;
    settings::write_theme(&path, name)?;
    *LOADED.lock() = None;
    refresh(cx);
    Ok(())
}

/// The file to edit to customize the current theme: the current theme's
/// own file when it is a user theme, else a new `<Name> (custom).json`
/// extending it, which becomes the current theme.
pub fn customize(cx: &mut App) -> Result<PathBuf, String> {
    let dir = themes_dir().ok_or("no config directory")?;
    let theme = current();
    if let Some((_, path)) = user_themes(&dir)
        .into_iter()
        .find(|(name, _)| *name == theme.name)
    {
        return Ok(path);
    }
    let name = format!("{} (custom)", theme.name);
    let path = dir.join(format!("{name}.json"));
    if !path.exists() {
        std::fs::create_dir_all(&dir).map_err(|err| err.to_string())?;
        std::fs::write(&path, customization(&theme, &name)).map_err(|err| err.to_string())?;
    }
    select(&name, cx)?;
    Ok(path)
}

fn hex(color: Hsla) -> String {
    let Rgba { r, g, b, .. } = color.to_rgb();
    let byte = |value: f32| (value * 255.).round() as u8;
    format!("#{:02x}{:02x}{:02x}", byte(r), byte(g), byte(b))
}

/// A theme extending `base` with one example per section, each keeping
/// the look of `base`.
fn customization(base: &Theme, name: &str) -> String {
    let keyword = Highlight::from_scope("keyword");
    let color =
        |language| keyword.map_or(base.ui.text, |keyword| base.syntax_color(language, keyword));
    let Rgb { r, g, b } = base.terminal.cursor;
    let python = Language::Python;
    format!(
        r##"{{
  "name": {name},
  "extends": {extends},
  "ui": {{ "accent": "{accent}" }},
  "terminal": {{ "cursor": "#{r:02x}{g:02x}{b:02x}" }},
  "syntax": {{ "keyword": {{ "color": "{keyword}", "bold": false }} }},
  "languages": {{ "{python}": {{ "syntax": {{ "keyword": "{python_keyword}" }} }} }},
  "semantic_modifiers": {{ "deprecated": {{ "strikethrough": true }} }}
}}
"##,
        name = Value::from(name),
        extends = Value::from(base.name.as_str()),
        accent = hex(base.ui.accent),
        keyword = hex(color(None)),
        python = python.id(),
        python_keyword = hex(color(Some(python))),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A themes folder for one test, removed when dropped.
    struct Dir(PathBuf);

    impl Dir {
        fn new(test: &str, files: &[(&str, &str)]) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "next_code-test-themes-{}-{test}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            for (file, json) in files {
                std::fs::write(dir.join(file), json).unwrap();
            }
            Self(dir)
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).ok();
        }
    }

    fn color(hex: u32) -> Option<Hsla> {
        Some(gpui::rgb(hex).into())
    }

    fn highlight(scope: &str) -> Highlight {
        Highlight::from_scope(scope).unwrap()
    }

    fn load_in(name: &str, dir: &Dir) -> Result<(Theme, Vec<String>), String> {
        load(name, Some(&dir.0), &mut Vec::new())
    }

    fn load_ok(name: &str, dir: &Dir) -> (Theme, Vec<String>) {
        load_in(name, dir).unwrap()
    }

    #[test]
    fn built_in_themes_parse_and_set_every_ui_color() {
        for (name, json) in BUILT_IN {
            let file: ThemeFile = serde_json::from_str(json).unwrap();
            assert_eq!(file.extends, None, "{name} stands alone");
            let (theme, warnings) =
                Theme::build(name, file).unwrap_or_else(|err| panic!("{name}: {err}"));
            assert_eq!(warnings, Vec::<String>::new(), "{name}");
            assert_eq!(theme.name, *name);
        }
    }

    #[test]
    fn extends_merges_key_by_key_with_the_child_winning() {
        let dir = Dir::new(
            "extends",
            &[
                (
                    "base.json",
                    r##"{ "name": "Base", "extends": "Dark",
                        "ui": { "accent": "#000001", "panel": "#000002" },
                        "syntax": { "keyword": { "color": "#000003", "bold": true }, "string": "#000004" } }"##,
                ),
                (
                    "child.json",
                    r##"{ "name": "Child", "extends": "Base",
                        "ui": { "accent": "#000005" },
                        "terminal": { "cursor": "#000006" },
                        "syntax": { "keyword": { "italic": true } } }"##,
                ),
            ],
        );
        let (theme, _) = load_ok("Child", &dir);
        assert_eq!(Some(theme.ui.accent), color(0x000005));
        assert_eq!(Some(theme.ui.panel), color(0x000002));
        assert_eq!(theme.terminal.cursor, Rgb { r: 0, g: 0, b: 6 });
        assert_eq!(
            theme.syntax_style(None, highlight("keyword")),
            SyntaxStyle {
                color: color(0x000003),
                bold: Some(true),
                italic: Some(true),
                ..Default::default()
            }
        );
        assert_eq!(
            theme.syntax_style(None, highlight("string")).color,
            color(0x000004)
        );
        // Untouched keys come from the root.
        assert_eq!(theme.ui.background, fallback().ui.background);
    }

    #[test]
    fn scopes_resolve_by_their_defined_prefixes_and_language_overrides() {
        let dir = Dir::new(
            "prefixes",
            &[(
                "t.json",
                r##"{ "name": "T", "extends": "Dark",
                    "syntax": {
                        "function": "#000001", "function.method": "#000002",
                        "keyword": "#000003", "keyword.control": { "bold": true }
                    },
                    "languages": { "python": { "syntax": { "keyword": "#000004" } } } }"##,
            )],
        );
        let (theme, warnings) = load_ok("T", &dir);
        assert_eq!(warnings, Vec::<String>::new());
        let style = |language, scope| theme.syntax_style(language, highlight(scope));
        assert_eq!(style(None, "function.builtin").color, color(0x000001));
        assert_eq!(style(None, "function.method").color, color(0x000002));
        assert_eq!(
            style(None, "keyword.control"),
            SyntaxStyle {
                color: color(0x000003),
                bold: Some(true),
                ..Default::default()
            }
        );
        let python = Some(Language::Python);
        assert_eq!(style(python, "keyword.import").color, color(0x000004));
        assert_eq!(style(python, "keyword.control").bold, Some(true));
        assert_eq!(
            style(Some(Language::Rust), "keyword.import").color,
            color(0x000003)
        );
        assert_eq!(style(python, "function.method").color, color(0x000002));
        // Nothing defines `variable`: the text color.
        assert_eq!(style(None, "variable.member").color, None);
        assert_eq!(
            theme.syntax_color(python, highlight("variable.member")),
            theme.ui.text
        );
    }

    #[test]
    fn unknown_ui_keys_fail_and_unknown_scopes_warn() {
        let dir = Dir::new(
            "unknown",
            &[
                (
                    "typo.json",
                    r##"{ "name": "Typo", "extends": "Dark", "ui": { "backgrund": "#000000" } }"##,
                ),
                (
                    "scopes.json",
                    r##"{ "name": "Scopes", "extends": "Dark",
                        "syntax": { "markup": { "bold": true }, "keyword.nope": "#000000" },
                        "languages": { "klingon": { "syntax": {} } } }"##,
                ),
            ],
        );
        let err = load_in("Typo", &dir).unwrap_err();
        assert!(err.contains("backgrund"), "{err}");
        let (_, warnings) = load_ok("Scopes", &dir);
        assert_eq!(
            warnings,
            ["unknown scopes: keyword.nope", "unknown languages: klingon"]
        );
        assert!(load_in("Missing", &dir).is_err());
    }

    #[test]
    fn a_theme_without_extends_must_set_every_color() {
        let dir = Dir::new(
            "partial",
            &[(
                "p.json",
                r##"{ "name": "P", "ui": { "text": "#000000" } }"##,
            )],
        );
        let err = load_in("P", &dir).unwrap_err();
        assert!(err.contains("is not set"), "{err}");
    }

    #[test]
    fn customization_extends_its_base_without_changing_it() {
        let dark = fallback();
        let dir = Dir::new(
            "customize",
            &[("Dark (custom).json", &customization(&dark, "Dark (custom)"))],
        );
        let (custom, warnings) = load_ok("Dark (custom)", &dir);
        assert_eq!(warnings, Vec::<String>::new());
        assert_eq!(custom.ui, dark.ui);
        assert_eq!(custom.terminal, dark.terminal);
        for language in Language::ALL.map(Some).into_iter().chain([None]) {
            for highlight in Highlight::all() {
                assert_eq!(
                    custom.syntax_style(language, highlight).color,
                    dark.syntax_style(language, highlight).color,
                    "{}",
                    highlight.scope()
                );
            }
        }
    }
}
