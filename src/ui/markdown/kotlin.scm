; Highlight query for the Kotlin grammar in `tree-sitter-kotlin-ng`.
; The crate does not ship one. The node names are that grammar's, and the capture
; names are the same ones the other four languages use, so one colour table covers
; all five.

[
  "abstract" "actual" "annotation" "as" "as?" "by" "catch" "class" "companion"
  "const" "constructor" "crossinline" "data" "delegate" "do" "dynamic" "else"
  "enum" "expect" "external" "final" "finally" "for" "fun" "if" "import" "in"
  "infix" "init" "inline" "inner" "interface" "internal" "is" "lateinit"
  "noinline" "object" "open" "operator" "out" "override" "package" "private"
  "protected" "public" "return" "sealed" "super" "suspend" "tailrec" "this"
  "throw" "try" "typealias" "val" "var" "vararg" "when" "where" "while"
] @keyword

(line_comment) @comment
(block_comment) @comment

(string_literal) @string
(multiline_string_literal) @string

(number_literal) @number

(function_declaration name: (identifier) @function)
(class_declaration name: (identifier) @type)
(user_type (identifier) @type)
