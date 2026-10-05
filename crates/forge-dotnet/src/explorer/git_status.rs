//! Git status in the Solution Explorer, as the project panel shows it: changed files take
//! the git colour and a letter (M, A, U, D, !); folders, projects and the solution that
//! contain changes take the colour and a dot.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use git::status::GitSummary;
use gpui::{App, Entity};
use project::Project;
use ui::Color;

/// Every changed file, and every folder above one up to its repository's root, with the
/// summary of the changes under it.
pub(super) fn collect(project: &Entity<Project>, cx: &App) -> HashMap<PathBuf, GitSummary> {
    let mut out: HashMap<PathBuf, GitSummary> = HashMap::new();
    for repo in project.read(cx).git_store().read(cx).repositories().values() {
        let repo = repo.read(cx);
        let root = repo.work_directory_abs_path.to_path_buf();
        for entry in repo.status() {
            if entry.status.is_ignored() {
                continue;
            }
            let summary = entry.status.summary();
            let path = root.join(entry.repo_path.as_std_path());
            *out.entry(path.clone()).or_default() += summary;
            for dir in path.ancestors().skip(1) {
                if !dir.starts_with(&root) {
                    break;
                }
                *out.entry(dir.to_path_buf()).or_default() += summary;
            }
        }
    }
    out
}

/// The colour for a row with these changes; `None` when there are none.
pub(super) fn color(summary: &GitSummary) -> Option<Color> {
    let tracked = summary.index + summary.worktree;
    if summary.conflict > 0 {
        Some(Color::Conflict)
    } else if tracked.deleted > 0 {
        Some(Color::Deleted)
    } else if tracked.modified > 0 {
        Some(Color::Modified)
    } else if tracked.added > 0 || summary.untracked > 0 {
        Some(Color::Created)
    } else {
        None
    }
}

/// The letter for a changed file, like the project panel's.
pub(super) fn letter(summary: &GitSummary) -> Option<&'static str> {
    let tracked = summary.index + summary.worktree;
    if summary.conflict > 0 {
        Some("!")
    } else if summary.untracked > 0 {
        Some("U")
    } else if tracked.deleted > 0 {
        Some("D")
    } else if tracked.modified > 0 {
        Some("M")
    } else if tracked.added > 0 {
        Some("A")
    } else {
        None
    }
}

/// The path whose changes a row shows: projects show their folder's.
pub(super) fn row_path(path: &Path, is_project: bool) -> &Path {
    if is_project { path.parent().unwrap_or(path) } else { path }
}
