//! After a turn in which the agent changed files: the errors and warnings in those files,
//! once the language servers have caught up, compared with before the turn. The thread
//! shows them (each one opens its line) with a button to send them back to the agent.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use gpui::{App, AsyncApp, Entity, Task, WeakEntity};
use language::{DiagnosticSeverity, Point};
use project::Project;

/// How long to wait for the language servers to report on the changed files.
const SETTLE: Duration = Duration::from_millis(1500);
const MAX_WAIT: Duration = Duration::from_secs(30);
const MAX_PROBLEMS_PER_FILE: usize = 20;

#[derive(Clone, Debug, PartialEq)]
pub struct Problem {
    /// Zero-based.
    pub line: u32,
    pub error: bool,
    pub message: String,
}

/// One changed file's problems now, and how many errors and warnings it had before the turn.
#[derive(Clone, Debug, PartialEq)]
pub struct FileCheck {
    pub path: PathBuf,
    pub problems: Vec<Problem>,
    pub before: (usize, usize),
}

impl FileCheck {
    pub fn counts(&self) -> (usize, usize) {
        let errors = self.problems.iter().filter(|p| p.error).count();
        (errors, self.problems.len() - errors)
    }

    /// More errors or warnings than before the turn.
    pub fn got_worse(&self) -> bool {
        let (e, w) = self.counts();
        e > self.before.0 || w > self.before.1
    }
}

/// (errors, warnings) per file, as the language servers report them now.
pub fn diagnostic_counts(project: &Entity<Project>, cx: &App) -> HashMap<PathBuf, (usize, usize)> {
    let project = project.read(cx);
    let mut out: HashMap<PathBuf, (usize, usize)> = HashMap::new();
    for (path, _, summary) in project.diagnostic_summaries(false, cx) {
        if let Some(abs) = project.absolute_path(&path, cx) {
            let entry = out.entry(abs).or_default();
            entry.0 += summary.error_count;
            entry.1 += summary.warning_count;
        }
    }
    out
}

/// Waits for the language servers to settle, then reads the problems in `files`.
pub fn check(project: WeakEntity<Project>, files: Vec<PathBuf>, before: HashMap<PathBuf, (usize, usize)>, cx: &mut App) -> Task<Vec<FileCheck>> {
    cx.spawn(async move |cx: &mut AsyncApp| {
        let started = Instant::now();
        // Saved files get re-checked; give the servers a moment, then wait while any of
        // them is still working on disk-based diagnostics.
        cx.background_executor().timer(SETTLE).await;
        while started.elapsed() < MAX_WAIT {
            let busy = project
                .read_with(cx, |p, cx| {
                    p.language_servers_running_disk_based_diagnostics(cx).next().is_some()
                        || p.language_server_statuses(cx).any(|(_, s)| !s.pending_work.is_empty() || s.has_pending_diagnostic_updates)
                })
                .unwrap_or(false);
            if !busy {
                break;
            }
            cx.background_executor().timer(Duration::from_millis(250)).await;
        }
        read_problems(project, files, before, cx).await
    })
}

/// The problems in `files` now (files that no longer exist are left out).
pub async fn read_problems(project: WeakEntity<Project>, files: Vec<PathBuf>, before: HashMap<PathBuf, (usize, usize)>, cx: &mut AsyncApp) -> Vec<FileCheck> {
    {
        let mut checks = Vec::new();
        for path in files {
            // Deleted since: nothing to check.
            let Some(problems) = problems_in(&project, &path, cx).await else { continue };
            let before = before.get(&path).copied().unwrap_or_default();
            checks.push(FileCheck { path, problems, before });
        }
        checks
    }
}

async fn problems_in(project: &WeakEntity<Project>, path: &Path, cx: &mut AsyncApp) -> Option<Vec<Problem>> {
    let open = project.update(cx, |p, cx| p.open_local_buffer(path, cx)).ok()?;
    let buffer = open.await.ok()?;
    if buffer.read_with(cx, |b, _| b.file().is_some_and(|f| f.disk_state().exists())) == false {
        return None;
    }
    let snapshot = cx.update(|cx| buffer.read(cx).snapshot());
    let mut out = Vec::new();
    for entry in snapshot.diagnostics_in_range::<_, Point>(0..snapshot.len(), false) {
        let error = match entry.diagnostic.severity {
            DiagnosticSeverity::ERROR => true,
            DiagnosticSeverity::WARNING => false,
            _ => continue,
        };
        if !entry.diagnostic.is_primary {
            continue;
        }
        let message = entry.diagnostic.message.as_ref().lines().next().unwrap_or_default().to_string();
        out.push(Problem { line: entry.range.start.row, error, message });
        if out.len() >= MAX_PROBLEMS_PER_FILE {
            break;
        }
    }
    out.sort_by_key(|p| (!p.error, p.line));
    Some(out)
}

/// The message asking the agent to fix the problems (with mentions of each spot).
pub fn fix_prompt(checks: &[FileCheck], root: &Path) -> String {
    let mut text = String::from("These problems are in the files you just changed. Please fix them:\n");
    for check in checks.iter().filter(|c| !c.problems.is_empty()) {
        let rel = check.path.strip_prefix(root).unwrap_or(&check.path).to_string_lossy();
        for p in &check.problems {
            text.push_str(&format!("- @{rel}:{} {}: {}\n", p.line + 1, if p.error { "error" } else { "warning" }, p.message));
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worse_and_prompt() {
        let check = FileCheck {
            path: "/p/src/a.rs".into(),
            problems: vec![Problem { line: 4, error: true, message: "mismatched types".into() }, Problem { line: 9, error: false, message: "unused".into() }],
            before: (0, 1),
        };
        assert_eq!(check.counts(), (1, 1));
        assert!(check.got_worse(), "a new error");
        assert!(!FileCheck { before: (1, 1), ..check.clone() }.got_worse());
        assert_eq!(
            fix_prompt(&[check], Path::new("/p")),
            "These problems are in the files you just changed. Please fix them:\n- @src/a.rs:5 error: mismatched types\n- @src/a.rs:10 warning: unused\n"
        );
    }
}
