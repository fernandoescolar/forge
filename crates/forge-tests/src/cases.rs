//! The data rows of a data-driven .NET test (xUnit `[InlineData]`, NUnit `[TestCase]`,
//! MSTest `[DataRow]`) and the results that belong to each.
//!
//! The source gives the rows and their arguments as written (`5, 4`); the runners name each
//! execution after its arguments as they print them: xUnit `Ns.C.M(input: 5, expected: 4)`,
//! NUnit `M(5,4)`, MSTest `M (5,4)`. Both are reduced to the same list of values to match
//! them. Rows the source doesn't show (`[MemberData]`, `[TestCaseSource]`) still get one
//! entry per result.

use std::path::PathBuf;

use crate::discovery::{DataCase, NO_ROW, TestMethod};
use crate::trx::CaseResult;

/// One row of a data-driven test, with what its last run gave.
#[derive(Clone, Debug, PartialEq)]
pub struct CaseRow<'a> {
    /// The arguments, as the source writes them or the runner names them.
    pub label: String,
    pub file: PathBuf,
    /// Zero-based row of its attribute, or `NO_ROW`.
    pub row: u32,
    pub results: Vec<&'a CaseResult>,
}

impl CaseRow<'_> {
    /// A `dotnet test --filter` for this row alone, from a result's name; `None` when it
    /// has none yet, or its name holds characters a filter can't take.
    pub fn filter(&self, method_fqn: &str) -> Option<String> {
        let name = &self.results.first()?.display_name;
        let inner = name.split_once('(')?.1.strip_suffix(')')?;
        if inner.is_empty() || inner.contains(['\\', '(', ')', '&', '|', '=', '!', '~']) {
            return None;
        }
        Some(format!("FullyQualifiedName={method_fqn}&DisplayName~{inner}"))
    }
}

/// The rows of `method`: those its attributes write, in source order, then results that
/// match none of them. Empty for a test that isn't data-driven.
pub fn rows<'a>(method: &TestMethod, results: &'a [CaseResult]) -> Vec<CaseRow<'a>> {
    let mut rows: Vec<CaseRow<'a>> = method
        .cases
        .iter()
        .map(|case: &DataCase| CaseRow { label: format!("({})", case.arguments), file: method.file.clone(), row: case.row, results: Vec::new() })
        .collect();
    let written: Vec<Vec<String>> = method.cases.iter().map(|case| split_values(&case.arguments)).collect();
    for result in results {
        let Some(values) = result_values(&result.display_name) else { continue };
        match written.iter().position(|w| *w == values) {
            Some(index) => rows[index].results.push(result),
            None => match rows.iter_mut().find(|row| row.row == NO_ROW && row.label == short_label(&result.display_name)) {
                Some(row) => row.results.push(result),
                None => rows.push(CaseRow { label: short_label(&result.display_name), file: method.file.clone(), row: NO_ROW, results: vec![result] }),
            },
        }
    }
    // A plain test reports one result without arguments: no rows.
    if method.cases.is_empty() && rows.len() < 2 {
        return Vec::new();
    }
    rows
}

/// `(input: 5, expected: 4)` of a result's name.
fn short_label(display_name: &str) -> String {
    display_name.find('(').map(|at| display_name[at..].to_string()).unwrap_or_else(|| display_name.to_string())
}

/// The argument values in a result's name, without xUnit's parameter names.
fn result_values(display_name: &str) -> Option<Vec<String>> {
    let inner = display_name.split_once('(')?.1.strip_suffix(')')?;
    Some(
        split_arguments(inner)
            .into_iter()
            .map(|value| {
                let value = value.trim();
                match value.split_once(": ") {
                    Some((name, rest)) if !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_') => normalize(rest),
                    _ => normalize(value),
                }
            })
            .collect(),
    )
}

/// Arguments as written, normalized for matching (see [`normalize`]).
fn split_values(arguments: &str) -> Vec<String> {
    split_arguments(arguments).iter().map(|value| normalize(value)).collect()
}

/// Splits arguments at the commas between them, not those in strings or brackets.
fn split_arguments(arguments: &str) -> Vec<String> {
    let mut values = Vec::new();
    let (mut current, mut depth, mut quote) = (String::new(), 0i32, None::<char>);
    let mut chars = arguments.chars();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(_), '\\') => {
                current.push(c);
                if let Some(next) = chars.next() {
                    current.push(next);
                }
                continue;
            }
            (Some(q), _) if c == q => quote = None,
            (None, '"' | '\'') => quote = Some(c),
            (None, '(' | '[' | '{') => depth += 1,
            (None, ')' | ']' | '}') => depth -= 1,
            (None, ',') if depth == 0 => {
                values.push(std::mem::take(&mut current));
                continue;
            }
            _ => {}
        }
        current.push(c);
    }
    if !current.trim().is_empty() || !values.is_empty() {
        values.push(current);
    }
    values
}

/// Trimmed, lowercase outside strings (`True`/`true`), without a numeric suffix (`1.5m`).
fn normalize(value: &str) -> String {
    let value = value.trim();
    if value.starts_with(['"', '\'']) {
        return value.to_string();
    }
    let lower = value.to_lowercase();
    let numeric = lower.trim_end_matches(['m', 'f', 'd', 'l', 'u']);
    if !numeric.is_empty() && numeric != lower && numeric.chars().all(|c| c.is_ascii_digit() || c == '.' || c == '-') {
        return numeric.to_string();
    }
    lower
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trx::Outcome;
    use std::time::Duration;

    fn result(display_name: &str, outcome: Outcome) -> CaseResult {
        CaseResult { method_fqn: "N.T.M".into(), display_name: display_name.into(), framework: None, outcome, duration: Duration::ZERO, message: None, stack_trace: None }
    }

    fn method(cases: &[(u32, &str)]) -> TestMethod {
        TestMethod {
            fqn: "N.T.M".into(),
            name: "M".into(),
            file: "/t/T.cs".into(),
            row: 10,
            cases: cases.iter().map(|(row, arguments)| DataCase { row: *row, arguments: arguments.to_string() }).collect(),
        }
    }

    #[test]
    fn matches_each_frameworks_names_to_the_source_rows() {
        let xunit = [result("N.T.M(input: 5, expected: 4)", Outcome::Failed), result("N.T.M(input: 1, expected: 1)", Outcome::Passed)];
        let rows = rows(&method(&[(7, "1, 1"), (8, "5, 4")]), &xunit);
        assert_eq!(rows.iter().map(|r| (r.label.as_str(), r.row, r.results.len())).collect::<Vec<_>>(), [("(1, 1)", 7, 1), ("(5, 4)", 8, 1)]);
        assert_eq!(rows[1].results[0].outcome, Outcome::Failed);

        let nunit = [result("M(2,\"a, b\")", Outcome::Passed)];
        assert_eq!(super::rows(&method(&[(3, "2, \"a, b\"")]), &nunit)[0].results.len(), 1, "commas in strings");

        let mstest = [result("M (True,1.5)", Outcome::Passed)];
        assert_eq!(super::rows(&method(&[(3, "true, 1.5m")]), &mstest)[0].results.len(), 1, "case and numeric suffixes");
    }

    #[test]
    fn keeps_rows_the_source_doesnt_write() {
        let member_data = [result("N.T.M(x: 1)", Outcome::Passed), result("N.T.M(x: 2)", Outcome::Failed)];
        let rows = rows(&method(&[]), &member_data);
        assert_eq!(rows.iter().map(|r| (r.label.as_str(), r.row)).collect::<Vec<_>>(), [("(x: 1)", NO_ROW), ("(x: 2)", NO_ROW)]);
        assert!(super::rows(&method(&[]), &[result("N.T.M", Outcome::Passed)]).is_empty(), "a plain test has no rows");
    }

    #[test]
    fn filters_one_row_by_its_name() {
        let results = [result("N.T.M(input: 5, expected: 4)", Outcome::Failed)];
        let rows = rows(&method(&[(8, "5, 4")]), &results);
        assert_eq!(rows[0].filter("N.T.M").as_deref(), Some("FullyQualifiedName=N.T.M&DisplayName~input: 5, expected: 4"));
        let unsafe_name = [result("N.T.M(text: \"a|b\")", Outcome::Passed)];
        assert_eq!(super::rows(&method(&[(8, "\"a|b\"")]), &unsafe_name)[0].filter("N.T.M"), None);
        assert_eq!(super::rows(&method(&[(8, "1")]), &[])[0].filter("N.T.M"), None, "not run yet");
    }
}
