; Parameters, from tree-sitter-javascript 0.25.0 `queries/highlights-params.scm`
; (MIT), which the crate does not export.

(formal_parameters
  [
    (identifier) @variable.parameter
    (array_pattern
      (identifier) @variable.parameter)
    (object_pattern
      [
        (pair_pattern value: (identifier) @variable.parameter)
        (shorthand_property_identifier_pattern) @variable.parameter
      ])
  ]
)
