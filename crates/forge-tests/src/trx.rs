//! Reads the TRX (Visual Studio test results) files `dotnet test --logger trx` writes.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context as _, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Outcome {
    Passed,
    Skipped,
    Failed,
}

/// One execution of a test: a `[Fact]`, or one data row of a `[Theory]`, on one framework.
#[derive(Clone, Debug, PartialEq)]
pub struct CaseResult {
    /// `Namespace.Class.Method`, shared by every row of a theory.
    pub method_fqn: String,
    /// What the runner calls this execution, e.g. `Ns.Class.Doubles(x: 2, expected: 5)`.
    pub display_name: String,
    pub framework: Option<String>,
    pub outcome: Outcome,
    pub duration: Duration,
    pub message: Option<String>,
    pub stack_trace: Option<String>,
}

fn child<'a, 'input>(node: roxmltree::Node<'a, 'input>, name: &str) -> Option<roxmltree::Node<'a, 'input>> {
    node.children().find(|c| c.has_tag_name(name))
}

fn text(node: Option<roxmltree::Node<'_, '_>>) -> Option<String> {
    node.and_then(|n| n.text()).map(str::trim).filter(|t| !t.is_empty()).map(String::from)
}

/// `framework` is the target framework the file belongs to, when known.
pub fn parse(xml: &str, framework: Option<&str>) -> Result<Vec<CaseResult>> {
    let doc = roxmltree::Document::parse(xml).context("invalid TRX")?;

    // testId -> Namespace.Class.Method, from <UnitTest id><TestMethod className name/></UnitTest>.
    let methods: HashMap<&str, String> = doc
        .descendants()
        .filter(|n| n.has_tag_name("UnitTest"))
        .filter_map(|test| {
            let method = child(test, "TestMethod")?;
            Some((test.attribute("id")?, format!("{}.{}", method.attribute("className")?, method.attribute("name")?)))
        })
        .collect();

    Ok(doc
        .descendants()
        .filter(|n| n.has_tag_name("UnitTestResult"))
        .filter_map(|result| {
            let display_name = result.attribute("testName")?.to_string();
            let method_fqn = result
                .attribute("testId")
                .and_then(|id| methods.get(id).cloned())
                .unwrap_or_else(|| display_name.split('(').next().unwrap_or(&display_name).to_string());
            let outcome = match result.attribute("outcome")? {
                "Passed" => Outcome::Passed,
                "NotExecuted" | "Inconclusive" | "Pending" => Outcome::Skipped,
                _ => Outcome::Failed,
            };
            let error = child(result, "Output").and_then(|output| child(output, "ErrorInfo"));
            Some(CaseResult {
                method_fqn,
                display_name,
                framework: framework.map(String::from),
                outcome,
                duration: result.attribute("duration").and_then(parse_duration).unwrap_or_default(),
                message: text(error.and_then(|e| child(e, "Message"))),
                stack_trace: text(error.and_then(|e| child(e, "StackTrace"))),
            })
        })
        .collect())
}

/// The target framework in a TRX file name `dotnet test` generated, e.g. `…_net10.0.trx`.
pub fn framework_from_file_name(name: &str) -> Option<String> {
    let stem = name.strip_suffix(".trx")?;
    let framework = stem.rsplit('_').next()?;
    framework.starts_with("net").then(|| framework.to_string())
}

/// `hh:mm:ss.fffffff`
fn parse_duration(value: &str) -> Option<Duration> {
    let mut parts = value.split(':');
    let hours: f64 = parts.next()?.parse().ok()?;
    let minutes: f64 = parts.next()?.parse().ok()?;
    let seconds: f64 = parts.next()?.parse().ok()?;
    Some(Duration::from_secs_f64(hours * 3600. + minutes * 60. + seconds))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_facts_theories_failures_and_skips() {
        let xml = include_str!("../fixtures/demo_net10.0.trx");
        let mut results = parse(xml, Some("net10.0")).unwrap();
        results.sort_by(|a, b| a.display_name.cmp(&b.display_name));
        let summary: Vec<_> = results.iter().map(|r| (r.display_name.as_str(), r.method_fqn.as_str(), r.outcome)).collect();
        assert_eq!(
            summary,
            [
                ("Demo.Tests.MathTests.Adds", "Demo.Tests.MathTests.Adds", Outcome::Passed),
                ("Demo.Tests.MathTests.Doubles(x: 1, expected: 2)", "Demo.Tests.MathTests.Doubles", Outcome::Passed),
                ("Demo.Tests.MathTests.Doubles(x: 2, expected: 5)", "Demo.Tests.MathTests.Doubles", Outcome::Failed),
                ("Demo.Tests.MathTests.Fails", "Demo.Tests.MathTests.Fails", Outcome::Failed),
                ("Demo.Tests.MathTests.Skipped", "Demo.Tests.MathTests.Skipped", Outcome::Skipped),
            ]
        );
        let fails = results.iter().find(|r| r.display_name.ends_with("Fails")).unwrap();
        assert!(fails.message.as_deref().unwrap().contains("Values differ"));
        assert!(fails.stack_trace.as_deref().unwrap().contains("MathTests.cs:line 9"));
        assert_eq!(fails.framework.as_deref(), Some("net10.0"));
    }

    #[test]
    fn reads_framework_and_duration() {
        assert_eq!(framework_from_file_name("host_2026-10-02_14_10_10_net8.0.trx").as_deref(), Some("net8.0"));
        assert_eq!(framework_from_file_name("results.trx"), None);
        assert_eq!(parse_duration("00:01:02.5"), Some(Duration::from_secs_f64(62.5)));
    }
}
