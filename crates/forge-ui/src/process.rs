//! Starting child processes that wait for their own children (`dotnet test`, `dotnet
//! restore`) from GPUI's threads, which block SIGCHLD: see `forge_process`.

use std::io;

use util::command::{Child, Command};

/// Spawns `command` with no signals blocked.
pub fn spawn_unblocked(mut command: Command) -> io::Result<Child> {
    forge_process::spawn_unblocked(move || command.spawn())
}
