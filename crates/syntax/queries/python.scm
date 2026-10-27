; Refines tree-sitter-python's highlights.scm; appended after it, so these
; patterns win for the same node.

["def" "lambda"] @keyword.function

"class" @keyword.type

["return" "yield"] @keyword.return

[
  "if"
  "elif"
  "else"
  "match"
  "case"
  "for"
  "while"
  "break"
  "continue"
  "try"
  "except"
  "finally"
  "raise"
] @keyword.control

[
  "import"
  "__future__"
] @keyword.import
(import_from_statement "from" @keyword.import)
(future_import_statement "from" @keyword.import)
(aliased_import "as" @keyword.import)
(raise_statement "from" @keyword.control)

(import_statement name: (dotted_name (identifier) @module))
(import_from_statement module_name: (dotted_name (identifier) @module))
(import_statement
  name: (aliased_import name: (dotted_name (identifier) @module)))
(relative_import (dotted_name (identifier) @module))

["global" "nonlocal"] @keyword.storage

[
  "and"
  "or"
  "not"
  "in"
  "is"
  "is not"
  "not in"
] @keyword.operator

[(true) (false)] @boolean

(class_definition
  name: (identifier) @type)

; Calls to capitalized names: constructors, not upstream's later `@function`.
((call
  function: (identifier) @constructor)
 (#match? @constructor "^[A-Z]"))

(class_definition
  body: (block
    (function_definition
      name: (identifier) @function.method)))
(class_definition
  body: (block
    (decorated_definition
      definition: (function_definition
        name: (identifier) @function.method))))

(type (identifier) @type)
(generic_type (identifier) @type)
((type (identifier) @type.builtin)
 (#match? @type.builtin "^(bool|bytearray|bytes|complex|dict|float|frozenset|int|list|object|set|str|tuple|type)$"))

(decorator) @attribute
(decorator (identifier) @attribute)
(decorator (attribute attribute: (identifier) @attribute))

(parameters (list_splat_pattern (identifier) @variable.parameter))
(parameters (dictionary_splat_pattern (identifier) @variable.parameter))
(typed_parameter (list_splat_pattern (identifier) @variable.parameter))
(typed_parameter (dictionary_splat_pattern (identifier) @variable.parameter))
(lambda_parameters (list_splat_pattern (identifier) @variable.parameter))
(lambda_parameters (dictionary_splat_pattern (identifier) @variable.parameter))
(parameters (identifier) @variable.parameter)
(lambda_parameters (identifier) @variable.parameter)
(default_parameter name: (identifier) @variable.parameter)
(typed_parameter (identifier) @variable.parameter)
(typed_default_parameter name: (identifier) @variable.parameter)
(keyword_argument name: (identifier) @variable.parameter)

((identifier) @variable.builtin
 (#match? @variable.builtin "^(self|cls)$"))

; Docstrings: PyCharm styles them as documentation comments.
(module . (expression_statement (string) @comment.doc))
(class_definition
  body: (block . (expression_statement (string) @comment.doc)))
(function_definition
  body: (block . (expression_statement (string) @comment.doc)))

["(" ")" "[" "]" "{" "}"] @punctuation.bracket
["," "." ":" ";"] @punctuation.delimiter

(interpolation
  "{" @punctuation.special
  "}" @punctuation.special)
