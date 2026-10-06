//! Forge's run controls: finds what a workspace can run (Aspire app hosts, .NET apps and tests, Rust and Go
//! programs, `package.json` scripts and Python programs) and runs, debugs or stops the selected target.

pub mod aspire;
pub mod controller;
pub mod targets;

pub use controller::{Debug, OpenDashboard, Run, RunController, State, Stop, Watch};

pub fn init(cx: &mut gpui::App) {
    controller::init(cx);
}
