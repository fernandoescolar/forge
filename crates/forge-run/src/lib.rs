//! Forge's run controls: finds what a workspace can run (.NET apps and tests, Rust and Go
//! programs, `package.json` scripts and Python programs) and runs, debugs or stops the selected target.

pub mod controller;
pub mod targets;

pub use controller::{Debug, Run, RunController, State, Stop, Watch};

pub fn init(cx: &mut gpui::App) {
    controller::init(cx);
}
