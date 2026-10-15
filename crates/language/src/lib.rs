//! Glue between LSP data and [`text::Buffer`]: positions, edits, navigation
//! targets, completion and hover Markdown. Free of UI code so it can be tested
//! and reused by any front end.

pub mod completion;
pub mod markdown;
pub mod symbols;

use std::{ops::Range, path::PathBuf};

use lsp::types::{GotoDefinitionResponse, Hover, HoverContents, MarkedString, Position, TextEdit};
use text::{Buffer, Encoding, Point};

pub fn to_position(buffer: &Buffer, offset: usize, encoding: Encoding) -> Position {
    let point = buffer.point_in(offset, encoding);
    Position::new(point.row as u32, point.column as u32)
}

/// Buffer offset of a server position, clamped into the text.
pub fn to_offset(buffer: &Buffer, position: Position, encoding: Encoding) -> usize {
    let point = Point {
        row: position.line as usize,
        column: position.character as usize,
    };
    buffer.offset_of(point, encoding)
}

pub fn to_range(buffer: &Buffer, range: lsp::types::Range, encoding: Encoding) -> Range<usize> {
    let start = to_offset(buffer, range.start, encoding);
    start..to_offset(buffer, range.end, encoding).max(start)
}

/// Server text edits as buffer edits, ready for [`Buffer::edit_batch`].
pub fn resolve_edits(
    buffer: &Buffer,
    edits: &[TextEdit],
    encoding: Encoding,
) -> Vec<(Range<usize>, String)> {
    edits
        .iter()
        .map(|edit| {
            (
                to_range(buffer, edit.range, encoding),
                edit.new_text.clone(),
            )
        })
        .collect()
}

/// A navigation target. The range stays in the target server's coordinates
/// until the file is open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    pub path: PathBuf,
    pub range: lsp::types::Range,
}

impl Location {
    pub fn from_lsp(location: &lsp::types::Location) -> Option<Self> {
        Some(Self {
            path: lsp::uri_to_path(&location.uri)?,
            range: location.range,
        })
    }
}

/// Definition/declaration results as locations, dropping non-file URIs.
pub fn definition_locations(response: GotoDefinitionResponse) -> Vec<Location> {
    match response {
        GotoDefinitionResponse::Scalar(location) => {
            Location::from_lsp(&location).into_iter().collect()
        }
        GotoDefinitionResponse::Array(locations) => {
            locations.iter().filter_map(Location::from_lsp).collect()
        }
        GotoDefinitionResponse::Link(links) => links
            .into_iter()
            .filter_map(|link| {
                Some(Location {
                    path: lsp::uri_to_path(&link.target_uri)?,
                    range: link.target_selection_range,
                })
            })
            .collect(),
    }
}

/// Hover contents as one Markdown string.
pub fn hover_markdown(hover: Hover) -> String {
    let marked = |marked: MarkedString| match marked {
        MarkedString::String(text) => text,
        MarkedString::LanguageString(code) => format!("```{}\n{}\n```", code.language, code.value),
    };
    match hover.contents {
        HoverContents::Scalar(item) => marked(item),
        HoverContents::Array(items) => items
            .into_iter()
            .map(marked)
            .collect::<Vec<_>>()
            .join("\n\n"),
        HoverContents::Markup(markup) => markup.value,
    }
}

#[cfg(test)]
mod tests {
    use lsp::types::{LocationLink, Uri};

    use super::*;

    fn uri(path: &str) -> Uri {
        lsp::path_to_uri(&std::env::temp_dir().join(path)).unwrap()
    }

    fn range(line: u32, start: u32, end: u32) -> lsp::types::Range {
        lsp::types::Range::new(Position::new(line, start), Position::new(line, end))
    }

    #[test]
    fn positions_round_trip_in_the_server_encoding() {
        let buffer = Buffer::new("a😀b\nxyz");
        let b = 5;
        assert_eq!(
            to_position(&buffer, b, Encoding::Utf16),
            Position::new(0, 3)
        );
        assert_eq!(to_position(&buffer, b, Encoding::Utf8), Position::new(0, 5));
        assert_eq!(to_offset(&buffer, Position::new(0, 3), Encoding::Utf16), b);
        // Reversed ranges collapse instead of panicking.
        assert_eq!(to_range(&buffer, range(1, 2, 1), Encoding::Utf8), 9..9);
    }

    #[test]
    fn definition_links_target_the_selection_range() {
        let link = LocationLink {
            origin_selection_range: None,
            target_uri: uri("lib.rs"),
            target_range: range(3, 0, 40),
            target_selection_range: range(3, 7, 10),
        };
        let other_scheme = lsp::types::Location::new("untitled:x".parse().unwrap(), range(0, 0, 0));
        assert_eq!(
            definition_locations(GotoDefinitionResponse::Link(vec![link])),
            [Location {
                path: std::env::temp_dir().join("lib.rs"),
                range: range(3, 7, 10),
            }]
        );
        assert!(definition_locations(GotoDefinitionResponse::Scalar(other_scheme)).is_empty());
    }

    #[test]
    fn hover_marked_strings_become_fenced_markdown() {
        let hover = Hover {
            contents: HoverContents::Array(vec![
                MarkedString::LanguageString(lsp::types::LanguageString {
                    language: "python".into(),
                    value: "def f() -> int".into(),
                }),
                MarkedString::String("Docs.".into()),
            ]),
            range: None,
        };
        assert_eq!(
            hover_markdown(hover),
            "```python\ndef f() -> int\n```\n\nDocs."
        );
    }
}
