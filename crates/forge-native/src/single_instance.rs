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

/// Where the socket is: the session's runtime folder, else `data_dir`.
pub fn socket_path(data_dir: &Path) -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).filter(|d| d.is_dir()).unwrap_or_else(|| data_dir.to_path_buf()).join("forge.sock")
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
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
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
                let mut line = String::new();
                let mut reader = BufReader::new(&stream);
                if reader.read_line(&mut line).is_err() {
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
