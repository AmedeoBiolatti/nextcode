; Refines tree-sitter-c's highlights.scm (and is reused for C++ after
; tree-sitter-cpp's); appended after it, so these patterns win for the same node.

[
  "if"
  "else"
  "switch"
  "case"
  "while"
  "for"
  "do"
  "break"
  "continue"
  "goto"
] @keyword.control

; Not C++'s `= default`.
(case_statement "default" @keyword.control)

"return" @keyword.return

["struct" "union" "enum" "typedef"] @keyword.type

[
  (storage_class_specifier)
  (type_qualifier)
] @keyword.storage

"sizeof" @keyword.operator

[
  "#define"
  "#elif"
  "#else"
  "#endif"
  "#if"
  "#ifdef"
  "#ifndef"
  (preproc_directive)
] @keyword.directive
"#include" @keyword.import

[
  (primitive_type)
  (sized_type_specifier)
] @type.builtin

(null) @constant.builtin
[(true) (false)] @boolean

(char_literal) @string
(escape_sequence) @string.escape

(preproc_def name: (identifier) @constant)

(parameter_declaration
  declarator: (identifier) @variable.parameter)
(parameter_declaration
  declarator: (pointer_declarator
    declarator: (identifier) @variable.parameter))
(parameter_declaration
  declarator: (pointer_declarator
    declarator: (pointer_declarator
      declarator: (identifier) @variable.parameter)))

[
  "!"
  "%"
  "%="
  "*="
  "/"
  "/="
  "<<"
  "<<="
  "<="
  ">="
  ">>"
  ">>="
  "^"
  "^="
  "|"
  "|="
  "~"
  "&="
  "?"
] @operator

["(" ")" "[" "]" "{" "}"] @punctuation.bracket
"," @punctuation.delimiter

((comment) @comment.doc
 (#match? @comment.doc "^(///[^/]|//!|/\\*\\*[^*/]|/\\*!)"))
