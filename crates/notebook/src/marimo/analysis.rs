//! marimo's static analysis of a cell (a port of `marimo._ast.visitor.
//! ScopedVisitor` on tree-sitter-python): the global names a cell defines,
//! the names it references, and what marimo's code generator needs
//! (top-level await, the single function/class of a cell, annotations).

use std::{
    cell::RefCell,
    collections::{BTreeSet, HashMap, HashSet},
};

use tree_sitter::{Node, Parser, Tree};

/// `marimo._ast.variables.BUILTINS` (Python 3.13's `builtins` plus the
/// module globals marimo adds).
pub(super) const BUILTINS: &[&str] = &[
    "ArithmeticError",
    "AssertionError",
    "AttributeError",
    "BaseException",
    "BaseExceptionGroup",
    "BlockingIOError",
    "BrokenPipeError",
    "BufferError",
    "BytesWarning",
    "ChildProcessError",
    "ConnectionAbortedError",
    "ConnectionError",
    "ConnectionRefusedError",
    "ConnectionResetError",
    "DeprecationWarning",
    "EOFError",
    "Ellipsis",
    "EncodingWarning",
    "EnvironmentError",
    "Exception",
    "ExceptionGroup",
    "False",
    "FileExistsError",
    "FileNotFoundError",
    "FloatingPointError",
    "FutureWarning",
    "GeneratorExit",
    "IOError",
    "ImportError",
    "ImportWarning",
    "IndentationError",
    "IndexError",
    "InterruptedError",
    "IsADirectoryError",
    "KeyError",
    "KeyboardInterrupt",
    "LookupError",
    "MemoryError",
    "ModuleNotFoundError",
    "NameError",
    "None",
    "NotADirectoryError",
    "NotImplemented",
    "NotImplementedError",
    "OSError",
    "OverflowError",
    "PendingDeprecationWarning",
    "PermissionError",
    "ProcessLookupError",
    "PythonFinalizationError",
    "RecursionError",
    "ReferenceError",
    "ResourceWarning",
    "RuntimeError",
    "RuntimeWarning",
    "StopAsyncIteration",
    "StopIteration",
    "SyntaxError",
    "SyntaxWarning",
    "SystemError",
    "SystemExit",
    "TabError",
    "TimeoutError",
    "True",
    "TypeError",
    "UnboundLocalError",
    "UnicodeDecodeError",
    "UnicodeEncodeError",
    "UnicodeError",
    "UnicodeTranslateError",
    "UnicodeWarning",
    "UserWarning",
    "ValueError",
    "Warning",
    "ZeroDivisionError",
    "_IncompleteInputError",
    "__build_class__",
    "__builtin__",
    "__builtins__",
    "__class__",
    "__debug__",
    "__doc__",
    "__file__",
    "__import__",
    "__loader__",
    "__name__",
    "__package__",
    "__spec__",
    "abs",
    "aiter",
    "all",
    "anext",
    "any",
    "ascii",
    "bin",
    "bool",
    "breakpoint",
    "bytearray",
    "bytes",
    "callable",
    "chr",
    "classmethod",
    "compile",
    "complex",
    "copyright",
    "credits",
    "delattr",
    "dict",
    "dir",
    "divmod",
    "enumerate",
    "eval",
    "exec",
    "exit",
    "filter",
    "float",
    "format",
    "frozenset",
    "getattr",
    "globals",
    "hasattr",
    "hash",
    "help",
    "hex",
    "id",
    "input",
    "int",
    "isinstance",
    "issubclass",
    "iter",
    "len",
    "license",
    "list",
    "locals",
    "map",
    "max",
    "memoryview",
    "min",
    "next",
    "object",
    "oct",
    "open",
    "ord",
    "pow",
    "print",
    "property",
    "quit",
    "range",
    "repr",
    "reversed",
    "round",
    "set",
    "setattr",
    "slice",
    "sorted",
    "staticmethod",
    "str",
    "sum",
    "super",
    "tuple",
    "type",
    "vars",
    "zip",
];

pub(super) fn is_builtin(name: &str) -> bool {
    BUILTINS.binary_search(&name).is_ok()
}

/// marimo's "local" names: `_x` (not dunder) and `__`, private to a cell.
pub(super) fn is_local(name: &str) -> bool {
    name == "__" || (name.starts_with('_') && !name.starts_with("__"))
}

thread_local! {
    static PARSER: RefCell<Parser> = RefCell::new({
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_python::LANGUAGE.into())
            .expect("tree-sitter-python language");
        parser
    });
}

pub(super) fn parse_python(code: &str) -> Tree {
    PARSER.with(|parser| {
        parser
            .borrow_mut()
            .parse(code, None)
            .expect("tree-sitter parse")
    })
}

/// Named children, comments excluded.
pub(super) fn named(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter(|child| child.kind() != "comment")
        .collect()
}

/// Every child with its field name.
pub(super) fn fields<'t>(node: Node<'t>) -> Vec<(Option<&'t str>, Node<'t>)> {
    let mut cursor = node.walk();
    let mut out = Vec::new();
    if cursor.goto_first_child() {
        loop {
            out.push((cursor.field_name(), cursor.node()));
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum VarKind {
    Variable,
    Function,
    Method,
    Class,
    Import,
}

#[derive(Debug, Clone)]
pub(super) struct VarData {
    pub kind: VarKind,
    pub required_refs: HashSet<String>,
    pub unbounded_refs: HashSet<String>,
    /// `x: T = …` as the first definition at the top level: the
    /// annotation as `ast.unparse` writes it and the names it references.
    pub annotation: Option<(String, HashSet<String>)>,
}

impl VarData {
    fn new(kind: VarKind) -> Self {
        Self {
            kind,
            required_refs: HashSet::new(),
            unbounded_refs: HashSet::new(),
            annotation: None,
        }
    }
}

/// Everything marimo's compiler and code generator know about a cell.
#[derive(Debug, Clone, Default)]
pub(super) struct Detail {
    /// Top-level names, private ones included.
    pub defs: BTreeSet<String>,
    pub refs: BTreeSet<String>,
    pub syntax_error: Option<String>,
    /// Top-level `await` (marimo writes `async def`).
    pub coroutine: bool,
    /// First definition data of each top-level name.
    pub variables: HashMap<String, VarData>,
    /// The cell is exactly one `def`/`class` statement: its name.
    pub single_definition: Option<String>,
    /// The last top-level statement is an import, `def` or `class` ending
    /// on the cell's last line (marimo then writes a blank line before
    /// `return`, as ruff formats).
    pub ends_with_definition: bool,
    /// The cell's last token is a comment at column 0.
    pub trailing_comment: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scope {
    Module,
    Function,
    Class,
    Comprehension,
    Annotation,
}

struct Block {
    id: usize,
    defs: HashSet<String>,
    scope: Scope,
    globals: HashSet<String>,
}

struct RefData {
    block: usize,
    parents: Vec<usize>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Ctx {
    Store,
    Del,
}

struct Visitor<'a> {
    src: &'a str,
    blocks: Vec<Block>,
    next_block: usize,
    ref_stack: Vec<HashSet<String>>,
    obscured: Vec<Option<String>>,
    refs: HashMap<String, Vec<RefData>>,
    /// Top-level variable data in first-definition order.
    module_vars: Vec<(String, Vec<VarData>)>,
    /// Loop depth per function (innermost last), for `break`/`continue`.
    loops: Vec<usize>,
    coroutine: bool,
    error: Option<String>,
}

pub(super) fn analyze_detail(code: &str) -> Detail {
    let tree = parse_python(code);
    let root = tree.root_node();
    if root.has_error() {
        return Detail {
            syntax_error: Some(syntax_error_message(root)),
            ..Detail::default()
        };
    }
    let mut visitor = Visitor::new(code);
    visitor.visit_children(root);
    if let Some(error) = visitor.error.take() {
        return Detail {
            syntax_error: Some(error),
            ..Detail::default()
        };
    }
    let statements = named(root);
    let defs: BTreeSet<String> = visitor.blocks[0].defs.iter().cloned().collect();
    let single_definition = match statements.as_slice() {
        [statement] if defs.len() == 1 => definition_name(*statement, code)
            .filter(|name| defs.contains(*name))
            .map(str::to_owned),
        _ => None,
    };
    let ends_with_definition = statements.last().is_some_and(|last| {
        let definition = matches!(
            last.kind(),
            "import_statement"
                | "import_from_statement"
                | "future_import_statement"
                | "function_definition"
                | "class_definition"
                | "decorated_definition"
        );
        let last_line = code.trim_end().lines().last().unwrap_or("");
        definition && !last_line.trim_start().starts_with('#')
    });
    let trailing_comment = last_leaf(root)
        .is_some_and(|leaf| leaf.kind() == "comment" && leaf.start_position().column == 0);
    let variables = visitor
        .module_vars
        .iter()
        .filter_map(|(name, data)| Some((name.clone(), data.first()?.clone())))
        .collect();
    Detail {
        refs: visitor.refs.keys().cloned().collect(),
        defs,
        syntax_error: None,
        coroutine: visitor.coroutine,
        variables,
        single_definition,
        ends_with_definition,
        trailing_comment,
    }
}

fn last_leaf(node: Node<'_>) -> Option<Node<'_>> {
    let mut node = node;
    loop {
        let count = node.child_count();
        if count == 0 {
            return Some(node);
        }
        node = node.child(count - 1)?;
    }
}

/// The name of a `def`/`class` statement (decorated or not).
pub(super) fn definition_name<'a>(statement: Node<'_>, src: &'a str) -> Option<&'a str> {
    let definition = match statement.kind() {
        "decorated_definition" => statement.child_by_field_name("definition")?,
        "function_definition" | "class_definition" => statement,
        _ => return None,
    };
    let name = definition.child_by_field_name("name")?;
    Some(&src[name.byte_range()])
}

fn syntax_error_message(root: Node<'_>) -> String {
    fn first_error(node: Node<'_>) -> Option<Node<'_>> {
        if node.is_error() || node.is_missing() {
            return Some(node);
        }
        let mut cursor = node.walk();
        let children: Vec<Node<'_>> = node.children(&mut cursor).collect();
        children
            .into_iter()
            .filter(|child| child.has_error())
            .find_map(first_error)
    }
    match first_error(root) {
        Some(node) => format!(
            "SyntaxError: invalid syntax (line {})",
            node.start_position().row + 1
        ),
        None => "SyntaxError: invalid syntax".into(),
    }
}

impl<'a> Visitor<'a> {
    fn new(src: &'a str) -> Self {
        Self {
            src,
            blocks: vec![Block {
                id: 0,
                defs: HashSet::new(),
                scope: Scope::Module,
                globals: HashSet::new(),
            }],
            next_block: 1,
            ref_stack: vec![HashSet::new()],
            obscured: Vec::new(),
            refs: HashMap::new(),
            module_vars: Vec::new(),
            loops: vec![0],
            coroutine: false,
            error: None,
        }
    }

    fn text(&self, node: Node<'_>) -> &'a str {
        &self.src[node.byte_range()]
    }

    fn fail(&mut self, message: impl Into<String>) {
        if self.error.is_none() {
            self.error = Some(message.into());
        }
    }

    // --- blocks, definitions, references ------------------------------

    fn push_block(&mut self, scope: Scope) {
        let id = self.next_block;
        self.next_block += 1;
        self.blocks.push(Block {
            id,
            defs: HashSet::new(),
            scope,
            globals: HashSet::new(),
        });
    }

    fn pop_block(&mut self) {
        self.blocks.pop();
    }

    fn current_scope(&self) -> Scope {
        self.blocks
            .last()
            .map_or(Scope::Module, |block| block.scope)
    }

    fn in_function(&self) -> bool {
        self.blocks
            .iter()
            .any(|block| block.scope == Scope::Function)
    }

    fn is_defined(&self, name: &str) -> bool {
        self.blocks.iter().any(|block| block.defs.contains(name))
    }

    fn is_obscured(&self, name: &str) -> bool {
        self.obscured.iter().any(|o| o.as_deref() == Some(name))
    }

    fn add_ref(&mut self, name: &str) {
        let current = self.blocks.last().map_or(0, |block| block.id);
        let parents: Vec<usize> = self.blocks[..self.blocks.len() - 1]
            .iter()
            .map(|block| block.id)
            .collect();
        let entry = self.refs.entry(name.to_owned()).or_default();
        if !entry.iter().any(|r| r.block == current) {
            entry.push(RefData {
                block: current,
                parents,
            });
        }
        if let Some(top) = self.ref_stack.last_mut() {
            top.insert(name.to_owned());
        }
    }

    fn define_in_block(&mut self, name: &str, data: VarData, block_idx: usize) {
        let block_id = self.blocks[block_idx].id;
        self.blocks[block_idx].defs.insert(name.to_owned());
        let kind = data.kind;
        if block_idx == 0 {
            match self.module_vars.iter_mut().find(|(n, _)| n == name) {
                Some((_, list)) => list.push(data),
                None => self.module_vars.push((name.to_owned(), vec![data])),
            }
        }
        if kind != VarKind::Method
            && let Some(refs) = self.refs.get_mut(name)
            && refs.iter().any(|r| r.parents.contains(&block_id))
        {
            refs.retain(|r| !r.parents.contains(&block_id));
            if refs.is_empty() {
                self.refs.remove(name);
            }
        }
    }

    fn define(&mut self, name: &str, data: VarData) {
        let last = self.blocks.len() - 1;
        let idx = if self.blocks[last].globals.contains(name) {
            0
        } else {
            last
        };
        self.define_in_block(name, data, idx);
    }

    fn define_variable(&mut self, name: &str) {
        let data = VarData {
            required_refs: self.ref_stack.last().cloned().unwrap_or_default(),
            ..VarData::new(VarKind::Variable)
        };
        self.define(name, data);
    }

    fn load(&mut self, name: &str) {
        if self.is_obscured(name) {
            return;
        }
        if !self.is_defined(name) && !is_local(name) {
            self.add_ref(name);
        }
        if self.is_defined(name)
            && !self.ref_stack.last().is_some_and(|top| top.contains(name))
            && (self.blocks.len() == 1
                || !self.blocks[1..]
                    .iter()
                    .any(|block| block.defs.contains(name)))
            && let Some(top) = self.ref_stack.last_mut()
        {
            top.insert(name.to_owned());
        }
    }

    fn store(&mut self, name: &str, ctx: Ctx) {
        if self.is_obscured(name) {
            return;
        }
        match ctx {
            Ctx::Store => self.define_variable(name),
            Ctx::Del => {
                if !self.is_defined(name) && !is_local(name) {
                    self.add_ref(name);
                }
            }
        }
    }

    fn with_ref_scope(&mut self, f: impl FnOnce(&mut Self)) {
        self.ref_stack.push(HashSet::new());
        f(self);
        let refs = self.ref_stack.pop().unwrap_or_default();
        if let Some(top) = self.ref_stack.last_mut() {
            top.extend(refs);
        }
    }

    // --- visiting -----------------------------------------------------

    fn visit_children(&mut self, node: Node<'_>) {
        for child in named(node) {
            self.visit(child);
        }
    }

    fn visit_opt(&mut self, node: Option<Node<'_>>) {
        if let Some(node) = node {
            self.visit(node);
        }
    }

    fn visit(&mut self, node: Node<'_>) {
        match node.kind() {
            "comment" => {}
            "identifier" | "keyword_identifier" => {
                let name = self.text(node);
                self.load(name);
            }
            "attribute" => self.visit_opt(node.child_by_field_name("object")),
            "keyword_argument" => self.visit_opt(node.child_by_field_name("value")),
            "assignment" => self.visit_assignment(node),
            "augmented_assignment" => {
                self.with_ref_scope(|this| {
                    this.visit_opt(node.child_by_field_name("right"));
                    if let Some(left) = node.child_by_field_name("left") {
                        this.target(left, Ctx::Store);
                    }
                });
            }
            "import_statement" | "future_import_statement" => {
                for (field, child) in fields(node) {
                    if field == Some("name") {
                        self.define_import(child, true);
                    }
                }
            }
            "import_from_statement" => {
                for (field, child) in fields(node) {
                    match (field, child.kind()) {
                        (Some("name"), _) => self.define_import(child, false),
                        (_, "wildcard_import") => self.fail(format!(
                            "line {} SyntaxError: Importing symbols with `import *` is not allowed in marimo.",
                            node.start_position().row + 1
                        )),
                        _ => {}
                    }
                }
            }
            "decorated_definition" => {
                let decorators: Vec<Node<'_>> = named(node)
                    .into_iter()
                    .filter(|child| child.kind() == "decorator")
                    .collect();
                if let Some(definition) = node.child_by_field_name("definition") {
                    match definition.kind() {
                        "class_definition" => self.visit_class(definition, &decorators),
                        _ => self.visit_function(definition, &decorators),
                    }
                }
            }
            "function_definition" => self.visit_function(node, &[]),
            "class_definition" => self.visit_class(node, &[]),
            "for_statement" => {
                if is_async(node) && !self.in_function() {
                    self.coroutine = true;
                }
                if let Some(left) = node.child_by_field_name("left") {
                    self.target(left, Ctx::Store);
                }
                self.visit_opt(node.child_by_field_name("right"));
                self.visit_loop_body(node.child_by_field_name("body"));
                self.visit_opt(node.child_by_field_name("alternative"));
            }
            "while_statement" => {
                self.visit_opt(node.child_by_field_name("condition"));
                self.visit_loop_body(node.child_by_field_name("body"));
                self.visit_opt(node.child_by_field_name("alternative"));
            }
            "with_statement" => {
                if is_async(node) && !self.in_function() {
                    self.coroutine = true;
                }
                for child in named(node) {
                    if child.kind() == "with_clause" {
                        for item in named(child) {
                            self.visit_with_item(item);
                        }
                    } else {
                        self.visit(child);
                    }
                }
            }
            "try_statement" => {
                for child in named(node) {
                    match child.kind() {
                        "except_clause" | "except_group_clause" => {
                            // `except E as name` (an `alias` field, or an
                            // `as_pattern` in older grammars).
                            let mut alias = child
                                .child_by_field_name("alias")
                                .map(|alias| self.text(alias).to_owned());
                            let mut parts = Vec::new();
                            for (field, part) in fields(child) {
                                if field == Some("alias") || !part.is_named() {
                                    continue;
                                }
                                if part.kind() == "as_pattern" {
                                    for (field, inner) in fields(part) {
                                        if field == Some("alias") {
                                            alias = Some(self.text(inner).trim().to_owned());
                                        } else if inner.is_named() {
                                            parts.push(inner);
                                        }
                                    }
                                } else {
                                    parts.push(part);
                                }
                            }
                            self.obscured.push(alias);
                            for part in parts {
                                self.visit(part);
                            }
                            self.obscured.pop();
                        }
                        _ => self.visit(child),
                    }
                }
            }
            "lambda" => {
                if let Some(top) = self.ref_stack.last_mut() {
                    top.insert("_lambda".into());
                }
                self.push_block(Scope::Function);
                self.loops.push(0);
                if let Some(parameters) = node.child_by_field_name("parameters") {
                    self.visit_parameters(parameters);
                }
                self.visit_opt(node.child_by_field_name("body"));
                self.loops.pop();
                self.pop_block();
            }
            "list_comprehension"
            | "set_comprehension"
            | "generator_expression"
            | "dictionary_comprehension" => self.visit_comprehension(node),
            "named_expression" => {
                self.visit_opt(node.child_by_field_name("value"));
                let Some(target) = node.child_by_field_name("name") else {
                    return;
                };
                let name = self.text(target);
                if self.current_scope() == Scope::Comprehension {
                    if let Some(idx) = self
                        .blocks
                        .iter()
                        .rposition(|block| block.scope != Scope::Comprehension)
                    {
                        self.define_in_block(name, VarData::new(VarKind::Variable), idx);
                    }
                } else {
                    self.store(name, Ctx::Store);
                }
            }
            "global_statement" => {
                for child in named(node) {
                    let name = self.text(child);
                    if let Some(block) = self.blocks.last_mut() {
                        block.globals.insert(name.to_owned());
                    }
                    if !self.blocks[0].defs.contains(name) {
                        self.add_ref(name);
                    }
                }
            }
            "nonlocal_statement" => {}
            "delete_statement" => {
                for child in named(node) {
                    self.target(child, Ctx::Del);
                }
            }
            "match_statement" => {
                for (field, child) in fields(node) {
                    match field {
                        Some("subject") => self.visit(child),
                        Some("body") => {
                            for case in named(child) {
                                self.visit_case(case);
                            }
                        }
                        _ => {}
                    }
                }
            }
            "type_alias_statement" => {
                let left = node.child_by_field_name("left");
                let (name, params) =
                    left.map_or((None, Vec::new()), |left| self.type_alias_name(left));
                if let Some(name) = name {
                    self.store(name, Ctx::Store);
                }
                self.push_block(Scope::Annotation);
                for param in params {
                    self.define_variable(param);
                }
                self.visit_opt(node.child_by_field_name("right"));
                self.pop_block();
            }
            "await" => {
                if !self.in_function() {
                    self.coroutine = true;
                }
                self.visit_children(node);
            }
            "return_statement" => {
                if !self.in_function() {
                    self.fail("SyntaxError: 'return' outside function");
                }
                self.visit_children(node);
            }
            "yield" => {
                if !self.in_function() {
                    self.fail("SyntaxError: 'yield' outside function");
                }
                self.visit_children(node);
            }
            "break_statement" | "continue_statement" => {
                if self.loops.last().copied().unwrap_or(0) == 0 {
                    let keyword = if node.kind() == "break_statement" {
                        "break"
                    } else {
                        "continue"
                    };
                    self.fail(format!("SyntaxError: '{keyword}' outside loop"));
                }
            }
            "parameters" | "lambda_parameters" => self.visit_parameters(node),
            _ => self.visit_children(node),
        }
    }

    fn visit_loop_body(&mut self, body: Option<Node<'_>>) {
        if let Some(depth) = self.loops.last_mut() {
            *depth += 1;
        }
        self.visit_opt(body);
        if let Some(depth) = self.loops.last_mut() {
            *depth -= 1;
        }
    }

    fn define_import(&mut self, node: Node<'_>, plain_import: bool) {
        let name = match node.kind() {
            "aliased_import" => node
                .child_by_field_name("alias")
                .map(|alias| self.text(alias)),
            "dotted_name" => {
                let text = self.text(node);
                Some(if plain_import {
                    text.split('.').next().unwrap_or(text).trim()
                } else {
                    text
                })
            }
            _ => None,
        };
        if let Some(name) = name {
            self.define(name, VarData::new(VarKind::Import));
        }
    }

    fn visit_with_item(&mut self, item: Node<'_>) {
        let Some(value) = item.child_by_field_name("value") else {
            return;
        };
        if value.kind() == "as_pattern" {
            let mut alias = None;
            for (field, child) in fields(value) {
                if field == Some("alias") {
                    alias = Some(child);
                } else if child.is_named() {
                    self.visit(child);
                }
            }
            if let Some(alias) = alias {
                for target in named(alias) {
                    self.target(target, Ctx::Store);
                }
            }
        } else {
            self.visit(value);
        }
    }

    fn visit_assignment(&mut self, node: Node<'_>) {
        if let Some(annotation) = node.child_by_field_name("type") {
            // AnnAssign.
            self.with_ref_scope(|this| {
                this.visit_opt(node.child_by_field_name("right"));
                this.ref_stack.push(HashSet::new());
                this.visit(annotation);
                let annotation_refs = this.ref_stack.pop().unwrap_or_default();
                if let Some(top) = this.ref_stack.last_mut() {
                    top.extend(annotation_refs.iter().cloned());
                }
                let Some(left) = node.child_by_field_name("left") else {
                    return;
                };
                let record = left.kind() == "identifier" && this.blocks.len() == 1 && {
                    let name = this.text(left);
                    !this.module_vars.iter().any(|(n, _)| n == name)
                };
                this.target(left, Ctx::Store);
                if record {
                    let name = this.text(left);
                    let repr = unparse_annotation(annotation, this.src);
                    if let Some((_, data)) = this.module_vars.iter_mut().find(|(n, _)| n == name)
                        && let Some(first) = data.first_mut()
                    {
                        first.annotation = Some((repr, annotation_refs));
                    }
                }
            });
            return;
        }
        // `a = b = value`: targets in order, value visited first.
        let mut targets = Vec::new();
        let mut value = Some(node);
        while let Some(current) = value {
            if current.kind() != "assignment" || current.child_by_field_name("type").is_some() {
                break;
            }
            if let Some(left) = current.child_by_field_name("left") {
                targets.push(left);
            }
            value = current.child_by_field_name("right");
        }
        self.with_ref_scope(|this| {
            this.visit_opt(value);
            for target in targets {
                this.target(target, Ctx::Store);
            }
        });
    }

    fn target(&mut self, node: Node<'_>, ctx: Ctx) {
        match node.kind() {
            "identifier" | "keyword_identifier" => {
                let name = self.text(node);
                self.store(name, ctx);
            }
            "pattern_list"
            | "tuple_pattern"
            | "list_pattern"
            | "tuple"
            | "list"
            | "parenthesized_expression"
            | "expression_list"
            | "list_splat_pattern"
            | "list_splat"
            | "as_pattern_target" => {
                for child in named(node) {
                    self.target(child, ctx);
                }
            }
            "attribute" => self.visit_opt(node.child_by_field_name("object")),
            "subscript" => self.visit_children(node),
            _ => self.visit(node),
        }
    }

    fn visit_parameters(&mut self, node: Node<'_>) {
        let params = named(node);
        // Defaults are evaluated first (`def f(x=x)` references `x`).
        for param in &params {
            if matches!(
                param.kind(),
                "default_parameter" | "typed_default_parameter"
            ) {
                self.visit_opt(param.child_by_field_name("value"));
            }
        }
        for param in params {
            match param.kind() {
                "identifier" => {
                    let name = self.text(param);
                    self.define(name, VarData::new(VarKind::Variable));
                }
                "default_parameter" | "typed_default_parameter" => {
                    if let Some(name) = param.child_by_field_name("name")
                        && name.kind() == "identifier"
                    {
                        let name = self.text(name);
                        self.define(name, VarData::new(VarKind::Variable));
                    }
                    self.visit_opt(param.child_by_field_name("type"));
                }
                "typed_parameter" => {
                    for child in named(param) {
                        if param.child_by_field_name("type") == Some(child) {
                            continue;
                        }
                        self.define_parameter(child);
                    }
                    self.visit_opt(param.child_by_field_name("type"));
                }
                "list_splat_pattern" | "dictionary_splat_pattern" => self.define_parameter(param),
                _ => {}
            }
        }
    }

    fn define_parameter(&mut self, node: Node<'_>) {
        match node.kind() {
            "identifier" => {
                let name = self.text(node);
                self.define(name, VarData::new(VarKind::Variable));
            }
            "list_splat_pattern" | "dictionary_splat_pattern" => {
                for child in named(node) {
                    self.define_parameter(child);
                }
            }
            _ => {}
        }
    }

    fn type_parameters(&self, node: Option<Node<'_>>) -> Vec<&'a str> {
        let mut out = Vec::new();
        let Some(node) = node else {
            return out;
        };
        for param in named(node) {
            // `T`, `T: bound`, `*Ts`, `**P`: the first identifier.
            fn first_identifier(node: Node<'_>) -> Option<Node<'_>> {
                if node.kind() == "identifier" {
                    return Some(node);
                }
                named(node).into_iter().find_map(first_identifier)
            }
            if let Some(identifier) = first_identifier(param) {
                out.push(self.text(identifier));
            }
        }
        out
    }

    fn type_alias_name(&self, left: Node<'_>) -> (Option<&'a str>, Vec<&'a str>) {
        fn find<'t>(node: Node<'t>, kind: &str) -> Option<Node<'t>> {
            if node.kind() == kind {
                return Some(node);
            }
            named(node).into_iter().find_map(|child| find(child, kind))
        }
        let name = find(left, "identifier").map(|n| self.text(n));
        let params = find(left, "type_parameter")
            .map(|params| self.type_parameters(Some(params)))
            .unwrap_or_default();
        (name, params)
    }

    fn visit_function(&mut self, node: Node<'_>, decorators: &[Node<'_>]) {
        let Some(name_node) = node.child_by_field_name("name") else {
            return;
        };
        let name = self.text(name_node);
        let generics = self.type_parameters(node.child_by_field_name("type_parameters"));
        self.ref_stack.push(HashSet::new());
        // What is evaluated where the function is defined: the signature
        // and decorators, without the body.
        self.push_block(Scope::Function);
        for generic in &generics {
            self.define_variable(generic);
        }
        if let Some(parameters) = node.child_by_field_name("parameters") {
            self.visit_parameters(parameters);
        }
        for decorator in decorators {
            self.visit_children(*decorator);
        }
        self.visit_opt(node.child_by_field_name("return_type"));
        self.pop_block();
        let mut unbounded = self.ref_stack.last().cloned().unwrap_or_default();

        let in_class = self.current_scope() == Scope::Class;
        if !in_class && let Some(block) = self.blocks.last_mut() {
            block.defs.insert(name.to_owned());
        }
        self.push_block(Scope::Function);
        self.loops.push(0);
        for generic in &generics {
            self.define_variable(generic);
        }
        if let Some(parameters) = node.child_by_field_name("parameters") {
            self.visit_parameters(parameters);
        }
        self.visit_opt(node.child_by_field_name("body"));
        for decorator in decorators {
            self.visit_children(*decorator);
        }
        self.visit_opt(node.child_by_field_name("return_type"));
        self.loops.pop();
        self.pop_block();
        let mut refs = self.ref_stack.pop().unwrap_or_default();
        if let Some(top) = self.ref_stack.last_mut() {
            top.extend(refs.iter().cloned());
        }
        for generic in &generics {
            refs.remove(*generic);
            unbounded.remove(*generic);
        }
        let kind = if in_class {
            VarKind::Method
        } else {
            VarKind::Function
        };
        self.define(
            name,
            VarData {
                required_refs: refs,
                unbounded_refs: unbounded,
                ..VarData::new(kind)
            },
        );
    }

    fn visit_class(&mut self, node: Node<'_>, decorators: &[Node<'_>]) {
        let Some(name_node) = node.child_by_field_name("name") else {
            return;
        };
        let name = self.text(name_node);
        let generics = self.type_parameters(node.child_by_field_name("type_parameters"));
        self.ref_stack.push(HashSet::new());

        // The class body runs where the class is defined: analyse it as a
        // module to find what it needs from outside.
        let mut unbounded = HashSet::new();
        if let Some(body) = node.child_by_field_name("body") {
            let mut inner = Visitor::new(self.src);
            inner.visit_children(body);
            let mut ignore = HashSet::new();
            let mut class_defs: HashSet<String> = HashSet::new();
            for (var, data) in &inner.module_vars {
                for datum in data {
                    if matches!(
                        datum.kind,
                        VarKind::Function | VarKind::Method | VarKind::Class
                    ) {
                        unbounded.extend(datum.unbounded_refs.difference(&class_defs).cloned());
                        ignore.extend(datum.required_refs.iter().cloned());
                    } else {
                        unbounded.extend(
                            datum
                                .required_refs
                                .iter()
                                .filter(|r| !class_defs.contains(*r) && *r != var)
                                .cloned(),
                        );
                    }
                    class_defs.insert(var.clone());
                }
            }
            unbounded.extend(inner.refs.keys().filter(|r| !ignore.contains(*r)).cloned());
        }
        self.push_block(Scope::Class);
        for generic in &generics {
            self.define_variable(generic);
        }
        self.visit_opt(node.child_by_field_name("superclasses"));
        for decorator in decorators {
            self.visit_children(*decorator);
        }
        self.pop_block();
        unbounded.extend(self.ref_stack.last().cloned().unwrap_or_default());

        self.push_block(Scope::Class);
        for generic in &generics {
            self.define_variable(generic);
        }
        self.visit_opt(node.child_by_field_name("superclasses"));
        self.loops.push(0);
        self.visit_opt(node.child_by_field_name("body"));
        self.loops.pop();
        for decorator in decorators {
            self.visit_children(*decorator);
        }
        self.pop_block();
        let mut refs = self.ref_stack.pop().unwrap_or_default();
        if let Some(top) = self.ref_stack.last_mut() {
            top.extend(refs.iter().cloned());
        }
        for generic in &generics {
            refs.remove(*generic);
            unbounded.remove(*generic);
        }
        self.define(
            name,
            VarData {
                required_refs: refs,
                unbounded_refs: unbounded,
                ..VarData::new(VarKind::Class)
            },
        );
    }

    fn visit_comprehension(&mut self, node: Node<'_>) {
        self.push_block(Scope::Comprehension);
        for child in named(node) {
            match child.kind() {
                "for_in_clause" => {
                    if is_async(child) && !self.in_function() {
                        self.coroutine = true;
                    }
                    for (field, part) in fields(child) {
                        if field == Some("right") && part.is_named() {
                            self.visit(part);
                        }
                    }
                    if let Some(left) = child.child_by_field_name("left") {
                        self.target(left, Ctx::Store);
                    }
                }
                "if_clause" => self.visit_children(child),
                _ => {}
            }
        }
        if let Some(body) = node.child_by_field_name("body") {
            if body.kind() == "pair" {
                self.visit_opt(body.child_by_field_name("value"));
                self.visit_opt(body.child_by_field_name("key"));
            } else {
                self.visit(body);
            }
        }
        self.pop_block();
    }

    // --- match patterns -----------------------------------------------

    fn visit_case(&mut self, case: Node<'_>) {
        for (field, child) in fields(case) {
            match (field, child.kind()) {
                (Some("consequence"), _) | (Some("guard"), _) => self.visit(child),
                (_, "case_pattern") => self.visit_pattern(child),
                _ => {}
            }
        }
    }

    fn capture(&mut self, node: Node<'_>) {
        let name = self.text(node);
        if name != "_" {
            self.define(name, VarData::new(VarKind::Variable));
        }
    }

    /// A value pattern (`Color.RED`) or a class name: a load of its first
    /// component.
    fn load_dotted(&mut self, node: Node<'_>) {
        if let Some(first) = named(node).first() {
            let name = self.text(*first);
            self.load(name);
        }
    }

    fn visit_pattern(&mut self, node: Node<'_>) {
        match node.kind() {
            "identifier" => self.capture(node),
            "dotted_name" => {
                let parts = named(node);
                if parts.len() == 1 {
                    self.capture(parts[0]);
                } else {
                    self.load_dotted(node);
                }
            }
            "class_pattern" => {
                for (i, child) in named(node).into_iter().enumerate() {
                    if i == 0 && child.kind() == "dotted_name" {
                        self.load_dotted(child);
                    } else {
                        self.visit_pattern(child);
                    }
                }
            }
            "keyword_pattern" => {
                for (i, child) in named(node).into_iter().enumerate() {
                    if i > 0 {
                        self.visit_pattern(child);
                    }
                }
            }
            "as_pattern" => {
                for (field, child) in fields(node) {
                    if field == Some("alias") {
                        for target in named(child) {
                            self.visit_pattern(target);
                        }
                    } else if child.is_named() {
                        self.visit_pattern(child);
                    }
                }
            }
            "splat_pattern" => {
                for child in named(node) {
                    self.capture(child);
                }
            }
            "dict_pattern" => {
                for (field, child) in fields(node) {
                    match field {
                        Some("key") if child.kind() == "dotted_name" => self.load_dotted(child),
                        Some("key") => {}
                        _ if child.is_named() => self.visit_pattern(child),
                        _ => {}
                    }
                }
            }
            "case_pattern" | "list_pattern" | "tuple_pattern" | "union_pattern" => {
                for child in named(node) {
                    self.visit_pattern(child);
                }
            }
            _ => {}
        }
    }
}

fn is_async(node: Node<'_>) -> bool {
    node.child(0).is_some_and(|first| first.kind() == "async")
}

/// An annotation as `ast.unparse` (then marimo's quote standardisation)
/// writes it: tokens re-spaced, single-quoted strings double-quoted.
fn unparse_annotation(node: Node<'_>, src: &str) -> String {
    fn leaves<'t>(node: Node<'t>, out: &mut Vec<Node<'t>>) {
        if node.child_count() == 0 || node.kind() == "string" {
            out.push(node);
            return;
        }
        let mut cursor = node.walk();
        let children: Vec<Node<'t>> = node.children(&mut cursor).collect();
        for child in children {
            if child.kind() != "comment" {
                leaves(child, out);
            }
        }
    }
    let mut tokens = Vec::new();
    leaves(node, &mut tokens);
    let mut out = String::new();
    let mut prev: Option<&str> = None;
    for token in tokens {
        let mut text = src[token.byte_range()].to_owned();
        if token.kind() == "string" {
            text = text.replace('\n', "");
            if let Some(inner) = text.strip_prefix('\'').and_then(|t| t.strip_suffix('\''))
                && !inner.contains(['"', '\''])
            {
                text = format!("\"{inner}\"");
            }
        }
        let space_before = match prev {
            None => false,
            Some(_) if matches!(text.as_str(), "," | ")" | "]" | "." | "(" | "[" | ":" | "=") => {
                false
            }
            Some(p) => !matches!(p, "(" | "[" | "." | "=" | "*" | "**" | "~"),
        };
        if space_before {
            out.push(' ');
        }
        out.push_str(&text);
        prev = Some(if token.is_named() {
            "x"
        } else {
            &src[token.byte_range()]
        });
    }
    out
}
