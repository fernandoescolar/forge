//! Lets ACP agents read and write files through Zed's project instead of the raw disk:
//! reads see unsaved editor buffers, writes update open buffers (and save them), and
//! anything outside the project's worktrees is refused unless the agent permissions allow
//! files outside the workspace (see `permissions`).
//!
//! Writes can be gated on review: unless the exact content was already approved as a diff
//! (e.g. in a permission request), the panel shows the change and the write waits for the
//! user's decision.
//!
//! `acp-client` runs on tokio and talks to an `ide_api::WorkspaceEngine`; this adapter
//! forwards those calls to a task on the GPUI thread that owns the project.

use async_trait::async_trait;
use futures::{
    StreamExt as _,
    channel::{mpsc, oneshot},
};
use gpui::{App, AsyncApp, Entity, WeakEntity};
use ide_api::{IdeError, IdeResult, WorkspaceEngine};
use project::Project;
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

/// A write waiting for the user. Dropping `reply` counts as a rejection.
pub struct WriteReview {
    pub path: PathBuf,
    pub old_text: Option<String>,
    pub new_text: String,
    pub reply: oneshot::Sender<ReviewDecision>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ReviewDecision {
    Accept,
    /// Some hunks accepted: write this text instead and tell the agent the file differs.
    Partial(String),
    Reject,
}

/// Edits the user already approved as diffs; a matching write is applied without asking again.
#[derive(Clone, Default)]
pub struct ApprovedEdits(Arc<Mutex<Vec<(PathBuf, String)>>>);

impl ApprovedEdits {
    pub fn approve(&self, path: PathBuf, new_text: String) {
        self.0.lock().unwrap().push((path, new_text));
    }

    fn take(&self, path: &Path, new_text: &str) -> bool {
        let mut approved = self.0.lock().unwrap();
        match approved.iter().position(|(p, t)| p == path && t == new_text) {
            Some(i) => {
                approved.remove(i);
                true
            }
            None => false,
        }
    }
}

/// How writes are reviewed. The default applies writes directly.
#[derive(Clone, Default)]
pub struct ReviewPolicy {
    pub reviews: Option<mpsc::UnboundedSender<WriteReview>>,
    pub approved: ApprovedEdits,
    /// Outside-the-workspace access and super user (no reviews), read on every request.
    pub permissions: crate::permissions::SharedPermissions,
    /// Every write that reached the file, so the thread can list, diff and undo them.
    pub written: Option<mpsc::UnboundedSender<WriteRecord>>,
}

/// A file the agent changed: what it held before (`None`: it didn't exist) and now.
#[derive(Debug, Clone, PartialEq)]
pub struct WriteRecord {
    pub path: PathBuf,
    pub old_text: Option<String>,
    pub new_text: String,
}

enum Request {
    Read(PathBuf, oneshot::Sender<IdeResult<String>>),
    Write(PathBuf, String, oneshot::Sender<IdeResult<()>>),
}

pub struct ProjectFs {
    root: PathBuf,
    tx: mpsc::UnboundedSender<Request>,
}

impl ProjectFs {
    pub fn new(project: &Entity<Project>, root: PathBuf, policy: ReviewPolicy, cx: &mut App) -> Self {
        let (tx, mut rx) = mpsc::unbounded::<Request>();
        let project = project.downgrade();
        let fs = <dyn fs::Fs>::global(cx);
        cx.spawn(async move |cx: &mut AsyncApp| {
            while let Some(req) = rx.next().await {
                match req {
                    Request::Read(path, reply) => {
                        let outside_ok = policy.permissions.get().may_touch_outside();
                        let _ = reply.send(read(&project, &fs, &path, outside_ok, cx).await);
                    }
                    Request::Write(path, text, reply) => {
                        let _ = reply.send(write(&project, &fs, &path, text, &policy, cx).await);
                    }
                }
            }
        })
        .detach();
        Self { root, tx }
    }

    fn absolute(&self, path: &Path) -> PathBuf {
        if path.is_absolute() { path.to_path_buf() } else { self.root.join(path) }
    }
}

async fn read(project: &WeakEntity<Project>, fs: &std::sync::Arc<dyn fs::Fs>, path: &Path, outside_ok: bool, cx: &mut AsyncApp) -> IdeResult<String> {
    let open = project
        .update(cx, |project, cx| {
            let project_path = project.find_project_path(path, cx)?;
            Some(project.get_open_buffer(&project_path, cx).map(|b| b.read(cx).text()))
        })
        .map_err(|_| IdeError::Unavailable("project closed".into()))?;
    match open {
        None if outside_ok => fs.load(path).await.map_err(|e| IdeError::Io(format!("{e:#}"))),
        None => Err(IdeError::InvalidInput(format!("{} is outside the project", path.display()))),
        Some(Some(text)) => Ok(text),
        Some(None) => fs.load(path).await.map_err(|e| IdeError::Io(format!("{e:#}"))),
    }
}

async fn write(project: &WeakEntity<Project>, fs: &std::sync::Arc<dyn fs::Fs>, path: &Path, text: String, policy: &ReviewPolicy, cx: &mut AsyncApp) -> IdeResult<()> {
    let permissions = policy.permissions.get();
    let target = match project
        .update(cx, |project, cx| {
            let project_path = project.find_project_path(path, cx)?;
            Some(project.get_open_buffer(&project_path, cx))
        })
        .map_err(|_| IdeError::Unavailable("project closed".into()))?
    {
        Some(target) => target,
        // Outside the project: written straight to disk, when allowed.
        None if permissions.may_touch_outside() => None,
        None => return Err(IdeError::InvalidInput(format!("{} is outside the project", path.display()))),
    };

    let old_text = match &target {
        Some(buffer) => Some(cx.update(|cx| buffer.read(cx).text())),
        None => fs.load(path).await.ok(),
    };
    let record = |new_text: &str| {
        if let Some(written) = &policy.written {
            written.unbounded_send(WriteRecord { path: path.to_path_buf(), old_text: old_text.clone(), new_text: new_text.to_string() }).ok();
        }
    };
    if let Some(reviews) = policy.reviews.as_ref().filter(|_| !permissions.skips_review()) {
        let old_text = old_text.clone();
        let unchanged = old_text.as_deref() == Some(text.as_str());
        if !unchanged && !policy.approved.take(path, &text) {
            let (reply, decision) = oneshot::channel();
            reviews
                .unbounded_send(WriteReview { path: path.to_path_buf(), old_text, new_text: text.clone(), reply })
                .map_err(|_| IdeError::Unavailable("agent panel closed".into()))?;
            match decision.await.unwrap_or(ReviewDecision::Reject) {
                ReviewDecision::Accept => {}
                ReviewDecision::Reject => return Err(IdeError::InvalidInput(format!("the user rejected the edit to {}", path.display()))),
                ReviewDecision::Partial(merged) => {
                    apply(project, fs, path, target, merged.clone(), cx).await?;
                    record(&merged);
                    // The agent must not assume its version is on disk.
                    return Err(IdeError::InvalidInput(format!(
                        "the user accepted only some of the changes to {}; the file now differs from what you wrote, re-read it before editing it again",
                        path.display()
                    )));
                }
            }
        }
    }

    apply(project, fs, path, target, text.clone(), cx).await?;
    record(&text);
    Ok(())
}

/// Puts `path` back as it was before the agent touched it: `original` written through its
/// buffer when open (and saved), or the file removed when the agent created it.
pub async fn restore(project: &WeakEntity<Project>, path: &Path, original: Option<String>, cx: &mut AsyncApp) -> anyhow::Result<()> {
    let fs = cx.update(|cx| <dyn fs::Fs>::global(cx));
    let target = project
        .update(cx, |project, cx| {
            let project_path = project.find_project_path(path, cx)?;
            project.get_open_buffer(&project_path, cx)
        })
        .ok()
        .flatten();
    match original {
        Some(text) => apply(project, &fs, path, target, text, cx).await.map_err(|e| anyhow::anyhow!("{e:?}")),
        None => {
            if let Some(buffer) = target {
                // Leave no editor showing a file that no longer exists as modified.
                cx.update(|cx| buffer.update(cx, |b, cx| b.set_text("", cx)));
            }
            fs.remove_file(path, fs::RemoveOptions { recursive: false, ignore_if_not_exists: true }).await
        }
    }
}

async fn apply(project: &WeakEntity<Project>, fs: &std::sync::Arc<dyn fs::Fs>, path: &Path, target: Option<Entity<language::Buffer>>, text: String, cx: &mut AsyncApp) -> IdeResult<()> {
    match target {
        Some(buffer) => {
            let save = project
                .update(cx, |project, cx| {
                    buffer.update(cx, |b, cx| b.set_text(text.as_str(), cx));
                    project.save_buffer(buffer, cx)
                })
                .map_err(|_| IdeError::Unavailable("project closed".into()))?;
            save.await.map_err(|e| IdeError::Io(format!("{e:#}")))
        }
        None => fs.atomic_write(path.to_path_buf(), text).await.map_err(|e| IdeError::Io(format!("{e:#}"))),
    }
}

#[async_trait]
impl WorkspaceEngine for ProjectFs {
    fn root(&self) -> &Path {
        &self.root
    }

    async fn read_file(&self, path: &Path) -> IdeResult<String> {
        let (tx, rx) = oneshot::channel();
        self.tx.unbounded_send(Request::Read(self.absolute(path), tx)).map_err(|_| IdeError::Unavailable("project closed".into()))?;
        rx.await.map_err(|_| IdeError::Unavailable("project closed".into()))?
    }

    async fn write_file(&self, path: &Path, text: String) -> IdeResult<()> {
        let (tx, rx) = oneshot::channel();
        self.tx.unbounded_send(Request::Write(self.absolute(path), text, tx)).map_err(|_| IdeError::Unavailable("project closed".into()))?;
        rx.await.map_err(|_| IdeError::Unavailable("project closed".into()))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::{FakeFs, Fs as _};
    use gpui::{AppContext as _, TestAppContext};
    use serde_json::json;
    use settings::SettingsStore;

    #[gpui::test]
    async fn agents_see_unsaved_buffers_and_write_through_the_project(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
            release_channel::init(semver::Version::new(0, 0, 0), cx);
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/root", json!({ "open.txt": "on disk", "closed.txt": "closed" })).await;
        let project = Project::test(fs.clone(), ["/root".as_ref()], cx).await;
        cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));
        let buffer = project.update(cx, |p, cx| p.open_local_buffer("/root/open.txt", cx)).await.unwrap();
        buffer.update(cx, |b, cx| b.set_text("unsaved edit", cx));
        let adapter = cx.update(|cx| ProjectFs::new(&project, "/root".into(), ReviewPolicy::default(), cx));

        // Reads prefer the open buffer, fall back to disk, and stay inside the project.
        assert_eq!(adapter.read_file(Path::new("open.txt")).await.unwrap(), "unsaved edit");
        assert_eq!(adapter.read_file(Path::new("/root/closed.txt")).await.unwrap(), "closed");
        assert!(matches!(adapter.read_file(Path::new("/etc/passwd")).await, Err(IdeError::InvalidInput(_))));

        // Writes update (and save) open buffers, or write closed files directly.
        adapter.write_file(Path::new("open.txt"), "from agent".into()).await.unwrap();
        cx.run_until_parked();
        assert_eq!(buffer.read_with(cx, |b, _| b.text()), "from agent");
        assert!(!buffer.read_with(cx, |b, _| b.is_dirty()));
        assert_eq!(fs.load(Path::new("/root/open.txt")).await.unwrap(), "from agent");
        adapter.write_file(Path::new("closed.txt"), "new".into()).await.unwrap();
        assert_eq!(fs.load(Path::new("/root/closed.txt")).await.unwrap(), "new");
        assert!(adapter.write_file(Path::new("/tmp/outside.txt"), "x".into()).await.is_err());
    }

    #[gpui::test]
    async fn writes_wait_for_review_unless_already_approved(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
            release_channel::init(semver::Version::new(0, 0, 0), cx);
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/root", json!({ "a.txt": "old" })).await;
        let project = Project::test(fs.clone(), ["/root".as_ref()], cx).await;
        cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));
        let (tx, mut reviews) = mpsc::unbounded();
        let policy = ReviewPolicy { reviews: Some(tx), approved: ApprovedEdits::default(), permissions: Default::default(), written: None };
        let adapter = Arc::new(cx.update(|cx| ProjectFs::new(&project, "/root".into(), policy.clone(), cx)));

        // Rejected: nothing is written and the agent gets an error.
        let write = cx.background_spawn({
            let adapter = adapter.clone();
            async move { adapter.write_file(Path::new("/root/a.txt"), "rejected".into()).await }
        });
        let review = reviews.next().await.unwrap();
        assert_eq!((review.old_text.as_deref(), review.new_text.as_str()), (Some("old"), "rejected"));
        review.reply.send(ReviewDecision::Reject).unwrap();
        assert!(write.await.unwrap_err().to_string().contains("rejected"));
        assert_eq!(fs.load(Path::new("/root/a.txt")).await.unwrap(), "old");

        // Accepted: applied. New files show as old_text = None.
        let write = cx.background_spawn({
            let adapter = adapter.clone();
            async move { adapter.write_file(Path::new("/root/new.txt"), "hello".into()).await }
        });
        let review = reviews.next().await.unwrap();
        assert_eq!(review.old_text, None);
        review.reply.send(ReviewDecision::Accept).unwrap();
        write.await.unwrap();
        assert_eq!(fs.load(Path::new("/root/new.txt")).await.unwrap(), "hello");

        // Approved beforehand (e.g. via a permission showing this diff): no second review.
        policy.approved.approve("/root/a.txt".into(), "approved".into());
        adapter.write_file(Path::new("/root/a.txt"), "approved".into()).await.unwrap();
        assert_eq!(fs.load(Path::new("/root/a.txt")).await.unwrap(), "approved");
        assert!(reviews.try_recv().is_err(), "no review was requested");

        // Partially accepted: the merged text is written and the agent is told.
        let write = cx.background_spawn({
            let adapter = adapter.clone();
            async move { adapter.write_file(Path::new("/root/a.txt"), "proposed".into()).await }
        });
        reviews.next().await.unwrap().reply.send(ReviewDecision::Partial("merged".into())).unwrap();
        assert!(write.await.unwrap_err().to_string().contains("only some of the changes"));
        assert_eq!(fs.load(Path::new("/root/a.txt")).await.unwrap(), "merged");
    }
}
