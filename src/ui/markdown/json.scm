; JSON highlighting. A key is a string in the grammar, and the blanket string
; pattern would paint it the same green as a value. The key pattern is listed
; after that one so the field name keeps its own colour.

(comment) @comment

(string) @string

(pair
  key: (string) @string.special.key)

(number) @number

[
  (null)
  (true)
  (false)
] @constant.builtin

(escape_sequence) @escape
