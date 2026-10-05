(comment) @annotation

(namespace_declaration
  "namespace" @context
  name: (_) @name) @item

(file_scoped_namespace_declaration
  "namespace" @context
  name: (_) @name) @item

(class_declaration
  (modifier)* @context
  "class" @context
  name: (identifier) @name) @item

(struct_declaration
  (modifier)* @context
  "struct" @context
  name: (identifier) @name) @item

(record_declaration
  (modifier)* @context
  "record" @context
  name: (identifier) @name) @item

(interface_declaration
  (modifier)* @context
  "interface" @context
  name: (identifier) @name) @item

(enum_declaration
  (modifier)* @context
  "enum" @context
  name: (identifier) @name) @item

(enum_member_declaration
  name: (identifier) @name) @item

(delegate_declaration
  (modifier)* @context
  "delegate" @context
  name: (identifier) @name) @item

(constructor_declaration
  (modifier)* @context
  name: (identifier) @name
  parameters: (parameter_list
    "(" @context
    ")" @context)) @item

(method_declaration
  (modifier)* @context
  returns: (_) @context
  name: (identifier) @name
  parameters: (parameter_list
    "(" @context
    ")" @context)) @item

(property_declaration
  (modifier)* @context
  type: (_) @context
  name: (identifier) @name) @item

(event_declaration
  (modifier)* @context
  "event" @context
  name: (identifier) @name) @item

(field_declaration
  (modifier)* @context
  (variable_declaration
    type: (_) @context
    (variable_declarator
      name: (identifier) @name) @item))
