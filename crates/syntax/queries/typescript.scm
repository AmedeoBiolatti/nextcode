; Refines tree-sitter-typescript's highlights.scm for TypeScript and TSX;
; appended after it and `ecma.scm`.

["interface" "type" "enum" "namespace" "module"] @keyword.type

[
  "abstract"
  "declare"
  "private"
  "protected"
  "public"
  "readonly"
  "override"
] @keyword.storage

"keyof" @keyword.operator
(as_expression "as" @keyword.operator)
(satisfies_expression "satisfies" @keyword.operator)

(type_parameters ["<" ">"] @punctuation.bracket)

(required_parameter
  pattern: [
    (array_pattern (identifier) @variable.parameter)
    (object_pattern
      [
        (pair_pattern value: (identifier) @variable.parameter)
        (shorthand_property_identifier_pattern) @variable.parameter
      ])
  ])
