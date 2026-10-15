; Refines tree-sitter-css's highlights.scm; appended after it, so these
; patterns win for the same node.

"@import" @keyword.import

; Upstream lists these before `(property_name) @property`, expecting the
; first pattern to win.
((property_name) @variable
 (#match? @variable "^--"))

((declaration (plain_value) @constant)
 (#not-match? @constant "^--"))

["[" "]"] @punctuation.bracket

; Upstream's `(pseudo_element_selector (tag_name) @attribute)` also takes
; the element in `a::before`.
(pseudo_element_selector . (tag_name) @tag)
