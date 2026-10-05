//! Shows the last result of the test (or class) on each line in the editor's gutter run
//! buttons: passed, failed, running (Zed's own icons and colors for task results). Uses
//! the `RunIndicatorStatus` hook from `patches/zed/0002-editor-run-indicator-status.patch`.

use std::collections::HashMap;
use std::path::PathBuf;
#[cfg(test)]
use std::path::Path;
use std::sync::{Arc, LazyLock, RwLock};

use editor::{Editor, RunIndicatorStatus, RunnableTaskStatus};
use gpui::App;
use workspace::Workspace;

use crate::panel::Status;

/// File → row → status, written by every Tests panel for the files of its projects.
static STATUSES: LazyLock<RwLock<HashMap<PathBuf, HashMap<u32, Status>>>> = LazyLock::new(Default::default);

pub fn init(cx: &mut App) {
    cx.set_global(RunIndicatorStatus(Arc::new(|path, row, _| {
        let status = *STATUSES.read().ok()?.get(path)?.get(&row)?;
        indicator(status)
    })));
}

fn indicator(status: Status) -> Option<RunnableTaskStatus> {
    match status {
        Status::Passed => Some(RunnableTaskStatus::Passed),
        Status::Failed => Some(RunnableTaskStatus::Failed),
        Status::Running => Some(RunnableTaskStatus::Running),
        Status::NotRun | Status::Skipped => None,
    }
}

/// Replaces what is shown for `files` with `statuses`, and repaints the workspace's editors.
pub fn publish(
    files: impl IntoIterator<Item = PathBuf>,
    statuses: Vec<(PathBuf, u32, Status)>,
    workspace: Option<&gpui::Entity<Workspace>>,
    cx: &mut App,
) {
    if let Ok(mut map) = STATUSES.write() {
        for file in files {
            map.remove(&file);
        }
        for (file, row, status) in statuses {
            map.entry(file).or_default().insert(row, status);
        }
    }
    let Some(workspace) = workspace.map(|w| w.downgrade()) else {
        return;
    };
    // Runs start from workspace actions, while the workspace is being updated: repaint after.
    cx.defer(move |cx| {
        let Some(workspace) = workspace.upgrade() else { return };
        let editors: Vec<_> = workspace.read(cx).items_of_type::<Editor>(cx).collect();
        for editor in editors {
            editor.update(cx, |_, cx| cx.notify());
        }
    });
}

#[cfg(test)]
pub(crate) fn status_at(path: &Path, row: u32) -> Option<Status> {
    STATUSES.read().ok()?.get(path)?.get(&row).copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indicators_by_status() {
        assert_eq!(indicator(Status::Passed), Some(RunnableTaskStatus::Passed));
        assert_eq!(indicator(Status::Failed), Some(RunnableTaskStatus::Failed));
        assert_eq!(indicator(Status::NotRun), None);
    }
}
