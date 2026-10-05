; Test methods: xUnit ([Fact], [Theory]), NUnit ([Test], [TestCase], [TestCaseSource])
; and MSTest ([TestMethod], [DataTestMethod]), with or without the `Attribute` suffix
; or a namespace qualifier.
((class_declaration
  name: (identifier) @_class_name
  body: (declaration_list
    (method_declaration
      (attribute_list
        (attribute
          name: [
            (identifier) @_attribute
            (qualified_name
              name: (identifier) @_attribute)
          ]))
      name: (identifier) @run @_method_name)))
  (#match? @_attribute "^(Fact|Theory|SkippableFact|SkippableTheory|Test|TestCase|TestCaseSource|TestMethod|DataTestMethod)(Attribute)?$")
  (#set! tag csharp-test))

; Classes with at least one test method: run all of them.
((class_declaration
  name: (identifier) @run @_class_name
  body: (declaration_list
    (method_declaration
      (attribute_list
        (attribute
          name: [
            (identifier) @_attribute
            (qualified_name
              name: (identifier) @_attribute)
          ])))))
  (#match? @_attribute "^(Fact|Theory|SkippableFact|SkippableTheory|Test|TestCase|TestCaseSource|TestMethod|DataTestMethod)(Attribute)?$")
  (#set! tag csharp-test-class))
