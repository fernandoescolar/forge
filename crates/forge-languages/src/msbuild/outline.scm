; Groups and targets, with their condition, make the outline of a project file.
(element
  (STag
    (Name) @name
    (Attribute (Name) @_attr (AttValue) @context)?)
  (#any-of? @name "PropertyGroup" "ItemGroup" "Target" "ImportGroup" "ItemDefinitionGroup" "Choose" "When" "Otherwise" "Folder")) @item

(element
  (EmptyElemTag
    (Name) @name
    (Attribute (Name) @_attr (AttValue) @context)?)
  (#any-of? @name "Import" "Folder")) @item
