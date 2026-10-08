//! Commits the agent proposes (`propose_commit`, or a `git commit` it tried to run): a card
//! in the thread with the message and the files. The user commits them from there, takes
//! them to the Git panel to edit the message first, or declines; the agent hears which.
//!
//! Git runs in the thread's folder (its worktree, when it has one) with the user's own
//! `git`, so hooks and signing behave as in a terminal; Zed's Git panel picks the commit
//! up from the repository.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context as _, Result};

/// Where a proposal came from.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Origin {
    /// The agent called `propose_commit`; its call waits for the answer.
    Tool,
    /// The agent tried to run `git commit`; Forge declined it and brought it here.
    Command,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum CommitState {
    Waiting,
    /// In the Git panel, waiting for the user to commit there (`head`: HEAD before).
    InPanel { head: Option<String> },
    Committing,
    Committed { sha: String, message: String },
    Declined,
    Failed(String),
}

impl CommitState {
    pub fn is_open(&self) -> bool {
        matches!(self, Self::Waiting | Self::InPanel { .. } | Self::Failed(_))
    }
}

/// What the agent is told.
pub(crate) fn report(state: &CommitState) -> crate::forge_mcp::ToolReply {
    match state {
        CommitState::Committed { sha, message } => Ok(format!("The user committed it as {} with this message:\n\n{message}", short(sha))),
        CommitState::Declined => Err("The user declined the commit. Nothing was committed; ask them what to change if it isn't clear.".into()),
        CommitState::Failed(e) => Err(format!("The commit failed: {e}")),
        _ => Err("The commit is still waiting for the user.".into()),
    }
}

/// How it ended, for the saved conversation.
pub(crate) fn outcome(state: &CommitState) -> String {
    match state {
        CommitState::Committed { sha, .. } => format!("committed as {}", short(sha)),
        CommitState::Declined => "declined".into(),
        CommitState::Failed(e) => format!("failed: {e}"),
        _ => "not decided".into(),
    }
}

pub(crate) fn short(sha: &str) -> String {
    sha.chars().take(7).collect()
}

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output().context("cannot run git; is it installed?")?;
    anyhow::ensure!(out.status.success(), "{}", stderr(&out));
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn stderr(out: &std::process::Output) -> String {
    let text = String::from_utf8_lossy(&out.stderr).trim().to_string();
    let text = if text.is_empty() { String::from_utf8_lossy(&out.stdout).trim().to_string() } else { text };
    text.trim_start_matches("fatal: ").trim_start_matches("error: ").to_string()
}

/// The repository's top folder for `dir`.
pub(crate) fn top_level(dir: &Path) -> Result<PathBuf> {
    Ok(PathBuf::from(git(dir, &["rev-parse", "--show-toplevel"])?.trim()))
}

pub(crate) fn head(dir: &Path) -> Option<String> {
    git(dir, &["rev-parse", "HEAD"]).ok().map(|s| s.trim().to_string())
}

/// The files to commit, absolute: those named (relative to `root`), or every change.
pub(crate) fn resolve_files(root: &Path, files: &[String]) -> Result<Vec<PathBuf>> {
    if !files.is_empty() {
        return Ok(files.iter().map(|f| root.join(f)).collect());
    }
    let top = top_level(root)?;
    // `-z`: paths as they are, NUL-separated; renames carry their old path next.
    let status = git(root, &["status", "--porcelain=v1", "-z", "--untracked-files=all"])?;
    let mut files = Vec::new();
    let mut entries = status.split('\0').filter(|e| !e.is_empty());
    while let Some(entry) = entries.next() {
        let (code, path) = entry.split_at(3.min(entry.len()));
        files.push(top.join(path));
        if code.starts_with('R') || code.starts_with('C') {
            entries.next();
        }
    }
    anyhow::ensure!(!files.is_empty(), "there are no changes to commit");
    Ok(files)
}

/// Makes the index hold exactly `files` as they are on disk (new, changed or deleted).
pub(crate) fn stage_only(root: &Path, files: &[PathBuf]) -> Result<()> {
    // Nothing to reset before the first commit.
    if head(root).is_some() {
        git(root, &["reset", "-q"])?;
    }
    let mut args = vec!["add", "-A", "--"];
    let paths: Vec<String> = files.iter().map(|f| f.to_string_lossy().into_owned()).collect();
    args.extend(paths.iter().map(String::as_str));
    git(root, &args)?;
    Ok(())
}

/// Commits `files` with `message`; returns the new commit's SHA and its final message.
pub(crate) fn commit(root: &Path, files: &[PathBuf], message: &str) -> Result<(String, String)> {
    stage_only(root, files)?;
    let mut child = Command::new("git").arg("-C").arg(root).args(["commit", "-q", "-F", "-"]).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().context("cannot run git; is it installed?")?;
    {
        use std::io::Write as _;
        child.stdin.take().context("no stdin")?.write_all(message.as_bytes())?;
    }
    let out = child.wait_with_output()?;
    anyhow::ensure!(out.status.success(), "{}", stderr(&out));
    last_commit(root)
}

/// HEAD's SHA and message.
pub(crate) fn last_commit(root: &Path) -> Result<(String, String)> {
    let sha = head(root).context("no commit")?;
    let message = git(root, &["log", "-1", "--format=%B"])?.trim().to_string();
    Ok((sha, message))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git(root, &["init", "-q"]).unwrap();
        git(root, &["config", "user.email", "t@t"]).unwrap();
        git(root, &["config", "user.name", "t"]).unwrap();
        git(root, &["config", "commit.gpgsign", "false"]).unwrap();
        dir
    }

    #[test]
    fn commits_only_the_proposed_files() {
        let dir = repo();
        let root = dir.path();
        std::fs::write(root.join("a.txt"), "a\n").unwrap();
        std::fs::write(root.join("b.txt"), "b\n").unwrap();
        // The first commit: nothing to reset yet.
        let (first, _) = commit(root, &[root.join("a.txt")], "Add a").unwrap();
        assert_eq!(git(root, &["show", "--name-only", "--format=", &first]).unwrap().trim(), "a.txt");

        // Already staged files that weren't proposed stay out.
        std::fs::write(root.join("a.txt"), "a2\n").unwrap();
        git(root, &["add", "b.txt"]).unwrap();
        let (sha, message) = commit(root, &[root.join("a.txt")], "Change a\n\nWith a body.").unwrap();
        assert_eq!(message, "Change a\n\nWith a body.");
        assert_eq!(git(root, &["show", "--name-only", "--format=", &sha]).unwrap().trim(), "a.txt");
        assert_eq!(git(root, &["status", "--porcelain"]).unwrap().trim(), "?? b.txt");
    }

    #[test]
    fn resolves_every_change_when_no_files_are_named() {
        let dir = repo();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(root.join("a.txt"), "a\n").unwrap();
        commit(&root, &[root.join("a.txt")], "Add a").unwrap();
        assert!(resolve_files(&root, &[]).is_err(), "nothing to commit");

        std::fs::remove_file(root.join("a.txt")).unwrap();
        std::fs::create_dir(root.join("src")).unwrap();
        std::fs::write(root.join("src/new file.rs"), "x").unwrap();
        let mut files = resolve_files(&root, &[]).unwrap();
        files.sort();
        assert_eq!(files, vec![root.join("a.txt"), root.join("src/new file.rs")]);
        assert_eq!(resolve_files(&root, &["src/x.rs".into()]).unwrap(), vec![root.join("src/x.rs")]);

        // Deletions commit too.
        let (sha, _) = commit(&root, &files, "Replace a").unwrap();
        assert_eq!(git(&root, &["show", "--name-status", "--format=", &sha]).unwrap().trim(), "D\ta.txt\nA\tsrc/new file.rs");
    }

    #[test]
    fn reports_failures() {
        let dir = repo();
        let err = commit(dir.path(), &[dir.path().join("missing.txt")], "Nope").unwrap_err();
        assert!(format!("{err:#}").contains("missing.txt"), "{err:#}");
    }
}
