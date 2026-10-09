//! One Forge per user where the system doesn't see to it (Linux, Windows): `forge .` while
//! Forge is running hands the paths to the running Forge and exits. On macOS, `open -a
//! Forge` (what the `forge` command runs) already does that.
//!
//! The first Forge listens: on a Unix socket (`$XDG_RUNTIME_DIR/forge.sock`, or in its data
//! folder) or on Windows a named pipe (`\\.\pipe\forge-<user>`). Each later one sends a line
//! of JSON, the paths to open (none: just come to the front), and waits for `ok`. A socket
//! nobody answers on is left over from a Forge that crashed: the new Forge takes its place
//! (a pipe goes away with its Forge).

use std::io::{BufRead as _, BufReader, Read, Write};
use std::path::{Path, PathBuf};

pub enum Instance {
    /// This is the Forge that runs: it serves the socket or pipe.
    First(Listener),
    /// Another Forge runs and took the paths.
    Forwarded,
}

/// Sends `paths` over a connection to the running Forge and waits for its `ok`.
fn forward(mut stream: impl Read + Write, paths: &[PathBuf]) -> std::io::Result<()> {
    let line = serde_json::to_string(paths).map_err(std::io::Error::other)?;
    stream.write_all(format!("{line}\n").as_bytes())?;
    stream.flush()?;
    let mut answer = String::new();
    BufReader::new(stream).read_line(&mut answer)?;
    if answer.trim() == "ok" { Ok(()) } else { Err(std::io::Error::other("no answer")) }
}

/// Answers one connection: its paths go to `open`.
fn answer(stream: impl Read + Write, open: &dyn Fn(Vec<PathBuf>)) {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return;
    }
    let Ok(paths) = serde_json::from_str::<Vec<PathBuf>>(line.trim()) else { return };
    open(paths);
    let stream = reader.get_mut();
    stream.write_all(b"ok\n").ok();
    stream.flush().ok();
}

#[cfg(unix)]
pub use unix::*;

#[cfg(unix)]
mod unix {
    use super::*;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::time::Duration;

    pub struct Listener(UnixListener);

    /// Where the socket is: the session's runtime folder, else `data_dir`.
    pub fn endpoint(data_dir: &Path) -> PathBuf {
        std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).filter(|d| d.is_dir()).unwrap_or_else(|| data_dir.to_path_buf()).join("forge.sock")
    }

    /// Hands `paths` to a running Forge, or becomes the one that runs.
    pub fn claim(socket: &Path, paths: &[PathBuf]) -> std::io::Result<Instance> {
        if let Ok(stream) = UnixStream::connect(socket) {
            stream.set_read_timeout(Some(Duration::from_secs(3)))?;
            if forward(stream, paths).is_ok() {
                return Ok(Instance::Forwarded);
            }
        }
        // Nobody answered: a stale socket, or none.
        std::fs::remove_file(socket).ok();
        if let Some(parent) = socket.parent() {
            std::fs::create_dir_all(parent)?;
        }
        Ok(Instance::First(Listener(UnixListener::bind(socket)?)))
    }

    /// Serves the socket on a thread of its own: each request's paths go to `open`.
    pub fn serve(listener: Listener, open: impl Fn(Vec<PathBuf>) + Send + 'static) {
        std::thread::Builder::new()
            .name("forge-instance".into())
            .spawn(move || {
                for stream in listener.0.incoming().flatten() {
                    answer(stream, &open);
                }
            })
            .ok();
    }
}

#[cfg(windows)]
pub use windows_pipe::*;

#[cfg(windows)]
mod windows_pipe {
    use super::*;
    use std::fs::File;
    use std::os::windows::io::FromRawHandle as _;
    use windows::Win32::Foundation::{ERROR_PIPE_CONNECTED, HANDLE};
    use windows::Win32::Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX};
    use windows::Win32::System::Pipes::{ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT};
    use windows::core::HSTRING;

    /// The pipe's name, waiting for the next connection's instance.
    pub struct Listener {
        name: PathBuf,
        next: File,
    }

    /// The pipe's name: one per user (pipes are shared by the whole machine).
    pub fn endpoint(_data_dir: &Path) -> PathBuf {
        let user: String = std::env::var("USERNAME").unwrap_or_default().chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect();
        PathBuf::from(format!(r"\\.\pipe\forge-{user}"))
    }

    /// A new instance of pipe `name`; `first` fails when another Forge already serves it.
    fn create(name: &Path, first: bool) -> std::io::Result<File> {
        let mode = if first { PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE } else { PIPE_ACCESS_DUPLEX };
        let handle: HANDLE = unsafe { CreateNamedPipeW(&HSTRING::from(name.as_os_str()), mode, PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT, PIPE_UNLIMITED_INSTANCES, 4096, 4096, 0, None) };
        if handle.is_invalid() {
            return Err(std::io::Error::last_os_error());
        }
        // The handle is ours alone from here: the File closes it.
        Ok(unsafe { File::from_raw_handle(handle.0) })
    }

    /// Hands `paths` to a running Forge, or becomes the one that runs.
    pub fn claim(name: &Path, paths: &[PathBuf]) -> std::io::Result<Instance> {
        let connect = || std::fs::OpenOptions::new().read(true).write(true).open(name);
        if let Ok(pipe) = connect() {
            if forward(pipe, paths).is_ok() {
                return Ok(Instance::Forwarded);
            }
        }
        match create(name, true) {
            Ok(next) => Ok(Instance::First(Listener { name: name.to_path_buf(), next })),
            // Another Forge started at the same moment and serves it now.
            Err(e) => match connect().and_then(|pipe| forward(pipe, paths)) {
                Ok(()) => Ok(Instance::Forwarded),
                Err(_) => Err(e),
            },
        }
    }

    /// Serves the pipe on a thread of its own: each request's paths go to `open`.
    pub fn serve(listener: Listener, open: impl Fn(Vec<PathBuf>) + Send + 'static) {
        std::thread::Builder::new()
            .name("forge-instance".into())
            .spawn(move || {
                let Listener { name, mut next } = listener;
                loop {
                    let handle = HANDLE(std::os::windows::io::AsRawHandle::as_raw_handle(&next));
                    // A client that connected between the pipe's creation and this call is
                    // connected already (ERROR_PIPE_CONNECTED).
                    let connected = unsafe { ConnectNamedPipe(handle, None) };
                    if connected.is_ok() || connected.as_ref().is_err_and(|e| e.code() == ERROR_PIPE_CONNECTED.to_hresult()) {
                        answer(&next, &open);
                    }
                    next = match create(&name, false) {
                        Ok(pipe) => pipe,
                        Err(e) => return log::warn!("the instance pipe stopped: {e}"),
                    };
                }
            })
            .ok();
    }
}

#[cfg(all(test, any(unix, windows)))]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn a_second_forge_hands_its_paths_to_the_first() {
        let dir = tempfile::tempdir().unwrap();
        // A socket in a folder of its own, or a pipe of its own.
        let socket = if cfg!(windows) { PathBuf::from(format!(r"\\.\pipe\forge-test-{}", std::process::id())) } else { dir.path().join("forge.sock") };
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
    #[cfg(unix)]
    fn a_socket_left_by_a_crashed_forge_is_taken_over() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("forge.sock");
        drop(std::os::unix::net::UnixListener::bind(&socket).unwrap());
        assert!(socket.exists(), "the file stays after its Forge is gone");
        assert!(matches!(claim(&socket, &[PathBuf::from("/x")]).unwrap(), Instance::First(_)));
    }
}
