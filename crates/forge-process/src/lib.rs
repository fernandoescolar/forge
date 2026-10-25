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

/// Windows' `CREATE_NO_WINDOW`: a console program started by Forge (a GUI program) gets no
/// console window of its own.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// `std::process::Command::new(program)`, without a console window on Windows (`git`,
/// `npx`… would otherwise open one each time).
pub fn std_command(program: impl AsRef<OsStr>) -> std::process::Command {
    #[allow(unused_mut)]
    let mut command = std::process::Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

/// Where `command` is: itself when it is a path (it has a separator), else the first match
/// on `path` (the `PATH` variable's value; the process's when `None`), as the shell finds
/// it. On Windows each folder is tried with the command as given and then with `PATHEXT`'s
/// extensions (Rust's `Command` only adds `.exe`, so `npx.cmd` wouldn't be found).
pub fn find_program(command: &str, path: Option<&OsStr>) -> Option<PathBuf> {
    find_in(command, path, cfg!(windows).then(pathext).as_deref())
}

/// `find_program`, or the command as given when it isn't found (starting it says so).
pub fn program_path(command: &str, path: Option<&OsStr>) -> PathBuf {
    find_program(command, path).unwrap_or_else(|| PathBuf::from(command))
}

/// [`program_path`], under the name Forge's crates use.
pub fn resolve(program: &str, path: Option<&OsStr>) -> PathBuf {
    program_path(program, path)
}

fn pathext() -> Vec<String> {
    let value = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
    value.split(';').map(str::trim).filter(|e| e.starts_with('.')).map(str::to_lowercase).collect()
}

/// `extensions`: what to try appending in each folder (Windows), or `None`.
fn find_in(command: &str, path: Option<&OsStr>, extensions: Option<&[String]>) -> Option<PathBuf> {
    if command.is_empty() {
        return None;
    }
    if command.contains('/') || command.contains(std::path::MAIN_SEPARATOR) {
        return Some(PathBuf::from(command));
    }
    let path: std::ffi::OsString = match path {
        Some(path) => path.to_owned(),
        None => std::env::var_os("PATH")?,
    };
    let has_extension = Path::new(command).extension().is_some();
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(command);
        if candidate.is_file() && (extensions.is_none() || has_extension) {
            return Some(candidate);
        }
        for extension in extensions.unwrap_or_default() {
            let candidate = dir.join(format!("{command}{extension}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// A command for `program` run with `env` (all of it when it has a `PATH`, which finds
/// the program; else on top of Forge's own environment), in a process group of its own.
pub fn command(program: &str, env: &[(String, String)]) -> std::process::Command {
    let path = env.iter().find(|(k, _)| k == "PATH").map(|(_, v)| std::ffi::OsString::from(v)).or_else(|| std::env::var_os("PATH"));
    let mut command = std_command(resolve(program, path.as_deref()));
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
    let mut command = std_command("git");
    command.arg("-C").arg(dir).args(args);
    let output = command.output().context("cannot run git; is it installed?")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let message = if stderr.is_empty() { String::from_utf8_lossy(&output.stdout).trim().to_string() } else { stderr };
        bail!("{}", message.trim_start_matches("fatal: ").trim_start_matches("error: "));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_programs_like_the_shell() {
        let dir = tempfile::tempdir().unwrap();
        let (first, second) = (dir.path().join("first"), dir.path().join("second"));
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        std::fs::write(second.join("tool"), "").unwrap();
        std::fs::write(second.join("npx.cmd"), "").unwrap();
        std::fs::write(first.join("npx"), "").unwrap();
        let path = std::env::join_paths([&first, &second]).unwrap();

        // Unix: the name as given, in PATH's order.
        assert_eq!(find_in("tool", Some(&path), None), Some(second.join("tool")));
        assert_eq!(find_in("npx", Some(&path), None), Some(first.join("npx")));
        assert_eq!(find_in("missing", Some(&path), None), None);

        // Windows: `npx` is `npx.cmd`; a file with no extension isn't a program there.
        let windows = [".exe".to_string(), ".cmd".to_string()];
        assert_eq!(find_in("npx", Some(&path), Some(&windows)), Some(second.join("npx.cmd")));
        assert_eq!(find_in("npx.cmd", Some(&path), Some(&windows)), Some(second.join("npx.cmd")));
        assert_eq!(find_in("tool", Some(&path), Some(&windows)), None);

        // Paths are used as they are.
        assert_eq!(find_in("./bin/agent", Some(&path), None), Some(PathBuf::from("./bin/agent")));
        assert_eq!(resolve("missing-everywhere", Some(&path)), PathBuf::from("missing-everywhere"));
    }

    /// What a process started through `command` starts goes with it.
    #[test]
    #[cfg(unix)]
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
    #[cfg(unix)]
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
    #[cfg(unix)]
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
