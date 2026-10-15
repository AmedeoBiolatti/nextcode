//! Document and workspace symbols: server responses as one flat, ordered
//! symbol tree, and the chain of symbols around a position.

use std::{ops::Range, path::PathBuf};

use lsp::types::{
    DocumentSymbol, DocumentSymbolResponse, OneOf, Position, SymbolInformation, SymbolKind,
    WorkspaceSymbolResponse,
};

/// One node of a document's symbol tree. Trees are stored flat in
/// pre-order (a parent, then its children's subtrees, siblings by
/// position); `P` is a position type: LSP positions as received, buffer
/// offsets once resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symbol<P> {
    pub name: String,
    pub detail: Option<String>,
    pub kind: SymbolKind,
    /// The whole declaration (body included).
    pub range: Range<P>,
    /// The name, where navigation lands.
    pub selection_range: Range<P>,
    pub depth: usize,
    /// Index of the parent in the flat list.
    pub parent: Option<usize>,
}

impl<P> Symbol<P> {
    /// The same symbol with its ranges converted by `map`.
    pub fn map<Q>(self, mut map: impl FnMut(Range<P>) -> Range<Q>) -> Symbol<Q> {
        Symbol {
            name: self.name,
            detail: self.detail,
            kind: self.kind,
            range: map(self.range),
            selection_range: map(self.selection_range),
            depth: self.depth,
            parent: self.parent,
        }
    }
}

/// A symbol in flat tree form, before its place in the tree is known.
struct Node {
    symbol: Symbol<Position>,
    children: Vec<Node>,
}

/// A `textDocument/documentSymbol` response as a flat pre-order tree.
/// Hierarchical responses keep their nesting; flat ones
/// (`SymbolInformation`) are nested by range containment, falling back to
/// the closest preceding symbol named like the `containerName`.
pub fn flatten(response: DocumentSymbolResponse) -> Vec<Symbol<Position>> {
    let roots = match response {
        DocumentSymbolResponse::Nested(symbols) => symbols.into_iter().map(nested_node).collect(),
        DocumentSymbolResponse::Flat(symbols) => nest_flat(symbols),
    };
    let mut out = Vec::new();
    emit(roots, 0, None, &mut out);
    out
}

fn nested_node(symbol: DocumentSymbol) -> Node {
    Node {
        children: symbol
            .children
            .unwrap_or_default()
            .into_iter()
            .map(nested_node)
            .collect(),
        symbol: Symbol {
            name: symbol.name,
            detail: symbol.detail.filter(|detail| !detail.is_empty()),
            kind: symbol.kind,
            range: symbol.range.start..symbol.range.end,
            selection_range: symbol.selection_range.start..symbol.selection_range.end,
            depth: 0,
            parent: None,
        },
    }
}

/// Rebuilds the tree of a flat response: sorted by start (wider first on
/// ties), each symbol's parent is the innermost earlier one containing it.
fn nest_flat(mut symbols: Vec<SymbolInformation>) -> Vec<Node> {
    symbols.sort_by(|a, b| {
        let (a, b) = (a.location.range, b.location.range);
        a.start.cmp(&b.start).then(b.end.cmp(&a.end))
    });
    // Parent index per symbol (into `symbols`).
    let mut parents: Vec<Option<usize>> = Vec::with_capacity(symbols.len());
    // Open ancestors of the current symbol, outermost first.
    let mut stack: Vec<usize> = Vec::new();
    for (index, symbol) in symbols.iter().enumerate() {
        let range = symbol.location.range;
        while let Some(&top) = stack.last() {
            let outer = symbols[top].location.range;
            if outer.start <= range.start && range.end <= outer.end {
                break;
            }
            stack.pop();
        }
        let parent = stack.last().copied().or_else(|| {
            let container = symbol.container_name.as_deref()?;
            (0..index).rev().find(|&i| symbols[i].name == container)
        });
        parents.push(parent);
        stack.push(index);
    }
    let mut nodes: Vec<Option<Node>> = symbols
        .into_iter()
        .map(|symbol| {
            let range = symbol.location.range;
            Some(Node {
                symbol: Symbol {
                    name: symbol.name,
                    detail: None,
                    kind: symbol.kind,
                    range: range.start..range.end,
                    selection_range: range.start..range.start,
                    depth: 0,
                    parent: None,
                },
                children: Vec::new(),
            })
        })
        .collect();
    // Parents always precede their children, so attaching from the end
    // moves complete subtrees.
    for index in (0..nodes.len()).rev() {
        if let Some(parent) = parents[index] {
            let node = nodes[index].take().expect("attached once");
            nodes[parent]
                .as_mut()
                .expect("parents precede children")
                .children
                .push(node);
        }
    }
    nodes.into_iter().flatten().collect()
}

fn emit(
    mut nodes: Vec<Node>,
    depth: usize,
    parent: Option<usize>,
    out: &mut Vec<Symbol<Position>>,
) {
    nodes.sort_by_key(|node| node.symbol.range.start);
    for node in nodes {
        let index = out.len();
        out.push(Symbol {
            depth,
            parent,
            ..node.symbol
        });
        emit(node.children, depth + 1, Some(index), out);
    }
}

/// The chain of symbols containing `at` (edges included), outermost
/// first: the deepest containing symbol and its ancestors. Empty when no
/// symbol contains `at`.
pub fn symbol_path_at<P: Ord + Copy>(symbols: &[Symbol<P>], at: P) -> Vec<usize> {
    let contains = |symbol: &Symbol<P>| symbol.range.start <= at && at <= symbol.range.end;
    // Pre-order: a containing child comes after its containing parent, so
    // extending the chain with each containing symbol whose parent is on
    // it ends at the innermost one.
    let mut chain: Vec<usize> = Vec::new();
    for (index, symbol) in symbols.iter().enumerate() {
        if !contains(symbol) {
            continue;
        }
        let keep = match symbol.parent {
            None => 0,
            Some(parent) => match chain.iter().position(|&i| i == parent) {
                Some(position) => position + 1,
                None => continue,
            },
        };
        chain.truncate(keep);
        chain.push(index);
    }
    chain
}

/// Indices of the symbols sharing `index`'s parent, in order.
pub fn siblings<P>(symbols: &[Symbol<P>], index: usize) -> Vec<usize> {
    let parent = symbols.get(index).and_then(|symbol| symbol.parent);
    (0..symbols.len())
        .filter(|&i| symbols[i].parent == parent)
        .collect()
}

/// Whether the symbol at `index` has children (they follow it directly).
pub fn has_children<P>(symbols: &[Symbol<P>], index: usize) -> bool {
    symbols
        .get(index + 1)
        .is_some_and(|next| next.parent == Some(index))
}

/// One `workspace/symbol` result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceSymbolItem {
    pub name: String,
    pub kind: SymbolKind,
    pub container: Option<String>,
    pub path: PathBuf,
    /// `None` when the server gave only the file.
    pub range: Option<lsp::types::Range>,
}

/// A `workspace/symbol` response as items, dropping non-file URIs.
pub fn workspace_symbols(response: WorkspaceSymbolResponse) -> Vec<WorkspaceSymbolItem> {
    match response {
        WorkspaceSymbolResponse::Flat(symbols) => symbols
            .into_iter()
            .filter_map(|symbol| {
                Some(WorkspaceSymbolItem {
                    path: lsp::uri_to_path(&symbol.location.uri)?,
                    range: Some(symbol.location.range),
                    name: symbol.name,
                    kind: symbol.kind,
                    container: symbol.container_name.filter(|name| !name.is_empty()),
                })
            })
            .collect(),
        WorkspaceSymbolResponse::Nested(symbols) => symbols
            .into_iter()
            .filter_map(|symbol| {
                let (uri, range) = match &symbol.location {
                    OneOf::Left(location) => (&location.uri, Some(location.range)),
                    OneOf::Right(location) => (&location.uri, None),
                };
                Some(WorkspaceSymbolItem {
                    path: lsp::uri_to_path(uri)?,
                    range,
                    name: symbol.name,
                    kind: symbol.kind,
                    container: symbol.container_name.filter(|name| !name.is_empty()),
                })
            })
            .collect(),
    }
}

/// Lower-case name of a symbol kind (`"enum member"`).
pub fn kind_name(kind: SymbolKind) -> &'static str {
    match kind {
        SymbolKind::FILE => "file",
        SymbolKind::MODULE => "module",
        SymbolKind::NAMESPACE => "namespace",
        SymbolKind::PACKAGE => "package",
        SymbolKind::CLASS => "class",
        SymbolKind::METHOD => "method",
        SymbolKind::PROPERTY => "property",
        SymbolKind::FIELD => "field",
        SymbolKind::CONSTRUCTOR => "constructor",
        SymbolKind::ENUM => "enum",
        SymbolKind::INTERFACE => "interface",
        SymbolKind::FUNCTION => "function",
        SymbolKind::VARIABLE => "variable",
        SymbolKind::CONSTANT => "constant",
        SymbolKind::STRING => "string",
        SymbolKind::NUMBER => "number",
        SymbolKind::BOOLEAN => "boolean",
        SymbolKind::ARRAY => "array",
        SymbolKind::OBJECT => "object",
        SymbolKind::KEY => "key",
        SymbolKind::NULL => "null",
        SymbolKind::ENUM_MEMBER => "enum member",
        SymbolKind::STRUCT => "struct",
        SymbolKind::EVENT => "event",
        SymbolKind::OPERATOR => "operator",
        SymbolKind::TYPE_PARAMETER => "type parameter",
        _ => "symbol",
    }
}

#[cfg(test)]
mod tests {
    use lsp::types::{Location, Uri, WorkspaceLocation, WorkspaceSymbol};

    use super::*;

    fn range(start: (u32, u32), end: (u32, u32)) -> lsp::types::Range {
        lsp::types::Range::new(Position::new(start.0, start.1), Position::new(end.0, end.1))
    }

    fn uri() -> Uri {
        "file:///tmp/a.rs".parse().unwrap()
    }

    #[allow(deprecated)]
    fn doc(
        name: &str,
        kind: SymbolKind,
        span: lsp::types::Range,
        children: Vec<DocumentSymbol>,
    ) -> DocumentSymbol {
        DocumentSymbol {
            name: name.into(),
            detail: Some(format!("{name} detail")),
            kind,
            tags: None,
            deprecated: None,
            range: span,
            selection_range: lsp::types::Range::new(span.start, span.start),
            children: (!children.is_empty()).then_some(children),
        }
    }

    #[allow(deprecated)]
    fn info(
        name: &str,
        kind: SymbolKind,
        span: lsp::types::Range,
        container: Option<&str>,
    ) -> SymbolInformation {
        SymbolInformation {
            name: name.into(),
            kind,
            tags: None,
            deprecated: None,
            location: Location::new(uri(), span),
            container_name: container.map(Into::into),
        }
    }

    fn shape(symbols: &[Symbol<Position>]) -> Vec<(&str, usize, Option<usize>)> {
        symbols
            .iter()
            .map(|symbol| (symbol.name.as_str(), symbol.depth, symbol.parent))
            .collect()
    }

    #[test]
    fn flattens_hierarchical_symbols_in_pre_order_by_position() {
        let response = DocumentSymbolResponse::Nested(vec![
            doc(
                "main",
                SymbolKind::FUNCTION,
                range((20, 0), (22, 1)),
                vec![],
            ),
            doc(
                "Point",
                SymbolKind::STRUCT,
                range((0, 0), (10, 1)),
                vec![
                    doc("y", SymbolKind::FIELD, range((2, 4), (2, 10)), vec![]),
                    doc("x", SymbolKind::FIELD, range((1, 4), (1, 10)), vec![]),
                    doc(
                        "Inner",
                        SymbolKind::CLASS,
                        range((3, 0), (9, 0)),
                        vec![doc("f", SymbolKind::METHOD, range((4, 0), (5, 0)), vec![])],
                    ),
                ],
            ),
        ]);
        let symbols = flatten(response);
        assert_eq!(
            shape(&symbols),
            [
                ("Point", 0, None),
                ("x", 1, Some(0)),
                ("y", 1, Some(0)),
                ("Inner", 1, Some(0)),
                ("f", 2, Some(3)),
                ("main", 0, None),
            ]
        );
        assert_eq!(symbols[0].detail.as_deref(), Some("Point detail"));
        assert!(has_children(&symbols, 0));
        assert!(!has_children(&symbols, 1));
        assert!(has_children(&symbols, 3));
        assert!(!has_children(&symbols, 5));
        assert_eq!(siblings(&symbols, 2), [1, 2, 3]);
        assert_eq!(siblings(&symbols, 5), [0, 5]);
    }

    #[test]
    fn nests_flat_symbols_by_containment_and_container_name() {
        let response = DocumentSymbolResponse::Flat(vec![
            info(
                "method",
                SymbolKind::METHOD,
                range((2, 4), (4, 5)),
                Some("Class"),
            ),
            info("Class", SymbolKind::CLASS, range((1, 0), (6, 0)), None),
            info("CONST", SymbolKind::CONSTANT, range((0, 0), (0, 9)), None),
            info(
                "attr",
                SymbolKind::FIELD,
                range((3, 8), (3, 12)),
                Some("method"),
            ),
            // Outside the class range, attached by its container name.
            info(
                "late",
                SymbolKind::METHOD,
                range((8, 0), (8, 4)),
                Some("Class"),
            ),
            info("free", SymbolKind::FUNCTION, range((7, 0), (7, 9)), None),
        ]);
        assert_eq!(
            shape(&flatten(response)),
            [
                ("CONST", 0, None),
                ("Class", 0, None),
                ("method", 1, Some(1)),
                ("attr", 2, Some(2)),
                ("late", 1, Some(1)),
                ("free", 0, None),
            ]
        );
    }

    #[test]
    fn symbol_path_is_the_innermost_containing_chain() {
        let symbols = flatten(DocumentSymbolResponse::Nested(vec![
            doc(
                "Point",
                SymbolKind::STRUCT,
                range((0, 0), (10, 1)),
                vec![
                    doc("x", SymbolKind::FIELD, range((1, 4), (1, 10)), vec![]),
                    doc(
                        "Inner",
                        SymbolKind::CLASS,
                        range((3, 0), (9, 0)),
                        vec![doc("f", SymbolKind::METHOD, range((4, 0), (5, 0)), vec![])],
                    ),
                ],
            ),
            doc(
                "main",
                SymbolKind::FUNCTION,
                range((12, 0), (14, 1)),
                vec![],
            ),
        ]));
        let at = |line, character| symbol_path_at(&symbols, Position::new(line, character));
        assert_eq!(at(4, 2), [0, 2, 3]);
        assert_eq!(at(1, 5), [0, 1]);
        assert_eq!(at(2, 0), [0]);
        // Range ends count as inside.
        assert_eq!(at(10, 1), [0]);
        // Between symbols.
        assert_eq!(at(11, 0), Vec::<usize>::new());
        assert_eq!(at(13, 0), [4]);
    }

    #[test]
    fn workspace_symbols_handle_flat_and_nested_responses() {
        let flat = workspace_symbols(WorkspaceSymbolResponse::Flat(vec![info(
            "run",
            SymbolKind::FUNCTION,
            range((3, 0), (5, 1)),
            Some(""),
        )]));
        assert_eq!(flat.len(), 1);
        assert_eq!(flat[0].name, "run");
        assert_eq!(flat[0].container, None);
        assert_eq!(flat[0].range, Some(range((3, 0), (5, 1))));
        assert!(flat[0].path.ends_with("a.rs"));

        let nested = workspace_symbols(WorkspaceSymbolResponse::Nested(vec![
            WorkspaceSymbol {
                name: "Point".into(),
                kind: SymbolKind::STRUCT,
                tags: None,
                container_name: Some("geometry".into()),
                location: OneOf::Right(WorkspaceLocation { uri: uri() }),
                data: None,
            },
            WorkspaceSymbol {
                name: "remote".into(),
                kind: SymbolKind::STRUCT,
                tags: None,
                container_name: None,
                location: OneOf::Left(Location::new(
                    "untitled:Untitled-1".parse().unwrap(),
                    range((0, 0), (0, 1)),
                )),
                data: None,
            },
        ]));
        assert_eq!(nested.len(), 1);
        assert_eq!(nested[0].container.as_deref(), Some("geometry"));
        assert_eq!(nested[0].range, None);
    }

    #[test]
    fn kind_names() {
        assert_eq!(kind_name(SymbolKind::ENUM_MEMBER), "enum member");
        assert_eq!(kind_name(SymbolKind::STRUCT), "struct");
    }
}
