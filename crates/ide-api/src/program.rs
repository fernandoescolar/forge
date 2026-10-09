//! Finding a program on a `PATH` the way the system's shell would. On Windows, Rust's
//! `Command` only adds `.exe`, so `npx`, `npm` or `code` (which are `npx.cmd`, `npm.cmd`
//! and `code.cmd`) aren't found without trying `PATHEXT`'s extensions. Shared by the agent
//! runtime and the extension host.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

/// Where `command` is: itself when it is a path (it has a separator), else the first match
/// on `path` (the `PATH` variable's value; the process's when `None`). On Windows each
/// folder is tried with the command as given and then with `PATHEXT`'s extensions.
pub fn find_program(command: &str, path: Option<&OsStr>) -> Option<PathBuf> {
    find_in(command, path, cfg!(windows).then(pathext).as_deref())
}

/// `find_program`, or the command as given when it isn't found (the spawn reports it).
pub fn program_path(command: &str, path: Option<&OsStr>) -> PathBuf {
    find_program(command, path).unwrap_or_else(|| PathBuf::from(command))
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
    let path: OsString = match path {
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
        assert_eq!(program_path("missing-everywhere", Some(&path)), PathBuf::from("missing-everywhere"));
    }
}
