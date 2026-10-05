; MSBuild project files, .props/.targets and .slnx: XML with MSBuild's vocabulary.

(XMLDecl "xml" @keyword)
(XMLDecl [ "version" "encoding" "standalone" ] @property)
(EncName) @string.special
(VersionNum) @number

; Structural MSBuild elements stand out from properties and items.
((STag (Name) @keyword)
  (#any-of? @keyword "Project" "PropertyGroup" "ItemGroup" "ItemDefinitionGroup" "Target" "Import" "ImportGroup" "Choose" "When" "Otherwise" "Sdk" "UsingTask" "Solution" "Folder" "Configurations"))
((EmptyElemTag (Name) @keyword)
  (#any-of? @keyword "Import" "Sdk" "Folder"))
((ETag (Name) @keyword)
  (#any-of? @keyword "Project" "PropertyGroup" "ItemGroup" "ItemDefinitionGroup" "Target" "ImportGroup" "Choose" "When" "Otherwise" "UsingTask" "Solution" "Folder" "Configurations"))

(STag (Name) @tag)
(ETag (Name) @tag)
(EmptyElemTag (Name) @tag)

((Attribute (Name) @keyword.control)
  (#any-of? @keyword.control "Condition"))
(Attribute (Name) @property)
(Attribute (AttValue) @string)

(EntityRef) @constant
(CharRef) @constant

(Comment) @comment

(CDSect (CData) @string)

(PI) @preproc

[
  "<?" "?>"
  "<!" "]]>"
  "<" ">"
  "</" "/>"
] @punctuation.delimiter

[ "=" ] @operator
