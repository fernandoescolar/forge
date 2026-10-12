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

/// Kills the processes started by the process `pid` (not `pid` itself), deepest first.
/// Killing only `cargo test` or `go test` leaves the test binary it started running,
/// adopted by init, with its pipes still open. They are found before any is killed: once
/// their parent dies they no longer descend from `pid`.
#[cfg(unix)]
pub fn kill_descendants(pid: u32) {
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;
    let output = match std::process::Command::new("ps").args(["-A", "-o", "pid=,ppid="]).output() {
        Ok(output) => output,
        Err(err) => return log::warn!("could not list processes to stop {pid}'s children: {err}"),
    };
    let pairs: Vec<(u32, u32)> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut numbers = line.split_whitespace().map(str::parse::<u32>);
            Some((numbers.next()?.ok()?, numbers.next()?.ok()?))
        })
        .collect();
    for child in descendants(pid, &pairs).into_iter().rev() {
        let Ok(child) = i32::try_from(child) else { continue };
        if let Err(err) = kill(Pid::from_raw(child), Signal::SIGKILL) {
            log::debug!("could not stop process {child}: {err}");
        }
    }
}

#[cfg(not(unix))]
pub fn kill_descendants(_pid: u32) {}

/// The processes below `pid` in `(pid, parent)` pairs, parents before their children.
fn descendants(pid: u32, pairs: &[(u32, u32)]) -> Vec<u32> {
    let mut found = vec![pid];
    let mut next = 0;
    while let Some(&parent) = found.get(next) {
        found.extend(pairs.iter().filter(|(child, of)| *of == parent && *child != parent && !found.contains(child)).map(|(child, _)| *child).collect::<Vec<_>>());
        next += 1;
    }
    found.remove(0);
    found
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use nix::sys::signal::{SigSet, SigmaskHow, Signal, pthread_sigmask};

    #[test]
    fn finds_children_and_their_children() {
        let pairs = [(10, 1), (11, 10), (12, 11), (13, 10), (20, 1), (21, 20)];
        assert_eq!(descendants(10, &pairs), [11, 13, 12]);
        assert!(descendants(99, &pairs).is_empty());
    }

    /// A shell's child outlives the shell when only the shell is killed; not with this.
    #[test]
    fn kills_what_a_process_started() {
        let mut command = util::command::new_command("sh");
        command.args(["-c", "sleep 30 & wait"]);
        let Ok(mut child) = spawn_unblocked(command) else { return };
        let started = std::time::Instant::now();
        let sleeper = loop {
            let output = std::process::Command::new("pgrep").args(["-P", &child.id().to_string()]).output().unwrap();
            if let Some(pid) = String::from_utf8_lossy(&output.stdout).split_whitespace().next().and_then(|p| p.parse::<i32>().ok()) {
                break pid;
            }
            assert!(started.elapsed().as_secs() < 5, "the shell started no child");
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        kill_descendants(child.id());
        child.kill().ok();
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(nix::sys::signal::kill(nix::unistd::Pid::from_raw(sleeper), None).is_err(), "the sleep is gone");
    }

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
