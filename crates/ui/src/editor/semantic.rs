//! LSP semantic tokens layered over tree-sitter highlighting.
//!
//! The editor asks for the whole document's tokens when it opens on a
//! server, [`DEBOUNCE`] after edits, and when the server asks for a
//! refresh. Tokens are decoded once into sorted byte ranges; until fresh
//! ones arrive, edits shift them like diagnostics and drop the ones they
//! touch. Painting styles a run with its tree-sitter scope, then the
//! token type's scope (unless tree-sitter already found a more specific
//! scope of the same family, e.g. `comment.doc` for a `comment` token),
//! then each modifier's theme style.

use lsp::types::{
    SemanticToken as LspToken, SemanticTokensFullOptions, SemanticTokensParams,
    SemanticTokensResult, SemanticTokensServerCapabilities, ServerCapabilities,
    request::SemanticTokensFullRequest,
};
use syntax::Highlight;

use super::*;
use crate::theme::Theme;

/// Pause after an edit before asking for fresh tokens, so typing sends one
/// request.
const DEBOUNCE: Duration = Duration::from_millis(200);
/// Wait before retrying a request the server answered "content modified".
const RETRY_DELAY: Duration = Duration::from_millis(400);

/// Token types this client understands, with the scope each is drawn as:
/// the standard LSP types first, then rust-analyzer's and basedpyright's
/// extras. `None` styles only through modifiers. Unknown types draw as
/// `None` too.
pub(crate) const TOKEN_TYPES: &[(&str, Option<&str>)] = &[
    ("namespace", Some("module")),
    ("type", Some("type")),
    ("class", Some("type.class")),
    ("enum", Some("type.enum")),
    ("interface", Some("type.interface")),
    ("struct", Some("type.struct")),
    ("typeParameter", Some("type.parameter")),
    ("parameter", Some("variable.parameter")),
    ("variable", Some("variable")),
    ("property", Some("property")),
    ("enumMember", Some("variant")),
    ("event", Some("property")),
    ("function", Some("function")),
    ("method", Some("function.method")),
    ("macro", Some("function.macro")),
    ("keyword", Some("keyword")),
    ("modifier", Some("keyword.storage")),
    ("comment", Some("comment")),
    ("string", Some("string")),
    ("number", Some("number")),
    ("regexp", Some("string.regex")),
    ("operator", Some("operator")),
    ("decorator", Some("attribute")),
    ("lifetime", Some("label")),
    ("selfKeyword", Some("variable.builtin")),
    ("selfTypeKeyword", Some("variable.builtin")),
    ("selfParameter", Some("variable.builtin")),
    ("clsParameter", Some("variable.builtin")),
    ("builtinType", Some("type.builtin")),
    ("typeAlias", Some("type")),
    ("union", Some("type.struct")),
    ("boolean", Some("boolean")),
    ("escapeSequence", Some("string.escape")),
    ("formatSpecifier", Some("string.special")),
    ("punctuation", Some("punctuation")),
    ("attribute", Some("attribute")),
    ("derive", Some("attribute")),
    ("builtinAttribute", Some("attribute")),
    ("deriveHelper", Some("attribute")),
    ("constParameter", Some("constant")),
    ("label", Some("label")),
    ("toolModule", Some("module")),
    ("generic", None),
    ("unresolvedReference", None),
];

/// Token modifiers this client declares: the standard ones, then common
/// extras. Themes style them by name under `semantic_modifiers`.
pub(crate) const TOKEN_MODIFIERS: &[&str] = &[
    "declaration",
    "definition",
    "readonly",
    "static",
    "deprecated",
    "abstract",
    "async",
    "modification",
    "documentation",
    "defaultLibrary",
    "mutable",
    "consuming",
    "unsafe",
    "callable",
    "constant",
    "controlFlow",
    "crateRoot",
    "injected",
    "intraDocLink",
    "library",
    "public",
    "reference",
    "trait",
    "associated",
    "attribute",
    "builtin",
];

/// The highlight a token type is drawn with, if any.
pub(crate) fn type_highlight(name: &str) -> Option<Highlight> {
    TOKEN_TYPES
        .iter()
        .find(|(known, _)| *known == name)
        .and_then(|(_, scope)| Highlight::from_scope((*scope)?))
}

/// A server's token legend, resolved: highlight per type index, name per
/// modifier bit.
#[derive(Debug)]
pub(crate) struct Legend {
    types: Box<[Option<Highlight>]>,
    modifiers: Box<[String]>,
}

impl Legend {
    /// The legend of a server that serves full-document tokens.
    pub(crate) fn from_capabilities(capabilities: &ServerCapabilities) -> Option<Self> {
        let options = match capabilities.semantic_tokens_provider.as_ref()? {
            SemanticTokensServerCapabilities::SemanticTokensOptions(options) => options,
            SemanticTokensServerCapabilities::SemanticTokensRegistrationOptions(options) => {
                &options.semantic_tokens_options
            }
        };
        if !matches!(
            options.full,
            Some(SemanticTokensFullOptions::Bool(true) | SemanticTokensFullOptions::Delta { .. })
        ) {
            return None;
        }
        Some(Self::new(
            options.legend.token_types.iter().map(|t| t.as_str()),
            options.legend.token_modifiers.iter().map(|m| m.as_str()),
        ))
    }

    fn new<'a>(
        types: impl IntoIterator<Item = &'a str>,
        modifiers: impl IntoIterator<Item = &'a str>,
    ) -> Self {
        Self {
            types: types.into_iter().map(type_highlight).collect(),
            modifiers: modifiers.into_iter().map(str::to_owned).collect(),
        }
    }

    /// Names of the modifiers set in `bits`.
    pub(crate) fn modifier_names(&self, bits: u32) -> impl Iterator<Item = &str> {
        self.modifiers
            .iter()
            .enumerate()
            .filter(move |(bit, _)| *bit < 32 && bits & (1 << bit) != 0)
            .map(|(_, name)| name.as_str())
    }
}

/// One decoded token: a byte range of the buffer within one line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SemanticToken {
    pub range: Range<usize>,
    pub highlight: Option<Highlight>,
    /// Bitset over the legend's modifiers.
    pub modifiers: u32,
}

/// Decodes relative-encoded tokens (positions in the server's `encoding`)
/// into sorted, non-overlapping byte ranges of `buffer`. Empty tokens,
/// tokens past the end and tokens overlapping the previous one (which
/// servers must not send) are skipped.
pub(crate) fn decode(
    data: &[LspToken],
    legend: &Legend,
    buffer: &Buffer,
    encoding: Encoding,
) -> Vec<SemanticToken> {
    let mut tokens: Vec<SemanticToken> = Vec::with_capacity(data.len());
    let (mut row, mut column) = (0, 0);
    for token in data {
        if token.delta_line > 0 {
            row += token.delta_line as usize;
            column = 0;
        }
        column += token.delta_start as usize;
        let start = buffer.offset_of(text::Point { row, column }, encoding);
        let end = buffer.offset_of(
            text::Point {
                row,
                column: column + token.length as usize,
            },
            encoding,
        );
        if start >= end || tokens.last().is_some_and(|last| last.range.end > start) {
            continue;
        }
        tokens.push(SemanticToken {
            range: start..end,
            highlight: legend
                .types
                .get(token.token_type as usize)
                .copied()
                .flatten(),
            modifiers: token.token_modifiers_bitset,
        });
    }
    tokens
}

/// Moves `tokens` across `change`: those before it stay, those after it
/// shift, those it touches (edges included) are dropped.
pub(crate) fn shift(tokens: &mut Vec<SemanticToken>, change: &Change) {
    let first = tokens.partition_point(|token| token.range.end < change.start);
    let touched_end =
        first + tokens[first..].partition_point(|token| token.range.start <= change.old_end);
    tokens.drain(first..touched_end);
    for token in &mut tokens[first..] {
        token.range = token.range.start - change.old_end + change.new_end
            ..token.range.end - change.old_end + change.new_end;
    }
}

/// The tokens overlapping `range` of sorted, non-overlapping `tokens`.
pub(crate) fn overlapping(tokens: &[SemanticToken], range: Range<usize>) -> &[SemanticToken] {
    let first = tokens.partition_point(|token| token.range.end <= range.start);
    let tokens = &tokens[first..];
    &tokens[..tokens.partition_point(|token| token.range.start < range.end)]
}

/// Request parameters for the whole document at `uri`.
pub(crate) fn params(uri: Uri) -> SemanticTokensParams {
    SemanticTokensParams {
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
        text_document: TextDocumentIdentifier::new(uri),
    }
}

/// The token data of a response; no result means no tokens.
pub(crate) fn response_data(result: Option<SemanticTokensResult>) -> Vec<LspToken> {
    match result {
        Some(SemanticTokensResult::Tokens(tokens)) => tokens.data,
        Some(SemanticTokensResult::Partial(partial)) => partial.data,
        None => Vec::new(),
    }
}

/// Whether `specific` is a sub-scope of `general` (`comment.doc` of
/// `comment`).
fn refines(specific: Highlight, general: Highlight) -> bool {
    specific
        .scope()
        .strip_prefix(general.scope())
        .is_some_and(|rest| rest.starts_with('.'))
}

/// Resolves run styles for one frame: the language's scope styles and the
/// theme style of each legend modifier, looked up once.
pub(crate) struct Styler<'a> {
    styles: &'a [SyntaxStyle],
    modifiers: Vec<Option<SyntaxStyle>>,
}

impl<'a> Styler<'a> {
    pub(crate) fn new(
        theme: &'a Theme,
        language: Option<Language>,
        legend: Option<&Legend>,
    ) -> Self {
        Self {
            styles: theme.syntax_styles(language),
            modifiers: legend
                .map(|legend| {
                    legend
                        .modifiers
                        .iter()
                        .map(|name| theme.modifier_style(name))
                        .collect()
                })
                .unwrap_or_default(),
        }
    }

    /// The style of text tree-sitter highlights as `syntax` and the server
    /// marks with `token`.
    pub(crate) fn style(
        &self,
        syntax: Option<Highlight>,
        token: Option<&SemanticToken>,
    ) -> SyntaxStyle {
        let mut style = syntax.map_or_else(SyntaxStyle::default, |highlight| {
            self.styles[highlight.index()]
        });
        let Some(token) = token else {
            return style;
        };
        if let Some(highlight) = token.highlight
            && !syntax.is_some_and(|syntax| refines(syntax, highlight))
        {
            style = self.styles[highlight.index()].over(style);
        }
        // rust-analyzer marks `+=` on a mutable binding `mutable` too;
        // modifier decorations are for names, not operators.
        let symbol = token.highlight.is_some_and(|highlight| {
            let scope = highlight.scope();
            scope.starts_with("operator") || scope.starts_with("punctuation")
        });
        let mut bits = if symbol { 0 } else { token.modifiers };
        while bits != 0 {
            let bit = bits.trailing_zeros() as usize;
            bits &= bits - 1;
            if let Some(Some(modifier)) = self.modifiers.get(bit) {
                style = modifier.over(style);
            }
        }
        style
    }
}

/// Per-editor token state.
pub(super) struct SemanticState {
    /// Sorted and non-overlapping; shifted with edits until replaced.
    pub(super) tokens: Vec<SemanticToken>,
    /// `"semantic_highlighting"` as last seen.
    enabled: bool,
    /// The pending (debounced) request; dropping it cancels the request.
    task: Option<Task<()>>,
}

impl SemanticState {
    pub(super) fn new() -> Self {
        Self {
            tokens: Vec::new(),
            enabled: theme::semantic_highlighting(),
            task: None,
        }
    }

    fn clear(&mut self) {
        self.tokens = Vec::new();
        self.task = None;
    }
}

impl Editor {
    /// Asks the server for this document's tokens after `delay`, replacing
    /// any pending request. No-op unless enabled and the server serves
    /// full-document tokens; a notebook cell gets its tokens from the
    /// notebook (`set_cell_semantic_tokens`).
    fn request_semantic_tokens(&mut self, delay: Duration, cx: &mut Context<Self>) {
        self.semantic.task = None;
        let Some(doc) = &self.lsp else {
            return;
        };
        if !self.semantic.enabled || doc.server.semantic_legend.is_none() || doc.cell.is_some() {
            return;
        }
        let server = doc.server.clone();
        let version = doc.version;
        let params = params(doc.uri.clone());
        self.semantic.task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            let mut result = server
                .connection
                .request::<SemanticTokensFullRequest>(params.clone())
                .await;
            if navigation::is_content_modified(&result) {
                cx.background_executor().timer(RETRY_DELAY).await;
                result = server
                    .connection
                    .request::<SemanticTokensFullRequest>(params)
                    .await;
            }
            this.update(cx, |this, cx| {
                // Edits replace this task, so a response for an older
                // version only arrives if the server changed meanwhile.
                let current = this
                    .lsp
                    .as_ref()
                    .is_some_and(|doc| Arc::ptr_eq(&doc.server, &server) && doc.version == version);
                let (Ok(result), Some(legend), true) =
                    (result, server.semantic_legend.as_ref(), current)
                else {
                    return;
                };
                this.semantic.tokens = decode(
                    &response_data(result),
                    legend,
                    &this.buffer,
                    server.encoding,
                );
                cx.notify();
            })
            .ok();
        }));
    }

    /// Re-requests tokens now (the document opened, or the server asked).
    pub fn refresh_semantic_tokens(&mut self, cx: &mut Context<Self>) {
        self.request_semantic_tokens(Duration::ZERO, cx);
    }

    /// Requests tokens for the edited text once typing pauses.
    pub(super) fn semantic_tokens_edited(&mut self, cx: &mut Context<Self>) {
        self.request_semantic_tokens(DEBOUNCE, cx);
    }

    /// Moves tokens across the drained `changes`.
    pub(super) fn shift_semantic_tokens(&mut self) {
        for change in &self.changes {
            shift(&mut self.semantic.tokens, change);
        }
    }

    /// Forgets tokens (the server went away).
    pub(super) fn clear_semantic_tokens(&mut self) {
        self.semantic.clear();
    }

    /// Applies a changed `"semantic_highlighting"` setting.
    pub(super) fn sync_semantic_setting(&mut self, cx: &mut Context<Self>) {
        let enabled = theme::semantic_highlighting();
        if enabled == self.semantic.enabled {
            return;
        }
        self.semantic.enabled = enabled;
        if enabled {
            self.refresh_semantic_tokens(cx);
        } else {
            self.semantic.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token(
        delta_line: u32,
        delta_start: u32,
        length: u32,
        token_type: u32,
        bits: u32,
    ) -> LspToken {
        LspToken {
            delta_line,
            delta_start,
            length,
            token_type,
            token_modifiers_bitset: bits,
        }
    }

    fn legend() -> Legend {
        Legend::new(
            [
                "variable",
                "string",
                "keyword",
                "parameter",
                "fancyNewThing",
            ],
            ["declaration", "mutable"],
        )
    }

    fn texts(buffer: &Buffer, tokens: &[SemanticToken]) -> Vec<String> {
        tokens
            .iter()
            .map(|token| buffer.slice(token.range.clone()))
            .collect()
    }

    #[test]
    fn decodes_relative_tokens_in_utf16_columns() {
        let buffer = Buffer::new("let é = \"😀x\";\nfn f(a: u8) {}\n");
        let legend = legend();
        let data = [
            token(0, 0, 3, 2, 0),    // let
            token(0, 4, 1, 0, 0b11), // é (column 4)
            token(0, 4, 5, 1, 0),    // "😀x" (column 8, the emoji is 2 units)
            token(1, 0, 2, 2, 0),    // fn
            token(0, 5, 1, 3, 0b01), // a
            token(0, 3, 2, 4, 0),    // u8: unknown type
        ];
        let tokens = decode(&data, &legend, &buffer, Encoding::Utf16);
        assert_eq!(
            texts(&buffer, &tokens),
            ["let", "é", "\"😀x\"", "fn", "a", "u8"]
        );
        let variable = Highlight::from_scope("variable");
        let string = Highlight::from_scope("string");
        let parameter = Highlight::from_scope("variable.parameter");
        assert_eq!(
            tokens[1],
            SemanticToken {
                range: 4..6,
                highlight: variable,
                modifiers: 0b11
            }
        );
        assert_eq!(
            (tokens[2].range.clone(), tokens[2].highlight),
            (9..16, string)
        );
        assert_eq!((tokens[4].highlight, tokens[4].modifiers), (parameter, 1));
        assert_eq!(tokens[5].highlight, None);
        assert_eq!(
            legend
                .modifier_names(tokens[1].modifiers)
                .collect::<Vec<_>>(),
            ["declaration", "mutable"]
        );
    }

    #[test]
    fn decodes_utf8_columns_and_skips_bad_tokens() {
        let buffer = Buffer::new("let é = 1;\nx\n");
        let data = [
            token(0, 4, 2, 0, 0), // é in bytes
            token(0, 1, 1, 0, 0), // overlaps é: skipped
            token(1, 0, 9, 0, 0), // clamped to the line
            token(5, 0, 1, 0, 0), // past the end: skipped
        ];
        let tokens = decode(&data, &legend(), &buffer, Encoding::Utf8);
        assert_eq!(texts(&buffer, &tokens), ["é", "x"]);
    }

    #[test]
    fn maps_token_types_to_scopes() {
        let scope = |name| type_highlight(name).map(Highlight::scope);
        assert_eq!(scope("parameter"), Some("variable.parameter"));
        assert_eq!(scope("struct"), Some("type.struct"));
        assert_eq!(scope("enumMember"), Some("variant"));
        assert_eq!(scope("namespace"), Some("module"));
        assert_eq!(scope("selfParameter"), Some("variable.builtin"));
        assert_eq!(scope("lifetime"), Some("label"));
        assert_eq!(scope("unresolvedReference"), None);
        assert_eq!(scope("somethingNew"), None);
        // Every mapped scope exists.
        for (name, scope) in TOKEN_TYPES {
            if let Some(scope) = scope {
                assert!(Highlight::from_scope(scope).is_some(), "{name}: {scope}");
            }
        }
    }

    #[test]
    fn edits_keep_earlier_tokens_shift_later_ones_and_drop_touched() {
        let mut buffer = Buffer::new("aa bb cc dd");
        let data = [
            token(0, 0, 2, 0, 0),
            token(0, 3, 2, 0, 0),
            token(0, 3, 2, 0, 0),
            token(0, 3, 2, 0, 0),
        ];
        let mut tokens = decode(&data, &legend(), &buffer, Encoding::Utf8);
        // Typing at the end of `bb` touches it.
        buffer.edit(5..5, "X");
        // Replacing the space between `cc` and `dd` touches both.
        buffer.edit(9..10, "--");
        for change in buffer.drain_changes() {
            shift(&mut tokens, &change);
        }
        assert_eq!(buffer.slice(0..buffer.len()), "aa bbX cc--dd");
        assert_eq!(texts(&buffer, &tokens), ["aa"]);

        let mut buffer = Buffer::new("aa bb cc");
        let mut tokens = decode(&data[..3], &legend(), &buffer, Encoding::Utf8);
        buffer.edit(3..5, "longer");
        for change in buffer.drain_changes() {
            shift(&mut tokens, &change);
        }
        assert_eq!(texts(&buffer, &tokens), ["aa", "cc"]);
        assert_eq!(tokens[1].range, 10..12);
    }

    #[test]
    fn styles_layer_semantic_types_and_modifiers_over_syntax() {
        let theme = theme::current();
        let legend = legend();
        let styler = Styler::new(&theme, Some(Language::Rust), Some(&legend));
        let syntax =
            |scope| theme.syntax_style(Some(Language::Rust), Highlight::from_scope(scope).unwrap());
        let token = |scope: &str, modifiers| SemanticToken {
            range: 0..1,
            highlight: Highlight::from_scope(scope),
            modifiers,
        };
        let variable = Highlight::from_scope("variable");
        // The type's scope wins over tree-sitter's guess.
        assert_eq!(
            styler.style(variable, Some(&token("variable.parameter", 0))),
            syntax("variable.parameter").over(syntax("variable"))
        );
        // A more specific tree-sitter scope of the same family stays.
        let doc = Highlight::from_scope("comment.doc");
        assert_eq!(
            styler.style(doc, Some(&token("comment", 0))),
            syntax("comment.doc")
        );
        // `mutable` (bit 1) underlines by default.
        assert_eq!(
            styler
                .style(variable, Some(&token("variable", 0b10)))
                .underline,
            Some(true)
        );
        // …but not on `+=` marked mutable.
        let operator = Highlight::from_scope("operator");
        assert_eq!(
            styler
                .style(operator, Some(&token("operator", 0b10)))
                .underline,
            syntax("operator").underline
        );
        assert_eq!(styler.style(variable, None), syntax("variable"));
    }
}
