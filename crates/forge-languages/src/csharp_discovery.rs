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
    }
}
