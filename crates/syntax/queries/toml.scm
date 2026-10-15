; Refines tree-sitter-toml-ng's highlights.scm; appended after it, so these
; patterns win for the same node. Quoted keys are keys too: properties in
; pairs, types in table headers (like bare keys upstream).

(pair (quoted_key) @property)
(pair (dotted_key (quoted_key) @property))

[
  (table (quoted_key) @type)
  (table (dotted_key (quoted_key) @type))
  (table_array_element (quoted_key) @type)
  (table_array_element (dotted_key (quoted_key) @type))
]

(escape_sequence) @string.escape
