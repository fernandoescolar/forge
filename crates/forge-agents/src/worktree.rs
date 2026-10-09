//! Threads that work in a git worktree of their own, so an agent never touches the files
//! you are editing, nor another thread's: `.forge/worktrees/<name>` on branch
//! `forge/<name>`, started from the project's last commit. When the agent is done, its
//! changes (committed or not) are applied to the project as uncommitted changes to review,
//! and the worktree can go.
//!
//! The worktree lives inside the project, so the agent's reads and writes stay within it
//! (and go through the editor like any other thread's); git is told to ignore the folder.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, anyhow, bail};

/// Where worktrees go, relative to the repository's top folder.
pub const DIR: &str = ".forge/worktrees";
const BRANCH_PREFIX: &str = "forge/";

#[derive(Clone, Debug, PartialEq)]
pub struct AgentWorktree {
    /// The repository's top folder (the project).
    pub repo: PathBuf,
    pub path: PathBuf,
    pub name: String,
}

impl AgentWorktree {
    pub fn branch(&self) -> String {
        format!("{BRANCH_PREFIX}{}", self.name)
    }
}

#[derive(Debug, PartialEq)]
pub enum Applied {
    /// Every change is in the project now.
    Clean { files: usize },
    /// Applied with conflict markers in these files.
    Conflicts(Vec<String>),
}

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let output = ide_api::std_command("git").arg("-C").arg(dir).args(args).output().context("cannot run git; is it installed?")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git {} failed: {}", args.join(" "), stderr.trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The repository that holds `dir`.
pub fn repo_root(dir: &Path) -> Result<PathBuf> {
    let top = git(dir, &["rev-parse", "--show-toplevel"]).map_err(|_| anyhow!("{} is not in a git repository", dir.display()))?;
    Ok(PathBuf::from(top.trim()))
}

/// Creates a worktree for a new thread in the repository holding `dir`.
pub fn create(dir: &Path) -> Result<AgentWorktree> {
    let repo = repo_root(dir)?;
    git(&repo, &["rev-parse", "--verify", "HEAD"]).map_err(|_| anyhow!("the repository has no commits yet; commit once to start threads in worktrees"))?;
    ignore_worktrees(&repo)?;
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    let (name, path) = (1..100)
        .map(|n| if n == 1 { format!("thread-{stamp}") } else { format!("thread-{stamp}-{n}") })
        .map(|name| (name.clone(), repo.join(DIR).join(&name)))
        .find(|(name, path)| !path.exists() && git(&repo, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{BRANCH_PREFIX}{name}")]).is_err())
        .context("no free worktree name")?;
    let worktree = AgentWorktree { repo: repo.clone(), path: path.clone(), name };
    git(&repo, &["worktree", "add", "-b", &worktree.branch(), &path.to_string_lossy(), "HEAD"])?;
    Ok(worktree)
}

/// Keeps worktrees out of the project's own git status (`.git/info/exclude`).
fn ignore_worktrees(repo: &Path) -> Result<()> {
    let exclude = git(repo, &["rev-parse", "--git-path", "info/exclude"])?;
    let exclude = repo.join(exclude.trim());
    let line = format!("/{DIR}/");
    let current = std::fs::read_to_string(&exclude).unwrap_or_default();
    if current.lines().any(|l| l.trim() == line) {
        return Ok(());
    }
    if let Some(dir) = exclude.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let separator = if current.is_empty() || current.ends_with('\n') { "" } else { "\n" };
    std::fs::write(&exclude, format!("{current}{separator}{line}\n"))?;
    Ok(())
}

/// The thread worktrees of the repository holding `dir`.
pub fn list(dir: &Path) -> Vec<AgentWorktree> {
    let Ok(repo) = repo_root(dir) else { return vec![] };
    let Ok(porcelain) = git(&repo, &["worktree", "list", "--porcelain"]) else { return vec![] };
    let base = repo.join(DIR);
    porcelain
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .map(PathBuf::from)
        .filter(|path| path.parent() == Some(base.as_path()))
        .filter_map(|path| Some(AgentWorktree { repo: repo.clone(), name: path.file_name()?.to_string_lossy().into_owned(), path }))
        .collect()
}

/// Where the worktree's branch left the project's: its changes are measured from here.
fn fork_point(worktree: &AgentWorktree) -> Result<String> {
    Ok(git(&worktree.repo, &["merge-base", "HEAD", &worktree.branch()])?.trim().to_string())
}

/// The files the agent changed since the worktree started (committed or not), as
/// `git diff --name-status` lines.
pub fn changed_files(worktree: &AgentWorktree) -> Result<Vec<String>> {
    let base = fork_point(worktree)?;
    git(&worktree.path, &["add", "-A"])?;
    let names = git(&worktree.path, &["diff", "--cached", "--name-status", &base])?;
    Ok(names.lines().filter(|l| !l.trim().is_empty()).map(|l| l.replace('\t', " ")).collect())
}

/// Applies the worktree's changes to the project, as uncommitted changes. Applies the
/// whole patch when it fits cleanly; otherwise merges each file three ways (from where the
/// worktree started, with your file as it is now, uncommitted changes included, and the
/// agent's), leaving conflict markers where both changed the same lines. Git's index is
/// never touched.
pub fn apply(worktree: &AgentWorktree) -> Result<Applied> {
    let base = fork_point(worktree)?;
    git(&worktree.path, &["add", "-A"])?;
    let patch = git(&worktree.path, &["diff", "--cached", "--binary", &base])?;
    if patch.trim().is_empty() {
        return Ok(Applied::Clean { files: 0 });
    }
    let changes: Vec<(char, String)> = git(&worktree.path, &["diff", "--cached", "--no-renames", "--name-status", &base])?
        .lines()
        .filter_map(|line| {
            let (status, path) = line.split_once('\t')?;
            Some((status.chars().next()?, path.to_string()))
        })
        .collect();
    let patch_file = tempfile::NamedTempFile::new()?;
    std::fs::write(patch_file.path(), &patch)?;
    let patch_path = patch_file.path().to_string_lossy().into_owned();
    if git(&worktree.repo, &["apply", "--check", &patch_path]).is_ok() {
        git(&worktree.repo, &["apply", &patch_path])?;
        return Ok(Applied::Clean { files: changes.len() });
    }
    let mut conflicts = Vec::new();
    for (status, path) in &changes {
        if merge_file(worktree, &base, *status, path)? {
            conflicts.push(path.clone());
        }
    }
    Ok(if conflicts.is_empty() { Applied::Clean { files: changes.len() } } else { Applied::Conflicts(conflicts) })
}

/// Brings the agent's version of `path` into the project, merging with yours; returns
/// whether it left a conflict.
fn merge_file(worktree: &AgentWorktree, base_commit: &str, status: char, path: &str) -> Result<bool> {
    let target = worktree.repo.join(path);
    let theirs = (status != 'D').then(|| std::fs::read(worktree.path.join(path))).transpose()?;
    let base = if status == 'A' { None } else { show(&worktree.repo, base_commit, path) };
    let ours = std::fs::read(&target).ok();
    let write = |bytes: &[u8]| -> Result<()> {
        if let Some(dir) = target.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&target, bytes).with_context(|| format!("cannot write {}", target.display()))
    };
    match (ours, theirs) {
        // You left the file as it was (or don't have it and the agent added it): take theirs.
        (ours, Some(theirs)) if ours == base => write(&theirs).map(|_| false),
        (Some(ours), Some(theirs)) if ours == theirs => Ok(false),
        // The agent deleted it.
        (ours, None) if ours == base => {
            if ours.is_some() {
                std::fs::remove_file(&target)?;
            }
            Ok(false)
        }
        // You changed (or deleted) a file the agent deleted (or changed): keep yours.
        (_, None) | (None, Some(_)) => Ok(true),
        (Some(ours), Some(theirs)) => {
            let base = base.unwrap_or_default();
            if [&ours, &theirs, &base].iter().any(|bytes| bytes.contains(&0)) {
                return Ok(true); // binary: yours stays
            }
            let dir = tempfile::tempdir()?;
            let (ours_file, base_file, theirs_file) = (dir.path().join("yours"), dir.path().join("base"), dir.path().join("agent"));
            std::fs::write(&ours_file, &ours)?;
            std::fs::write(&base_file, &base)?;
            std::fs::write(&theirs_file, &theirs)?;
            let agent = format!("agent ({})", worktree.branch());
            let output = ide_api::std_command("git")
                .args(["merge-file", "-p", "-L", "yours", "-L", "base", "-L", &agent])
                .args([&ours_file, &base_file, &theirs_file])
                .output()
                .context("cannot run git merge-file")?;
            // Exit code: the number of conflicts, or negative on errors.
            let conflicts = output.status.code().context("git merge-file was killed")?;
            if conflicts < 0 {
                bail!("git merge-file failed on {path}: {}", String::from_utf8_lossy(&output.stderr).trim());
            }
            write(&output.stdout)?;
            Ok(conflicts > 0)
        }
    }
}

/// `path` as it was in `commit`, if it existed.
fn show(repo: &Path, commit: &str, path: &str) -> Option<Vec<u8>> {
    let output = ide_api::std_command("git").arg("-C").arg(repo).args(["show", &format!("{commit}:{path}")]).output().ok()?;
    output.status.success().then_some(output.stdout)
}

/// Removes the worktree (its uncommitted changes go with it), and its branch unless `keep_branch`.
pub fn remove(worktree: &AgentWorktree, keep_branch: bool) -> Result<()> {
    git(&worktree.repo, &["worktree", "remove", "--force", &worktree.path.to_string_lossy()])?;
    if !keep_branch {
        git(&worktree.repo, &["branch", "-D", &worktree.branch()])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| git(dir.path(), args).unwrap();
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.name", "Test"]);
        run(&["config", "user.email", "test@example.com"]);
        std::fs::write(dir.path().join("lib.txt"), (1..=20).map(|n| format!("line {n}\n")).collect::<String>()).unwrap();
        run(&["add", "."]);
        run(&["commit", "-qm", "init"]);
        dir
    }

    #[test]
    fn a_thread_worktree_from_start_to_finish() {
        let dir = repo();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let wt = create(&root.join(".")).unwrap();
        assert!(wt.path.starts_with(root.join(DIR)) && wt.path.join("lib.txt").is_file());
        assert_eq!(list(&root), [wt.clone()]);
        assert_eq!(git(&root, &["status", "--porcelain"]).unwrap(), "", "the worktree folder is ignored");

        // The agent edits one line, adds a file and commits part of it; you edit another line.
        let lib = |dir: &Path| std::fs::read_to_string(dir.join("lib.txt")).unwrap();
        std::fs::write(wt.path.join("lib.txt"), lib(&wt.path).replace("line 2\n", "line two\n")).unwrap();
        git(&wt.path, &["commit", "-qam", "agent"]).unwrap();
        std::fs::write(wt.path.join("new.txt"), "new\n").unwrap();
        std::fs::write(root.join("lib.txt"), lib(&root).replace("line 18\n", "line eighteen\n")).unwrap();
        assert_eq!(changed_files(&wt).unwrap(), ["M lib.txt", "A new.txt"]);

        assert_eq!(apply(&wt).unwrap(), Applied::Clean { files: 2 });
        assert!(lib(&root).contains("line two\n") && lib(&root).contains("line eighteen\n"), "both changes: {}", lib(&root));
        assert!(root.join("new.txt").is_file());

        remove(&wt, false).unwrap();
        assert!(!wt.path.exists() && list(&root).is_empty());
        assert!(git(&root, &["rev-parse", "--verify", "--quiet", "refs/heads/forge/x"]).is_err());
    }

    #[test]
    fn conflicting_changes_leave_markers() {
        let dir = repo();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let wt = create(&root).unwrap();
        std::fs::write(wt.path.join("lib.txt"), "agent\n").unwrap();
        std::fs::write(root.join("lib.txt"), "you\n").unwrap();
        git(&root, &["commit", "-qam", "yours"]).unwrap();
        let Applied::Conflicts(files) = apply(&wt).unwrap() else { panic!("expected conflicts") };
        assert_eq!(files, ["lib.txt"]);
        assert!(std::fs::read_to_string(root.join("lib.txt")).unwrap().contains("<<<<<<<"));
        remove(&wt, true).unwrap();
        assert!(git(&root, &["rev-parse", "--verify", &wt.branch()]).is_ok(), "the branch stays");
    }

    #[test]
    fn merges_with_your_uncommitted_changes() {
        let dir = repo();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let wt = create(&root).unwrap();
        let lib = |dir: &Path| std::fs::read_to_string(dir.join("lib.txt")).unwrap();
        // The agent changes lines 1 and 10; you, uncommitted, change lines 5 and 10. (Git sees
        // changes to adjacent lines as one conflict, as `git merge` does.)
        std::fs::write(wt.path.join("lib.txt"), lib(&wt.path).replace("line 1\n", "line one\n").replace("line 10\n", "agent ten\n")).unwrap();
        std::fs::write(wt.path.join("new.txt"), "new\n").unwrap();
        std::fs::write(root.join("lib.txt"), lib(&root).replace("line 5\n", "line five\n").replace("line 10\n", "your ten\n")).unwrap();
        std::fs::write(root.join("notes.txt"), "yours\n").unwrap();

        let Applied::Conflicts(files) = apply(&wt).unwrap() else { panic!("line 10 conflicts") };
        assert_eq!(files, ["lib.txt"]);
        let merged = lib(&root);
        assert!(merged.starts_with("line one\n") && merged.contains("line five\n"), "both sides' separate changes are in: {merged}");
        assert!(merged.contains("<<<<<<< yours\nyour ten\n=======\nagent ten\n>>>>>>> agent (forge/"), "{merged}");
        assert_eq!(std::fs::read_to_string(root.join("new.txt")).unwrap(), "new\n");
        assert_eq!(std::fs::read_to_string(root.join("notes.txt")).unwrap(), "yours\n", "your other files are untouched");
        assert_eq!(git(&root, &["diff", "--cached", "--name-only"]).unwrap(), "", "the index is untouched");
    }

    #[test]
    fn keeps_your_version_when_the_agent_deleted_what_you_changed() {
        let dir = repo();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let wt = create(&root).unwrap();
        std::fs::remove_file(wt.path.join("lib.txt")).unwrap();
        std::fs::write(wt.path.join("other.txt"), "x\n").unwrap();
        std::fs::write(root.join("lib.txt"), "changed\n").unwrap();
        std::fs::write(root.join("other.txt"), "y\n").unwrap();
        assert_eq!(apply(&wt).unwrap(), Applied::Conflicts(vec!["lib.txt".into(), "other.txt".into()]));
        assert_eq!(std::fs::read_to_string(root.join("lib.txt")).unwrap(), "changed\n");
    }

    #[test]
    fn needs_a_repository_with_commits() {
        let dir = tempfile::tempdir().unwrap();
        assert!(create(dir.path()).unwrap_err().to_string().contains("not in a git repository"));
        git(dir.path(), &["init", "-q"]).unwrap();
        assert!(create(dir.path()).unwrap_err().to_string().contains("no commits yet"));
    }
}
