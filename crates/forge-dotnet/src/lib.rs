//! .NET in Forge, ported from vscode-solution-explorer: the Solution Explorer panel, the
//! NuGet package manager, and package names, versions and update hints while editing
//! project files. The .NET knowledge itself lives in `dotnet-model`.

pub mod assembly_explorer;
mod build_problems;
pub mod code_analysis;
pub mod config;
pub mod explorer;
mod fetch;
pub mod global_usings;
mod language_server_sync;
pub mod model;
pub mod nuget_view;
pub mod project_files;
pub mod project_properties;
mod restore;
mod roslyn_session;

pub use explorer::SolutionExplorer;

pub fn init(cx: &mut gpui::App) {
    cx.set_global(config::load());
    config::register_settings(cx);
    fetch::init(cx);
    explorer::init(cx);
    project_files::init(cx);
    global_usings::init(cx);
    code_analysis::init(cx);
}
