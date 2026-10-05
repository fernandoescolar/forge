(header) @comment.doc
(comment) @comment

[
  "Project" "EndProject"
  "ProjectSection" "EndProjectSection"
  "Global" "EndGlobal"
  "GlobalSection" "EndGlobalSection"
] @keyword

(timing) @constant
(section_name) @type

(project type: (string) @constant)
(project name: (string) @title)
(project path: (string) @string)
(project guid: (string) @constant)

(assignment key: (key) @property)
(assignment value: (value) @string)

[ "(" ")" ] @punctuation.bracket
[ "," ] @punctuation.delimiter
"=" @operator
