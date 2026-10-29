//! The files of a project as Zed indexes them, for features that look at the whole
//! project (test discovery, run targets, the tools a project needs).

use std::path::PathBuf;

use gpui::{App, Entity, Task};
use project::Project;

/// The files of the project's worktrees, as Zed indexed them (gitignored ones left out), by
/// worktree root: a local worktree's once its first scan is complete, a remote one's as the
/// host has sent them so far (more come as updated entries). A remote project's paths are
/// on its host: read them there (`forge_host`), not from this disk.
pub fn files(project: &Entity<Project>, cx: &App) -> Task<Vec<(PathBuf, Vec<PathBuf>)>> {
    let worktrees: Vec<_> = project.read(cx).visible_worktrees(cx).map(|wt| (wt.clone(), wt.read(cx).as_local().map(|local| local.scan_complete()))).collect();
    let executor = cx.background_executor().clone();
    cx.spawn(async move |cx| {
        let mut snapshots = Vec::new();
        for (worktree, scanned) in worktrees {
            if let Some(scanned) = scanned {
                scanned.await;
            }
            snapshots.push(worktree.read_with(cx, |wt, _| wt.snapshot()));
        }
        executor
            .spawn(async move {
                snapshots
                    .into_iter()
                    .map(|snapshot| {
                        let root = snapshot.abs_path().to_path_buf();
                        let files = snapshot.files(false, 0).map(|entry| root.join(entry.path.as_std_path())).collect();
                        (root, files)
                    })
                    .collect()
            })
            .await
    })
}
