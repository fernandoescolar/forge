//! One Forge per user where the system doesn't see to it (Linux): `forge .` while Forge is
//! running hands the paths to the running Forge, over a Unix socket, and exits. On macOS,
//! `open -a Forge` (what the `forge` command runs) already does that.
//!
//! The first Forge listens on `$XDG_RUNTIME_DIR/forge.sock` (or in its data folder). Each
//! later one sends a line of JSON, the paths to open (none: just come to the front), and
//! waits for `ok`. A socket nobody answers on is left over from a Forge that crashed: the
//! new Forge takes its place.

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub enum Instance {
    /// This is the Forge that runs: it serves the socket.
    First(UnixListener),
    /// Another Forge runs and took the paths.
    Forwarded,
}

/// The socket's name. Development builds (`cargo run`) have their own: they run next to an
/// installed Forge instead of handing their paths to it.
const SOCKET: &str = if cfg!(debug_assertions) { "forge-dev.sock" } else { "forge.sock" };

/// A request is one short line: anything longer, or slower, is dropped. Well within how
/// long a new Forge waits for the answer (`ANSWER_TIMEOUT`), after which it takes the
/// socket over as left behind.
const MAX_REQUEST: u64 = 64 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(1);
const ANSWER_TIMEOUT: Duration = Duration::from_secs(3);

/// Where the socket is: the session's runtime folder, else `data_dir`.
pub fn socket_path(data_dir: &Path) -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).filter(|d| d.is_dir()).unwrap_or_else(|| data_dir.to_path_buf()).join(SOCKET)
}

/// Hands `paths` to a running Forge, or becomes the one that runs.
pub fn claim(socket: &Path, paths: &[PathBuf]) -> std::io::Result<Instance> {
    if let Ok(stream) = UnixStream::connect(socket) {
        if forward(stream, paths).is_ok() {
            return Ok(Instance::Forwarded);
        }
    }
    // Nobody answered: a stale socket, or none.
    std::fs::remove_file(socket).ok();
    if let Some(parent) = socket.parent() {
        std::fs::create_dir_all(parent)?;
    }
    Ok(Instance::First(UnixListener::bind(socket)?))
}

fn forward(mut stream: UnixStream, paths: &[PathBuf]) -> std::io::Result<()> {
    stream.set_read_timeout(Some(ANSWER_TIMEOUT))?;
    let line = serde_json::to_string(paths).map_err(std::io::Error::other)?;
    stream.write_all(format!("{line}\n").as_bytes())?;
    let mut answer = String::new();
    BufReader::new(stream).read_line(&mut answer)?;
    if answer.trim() == "ok" { Ok(()) } else { Err(std::io::Error::other("no answer")) }
}

/// Serves the socket on a thread of its own: each request's paths go to `open`.
pub fn serve(listener: UnixListener, open: impl Fn(Vec<PathBuf>) + Send + 'static) {
    std::thread::Builder::new()
        .name("forge-instance".into())
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                // One client at a time: one that never finishes its line mustn't hold up
                // every later `forge .`.
                if stream.set_read_timeout(Some(REQUEST_TIMEOUT)).is_err() {
                    continue;
                }
                let mut line = String::new();
                let mut reader = BufReader::new(std::io::Read::take(&stream, MAX_REQUEST));
                if reader.read_line(&mut line).is_err() || !line.ends_with('\n') {
                    continue;
                }
                let Ok(paths) = serde_json::from_str::<Vec<PathBuf>>(line.trim()) else { continue };
                open(paths);
                (&stream).write_all(b"ok\n").ok();
            }
        })
        .ok();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    /// A client that connects and says nothing doesn't keep the next Forge from getting
    /// through.
    #[test]
    fn a_silent_client_doesnt_block_the_socket() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("forge.sock");
        let Instance::First(listener) = claim(&socket, &[]).unwrap() else { panic!("the first Forge runs") };
        let (tx, rx) = mpsc::channel();
        serve(listener, move |paths| tx.send(paths).unwrap());
        let _silent = UnixStream::connect(&socket).unwrap();
        let paths = vec![PathBuf::from("/work/app")];
        assert!(matches!(claim(&socket, &paths).unwrap(), Instance::Forwarded), "the next Forge got through");
        assert_eq!(rx.recv_timeout(Duration::from_secs(3)).unwrap(), paths);
    }

    #[test]
    fn a_second_forge_hands_its_paths_to_the_first() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("forge.sock");
        let Instance::First(listener) = claim(&socket, &[]).unwrap() else { panic!("the first Forge runs") };
        let (tx, rx) = mpsc::channel();
        serve(listener, move |paths| tx.send(paths).unwrap());

        let paths = vec![PathBuf::from("/work/app"), PathBuf::from("/work/notes.md")];
        assert!(matches!(claim(&socket, &paths).unwrap(), Instance::Forwarded));
        assert_eq!(rx.recv_timeout(Duration::from_secs(3)).unwrap(), paths);
        assert!(matches!(claim(&socket, &[]).unwrap(), Instance::Forwarded), "no paths: just come to the front");
        assert_eq!(rx.recv_timeout(Duration::from_secs(3)).unwrap(), Vec::<PathBuf>::new());
    }

    #[test]
    fn a_socket_left_by_a_crashed_forge_is_taken_over() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("forge.sock");
        drop(UnixListener::bind(&socket).unwrap());
        assert!(socket.exists(), "the file stays after its Forge is gone");
        assert!(matches!(claim(&socket, &[PathBuf::from("/x")]).unwrap(), Instance::First(_)));
    }
}
