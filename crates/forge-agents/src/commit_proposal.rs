//! Commits the agent proposes (`propose_commit`, or a `git commit` it tried to run): a card
//! in the thread with the message and the files. The user commits them from there, takes
//! them to the Git panel to edit the message first, or declines; the agent hears which.
//! The card signs the commit off when the project does (`forge_git::signoff`).
//!
//! Git runs in the thread's folder (its worktree, when it has one) with the user's own
//! `git`, so hooks and signing behave as in a terminal; Zed's Git panel picks the commit
//! up from the repository.

use std::path::{Path, PathBuf};
use std::process::Stdio;

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

/// Where a thread's git commands run: here, or on a remote project's host.
#[derive(Clone)]
pub(crate) enum Git {
    Here,
    Host(forge_host::Blocking),
}

impl Git {
    pub(crate) fn of(project: &gpui::WeakEntity<project::Project>, cx: &gpui::App) -> Self {
        let host = project.upgrade().and_then(|project| forge_host::Host::of(&project, cx));
        match host.and_then(|host| host.blocking(forge_host::files::GIT, cx).ok()) {
            Some(call) => Self::Host(call),
            None => Self::Here,
        }
    }

    /// `git` in `dir`: its output, or why it failed.
    pub(crate) fn run(&self, dir: &Path, args: &[&str]) -> Result<String> {
        self.run_with_input(dir, args, None)
    }

    /// Same, with `input` on git's stdin.
    fn run_with_input(&self, dir: &Path, args: &[&str], input: Option<&str>) -> Result<String> {
        match (self, input) {
            (Self::Here, None) => forge_process::git(dir, args),
            (Self::Here, Some(input)) => {
                let mut child = ide_api::std_command("git")
                    .arg("-C")
                    .arg(dir)
                    .args(args)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .context("cannot run git; is it installed?")?;
                {
                    use std::io::Write as _;
                    child.stdin.take().context("no stdin")?.write_all(input.as_bytes())?;
                }
                let out = child.wait_with_output()?;
                anyhow::ensure!(out.status.success(), "{}", stderr(&out));
                Ok(String::from_utf8_lossy(&out.stdout).into_owned())
            }
            (Self::Host(call), input) => call.call(&forge_host::files::GitParams { dir: dir.to_path_buf(), args: args.iter().map(|a| a.to_string()).collect(), input: input.map(str::to_string) }),
        }
    }

    /// Who commits in `dir` ("Name <email>"), as git says.
    pub(crate) fn signer(&self, dir: &Path) -> Option<String> {
        match self {
            Self::Here => forge_git::signoff::signer(dir),
            Self::Host(_) => forge_git::signoff::ident(&self.run(dir, &["var", "GIT_COMMITTER_IDENT"]).ok()?),
        }
    }
}

fn stderr(out: &std::process::Output) -> String {
    let text = String::from_utf8_lossy(&out.stderr).trim().to_string();
    let text = if text.is_empty() { String::from_utf8_lossy(&out.stdout).trim().to_string() } else { text };
    text.trim_start_matches("fatal: ").trim_start_matches("error: ").to_string()
}

/// The repository's top folder for `dir`.
pub(crate) fn top_level(git: &Git, dir: &Path) -> Result<PathBuf> {
    Ok(PathBuf::from(git.run(dir, &["rev-parse", "--show-toplevel"])?.trim()))
}

pub(crate) fn head(git: &Git, dir: &Path) -> Option<String> {
    git.run(dir, &["rev-parse", "HEAD"]).ok().map(|s| s.trim().to_string())
}

/// The files to commit, absolute: those named (relative to `root`), or every change.
pub(crate) fn resolve_files(git: &Git, root: &Path, files: &[String]) -> Result<Vec<PathBuf>> {
    if !files.is_empty() {
        return Ok(files.iter().map(|f| root.join(f)).collect());
    }
    let top = top_level(git, root)?;
    // `-z`: paths as they are, NUL-separated; renames carry their old path next.
    let status = git.run(root, &["status", "--porcelain=v1", "-z", "--untracked-files=all"])?;
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

fn paths(files: &[PathBuf]) -> Vec<String> {
    files.iter().map(|f| f.to_string_lossy().into_owned()).collect()
}

/// Stages `files` as they are on disk (new, changed or deleted).
fn stage(git: &Git, root: &Path, files: &[PathBuf]) -> Result<()> {
    let paths = paths(files);
    let mut args = vec!["add", "-A", "--"];
    args.extend(paths.iter().map(String::as_str));
    git.run(root, &args)?;
    Ok(())
}

/// Makes the index hold exactly `files` as they are on disk, for the Git panel to commit
/// them (the user sees what else was staged come off there).
pub(crate) fn stage_only(git: &Git, root: &Path, files: &[PathBuf]) -> Result<()> {
    // Nothing to reset before the first commit.
    if head(git, root).is_some() {
        git.run(root, &["reset", "-q"])?;
    }
    stage(git, root, files)
}

/// Commits `files` with `message`, signed off as the user when `signoff`; returns the new
/// commit's SHA and its final message. Whatever else the user had staged stays staged.
pub(crate) fn commit(git: &Git, root: &Path, files: &[PathBuf], message: &str, signoff: bool) -> Result<(String, String)> {
    // Staged first so new and deleted files are known to `--only`.
    stage(git, root, files)?;
    let paths = paths(files);
    let mut args = vec!["commit", "-q", "-F", "-"];
    if signoff {
        args.push("--signoff");
    }
    args.extend(["--only", "--"]);
    args.extend(paths.iter().map(String::as_str));
    git.run_with_input(root, &args, Some(message))?;
    last_commit(git, root)
}

/// HEAD's SHA and message.
pub(crate) fn last_commit(git: &Git, root: &Path) -> Result<(String, String)> {
    let sha = head(git, root).context("no commit")?;
    let message = git.run(root, &["log", "-1", "--format=%B"])?.trim().to_string();
    Ok((sha, message))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) -> Result<String> {
        Git::Here.run(dir, args)
    }

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
        let (first, _) = commit(&Git::Here, root, &[root.join("a.txt")], "Add a", false).unwrap();
        assert_eq!(git(root, &["show", "--name-only", "--format=", &first]).unwrap().trim(), "a.txt");

        // Already staged files that weren't proposed stay out, and stay staged.
        std::fs::write(root.join("a.txt"), "a2\n").unwrap();
        git(root, &["add", "b.txt"]).unwrap();
        let (sha, message) = commit(&Git::Here, root, &[root.join("a.txt")], "Change a\n\nWith a body.", false).unwrap();
        assert_eq!(message, "Change a\n\nWith a body.");
        assert_eq!(git(root, &["show", "--name-only", "--format=", &sha]).unwrap().trim(), "a.txt");
        assert_eq!(git(root, &["status", "--porcelain"]).unwrap().trim(), "A  b.txt");
    }

    #[test]
    fn staging_for_the_panel_holds_only_the_proposed_files() {
        let dir = repo();
        let root = dir.path();
        std::fs::write(root.join("a.txt"), "a\n").unwrap();
        commit(&Git::Here, root, &[root.join("a.txt")], "Add a", false).unwrap();
        std::fs::write(root.join("a.txt"), "a2\n").unwrap();
        std::fs::write(root.join("b.txt"), "b\n").unwrap();
        git(root, &["add", "b.txt"]).unwrap();
        stage_only(&Git::Here, root, &[root.join("a.txt")]).unwrap();
        assert_eq!(git(root, &["diff", "--cached", "--name-only"]).unwrap().trim(), "a.txt");
    }

    #[test]
    fn signs_off_as_the_user() {
        let dir = repo();
        let root = dir.path();
        std::fs::write(root.join("a.txt"), "a\n").unwrap();
        let (_, message) = commit(&Git::Here, root, &[root.join("a.txt")], "Add a", true).unwrap();
        assert_eq!(message, "Add a\n\nSigned-off-by: t <t@t>");
    }

    #[test]
    fn resolves_every_change_when_no_files_are_named() {
        let dir = repo();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(root.join("a.txt"), "a\n").unwrap();
        commit(&Git::Here, &root, &[root.join("a.txt")], "Add a", false).unwrap();
        assert!(resolve_files(&Git::Here, &root, &[]).is_err(), "nothing to commit");

        std::fs::remove_file(root.join("a.txt")).unwrap();
        std::fs::create_dir(root.join("src")).unwrap();
        std::fs::write(root.join("src/new file.rs"), "x").unwrap();
        let mut files = resolve_files(&Git::Here, &root, &[]).unwrap();
        files.sort();
        assert_eq!(files, vec![root.join("a.txt"), root.join("src/new file.rs")]);
        assert_eq!(resolve_files(&Git::Here, &root, &["src/x.rs".into()]).unwrap(), vec![root.join("src/x.rs")]);

        // Deletions commit too.
        let (sha, _) = commit(&Git::Here, &root, &files, "Replace a", false).unwrap();
        assert_eq!(git(&root, &["show", "--name-status", "--format=", &sha]).unwrap().trim(), "D\ta.txt\nA\tsrc/new file.rs");
    }

    #[test]
    fn reports_failures() {
        let dir = repo();
        let err = commit(&Git::Here, dir.path(), &[dir.path().join("missing.txt")], "Nope", false).unwrap_err();
        assert!(format!("{err:#}").contains("missing.txt"), "{err:#}");
    }
}
