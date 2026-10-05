(method_declaration
  body: (block
    "{"
    (_)* @function.inside
    "}")) @function.around

(constructor_declaration
  body: (block
    "{"
    (_)* @function.inside
    "}")) @function.around

(local_function_statement
  body: (block
    "{"
    (_)* @function.inside
    "}")) @function.around

(lambda_expression) @function.around

[
  (class_declaration)
  (struct_declaration)
  (record_declaration)
  (interface_declaration)
  (enum_declaration)
] @class.around

(declaration_list
  "{"
  (_)* @class.inside
  "}")

(comment)+ @comment.around
