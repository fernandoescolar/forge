//! `workspace.openText`: text an extension shows (`docker inspect`, a response…) in a
//! read-only editor tab, with the editor's search, selection, copy and folding.

use anyhow::Result;
use editor::{Editor, MultiBuffer};
use gpui::{AppContext as _, Context, Entity, Window};
use workspace::Workspace;

/// Shows `text` in `existing` (updating its text and title) or in a new tab, highlighted as
/// `language` (Zed's name for it: "JSON", "YAML"…).
pub(crate) fn open(workspace: &mut Workspace, existing: Option<Entity<Editor>>, text: String, title: String, language: Option<String>, window: &mut Window, cx: &mut Context<Workspace>) -> Result<Entity<Editor>> {
    let view = match existing {
        Some(view) => {
            if let Some(buffer) = view.read(cx).buffer().read(cx).as_singleton() {
                buffer.update(cx, |b, cx| b.set_text(text, cx));
            }
            let multibuffer = view.read(cx).buffer().clone();
            multibuffer.update(cx, |b, cx| b.set_title(title, cx));
            if let Some(pane) = workspace.panes().iter().find(|p| p.read(cx).index_for_item(&view).is_some()).cloned() {
                pane.update(cx, |pane, cx| {
                    if let Some(ix) = pane.index_for_item(&view) {
                        pane.activate_item(ix, true, true, window, cx);
                    }
                });
            } else {
                workspace.add_item_to_active_pane(Box::new(view.clone()), None, true, window, cx);
            }
            view
        }
        None => {
            let project = workspace.project().clone();
            let buffer = project.update(cx, |p, cx| p.create_local_buffer(&text, None, false, cx));
            let multibuffer = cx.new(|cx| MultiBuffer::singleton(buffer, cx).with_title(title));
            let view = cx.new(|cx| {
                let mut editor = Editor::for_multibuffer(multibuffer, Some(project), window, cx);
                editor.set_read_only(true);
                editor
            });
            workspace.add_item_to_active_pane(Box::new(view.clone()), None, true, window, cx);
            view
        }
    };
    if let Some(name) = language
        && let Some(buffer) = view.read(cx).buffer().read(cx).as_singleton()
    {
        let registry = workspace.project().read(cx).languages().clone();
        let buffer = buffer.downgrade();
        cx.spawn(async move |_, cx| {
            match registry.language_for_name(&name).await {
                Ok(language) => {
                    buffer.update(cx, |b, cx| b.set_language(Some(language), cx)).ok();
                }
                Err(e) => log::warn!("openText asked for the language {name}: {e:#}"),
            }
        })
        .detach();
    }
    Ok(view)
}
