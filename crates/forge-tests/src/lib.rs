//! Test explorer for Forge: finds test projects and their tests from source (.NET, Go,
//! Rust, Jest, Vitest and pytest), runs them with each one's tool, and shows results per test in
//! a dock panel.

pub mod discovery;
mod go;
mod gutter;
mod node;
pub mod python;
mod rust;
pub mod panel;
pub mod runner;
pub mod trx;

pub use panel::TestPanel;

pub fn init(cx: &mut gpui::App) {
    panel::init(cx);
    gutter::init(cx);
}
