//! The disk a project's files are on, for reading and writing the files the editor doesn't
//! have open: this machine's, or a remote project's host (through `forge_host::files`).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use gpui::{App, Entity};
use project::Project;

#[derive(Clone)]
pub enum Disk {
    Here(Arc<dyn fs::Fs>),
    Host(Arc<Calls>),
}

/// The host's file calls, ready to run from any thread.
pub struct Calls {
    read: forge_host::Blocking,
    write: forge_host::Blocking,
    canonicalize: forge_host::Blocking,
    remove: forge_host::Blocking,
}

impl Disk {
    /// Where `project`'s files are.
    pub fn of(project: &Entity<Project>, cx: &App) -> Self {
        let host = forge_host::Host::of(project, cx).and_then(|host| {
            let call = |method| host.blocking(method, cx).ok();
            use forge_host::files::{CANONICALIZE, READ, REMOVE, WRITE};
            Some(Calls { read: call(READ)?, write: call(WRITE)?, canonicalize: call(CANONICALIZE)?, remove: call(REMOVE)? })
        });
        match host {
            Some(calls) => Self::Host(Arc::new(calls)),
            None => Self::Here(<dyn fs::Fs>::global(cx)),
        }
    }

    pub fn is_remote(&self) -> bool {
        matches!(self, Self::Host(_))
    }

    pub async fn load(&self, path: &Path) -> Result<String> {
        match self {
            Self::Here(fs) => fs.load(path).await,
            Self::Host(calls) => {
                let (calls, path) = (calls.clone(), path.to_path_buf());
                smol::unblock(move || calls.read.call(&serde_json::json!({ "path": path }))).await
            }
        }
    }

    /// Writes `text` to `path`, making its folders.
    pub async fn atomic_write(&self, path: PathBuf, text: String) -> Result<()> {
        match self {
            Self::Here(fs) => {
                if let Some(dir) = path.parent() {
                    fs.create_dir(dir).await?;
                }
                fs.atomic_write(path, text).await
            }
            Self::Host(calls) => {
                let calls = calls.clone();
                smol::unblock(move || calls.write.call(&serde_json::json!({ "path": path, "text": text }))).await
            }
        }
    }

    pub async fn canonicalize(&self, path: &Path) -> Result<PathBuf> {
        match self {
            Self::Here(fs) => fs.canonicalize(path).await,
            Self::Host(calls) => {
                let (calls, path) = (calls.clone(), path.to_path_buf());
                smol::unblock(move || calls.canonicalize.call(&serde_json::json!({ "path": path }))).await
            }
        }
    }

    /// Removes the file at `path`, when there is one.
    pub async fn remove_file(&self, path: &Path) -> Result<()> {
        match self {
            Self::Here(fs) => fs.remove_file(path, fs::RemoveOptions { recursive: false, ignore_if_not_exists: true }).await,
            Self::Host(calls) => {
                let (calls, path) = (calls.clone(), path.to_path_buf());
                smol::unblock(move || calls.remove.call(&serde_json::json!({ "path": path }))).await
            }
        }
    }
}
