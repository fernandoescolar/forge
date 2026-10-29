//! What Forge and the server it runs on remote hosts agree on: how the server's binaries are
//! named and where each keeps its data. The server itself is `main.rs`.

use std::path::PathBuf;

/// Forge's server lives apart from Zed's (`~/.zed_server/zed-remote-server-…`), so that a
/// host both connect to keeps both, and neither cleans up the other's binaries as old ones.
pub const NAMES: paths::RemoteServerNames = paths::RemoteServerNames {
    dir: ".forge_server",
    binary_prefix: "forge-remote-server",
    cargo_package: "forge-remote-server",
    cargo_binary: "forge-remote-server",
};

/// Uses Forge's names for the remote server. Call it at startup, before any connection.
pub fn use_forge_names() {
    if let Err(names) = paths::set_remote_server_names(NAMES) {
        debug_assert!(false, "remote server names were already set: {names:?}");
    }
}

/// Forge's data folder: settings, databases, caches, downloaded language servers. Apart from
/// Zed's, so Forge never shares them with an installed Zed, here or on a remote host.
pub fn data_dir() -> PathBuf {
    let home = std::env::home_dir().unwrap_or_default();
    if cfg!(target_os = "macos") {
        return home.join("Library/Application Support/Forge");
    }
    if cfg!(windows) {
        let local = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(|| home.join("AppData").join("Local"));
        return local.join("Forge");
    }
    let data_home = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).filter(|p| p.is_absolute()).unwrap_or_else(|| home.join(".local/share"));
    data_home.join("forge")
}
