//! Starting child processes that wait for their own children (`dotnet test`, `dotnet
//! restore`).
//!
//! A child inherits the signal mask of the thread that spawns it, and neither std nor
//! Zed's `util::command` reset it (on purpose: deliberately blocked signals stay blocked).
//! GPUI's executor threads block most signals, SIGCHLD among them, so a process started
//! from one never learns that its own children exited: `dotnet test` hangs after its last
//! test, waiting for a test host that is already a zombie. Spawning from a short-lived
//! thread with an empty mask gives the child the default mask.

use std::io;

use util::command::{Child, Command};

/// Spawns `command` with no signals blocked.
pub fn spawn_unblocked(mut command: Command) -> io::Result<Child> {
    std::thread::Builder::new()
        .name("forge-spawn".into())
        .spawn(move || {
            #[cfg(unix)]
            {
                use nix::sys::signal::{SigSet, SigmaskHow, pthread_sigmask};
                if let Err(err) = pthread_sigmask(SigmaskHow::SIG_SETMASK, Some(&SigSet::empty()), None) {
                    log::warn!("could not clear the signal mask before spawning: {err}");
                }
            }
            command.spawn()
        })?
        .join()
        .map_err(|_| io::Error::other("the spawning thread panicked"))?
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use nix::sys::signal::{SigSet, SigmaskHow, Signal, pthread_sigmask};

    /// Even from a thread that blocks SIGCHLD, the child starts with it unblocked.
    #[test]
    fn children_start_without_blocked_signals() {
        std::thread::spawn(|| {
            let mut blocked = SigSet::empty();
            blocked.add(Signal::SIGCHLD);
            pthread_sigmask(SigmaskHow::SIG_BLOCK, Some(&blocked), None).unwrap();
            // The child reports its own blocked signals (`ps` doesn't show a thread's mask).
            let mut command = util::command::new_command("python3");
            command
                .args(["-c", "import signal; print(sorted(int(s) for s in signal.pthread_sigmask(signal.SIG_BLOCK, [])))"])
                .stdout(util::command::Stdio::piped());
            let Ok(child) = spawn_unblocked(command) else { return }; // no python3: nothing to check with
            let output = futures::executor::block_on(child.output()).unwrap();
            assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "[]", "the child starts with no blocked signals");
        })
        .join()
        .unwrap();
    }
}
