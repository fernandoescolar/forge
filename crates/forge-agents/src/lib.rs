//! Agents in Forge. Agents are external processes speaking the Agent Client Protocol
//! (crates/acp-client); this crate gives them threads: conversations shown as a document
//! in the editor area (thread tab), anchored in the code (ctrl-enter), following the
//! agent live, with access to the project's files through Zed's buffers.

pub mod agent_review;
pub mod agent_settings;
mod anchored;
mod auth;
pub mod context;
pub mod commit_message;
mod commit_proposal;
pub mod conflicts;
pub mod config;
pub mod diff;
mod entry_ui;
mod fix_with_agent;
mod forge_debug;
pub mod forge_mcp;
mod forge_tools;
mod history;
mod mentions;
pub mod permissions;
pub mod plan_usage;
pub mod presence;
mod push_proposal;
pub mod thread;
pub mod threads;
pub mod thread_picker;
pub mod threads_panel;
mod project_fs;
pub mod rules;
pub mod worktree;
pub mod settings;
mod terminals;
mod verify;

pub use fix_with_agent::FixProblemAtCursor;
pub use thread::{Status, Thread, ThreadEvent};
pub use anchored::AskHere;
pub use threads::{Cancel, ManageWorktrees, NewThread, NewThreadInWorktree, OpenThreads, Send, ThreadView};
pub use threads_panel::{OpenAgentSettings, ThreadsPanel, ToggleThreadsPanel};

/// Key bindings that replace Zed's defaults for the same keys; call after loading the
/// default keymap.
pub fn bind_keys(cx: &mut gpui::App) {
    agent_review::bind_keys(cx);
    commit_message::bind_keys(cx);
    context::bind_keys(cx);
}

pub fn init(cx: &mut gpui::App) {
    settings::init(cx);
    threads::init(cx);
    threads_panel::init(cx);
    thread_picker::init(cx);
    agent_settings::init(cx);
    anchored::init(cx);
    agent_review::init(cx);
    presence::init(cx);
    commit_message::init(cx);
    context::init(cx);
    fix_with_agent::init(cx);
    conflicts::init(cx);
}
