//! Finds the tests in a C# file without building it, using the same tree-sitter query
//! (`csharp/runnables.scm`) that puts play buttons in the gutter, so the test explorer and
//! the gutter always agree on what a test is.

use std::sync::LazyLock;

use streaming_iterator::StreamingIterator as _;
use tree_sitter::{Parser, Query, QueryCursor};

/// A test method found in source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveredTest {
    pub namespace: Option<String>,
    pub class: String,
    pub method: String,
    /// Zero-based row of the method name.
    pub row: u32,
    /// Zero-based row of the class name.
    pub class_row: u32,
    /// Its data rows: xUnit `[InlineData]`, NUnit `[TestCase]`, MSTest `[DataRow]`.
    pub cases: Vec<DataCase>,
}

/// One data row of a data-driven test.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DataCase {
    /// Zero-based row of the attribute.
    pub row: u32,
    /// Its arguments as written, e.g. `1, "two"`.
    pub arguments: String,
}

/// Attributes that each add one data row to a test.
const DATA_ATTRIBUTES: &[&str] = &["InlineData", "TestCase", "DataRow"];

/// The data rows in the attributes of a method declaration.
fn data_cases(method: tree_sitter::Node<'_>, source: &str) -> Vec<DataCase> {
    let mut cases = Vec::new();
    let mut lists = method.walk();
    for list in method.children(&mut lists).filter(|n| n.kind() == "attribute_list") {
        let mut attributes = list.walk();
        for attribute in list.children(&mut attributes).filter(|n| n.kind() == "attribute") {
            let Some(name) = attribute.child_by_field_name("name") else { continue };
            // `Xunit.InlineData`, `InlineDataAttribute`…
            let name = source[name.byte_range()].rsplit('.').next().unwrap_or_default();
            if !DATA_ATTRIBUTES.contains(&name.trim_end_matches("Attribute")) {
                continue;
            }
            let mut children = attribute.walk();
            let arguments = attribute
                .children(&mut children)
                .find(|n| n.kind() == "attribute_argument_list")
                .map(|list| source[list.byte_range()].trim_start_matches('(').trim_end_matches(')').split_whitespace().collect::<Vec<_>>().join(" "))
                .unwrap_or_default();
            cases.push(DataCase { row: attribute.start_position().row as u32, arguments });
        }
    }
    cases
}

impl DiscoveredTest {
    /// `Namespace.Class`, as `dotnet test` reports it.
    pub fn class_fqn(&self) -> String {
        match &self.namespace {
            Some(namespace) => format!("{namespace}.{}", self.class),
            None => self.class.clone(),
        }
    }

    /// `Namespace.Class.Method`, as `dotnet test` reports it.
    pub fn fqn(&self) -> String {
        format!("{}.{}", self.class_fqn(), self.method)
    }
}

static QUERY: LazyLock<Query> = LazyLock::new(|| {
    let source = crate::LanguageDir::get("csharp/runnables.scm").expect("csharp/runnables.scm is embedded");
    Query::new(&tree_sitter_c_sharp::LANGUAGE.into(), std::str::from_utf8(&source.data).unwrap())
        .expect("csharp/runnables.scm compiles")
});

/// Every test method in `source`, in source order.
pub fn csharp_tests(source: &str) -> Vec<DiscoveredTest> {
    let mut parser = Parser::new();
    if parser.set_language(&tree_sitter_c_sharp::LANGUAGE.into()).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(source, None) else {
        return Vec::new();
    };
    let namespace = crate::csharp_tasks::namespace_of(source);
    let capture = |name| QUERY.capture_index_for_name(name);
    let (Some(class_ix), Some(method_ix)) = (capture("_class_name"), capture("_method_name")) else {
        return Vec::new();
    };

    let mut tests = Vec::new();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(&QUERY, tree.root_node(), source.as_bytes());
    while let Some(m) = matches.next() {
        let node = |ix| m.captures.iter().find(|c| c.index == ix).map(|c| c.node);
        let (Some(class), Some(method)) = (node(class_ix), node(method_ix)) else {
            continue; // the class-level pattern
        };
        let test = DiscoveredTest {
            namespace: namespace.clone(),
            class: source[class.byte_range()].to_string(),
            method: source[method.byte_range()].to_string(),
            row: method.start_position().row as u32,
            class_row: class.start_position().row as u32,
            cases: method.parent().map(|declaration| data_cases(declaration, source)).unwrap_or_default(),
        };
        // A method with several test attributes matches once per attribute.
        if !tests.contains(&test) {
            tests.push(test);
        }
    }
    tests.sort_by_key(|test| test.row);
    tests
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_tests_with_namespace_and_rows() {
        let source = "namespace Demo.Tests;\n\npublic class MathTests\n{\n    [Fact]\n    public void Adds() {}\n\n    [Theory]\n    [InlineData(1)]\n    [InlineData(2)]\n    public void Doubles(int x) {}\n\n    public void Helper() {}\n}\n";
        let tests = csharp_tests(source);
        let names: Vec<_> = tests.iter().map(|t| (t.fqn(), t.row, t.class_row)).collect();
        assert_eq!(
            names,
            [
                ("Demo.Tests.MathTests.Adds".to_string(), 5, 2),
                ("Demo.Tests.MathTests.Doubles".to_string(), 10, 2),
            ]
        );
        assert!(csharp_tests("class Plain { void M() {} }").is_empty());
        let rows: Vec<_> = tests[1].cases.iter().map(|c| (c.row, c.arguments.as_str())).collect();
        assert_eq!(rows, [(8, "1"), (9, "2")]);
        assert!(tests[0].cases.is_empty());
    }

    #[test]
    fn finds_the_data_rows_of_each_framework() {
        let source = r#"namespace N;
public class T
{
    [Test]
    [TestCase(1, "a b")]
    [NUnit.Framework.TestCase(2,   "c")]
    public void NUnit(int x, string s) {}

    [TestMethod]
    [DataRow(true, 1.5)]
    [DataRowAttribute(false, 2.5)]
    public void MsTest(bool b, double d) {}
}
"#;
        let tests = csharp_tests(source);
        let cases = |i: usize| tests[i].cases.iter().map(|c| c.arguments.clone()).collect::<Vec<_>>();
        assert_eq!(cases(0), ["1, \"a b\"", "2, \"c\""]);
        assert_eq!(cases(1), ["true, 1.5", "false, 2.5"]);
        assert_eq!(tests[0].cases[0].row, 4);
    }
}
