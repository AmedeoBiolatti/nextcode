; Refines tree-sitter-md's block highlights.scm; appended after it, so these
; patterns win for the same node. Inline markup (emphasis, code spans,
; links in text) needs the separate inline grammar and is not highlighted.

[
  (atx_h1_marker)
  (atx_h2_marker)
  (atx_h3_marker)
  (atx_h4_marker)
  (atx_h5_marker)
  (atx_h6_marker)
  (setext_h1_underline)
  (setext_h2_underline)
] @markup.heading

[
  (list_marker_plus)
  (list_marker_minus)
  (list_marker_star)
  (list_marker_dot)
  (list_marker_parenthesis)
  (task_list_marker_checked)
  (task_list_marker_unchecked)
] @markup.list

(block_quote) @markup.quote
(block_quote (block_quote_marker) @markup.quote)
(block_quote (block_continuation) @markup.quote)

(info_string) @label

(pipe_table_header (pipe_table_cell) @markup.heading)
(pipe_table_delimiter_row) @punctuation.special
(pipe_table_header "|" @punctuation.special)
(pipe_table_row "|" @punctuation.special)
