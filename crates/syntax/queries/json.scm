; Refines tree-sitter-json's highlights.scm; appended after it, so these
; patterns win for the same node. Upstream's `(string) @string` follows its
; key pattern, which would otherwise make keys plain strings.

(pair key: (string) @property)

[(true) (false)] @boolean
(null) @constant.builtin

["{" "}" "[" "]"] @punctuation.bracket
["," ":"] @punctuation.delimiter
