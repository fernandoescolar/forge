//! Extension commands in Zed's command palette, and as a bindable action:
//!
//! ```json
//! { "bindings": { "cmd-alt-h": ["forge_extensions::RunCommand", { "id": "workspace-notes.hello" }] } }
//! ```

use crate::host::ExtensionHost;
use command_palette_hooks::{CommandInterceptItem, CommandInterceptResult, GlobalCommandPaletteInterceptor};
use fuzzy::StringMatchCandidate;
use gpui::{Action, App, AppContext as _, Task};
use schemars::JsonSchema;
use serde::Deserialize;
use workspace::Workspace;

/// Runs a command registered by an extension with `forge.commands.register`.
#[derive(PartialEq, Clone, Deserialize, Default, JsonSchema, Action)]
#[action(namespace = forge_extensions)]
#[serde(deny_unknown_fields)]
pub struct RunCommand {
    pub id: String,
}

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|_, action: &RunCommand, _, cx| {
            if let Some(host) = ExtensionHost::global(cx) {
                host.read(cx).run_command(&action.id);
            }
        });
    })
    .detach();

    GlobalCommandPaletteInterceptor::set(cx, |query, _workspace, cx| {
        let commands: Vec<(String, String)> = ExtensionHost::global(cx)
            .map(|h| h.read(cx).commands.iter().map(|c| (c.id.clone(), palette_label(&c.id, &c.title))).collect())
            .unwrap_or_default();
        let query = query.trim().to_string();
        if commands.is_empty() || query.is_empty() {
            return Task::ready(CommandInterceptResult::default());
        }
        let executor = cx.background_executor().clone();
        cx.background_spawn(async move {
            let candidates: Vec<_> = commands.iter().enumerate().map(|(i, (_, label))| StringMatchCandidate::new(i, label)).collect();
            let matches = fuzzy::match_strings(&candidates, &query, false, true, 20, &Default::default(), executor).await;
            CommandInterceptResult {
                results: matches
                    .into_iter()
                    .map(|m| {
                        let (id, label) = &commands[m.candidate_id];
                        CommandInterceptItem { action: Box::new(RunCommand { id: id.clone() }), string: label.clone(), positions: m.positions }
                    })
                    .collect(),
                exclusive: false,
            }
        })
    });
}

/// "workspace-notes.hello" + "Say hello" → "workspace-notes: Say hello" (like Zed's "editor: …").
fn palette_label(id: &str, title: &str) -> String {
    match id.split_once('.') {
        Some((ext, _)) => format!("{ext}: {title}"),
        None => title.to_string(),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn labels_follow_zed_palette_style() {
        assert_eq!(super::palette_label("workspace-notes.hello", "Say hello"), "workspace-notes: Say hello");
        assert_eq!(super::palette_label("plain", "Do it"), "Do it");
    }
}
