//! Starting programs the same way on every system (finding them as the shell would, and
//! without a console window on Windows): `forge_process`, re-exported for the crates that
//! reach it through `ide_api`.

pub use forge_process::{find_program, program_path, std_command};

/// [`std_command`] for tokio.
pub fn tokio_command(program: impl AsRef<std::ffi::OsStr>) -> tokio::process::Command {
    tokio::process::Command::from(std_command(program))
}
