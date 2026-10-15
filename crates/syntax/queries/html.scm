; Refines tree-sitter-html's highlights.scm; appended after it, so these
; patterns win for the same node.

(doctype) @keyword.directive
"<!" @punctuation.bracket
(entity) @string.escape
(quoted_attribute_value) @string
(attribute "=" @punctuation.delimiter)
