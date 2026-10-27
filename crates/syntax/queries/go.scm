; Refines tree-sitter-go's highlights.scm; appended after it, so these
; patterns win for the same node.

; Upstream lists these before its generic `(identifier) @variable` and
; `(field_identifier) @property`, expecting the first pattern to win.
(call_expression
  function: (identifier) @function)
((call_expression
  function: (identifier) @function.builtin)
 (#match? @function.builtin "^(append|cap|clear|close|complex|copy|delete|imag|len|make|max|min|new|panic|print|println|real|recover)$"))
(call_expression
  function: (selector_expression
    field: (field_identifier) @function.method))
(function_declaration
  name: (identifier) @function)
(method_declaration
  name: (field_identifier) @function.method)

"func" @keyword.function

"return" @keyword.return

[
  "if"
  "else"
  "switch"
  "select"
  "case"
  "default"
  "for"
  "range"
  "break"
  "continue"
  "goto"
  "fallthrough"
] @keyword.control

["import" "package"] @keyword.import

["struct" "interface" "type" "map" "chan"] @keyword.type

["var" "const"] @keyword.storage

(package_identifier) @module

((type_identifier) @type.builtin
 (#match? @type.builtin "^(any|bool|byte|comparable|complex64|complex128|error|float32|float64|int|int8|int16|int32|int64|rune|string|uint|uint8|uint16|uint32|uint64|uintptr)$"))

(parameter_declaration name: (identifier) @variable.parameter)
(variadic_parameter_declaration name: (identifier) @variable.parameter)

[(true) (false)] @boolean

(label_name) @label

["(" ")" "[" "]" "{" "}"] @punctuation.bracket
["," ";" "." ":"] @punctuation.delimiter
