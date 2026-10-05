; Based on the highlights query shipped with tree-sitter-c-sharp (MIT),
; with capture names adjusted to the ones Zed themes style.
; Later patterns take precedence over earlier ones.

(identifier) @variable

; Types

(interface_declaration name: (identifier) @type)
(class_declaration name: (identifier) @type)
(enum_declaration name: (identifier) @type)
(struct_declaration name: (identifier) @type)
(record_declaration name: (identifier) @type)
(delegate_declaration name: (identifier) @type)

(namespace_declaration name: (identifier) @namespace)
(namespace_declaration name: (qualified_name (identifier) @namespace))
(file_scoped_namespace_declaration name: (identifier) @namespace)
(file_scoped_namespace_declaration name: (qualified_name (identifier) @namespace))
(using_directive (identifier) @namespace)
(using_directive (qualified_name (identifier) @namespace))

(generic_name (identifier) @type)
(type_parameter (identifier) @type)
(parameter type: (identifier) @type)
(type_argument_list (identifier) @type)
(as_expression right: (identifier) @type)
(is_expression right: (identifier) @type)
(object_creation_expression type: (identifier) @type)
(_ type: (identifier) @type)
(base_list (identifier) @type)
(type_parameter_constraints_clause (identifier) @type)

(predefined_type) @type.builtin
(implicit_type) @keyword

; Members

(property_declaration name: (identifier) @property)
(enum_member_declaration name: (identifier) @constant)
(variable_declarator name: (identifier) @variable)
(field_declaration (variable_declaration (variable_declarator name: (identifier) @property)))

(constructor_declaration name: (identifier) @constructor)
(destructor_declaration name: (identifier) @constructor)

; Functions

(method_declaration name: (identifier) @function)
(local_function_statement name: (identifier) @function)
(invocation_expression function: (identifier) @function)
(invocation_expression function: (member_access_expression name: (identifier) @function))
(invocation_expression function: (generic_name (identifier) @function))
(invocation_expression function: (member_access_expression name: (generic_name (identifier) @function)))

(parameter name: (identifier) @variable.parameter)

; Attributes

(attribute name: (identifier) @attribute)
(attribute name: (qualified_name (identifier) @attribute))

; Literals

[
  (real_literal)
  (integer_literal)
] @number

[
  (character_literal)
  (string_literal)
  (raw_string_literal)
  (verbatim_string_literal)
  (interpolated_string_expression)
  (interpolation_start)
  (interpolation_quote)
] @string

(escape_sequence) @string.escape

[
  (boolean_literal)
  (null_literal)
] @constant.builtin

(comment) @comment

; Preprocessor

[
  "#if"
  "#elif"
  "#else"
  "#endif"
  "#region"
  "#endregion"
  "#define"
  "#undef"
  "#pragma"
  "#nullable"
  "#error"
  "#warning"
  "#line"
] @preproc

(preproc_arg) @string

; Punctuation

[
  ";"
  "."
  ","
] @punctuation.delimiter

[
  "--"
  "-"
  "-="
  "&"
  "&="
  "&&"
  "+"
  "++"
  "+="
  "<"
  "<="
  "<<"
  "<<="
  "="
  "=="
  "!"
  "!="
  "=>"
  ">"
  ">="
  ">>"
  ">>="
  ">>>"
  ">>>="
  "|"
  "|="
  "||"
  "?"
  "??"
  "??="
  "^"
  "^="
  "~"
  "*"
  "*="
  "/"
  "/="
  "%"
  "%="
  ":"
  ".."
] @operator

[
  "("
  ")"
  "["
  "]"
  "{"
  "}"
] @punctuation.bracket

(interpolation_brace) @punctuation.special

; Keywords

[
  (modifier)
  "this"
] @keyword

[
  "add"
  "alias"
  "as"
  "base"
  "break"
  "case"
  "catch"
  "checked"
  "class"
  "continue"
  "default"
  "delegate"
  "do"
  "else"
  "enum"
  "event"
  "explicit"
  "extern"
  "finally"
  "for"
  "foreach"
  "global"
  "goto"
  "if"
  "implicit"
  "interface"
  "is"
  "lock"
  "namespace"
  "notnull"
  "operator"
  "params"
  "return"
  "remove"
  "sizeof"
  "stackalloc"
  "static"
  "struct"
  "switch"
  "throw"
  "try"
  "typeof"
  "unchecked"
  "using"
  "while"
  "new"
  "await"
  "in"
  "yield"
  "get"
  "set"
  "when"
  "out"
  "ref"
  "from"
  "where"
  "select"
  "record"
  "init"
  "with"
  "let"
] @keyword
