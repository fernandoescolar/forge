//! Tells OmniSharp when a project was restored.
//!
//! When a project file changes (new package versions, another target framework),
//! OmniSharp reloads the project right away, before `dotnet restore` has written the
//! new `obj/project.assets.json`, so it loads stale or missing references. The restore
//! that follows is never reported to it: `obj/` is git-ignored and the editor doesn't
//! forward changes there. OmniSharp then keeps "type or namespace could not be found"
//! errors (CS0246) until it is restarted. So Forge watches every `project.assets.json`
//! itself and, when one changes, tells OmniSharp the project changed, which reloads it
//! with the restored references.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::Duration;

use dotnet_model::{paths, solution};
use futures::StreamExt as _;
use gpui::{App, AppContext as _, AsyncApp, Entity, Task, WeakEntity};
use project::Project;

const OMNISHARP: &str = "omnisharp";

/// One file-system watcher per workspace folder.
#[derive(Default)]
pub(crate) struct AssetsWatchers(HashMap<PathBuf, Task<()>>);

impl AssetsWatchers {
    /// Watches the folders in `roots`, and stops watching the ones no longer there.
    pub(crate) fn sync(&mut self, project: &Entity<Project>, roots: Vec<PathBuf>, cx: &mut App) {
        self.0.retain(|root, _| roots.contains(root));
        for root in roots {
            if self.0.contains_key(&root) {
                continue;
            }
            let task = watch(project.downgrade(), root.clone(), cx);
            self.0.insert(root, task);
        }
    }
}

fn watch(project: WeakEntity<Project>, root: PathBuf, cx: &mut App) -> Task<()> {
    let Some(fs) = project.upgrade().map(|p| p.read(cx).fs().clone()) else { return Task::ready(()) };
    cx.spawn(async move |cx: &mut AsyncApp| {
        let (mut events, _watcher) = fs.watch(&root, Duration::from_millis(500)).await;
        while let Some(batch) = events.next().await {
            let restored: BTreeSet<PathBuf> = batch.iter().filter(|e| paths::file_name(&e.path) == "project.assets.json").map(|e| e.path.clone()).collect();
            if restored.is_empty() {
                continue;
            }
            let files: Vec<PathBuf> = cx
                .background_spawn(async move { restored.iter().filter_map(|assets| project_file_for(assets)).collect() })
                .await;
            let Some(project) = project.upgrade() else { break };
            cx.update(|cx| notify_changed(&project, &files, cx));
        }
    })
}

/// The project whose restore output `assets` is: the project file next to its `obj/`.
fn project_file_for(assets: &Path) -> Option<PathBuf> {
    let dir = assets.parent()?.parent()?;
    std::fs::read_dir(dir).ok()?.flatten().map(|e| e.path()).find(|p| solution::PROJECT_EXTENSIONS.contains(&paths::extension(p).as_str()))
}

/// Sends OmniSharp a `workspace/didChangeWatchedFiles` for `files`.
fn notify_changed(project: &Entity<Project>, files: &[PathBuf], cx: &App) {
    let changes: Vec<lsp::FileEvent> = files
        .iter()
        .filter_map(|file| lsp::Uri::from_file_path(file).ok())
        .map(|uri| lsp::FileEvent { uri, typ: lsp::FileChangeType::CHANGED })
        .collect();
    if changes.is_empty() {
        return;
    }
    let lsp_store = project.read(cx).lsp_store();
    let lsp_store = lsp_store.read(cx);
    for (id, status) in lsp_store.language_server_statuses() {
        if status.name.0.as_ref() != OMNISHARP {
            continue;
        }
        if let Some(server) = lsp_store.language_server_for_id(id) {
            log::info!("telling OmniSharp {} project(s) were restored", changes.len());
            server.notify::<lsp::notification::DidChangeWatchedFiles>(lsp::DidChangeWatchedFilesParams { changes: changes.clone() }).ok();
        }
    }
}
