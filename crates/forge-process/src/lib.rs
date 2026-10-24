//! How Forge starts the programs it runs (agents, test runners, extensions' sidecars,
//! `git`, `dotnet`…), in one place:
//!
//! - [`command`] finds the program on the `PATH` of the environment it runs in (a project's
//!   shell environment, not Forge's own when it was started from the desktop) and puts it
//!   in a process group of its own, so what it starts in turn can be stopped with it.
//! - [`spawn_unblocked`] starts it with no signals blocked: GPUI's threads block SIGCHLD,
//!   and a child that inherits that never learns its own children exited (`dotnet test`
//!   hangs after its last test).
//! - [`Group`] stops a process group: SIGTERM, then SIGKILL to whatever is left;
//!   [`GroupGuard`] does it when dropped (a cancelled run).
//! - [`output`] runs a program to completion with the same error everywhere.
//!
//! It builds `std::process::Command`s; async callers convert them
//! (`smol::process::Command::from`, `tokio::process::Command::from`).

use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::Duration;

use anyhow::{Context as _, bail};

/// How long a process group has to exit after SIGTERM before it gets SIGKILL.
pub const GRACE: Duration = Duration::from_secs(2);

/// `program` as found on `path` (a `PATH` value): itself when it names a file (it has a
/// folder in it) or isn't found, so the error comes from starting it.
pub fn resolve(program: &str, path: Option<&OsStr>) -> PathBuf {
    let named = Path::new(program);
    if named.components().count() > 1 || named.is_absolute() {
        return named.to_path_buf();
    }
    let Some(path) = path else { return named.to_path_buf() };
    let extensions: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT").unwrap_or_else(|_| ".EXE;.CMD;.BAT;.COM".into()).split(';').map(|e| e.to_ascii_lowercase()).collect()
    } else {
        Vec::new()
    };
    for dir in std::env::split_paths(path) {
        let candidate = dir.join(program);
        if candidate.is_file() {
            return candidate;
        }
        for extension in &extensions {
            let candidate = dir.join(format!("{program}{extension}"));
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    named.to_path_buf()
}

/// A command for `program` run with `env` (all of it when it has a `PATH`, which finds
/// the program; else on top of Forge's own environment), in a process group of its own.
pub fn command(program: &str, env: &[(String, String)]) -> std::process::Command {
    let path = env.iter().find(|(k, _)| k == "PATH").map(|(_, v)| std::ffi::OsString::from(v)).or_else(|| std::env::var_os("PATH"));
    let mut command = std::process::Command::new(resolve(program, path.as_deref()));
    command.envs(env.iter().map(|(k, v)| (k, v)));
    own_group(&mut command);
    command
}

/// Puts what `command` starts in a process group of its own (Unix): stopping its
/// [`Group`] stops what it started too.
pub fn own_group(command: &mut std::process::Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }
    #[cfg(not(unix))]
    let _ = command;
}

/// Runs `spawn` (which starts a process) on a short-lived thread with no signals blocked,
/// so the process starts with none blocked either.
pub fn spawn_unblocked<T: Send + 'static>(spawn: impl FnOnce() -> io::Result<T> + Send + 'static) -> io::Result<T> {
    std::thread::Builder::new()
        .name("forge-spawn".into())
        .spawn(move || {
            #[cfg(unix)]
            unsafe {
                // SAFETY: an empty set from sigemptyset; pthread_sigmask only changes this
                // thread's mask.
                let mut empty: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&mut empty);
                libc::pthread_sigmask(libc::SIG_SETMASK, &empty, std::ptr::null_mut());
            }
            spawn()
        })?
        .join()
        .map_err(|_| io::Error::other("the spawning thread panicked"))?
}

/// The process group a process started through [`command`] leads (its id is the
/// process's).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Group(pub u32);

impl Group {
    /// SIGTERM to the group now, SIGKILL after [`GRACE`] to whatever is left.
    pub fn terminate(self) {
        if self.signal(Signal::Term) {
            std::thread::spawn(move || {
                std::thread::sleep(GRACE);
                self.signal(Signal::Kill);
            });
        }
    }

    /// SIGKILL to the group now.
    pub fn kill(self) {
        self.signal(Signal::Kill);
    }

    /// Whether any process of the group is still there.
    pub fn alive(self) -> bool {
        self.signal(Signal::Probe)
    }

    /// Sends `signal`; whether the group was there to get it.
    fn signal(self, signal: Signal) -> bool {
        #[cfg(unix)]
        {
            let Ok(pid) = i32::try_from(self.0) else { return false };
            if pid <= 0 {
                return false;
            }
            let signal = match signal {
                Signal::Term => libc::SIGTERM,
                Signal::Kill => libc::SIGKILL,
                Signal::Probe => 0,
            };
            // SAFETY: kill(2) only sends a signal; a group that is gone answers ESRCH.
            unsafe { libc::kill(-pid, signal) == 0 }
        }
        #[cfg(not(unix))]
        {
            let _ = signal;
            false
        }
    }
}

enum Signal {
    Term,
    Kill,
    Probe,
}

/// Terminates its group when dropped, unless disarmed: a run that is cancelled (its
/// future dropped) takes what it started with it.
#[derive(Debug)]
pub struct GroupGuard(Option<Group>);

impl GroupGuard {
    pub fn new(pid: Option<u32>) -> Self {
        Self(pid.map(Group))
    }

    /// The run finished on its own: leave what it left running (build servers) alone.
    pub fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for GroupGuard {
    fn drop(&mut self) {
        if let Some(group) = self.0.take() {
            group.terminate();
        }
    }
}

/// Runs `command` to completion; fails with its standard error when it fails, and says
/// when the program couldn't be started at all.
pub fn output(mut command: std::process::Command) -> anyhow::Result<Output> {
    // Its name, not where it was found.
    let program = Path::new(command.get_program()).file_name().unwrap_or(command.get_program()).to_string_lossy().into_owned();
    let output = command.output().with_context(|| format!("cannot run {program}; is it installed?"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim();
        if stderr.is_empty() {
            bail!("{program} failed ({})", output.status);
        }
        bail!("{program} failed: {stderr}");
    }
    Ok(output)
}

/// `git -C dir args…`: its standard output, or what it said went wrong (its standard
/// error, else its output, without git's `fatal: ` / `error: `).
pub fn git(dir: &Path, args: &[&str]) -> anyhow::Result<String> {
    let mut command = std::process::Command::new("git");
    command.arg("-C").arg(dir).args(args);
    let output = command.output().context("cannot run git; is it installed?")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let message = if stderr.is_empty() { String::from_utf8_lossy(&output.stdout).trim().to_string() } else { stderr };
        bail!("{}", message.trim_start_matches("fatal: ").trim_start_matches("error: "));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn finds_programs_on_the_given_path() {
        let dir = tempfile::tempdir().unwrap();
        let tool = dir.path().join("forge-test-tool");
        std::fs::write(&tool, "").unwrap();
        assert_eq!(resolve("forge-test-tool", Some(dir.path().as_os_str())), tool);
        assert_eq!(resolve("forge-missing-tool", Some(dir.path().as_os_str())), PathBuf::from("forge-missing-tool"));
        assert_eq!(resolve("./bin/x", Some(dir.path().as_os_str())), PathBuf::from("./bin/x"));
    }

    /// What a process started through `command` starts goes with it.
    #[test]
    fn terminating_a_group_stops_grandchildren() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("pid");
        let mut sh = command("sh", &[]);
        sh.args(["-c", &format!("sleep 300 & echo $! > '{}'; wait", pidfile.display())]);
        let mut child = spawn_unblocked(move || sh.spawn()).unwrap();
        let group = Group(child.id());
        let grandchild = (0..100)
            .find_map(|_| {
                std::thread::sleep(Duration::from_millis(20));
                std::fs::read_to_string(&pidfile).ok().filter(|p| !p.trim().is_empty())
            })
            .expect("the grandchild started");
        assert!(group.alive());
        group.terminate();
        child.wait().unwrap();
        std::thread::sleep(Duration::from_millis(200));
        let alive = std::process::Command::new("kill").args(["-0", grandchild.trim()]).status().unwrap().success();
        assert!(!alive, "the grandchild went with the group");
        assert!(!group.alive());
    }

    /// A process started from a thread that blocks SIGCHLD starts with nothing blocked.
    #[test]
    fn children_start_without_blocked_signals() {
        std::thread::spawn(|| {
            unsafe {
                let mut blocked: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&mut blocked);
                libc::sigaddset(&mut blocked, libc::SIGCHLD);
                libc::pthread_sigmask(libc::SIG_BLOCK, &blocked, std::ptr::null_mut());
            }
            // The child reports its own blocked signals.
            let mut python = command("python3", &[]);
            python.args(["-c", "import signal; print(sorted(int(s) for s in signal.pthread_sigmask(signal.SIG_BLOCK, [])))"]);
            let Ok(output) = spawn_unblocked(move || python.output()) else { return }; // no python3
            assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "[]");
        })
        .join()
        .unwrap();
    }

    #[test]
    fn output_says_what_failed() {
        let mut fails = command("sh", &[]);
        fails.args(["-c", "echo nope >&2; exit 3"]);
        assert_eq!(output(fails).unwrap_err().to_string(), "sh failed: nope");
        let missing = command("forge-no-such-program", &[]);
        assert!(output(missing).unwrap_err().to_string().contains("is it installed?"));
    }

    #[test]
    fn git_says_what_went_wrong() {
        let dir = tempfile::tempdir().unwrap();
        let error = git(dir.path(), &["rev-parse", "HEAD"]).unwrap_err().to_string();
        assert!(!error.starts_with("fatal: ") && error.contains("not a git repository"), "{error}");
        git(dir.path(), &["init", "-q"]).unwrap();
        assert!(git(dir.path(), &["status", "--short"]).unwrap().is_empty());
    }
}
