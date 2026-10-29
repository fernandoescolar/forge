//! Forge's run controls: finds what a workspace can run (Aspire app hosts, .NET apps and tests, Rust and Go
//! programs, `package.json` scripts and Python programs) and runs, debugs or stops the selected target.

#[cfg(feature = "ui")]
pub mod aspire;
#[cfg(feature = "ui")]
pub mod controller;
pub mod host;
pub mod targets;

#[cfg(feature = "ui")]
mod agent_tools;
#[cfg(feature = "ui")]
pub use controller::{Debug, OpenDashboard, Run, RunController, State, Stop, Watch};

#[cfg(feature = "ui")]
pub fn init(cx: &mut gpui::App) {
    controller::init(cx);
    agent_tools::init(cx);
}
