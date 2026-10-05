//! Files changed outside Forge while open in an editor.
//!
//! Without unsaved edits the editor simply shows the new content (Zed reloads the
//! buffer). With unsaved edits Zed only marks the file as conflicting; Forge asks what
//! to do: reload from disk (dropping the edits), compare the two versions, or keep the
//! edits (saving then asks before overwriting the file).

use std::collections::{HashMap, HashSet};

use gpui::{App, AppContext as _, Context, Entity, EntityId, Subscription, TaskExt as _, WeakEntity, Window};
use language::{Buffer, BufferEvent};
use project::buffer_store::BufferStoreEvent;
use workspace::{
    Workspace,
    notifications::{NotificationId, simple_message_notification::MessageNotification},
};

/// One watcher per workspace, dropped with it.
#[derive(Default)]
struct Watchers(HashMap<EntityId, Entity<ExternalChanges>>);
impl gpui::Global for Watchers {}

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else { return };
        let watcher = cx.new(|cx| ExternalChanges::new(workspace, window, cx));
        let id = cx.entity_id();
        cx.default_global::<Watchers>().0.insert(id, watcher);
        cx.on_release(move |_, cx| {
            cx.default_global::<Watchers>().0.remove(&id);
        })
        .detach();
    })
    .detach();
}

struct ExternalChanges {
    workspace: WeakEntity<Workspace>,
    buffers: HashMap<EntityId, Subscription>,
    /// (buffer, the file's mtime) already asked about: one question per change on disk.
    asked: HashSet<(EntityId, String)>,
    _store: Subscription,
}

struct ChangedOnDisk;

impl ExternalChanges {
    fn new(workspace: &Workspace, _: &mut Window, cx: &mut Context<Self>) -> Self {
        let project = workspace.project().clone();
        let store = project.read(cx).buffer_store().clone();
        let subscription = cx.subscribe(&store, |this, _, event: &BufferStoreEvent, cx| {
            if let BufferStoreEvent::BufferAdded(buffer) = event {
                this.watch(buffer, cx);
            }
        });
        let mut this = Self { workspace: workspace.weak_handle(), buffers: HashMap::new(), asked: HashSet::new(), _store: subscription };
        for buffer in project.read(cx).opened_buffers(cx) {
            this.watch(&buffer, cx);
        }
        this
    }

    fn watch(&mut self, buffer: &Entity<Buffer>, cx: &mut Context<Self>) {
        let id = buffer.entity_id();
        let subscription = cx.subscribe(buffer, |this, buffer, event: &BufferEvent, cx| match event {
            BufferEvent::FileHandleChanged => this.check(buffer, cx),
            // Saved or reloaded: nothing left to decide.
            BufferEvent::Saved | BufferEvent::Reloaded => this.dismiss(buffer.entity_id(), cx),
            _ => {}
        });
        self.buffers.insert(id, subscription);
    }

    fn check(&mut self, buffer: Entity<Buffer>, cx: &mut Context<Self>) {
        let b = buffer.read(cx);
        if !(b.is_dirty() && b.has_conflict()) {
            return;
        }
        let Some(file) = b.file() else { return };
        let mtime = format!("{:?}", file.disk_state().mtime());
        if !self.asked.insert((buffer.entity_id(), mtime)) {
            return;
        }
        self.ask(buffer, cx);
    }

    /// The question: reload, compare (the question comes back after), or keep yours.
    fn ask(&mut self, buffer: Entity<Buffer>, cx: &mut Context<Self>) {
        let Some(name) = buffer.read(cx).file().map(|f| f.file_name(cx).to_string()) else { return };
        let Some(workspace) = self.workspace.upgrade() else { return };
        let id = NotificationId::composite::<ChangedOnDisk>(buffer.entity_id().as_u64() as usize);
        let this = cx.weak_entity();
        let weak_workspace = self.workspace.clone();
        let weak_buffer = buffer.downgrade();
        workspace.update(cx, |workspace, cx| {
            workspace.show_notification(id, cx, move |cx| {
                cx.new(move |cx| {
                    MessageNotification::new(
                        format!("{name} was changed outside Forge, and you have unsaved edits. Reload it (your edits are lost), compare the two versions, or keep yours: saving will ask before overwriting the file."),
                        cx,
                    )
                    .with_title("File changed on disk")
                    .show_suppress_button(false)
                    .primary_message("Reload from disk")
                    .primary_on_click({
                        let (buffer, workspace) = (weak_buffer.clone(), weak_workspace.clone());
                        move |_, cx| {
                            if let Some(buffer) = buffer.upgrade() {
                                reload_from_disk(&buffer, &workspace, cx);
                            }
                        }
                    })
                    .secondary_message("Compare")
                    .secondary_on_click({
                        let (buffer, workspace, this) = (weak_buffer.clone(), weak_workspace.clone(), this.clone());
                        move |window, cx| {
                            let Some(buffer) = buffer.upgrade() else { return };
                            compare_with_disk(buffer.clone(), &workspace, window, cx);
                            // Buttons close the notification: ask again, to decide after comparing.
                            let this = this.clone();
                            cx.defer(move |cx| {
                                this.update(cx, |this, cx| this.ask(buffer, cx)).ok();
                            });
                        }
                    })
                })
            });
        });
    }

    fn dismiss(&mut self, buffer: EntityId, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else { return };
        let id = NotificationId::composite::<ChangedOnDisk>(buffer.as_u64() as usize);
        workspace.update(cx, |workspace, cx| workspace.dismiss_notification(&id, cx));
    }
}

fn reload_from_disk(buffer: &Entity<Buffer>, workspace: &WeakEntity<Workspace>, cx: &mut App) {
    let Some(workspace) = workspace.upgrade() else { return };
    let project = workspace.read(cx).project().clone();
    let buffers = HashSet::from_iter([buffer.clone()]);
    project.update(cx, |project, cx| project.reload_buffers(buffers, true, cx)).detach_and_log_err(cx);
}

/// Your version, with what differs from the file on disk shown inline (the disk's lines
/// removed, yours added).
fn compare_with_disk(buffer: Entity<Buffer>, workspace: &WeakEntity<Workspace>, window: &mut Window, cx: &mut App) {
    let Some(workspace) = workspace.upgrade() else { return };
    let Some(path) = buffer.read(cx).file().and_then(|f| f.as_local()).map(|f| f.abs_path(cx)) else { return };
    let fs = workspace.read(cx).app_state().fs.clone();
    let project = workspace.read(cx).project().clone();
    let languages = workspace.read(cx).app_state().languages.clone();
    window
        .spawn(cx, async move |cx| {
            let disk = fs.load(&path).await?;
            workspace.update_in(cx, |workspace, window, cx| {
                let editor = cx.new(|cx| editor::Editor::for_buffer(buffer, Some(project), window, cx));
                let diff = forge_agents::diff::show_changes_since(&editor, disk, languages, cx);
                editor.update(cx, |editor, _| {
                    editor.set_read_only(true);
                    // The diff stays current while the tab is open.
                    editor.register_addon(CompareAddon { _diff: diff });
                });
                // Next to yours: a pane holds one editor per file.
                workspace.split_item(workspace::SplitDirection::Right, Box::new(editor), window, cx);
            })
        })
        .detach_and_log_err(cx);
}

struct CompareAddon {
    _diff: gpui::Task<()>,
}

impl editor::Addon for CompareAddon {
    fn to_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, VisualTestContext};
    use serde_json::json;

    /// Unchanged buffers follow the file; edited ones ask, and Reload takes the disk's.
    #[gpui::test]
    async fn follows_or_asks_when_a_file_changes_on_disk(cx: &mut TestAppContext) {
        let params = cx.update(workspace::AppState::test);
        cx.update(|cx| {
            theme_settings::init(::theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            init(cx);
        });
        let fs = params.fs.as_fake();
        fs.insert_tree("/root", json!({ "a.txt": "one\n", "b.txt": "two\n" })).await;
        let project = project::Project::test(params.fs.clone(), ["/root".as_ref()], cx).await;
        let window = cx.add_window(|window, cx| workspace::MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window.read_with(cx, |mw, _| mw.workspace().clone()).unwrap();
        let cx = &mut VisualTestContext::from_window(window.into(), cx);
        let open = |path: &'static str, cx: &mut VisualTestContext| project.update(cx, |p, cx| p.open_local_buffer(path, cx));
        let a = open("/root/a.txt", cx).await.unwrap();
        let b = open("/root/b.txt", cx).await.unwrap();
        cx.run_until_parked();

        // No local edits: the new content shows, nobody is asked.
        fs.insert_file("/root/a.txt", b"one, changed outside\n".to_vec()).await;
        cx.run_until_parked();
        assert_eq!(a.read_with(cx, |b, _| b.text()), "one, changed outside\n");
        assert!(workspace.read_with(cx, |ws, _| ws.notification_ids().is_empty()));

        // Local edits: the user decides.
        b.update(cx, |b, cx| b.edit([(0..0, "mine ")], None, cx));
        fs.insert_file("/root/b.txt", b"two, changed outside\n".to_vec()).await;
        cx.run_until_parked();
        assert_eq!(b.read_with(cx, |b, _| b.text()), "mine two\n", "edits are kept until the user chooses");
        let id = NotificationId::composite::<ChangedOnDisk>(b.entity_id().as_u64() as usize);
        assert!(workspace.read_with(cx, |ws, _| ws.has_notification(&id)), "the user is asked");

        cx.update(|_, cx| reload_from_disk(&b, &workspace.downgrade(), cx));
        cx.run_until_parked();
        assert_eq!(b.read_with(cx, |b, _| b.text()), "two, changed outside\n");
        assert!(!workspace.read_with(cx, |ws, _| ws.has_notification(&id)), "nothing left to decide");
    }
}
