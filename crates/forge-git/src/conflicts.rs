//! Conflict markers in a file's text, as git (and `git merge-file`) writes them:
//!
//! ```text
//! <<<<<<< yours
//! your lines
//! ||||||| base          (only with diff3 / zdiff3 conflict style)
//! the original lines
//! =======
//! their lines
//! >>>>>>> theirs
//! ```

use std::ops::Range;

#[derive(Clone, Debug, PartialEq)]
pub struct Conflict {
    /// Zero-based rows of the whole block, `<<<<<<<` through `>>>>>>>`.
    pub rows: Range<u32>,
    pub ours: Vec<String>,
    pub base: Option<Vec<String>>,
    pub theirs: Vec<String>,
    pub ours_label: String,
    pub theirs_label: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Ours,
    Theirs,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Choice {
    Ours,
    Theirs,
    /// Ours, then theirs.
    Both,
}

impl Conflict {
    pub fn lines(&self, choice: Choice) -> Vec<String> {
        match choice {
            Choice::Ours => self.ours.clone(),
            Choice::Theirs => self.theirs.clone(),
            Choice::Both => self.ours.iter().chain(&self.theirs).cloned().collect(),
        }
    }
}

/// The conflicts in `text`, in order. Unfinished blocks (no `>>>>>>>`) are left out.
pub fn parse(text: &str) -> Vec<Conflict> {
    enum Part {
        Ours,
        Base,
        Theirs,
    }
    let mut conflicts = Vec::new();
    let mut open: Option<(u32, Conflict, Part)> = None;
    for (row, line) in text.lines().enumerate() {
        let row = row as u32;
        if let Some(label) = line.strip_prefix("<<<<<<<") {
            let conflict = Conflict { rows: row..row, ours: vec![], base: None, theirs: vec![], ours_label: label.trim().to_string(), theirs_label: String::new() };
            open = Some((row, conflict, Part::Ours));
            continue;
        }
        let Some((start, conflict, part)) = open.as_mut() else { continue };
        if line.starts_with("|||||||") && matches!(part, Part::Ours) {
            conflict.base = Some(vec![]);
            *part = Part::Base;
        } else if line.starts_with("=======") && !matches!(part, Part::Theirs) {
            *part = Part::Theirs;
        } else if let Some(label) = line.strip_prefix(">>>>>>>").filter(|_| matches!(part, Part::Theirs)) {
            conflict.theirs_label = label.trim().to_string();
            conflict.rows = *start..row + 1;
            conflicts.push(conflict.clone());
            open = None;
        } else {
            match part {
                Part::Ours => conflict.ours.push(line.to_string()),
                Part::Base => conflict.base.get_or_insert_with(Vec::new).push(line.to_string()),
                Part::Theirs => conflict.theirs.push(line.to_string()),
            }
        }
    }
    conflicts
}

/// `text` with every conflict resolved to `side`, and the rows each conflict's lines take
/// in it (empty ranges where a side has no lines).
pub fn side_text(text: &str, side: Side) -> (String, Vec<Range<u32>>) {
    let conflicts = parse(text);
    let lines: Vec<&str> = text.lines().collect();
    let mut out: Vec<String> = Vec::new();
    let mut regions = Vec::new();
    let mut row = 0u32;
    for conflict in &conflicts {
        out.extend(lines[row as usize..conflict.rows.start as usize].iter().map(|l| l.to_string()));
        let chosen = match side {
            Side::Ours => &conflict.ours,
            Side::Theirs => &conflict.theirs,
        };
        let start = out.len() as u32;
        out.extend(chosen.iter().cloned());
        regions.push(start..out.len() as u32);
        row = conflict.rows.end;
    }
    out.extend(lines[(row as usize).min(lines.len())..].iter().map(|l| l.to_string()));
    let mut joined = out.join("\n");
    if text.ends_with('\n') {
        joined.push('\n');
    }
    (joined, regions)
}

/// Which conflict the cursor on `row` of the result is in, or the next one after it (the
/// last one past the end).
pub fn at_or_after(conflicts: &[Conflict], row: u32) -> Option<usize> {
    if conflicts.is_empty() {
        return None;
    }
    Some(conflicts.iter().position(|c| row < c.rows.end).unwrap_or(conflicts.len() - 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEXT: &str = "fn main() {\n<<<<<<< HEAD\n    a();\n=======\n    b();\n    c();\n>>>>>>> feature\n    d();\n<<<<<<< yours\n||||||| base\n    old();\n=======\n    e();\n>>>>>>> agent (forge/x)\n}\n";

    #[test]
    fn parses_both_conflict_styles() {
        let conflicts = parse(TEXT);
        assert_eq!(conflicts.len(), 2);
        assert_eq!((conflicts[0].rows.clone(), conflicts[0].ours.clone(), conflicts[0].theirs.clone()), (1..7, vec!["    a();".to_string()], vec!["    b();".to_string(), "    c();".to_string()]));
        assert_eq!((conflicts[0].ours_label.as_str(), conflicts[0].theirs_label.as_str()), ("HEAD", "feature"));
        assert_eq!(conflicts[1].rows, 8..14);
        assert!(conflicts[1].ours.is_empty());
        assert_eq!(conflicts[1].base, Some(vec!["    old();".to_string()]));
        assert_eq!(conflicts[1].theirs_label, "agent (forge/x)");
        assert_eq!(conflicts[0].lines(Choice::Both), ["    a();", "    b();", "    c();"]);
        assert!(parse("<<<<<<< a\nx\n=======\n").is_empty(), "unfinished");
    }

    #[test]
    fn builds_each_side() {
        let (ours, regions) = side_text(TEXT, Side::Ours);
        assert_eq!(ours, "fn main() {\n    a();\n    d();\n}\n");
        assert_eq!(regions, [1..2, 3..3]);
        let (theirs, regions) = side_text(TEXT, Side::Theirs);
        assert_eq!(theirs, "fn main() {\n    b();\n    c();\n    d();\n    e();\n}\n");
        assert_eq!(regions, [1..3, 4..5]);
    }

    #[test]
    fn finds_the_conflict_at_the_cursor() {
        let conflicts = parse(TEXT);
        assert_eq!(at_or_after(&conflicts, 0), Some(0));
        assert_eq!(at_or_after(&conflicts, 4), Some(0));
        assert_eq!(at_or_after(&conflicts, 7), Some(1));
        assert_eq!(at_or_after(&conflicts, 99), Some(1));
        assert_eq!(at_or_after(&[], 0), None);
    }
}
