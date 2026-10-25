//! Reads Cobertura coverage reports, which both coverlet (`--collect "XPlat Code Coverage"`)
//! and Microsoft's collector (`--collect "Code Coverage;Format=cobertura"`) write.
//!
//! Coverlet names files relative to the report's `<sources>`; Microsoft's collector gives
//! absolute paths. A line can appear in several classes (partial classes, lambdas) and in
//! several reports (one per test project or framework): its hits add up.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};

/// How a line was covered.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LineCoverage {
    pub hits: u64,
    /// Branches taken and branches there are, for a line that branches.
    pub branches: Option<(u32, u32)>,
}

impl LineCoverage {
    pub fn state(&self) -> LineState {
        match self.branches {
            _ if self.hits == 0 => LineState::Uncovered,
            Some((taken, total)) if taken < total => LineState::Partial,
            _ => LineState::Covered,
        }
    }

    fn merge(&mut self, other: LineCoverage) {
        self.hits += other.hits;
        self.branches = match (self.branches, other.branches) {
            (Some((a, total)), Some((b, other_total))) => Some((a.max(b).min(total.max(other_total)), total.max(other_total))),
            (mine, theirs) => mine.or(theirs),
        };
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineState {
    Covered,
    /// Run, but some of its branches weren't taken.
    Partial,
    Uncovered,
}

/// Coverage by file, and in each file by line (from 1).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Coverage {
    pub files: HashMap<PathBuf, BTreeMap<u32, LineCoverage>>,
}

impl Coverage {
    /// Adds a report's lines to these.
    pub fn merge(&mut self, other: Coverage) {
        for (file, lines) in other.files {
            let mine = self.files.entry(file).or_default();
            for (line, coverage) in lines {
                mine.entry(line).or_default().merge(coverage);
            }
        }
    }

    /// Lines run and lines there are, in `file` or (with `None`) everywhere.
    pub fn counts(&self, file: Option<&Path>) -> (usize, usize) {
        let files: Vec<&BTreeMap<u32, LineCoverage>> = match file {
            Some(file) => self.files.get(file).into_iter().collect(),
            None => self.files.values().collect(),
        };
        let total = files.iter().map(|lines| lines.len()).sum();
        let covered = files.iter().flat_map(|lines| lines.values()).filter(|line| line.hits > 0).count();
        (covered, total)
    }

    /// The share of lines run, as a percentage.
    pub fn percent(&self, file: Option<&Path>) -> Option<f64> {
        let (covered, total) = self.counts(file);
        (total > 0).then(|| covered as f64 * 100.0 / total as f64)
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }
}

/// Parses one report. Relative file names are resolved against its `<source>` folders
/// (the first where the file exists), else `base`.
pub fn parse(xml: &str, base: &Path) -> Result<Coverage> {
    let doc = roxmltree::Document::parse(xml).context("invalid Cobertura report")?;
    let sources: Vec<PathBuf> = doc.descendants().filter(|n| n.has_tag_name("source")).filter_map(|n| n.text()).map(|t| PathBuf::from(t.trim())).collect();
    let resolve = |name: &str| -> PathBuf {
        let path = Path::new(name);
        if path.is_absolute() {
            return path.to_path_buf();
        }
        sources.iter().map(|source| source.join(path)).find(|candidate| candidate.exists()).unwrap_or_else(|| sources.first().map(|s| s.as_path()).unwrap_or(base).join(path))
    };
    let mut coverage = Coverage::default();
    for class in doc.descendants().filter(|n| n.has_tag_name("class")) {
        let Some(file) = class.attribute("filename") else { continue };
        let lines = coverage.files.entry(resolve(file)).or_default();
        // A class's own <lines> repeat its methods' lines.
        let Some(class_lines) = class.children().find(|n| n.has_tag_name("lines")) else { continue };
        for line in class_lines.children().filter(|n| n.has_tag_name("line")) {
            let Some(number) = line.attribute("number").and_then(|n| n.parse::<u32>().ok()) else { continue };
            let hits = line.attribute("hits").and_then(|h| h.parse::<u64>().ok()).unwrap_or(0);
            let branches = line.attribute("branch").filter(|b| b.eq_ignore_ascii_case("true")).and_then(|_| line.attribute("condition-coverage")).and_then(parse_condition_coverage);
            lines.entry(number).or_default().merge(LineCoverage { hits, branches });
        }
    }
    Ok(coverage)
}

/// `50% (1/2)` → `(1, 2)`.
fn parse_condition_coverage(text: &str) -> Option<(u32, u32)> {
    let inner = text.split('(').nth(1)?.trim_end_matches(')');
    let (taken, total) = inner.split_once('/')?;
    Some((taken.trim().parse().ok()?, total.trim().parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MICROSOFT: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<coverage line-rate="0.66" lines-covered="10" lines-valid="15">
  <packages>
    <package name="Calc">
      <classes>
        <class name="Calc.MathOps" filename="/src/Calc/MathOps.cs">
          <methods>
            <method name="Abs" signature="(int)">
              <lines><line number="6" hits="1" branch="False" /></lines>
            </method>
          </methods>
          <lines>
            <line number="6" hits="1" branch="False" />
            <line number="7" hits="1" branch="True" condition-coverage="50% (1/2)">
              <conditions><condition number="0" type="jump" coverage="50%" /></conditions>
            </line>
            <line number="8" hits="0" branch="False" />
            <line number="11" hits="3" branch="True" condition-coverage="100% (2/2)" />
          </lines>
        </class>
      </classes>
    </package>
  </packages>
</coverage>"#;

    const COVERLET: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<coverage line-rate="1" version="1.9">
  <sources><source>/src/</source></sources>
  <packages>
    <package name="Calc">
      <classes>
        <class name="Calc.MathOps" filename="Calc/MathOps.cs">
          <lines>
            <line number="8" hits="2" branch="False" />
            <line number="7" hits="4" branch="True" condition-coverage="100% (2/2)" />
          </lines>
        </class>
      </classes>
    </package>
  </packages>
</coverage>"#;

    #[test]
    fn reads_lines_and_branches() {
        let coverage = parse(MICROSOFT, Path::new("/elsewhere")).unwrap();
        let lines = &coverage.files[Path::new("/src/Calc/MathOps.cs")];
        assert_eq!(lines.len(), 4, "the class's lines, not its methods' copies");
        assert_eq!(lines[&6].state(), LineState::Covered);
        assert_eq!(lines[&7], LineCoverage { hits: 1, branches: Some((1, 2)) });
        assert_eq!(lines[&7].state(), LineState::Partial);
        assert_eq!(lines[&8].state(), LineState::Uncovered);
        assert_eq!(lines[&11].state(), LineState::Covered);
        assert_eq!(coverage.counts(None), (3, 4));
        assert_eq!(coverage.percent(Some(Path::new("/src/Calc/MathOps.cs"))), Some(75.0));
    }

    #[test]
    fn resolves_coverlet_sources_and_merges_reports() {
        let mut coverage = parse(MICROSOFT, Path::new("/elsewhere")).unwrap();
        let coverlet = parse(COVERLET, Path::new("/elsewhere")).unwrap();
        assert!(coverlet.files.contains_key(Path::new("/src/Calc/MathOps.cs")), "{:?}", coverlet.files.keys());
        coverage.merge(coverlet);
        let lines = &coverage.files[Path::new("/src/Calc/MathOps.cs")];
        assert_eq!(lines[&8], LineCoverage { hits: 2, branches: None }, "run by the other report");
        assert_eq!(lines[&7], LineCoverage { hits: 5, branches: Some((2, 2)) }, "every branch taken by one or the other");
        assert_eq!(coverage.counts(None), (4, 4));
    }
}
