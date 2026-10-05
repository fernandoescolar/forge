//! Forge's Output panel (its own log and the language servers' logs) and the status-bar
//! item that shows what the language servers are doing.

pub mod app_log;
pub mod panel;
pub mod status;

pub use panel::OutputPanel;
pub use status::LanguageServerStatus;

pub fn init(cx: &mut gpui::App) {
    panel::init(cx);
}
