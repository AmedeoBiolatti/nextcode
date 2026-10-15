; Refines tree-sitter-bash's highlights.scm; appended after it, so these
; patterns win for the same node.

"function" @keyword.function

[
  "if"
  "then"
  "elif"
  "else"
  "fi"
  "case"
  "esac"
  "for"
  "select"
  "in"
  "while"
  "until"
  "do"
  "done"
] @keyword.control

[
  "declare"
  "typeset"
  "export"
  "readonly"
  "local"
] @keyword.storage

(variable_name) @variable
(special_variable_name) @variable.builtin

(simple_expansion "$" @punctuation.special)
(expansion
  [
    "${"
    "}"
  ] @punctuation.special)

((command_name (word) @function.builtin)
 (#match? @function.builtin "^(\\.|:|alias|bg|bind|break|builtin|caller|cd|command|compgen|complete|continue|dirs|disown|echo|enable|eval|exec|exit|fc|fg|getopts|hash|help|history|jobs|kill|let|logout|mapfile|popd|printf|pushd|pwd|read|readarray|return|set|shift|shopt|source|suspend|test|times|trap|true|false|type|ulimit|umask|unalias|wait)$"))

(number) @number

(regex) @string.regex
[(ansi_c_string) (translated_string)] @string

(test_operator) @operator
["=" "==" "!=" "=~" "||" "|&" "&>" "&>>" ">&" "<&" "<<" "<<<" ">|"] @operator

["(" ")" "((" "))" "[" "]" "[[" "]]" "{" "}"] @punctuation.bracket
[";" ";;" ";&" ";;&"] @punctuation.delimiter

; A shebang.
((program . (comment) @keyword.directive)
 (#match? @keyword.directive "^#!"))
