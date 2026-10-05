//! .NET knowledge for Forge without any UI: solutions (`.sln`, `.slnx`), MSBuild project
//! evaluation and edits, restore output, NuGet feeds and file templates. Everything here is
//! plain Rust over the file system, so it can be tested without a window.

pub mod assets;
pub mod cli;
pub mod explorer;
pub mod msbuild;
pub mod nuget;
pub mod paths;
pub mod sln;
pub mod slnx;
pub mod solution;
pub mod templates;
pub mod xml_edit;

pub use msbuild::{EvalOptions, Project, evaluate};
pub use solution::{Solution, SolutionEdit, SolutionFormat};
