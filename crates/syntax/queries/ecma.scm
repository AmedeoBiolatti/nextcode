; Refines tree-sitter-javascript's highlights.scm for JavaScript, TypeScript
; and TSX; appended after the upstream queries, so these patterns win for the
; same node.

"function" @keyword.function

["return" "yield"] @keyword.return

[
  "if"
  "else"
  "switch"
  "case"
  "for"
  "while"
  "do"
  "break"
  "continue"
  "try"
  "catch"
  "finally"
  "throw"
] @keyword.control
(switch_default "default" @keyword.control)

["import" "export" "from"] @keyword.import
(import_specifier "as" @keyword.import)
(namespace_import "as" @keyword.import)
(export_specifier "as" @keyword.import)

["const" "let" "var" "static"] @keyword.storage

"class" @keyword.type

[
  "typeof"
  "instanceof"
  "in"
  "delete"
  "void"
  "new"
] @keyword.operator

[(true) (false)] @boolean

(class_declaration name: (_) @type)
(class name: (_) @type)
(new_expression constructor: (identifier) @constructor)

(arrow_function parameter: (identifier) @variable.parameter)

(decorator "@" @attribute)
(decorator (identifier) @attribute)
(decorator (call_expression function: (identifier) @attribute))

(regex) @string.regex
(regex "/" @string.regex)

(statement_identifier) @label
(private_property_identifier) @property

"..." @operator
(ternary_expression ["?" ":"] @operator)

(hash_bang_line) @keyword.directive

((comment) @comment.doc
 (#match? @comment.doc "^/\\*\\*[^*/]"))
