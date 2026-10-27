; Refines tree-sitter-cpp's highlights.scm; appended after it and `c.scm`.

["class" "typename" "namespace" "concept"] @keyword.type

[
  "constexpr"
  "consteval"
  "constinit"
  "explicit"
  "final"
  "friend"
  "mutable"
  "override"
  "virtual"
  "private"
  "protected"
  "public"
] @keyword.storage

["try" "catch" "throw"] @keyword.control

["co_return" "co_yield"] @keyword.return

["new" "delete"] @keyword.operator

((namespace_identifier) @module
 (#match? @module "^[a-z_]"))

(null "nullptr" @constant.builtin)
(auto) @type.builtin

(optional_parameter_declaration
  declarator: (identifier) @variable.parameter)
(parameter_declaration
  declarator: (reference_declarator
    (identifier) @variable.parameter))
(optional_parameter_declaration
  declarator: (reference_declarator
    (identifier) @variable.parameter))

"::" @punctuation.delimiter
(template_argument_list ["<" ">"] @punctuation.bracket)
(template_parameter_list ["<" ">"] @punctuation.bracket)
