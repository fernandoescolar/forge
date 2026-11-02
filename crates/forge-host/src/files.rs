//! Files and git on a remote project's host, for features that read or write a file the
//! editor doesn't have open, or run git (agents' reads and writes, their commits and pushes).

use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use gpui::{App, Task};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{Handler, Host, Log};

pub const READ: &str = "fs.read";
pub const WRITE: &str = "fs.write";
pub const CANONICALIZE: &str = "fs.canonicalize";
pub const REMOVE: &str = "fs.remove";
pub const GIT: &str = "git";

/// What the server answers, next to the feature crates' handlers.
pub const HANDLERS: &[(&str, Handler)] = &[(READ, read), (WRITE, write), (CANONICALIZE, canonicalize), (REMOVE, remove), (GIT, git)];

#[derive(Serialize, Deserialize)]
struct PathParams {
    path: PathBuf,
}

#[derive(Serialize, Deserialize)]
struct WriteParams {
    path: PathBuf,
    text: String,
}

#[derive(Serialize, Deserialize)]
pub struct GitParams {
    pub dir: PathBuf,
    pub args: Vec<String>,
    /// What git reads on its stdin (a commit message, for `-F -`).
    #[serde(default)]
    pub input: Option<String>,
}

fn read(params: Value, _: &Log) -> Result<Value> {
    let PathParams { path } = crate::params(params)?;
    let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    crate::result(&text)
}

/// Writes through a file next to it, renamed over it: readers never see half of it. Creates
/// the folders it is in.
fn write(params: Value, _: &Log) -> Result<Value> {
    let WriteParams { path, text } = crate::params(params)?;
    let dir = path.parent().context("no folder")?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let name = path.file_name().context("no file name")?.to_string_lossy();
    let temp = dir.join(format!(".{name}.forge-{}", std::process::id()));
    let mut file = std::fs::File::create(&temp).with_context(|| format!("writing {}", path.display()))?;
    file.write_all(text.as_bytes()).and_then(|()| file.sync_all()).with_context(|| format!("writing {}", path.display()))?;
    // Keep the file's permissions (an executable script stays executable).
    if let Ok(metadata) = std::fs::metadata(&path) {
        std::fs::set_permissions(&temp, metadata.permissions()).ok();
    }
    std::fs::rename(&temp, &path).with_context(|| format!("writing {}", path.display()))?;
    crate::result(&())
}

fn canonicalize(params: Value, _: &Log) -> Result<Value> {
    let PathParams { path } = crate::params(params)?;
    crate::result(&std::fs::canonicalize(&path).with_context(|| format!("resolving {}", path.display()))?)
}

/// Removes a file; one that isn't there is fine.
fn remove(params: Value, _: &Log) -> Result<Value> {
    let PathParams { path } = crate::params(params)?;
    match std::fs::remove_file(&path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e).with_context(|| format!("removing {}", path.display())),
        _ => crate::result(&()),
    }
}

/// `git` in `dir`: its output, or why it failed.
fn git(params: Value, _: &Log) -> Result<Value> {
    let GitParams { dir, args, input } = crate::params(params)?;
    let Some(input) = input else {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        return crate::result(&forge_process::git(&dir, &args)?);
    };
    let mut child = std::process::Command::new("git")
        .arg("-C")
        .arg(&dir)
        .args(&args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("cannot run git; is it installed?")?;
    child.stdin.take().context("no stdin")?.write_all(input.as_bytes())?;
    let out = child.wait_with_output()?;
    if !out.status.success() {
        let text = String::from_utf8_lossy(if out.stderr.is_empty() { &out.stdout } else { &out.stderr }).trim().to_string();
        anyhow::bail!("{}", text.trim_start_matches("fatal: ").trim_start_matches("error: "));
    }
    crate::result(&String::from_utf8_lossy(&out.stdout).into_owned())
}

impl Host {
    pub fn read(&self, path: &Path, cx: &App) -> Task<Result<String>> {
        self.call(READ, &PathParams { path: path.to_path_buf() }, None, cx)
    }

    pub fn write(&self, path: &Path, text: String, cx: &App) -> Task<Result<()>> {
        self.call(WRITE, &WriteParams { path: path.to_path_buf(), text }, None, cx)
    }

    pub fn canonicalize(&self, path: &Path, cx: &App) -> Task<Result<PathBuf>> {
        self.call(CANONICALIZE, &PathParams { path: path.to_path_buf() }, None, cx)
    }

    pub fn remove(&self, path: &Path, cx: &App) -> Task<Result<()>> {
        self.call(REMOVE, &PathParams { path: path.to_path_buf() }, None, cx)
    }
}
