//! Fetches in the background, so the title bar can tell when the server has commits the
//! branch doesn't (↓N). Only repositories whose branch tracks a remote branch are
//! fetched, and never interactively: a remote that needs a password the credential
//! helper or ssh-agent can't supply is simply skipped.

use std::{path::PathBuf, time::Duration};

use gpui::{App, AsyncApp, Entity};
use project::Project;
use workspace::Workspace;

/// After opening a project, then every `INTERVAL`.
const FIRST: Duration = Duration::from_secs(10);
const INTERVAL: Duration = Duration::from_secs(5 * 60);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, cx| {
        let project = workspace.project().downgrade();
        cx.spawn(async move |_, cx| {
            cx.background_executor().timer(FIRST).await;
            loop {
                let Some(project) = project.upgrade() else { break };
                fetch_all(&project, cx).await;
                drop(project);
                cx.background_executor().timer(INTERVAL).await;
            }
        })
        .detach();
    })
    .detach();
}

/// The working directories of repositories whose branch tracks a remote.
fn tracked_repositories(project: &Entity<Project>, cx: &App) -> Vec<PathBuf> {
    project
        .read(cx)
        .git_store()
        .read(cx)
        .repositories()
        .values()
        .filter_map(|repo| {
            let repo = repo.read(cx);
            let upstream = repo.branch.as_ref()?.upstream.as_ref()?;
            upstream.remote_name()?;
            Some(repo.work_directory_abs_path.to_path_buf())
        })
        .collect()
}

async fn fetch_all(project: &Entity<Project>, cx: &mut AsyncApp) {
    let dirs = cx.update(|cx| tracked_repositories(project, cx));
    for dir in dirs {
        let mut command = util::command::new_command("git");
        command
            .args(["fetch", "--quiet"])
            .current_dir(&dir)
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(util::command::Stdio::null())
            .stdout(util::command::Stdio::null())
            .stderr(util::command::Stdio::null());
        match command.status().await {
            Ok(status) if !status.success() => log::info!("background fetch in {} failed: {status}", dir.display()),
            Err(err) => log::info!("background fetch in {} failed: {err}", dir.display()),
            Ok(_) => {}
        }
    }
}
