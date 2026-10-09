//! Forge extension host: React extensions run in QuickJS and render as native GPUI elements.
//!
//! ```text
//!  extension.tsx ──► @forge-ide/api reconciler (QuickJS thread) ──ops──► Tree ──► GPUI panel
//!                         ▲                                                      │
//!                         └──────────────── events (click, input) ◄──────────────┘
//! ```

mod agent_tools;
pub mod js;
pub mod tree;

/// React, the reconciler and the API, bundled by build.rs.
pub const RUNTIME_JS: &str = include_str!("../../../packages/forge-api/dist/runtime.js");

pub mod api;
mod chart;
pub mod commands;
pub mod install;
pub mod host;
pub mod layout;
pub mod overview;
pub mod package;
pub mod panel;
pub mod process;
pub mod surface;
pub mod tab;

pub use host::ExtensionHost;
pub use panel::ExtensionsPanel;

use gpui::App;
use std::path::PathBuf;

/// Directories searched for extensions: `<data>/extensions`, the app bundle's
/// `Resources/extensions`, `$FORGE_EXTENSIONS_PATH`
/// (colon-separated, for development), and the repo's `extensions/` in debug builds.
pub fn extension_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![paths::data_dir().join("extensions")];
    // Extensions shipped inside Forge.app (Contents/Resources/extensions).
    if let Some(bundled) = std::env::current_exe().ok().and_then(|exe| Some(exe.parent()?.parent()?.join("Resources/extensions"))) {
        if bundled.is_dir() {
            dirs.push(bundled);
        }
    }
    if let Some(extra) = std::env::var_os("FORGE_EXTENSIONS_PATH") {
        dirs.extend(std::env::split_paths(&extra));
    }
    if cfg!(debug_assertions) {
        dirs.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../extensions"));
    }
    dirs
}

/// Starts the extension runtime and registers the panel's actions.
pub fn init(cx: &mut App) {
    panel::init(cx);
    commands::init(cx);
    install::init(cx);
    api::observe_saves(cx);
    if let Err(e) = ExtensionHost::init(extension_dirs(), cx) {
        log::error!("extension host failed to start: {e:#}");
    }
}

/// What `init` sets up besides starting the runtime, for tests that start their own host.
#[cfg(test)]
pub(crate) fn init_for_tests(cx: &mut App) {
    panel::init(cx);
    commands::init(cx);
    install::init(cx);
    api::observe_saves(cx);
}

#[cfg(test)]
mod e2e_tests;
