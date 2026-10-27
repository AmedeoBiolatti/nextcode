; Refines tree-sitter-rust's highlights.scm; appended after it, so these
; patterns win for the same node.

[
  "!"
  "!="
  "%"
  "%="
  "&&"
  "&="
  "*="
  "+"
  "+="
  "-"
  "-="
  "->"
  "/"
  "/="
  "<<"
  "<<="
  "<="
  "="
  "=="
  "=>"
  ">="
  ">>"
  ">>="
  "@"
  "^"
  "^="
  "|"
  "|="
  "||"
  ".."
  "..="
  "..."
  "?"
] @operator
(binary_expression ["<" ">"] @operator)

"fn" @keyword.function

["return" "yield"] @keyword.return

[
  "if"
  "else"
  "match"
  "loop"
  "while"
  "for"
  "break"
  "continue"
] @keyword.control
(for_expression "in" @keyword.control)

["use" "mod"] @keyword.import
(use_as_clause "as" @keyword.import)
(extern_crate_declaration
  "extern" @keyword.import
  (crate) @keyword.import)

[
  "struct"
  "enum"
  "union"
  "trait"
  "type"
] @keyword.type

[
  "const"
  "static"
  "pub"
  "unsafe"
  "extern"
  "dyn"
  "ref"
  "move"
  (mutable_specifier)
] @keyword.storage

"as" @keyword.operator

"macro_rules!" @function.macro
(macro_invocation
  macro: (scoped_identifier
    name: (identifier) @function.macro))
(macro_invocation "!" @function.macro)
(macro_definition name: (identifier) @function.macro)

(lifetime "'" @label)
(label "'" @label)
(label (identifier) @label)

[(integer_literal) (float_literal)] @number
(boolean_literal) @boolean

; Upstream's all-caps pattern never matches (its regex ends in a stray `'`).
((identifier) @constant
 (#match? @constant "^[A-Z][A-Z\\d_]+$"))

; Upstream ranks `@constructor` first; with later-wins it must follow calls.
((call_expression
  function: (identifier) @constructor)
 (#match? @constructor "^[A-Z]"))
((call_expression
  function: (scoped_identifier
    name: (identifier) @constructor))
 (#match? @constructor "^[A-Z]"))

(mod_item name: (identifier) @module)
((scoped_identifier
  path: (identifier) @module)
 (#match? @module "^[a-z_]"))
((scoped_type_identifier
  path: (identifier) @module)
 (#match? @module "^[a-z_]"))
((scoped_use_list
  path: (identifier) @module)
 (#match? @module "^[a-z_]"))

(closure_parameters (identifier) @variable.parameter)

(shorthand_field_identifier) @property

(attribute_item (attribute (identifier) @attribute))
(inner_attribute_item (attribute (identifier) @attribute))
(attribute_item ["#" "[" "]"] @attribute)
(inner_attribute_item ["#" "!" "[" "]"] @attribute)

; Doc comment markers are made of `/`, `*` and `!` tokens.
(outer_doc_comment_marker ["/" "*"] @comment.doc)
(inner_doc_comment_marker "!" @comment.doc)
