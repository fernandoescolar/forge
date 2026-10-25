; From ngalaiko/tree-sitter-go-template (MIT), with Helm's own functions (Helm and Sprig).

[
  (field)
  (field_identifier)
] @property

(variable) @variable

(function_call
  function: (identifier) @function)

(method_call
  method: (selector_expression
    field: (field_identifier) @function))

"|" @operator
":=" @operator

((identifier) @function.builtin
 (#match? @function.builtin "^(and|call|html|index|slice|js|len|not|or|print|printf|println|urlquery|eq|ne|lt|le|gt|ge)$"))

; Helm's functions and the Sprig ones charts use most.
((identifier) @function.builtin
 (#match? @function.builtin "^(include|tpl|required|lookup|toYaml|fromYaml|toJson|fromJson|toToml|default|empty|coalesce|ternary|quote|squote|indent|nindent|trim|trimSuffix|trimPrefix|upper|lower|title|replace|contains|hasPrefix|hasSuffix|list|dict|get|set|hasKey|keys|merge|mergeOverwrite|until|b64enc|b64dec|sha256sum|printf|semverCompare|kindIs|typeOf|toString|int|int64|float64|join|split|splitList|first|last|append|uniq|has|without|regexMatch|regexReplaceAll|trunc|fail|now|date|randAlphaNum|genCA|genSelfSignedCert)$"))

"." @punctuation.delimiter
"," @punctuation.delimiter

"{{" @punctuation.bracket
"}}" @punctuation.bracket
"{{-" @punctuation.bracket
"-}}" @punctuation.bracket
")" @punctuation.bracket
"(" @punctuation.bracket

[
  "else"
  "if"
  "range"
  "with"
  "end"
  "template"
  "define"
  "block"
] @keyword

[
  (interpreted_string_literal)
  (raw_string_literal)
  (rune_literal)
] @string

(escape_sequence) @string.escape

[
  (int_literal)
  (float_literal)
  (imaginary_literal)
] @number

[
  (true)
  (false)
  (nil)
] @constant.builtin

(comment) @comment
