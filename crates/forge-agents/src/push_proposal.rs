//! Pushes the agent proposes (`propose_push`, or a `git push` it tried to run): a card in
//! the thread with the branch, where it goes and the commits it takes. The user pushes from
//! there, through Zed's repository (credentials are asked as in the Git panel), or declines.
//!
//! Only the thread's current branch, never forced.

use std::path::Path;

use anyhow::{Context as _, Result};

/// What a push would do.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PushPlan {
    pub branch: String,
    pub remote: String,
    pub remote_branch: String,
    /// The branch has no upstream yet: the push sets it.
    pub set_upstream: bool,
    /// Commits the remote doesn't have, newest first: short SHA and subject.
    pub commits: Vec<(String, String)>,
}

impl PushPlan {
    pub fn target(&self) -> String {
        format!("{}/{}", self.remote, self.remote_branch)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum PushState {
    Waiting,
    Pushing,
    Pushed,
    Declined,
    Failed(String),
}

impl PushState {
    pub fn is_open(&self) -> bool {
        matches!(self, Self::Waiting | Self::Failed(_))
    }
}

/// What the agent is told.
pub(crate) fn report(plan: &PushPlan, state: &PushState) -> crate::forge_mcp::ToolReply {
    match state {
        PushState::Pushed => Ok(format!(
            "The user pushed {} to {}{}.",
            plural(plan.commits.len(), "commit"),
            plan.target(),
            if plan.set_upstream { " (now its upstream)" } else { "" }
        )),
        PushState::Declined => Err("The user declined the push. Nothing was pushed; ask them what to change if it isn't clear.".into()),
        PushState::Failed(e) => Err(format!("The push failed: {e}")),
        _ => Err("The push is still waiting for the user.".into()),
    }
}

/// How it ended, for the saved conversation.
pub(crate) fn outcome(plan: &PushPlan, state: &PushState) -> String {
    match state {
        PushState::Pushed => format!("pushed to {}", plan.target()),
        PushState::Declined => "declined".into(),
        PushState::Failed(e) => format!("failed: {e}"),
        _ => "not decided".into(),
    }
}

pub(crate) fn plural(n: usize, word: &str) -> String {
    format!("{n} {word}{}", if n == 1 { "" } else { "s" })
}

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = ide_api::std_command("git").arg("-C").arg(dir).args(args).output().context("cannot run git; is it installed?")?;
    anyhow::ensure!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr).trim().trim_start_matches("fatal: "));
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The push of the current branch in `dir`: to its upstream, or to `remote` (the branch's
/// push remote, `origin` or the only remote when not given) as a new branch.
pub(crate) fn plan(dir: &Path, remote: Option<&str>) -> Result<PushPlan> {
    let branch = git(dir, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    anyhow::ensure!(branch != "HEAD", "HEAD is detached: there is no branch to push");
    let remotes: Vec<String> = git(dir, &["remote"])?.lines().map(str::to_string).collect();
    anyhow::ensure!(!remotes.is_empty(), "the repository has no remote");
    // `origin/main` → (`origin`, `main`); remote names have no slash in practice.
    let upstream = git(dir, &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"]).ok().and_then(|u| u.split_once('/').map(|(r, b)| (r.to_string(), b.to_string())));
    let (remote, remote_branch, set_upstream) = match (remote, upstream) {
        (Some(asked), Some((r, b))) if asked == r => (r, b, false),
        (None, Some((r, b))) => (r, b, false),
        (asked, _) => {
            if let Some(asked) = asked {
                anyhow::ensure!(remotes.iter().any(|r| r == asked), "the repository has no remote named {asked}");
            }
            let configured = git(dir, &["config", &format!("branch.{branch}.pushRemote")]).ok();
            let remote = asked
                .map(str::to_string)
                .or(configured)
                .filter(|r| remotes.contains(r))
                .or_else(|| remotes.iter().find(|r| *r == "origin").cloned())
                .or_else(|| (remotes.len() == 1).then(|| remotes[0].clone()))
                .context("the branch has no upstream and there is no `origin` remote to push to")?;
            (remote, branch.clone(), true)
        }
    };
    let remote_ref = format!("refs/remotes/{remote}/{remote_branch}");
    let range = if git(dir, &["rev-parse", "--verify", "-q", &remote_ref]).is_ok() { format!("{remote_ref}..HEAD") } else { "HEAD".into() };
    let mut args = vec!["log", "-50", "--format=%h%x09%s", range.as_str()];
    let not_remote = format!("--remotes={remote}");
    if range == "HEAD" {
        // A new branch: what the remote has under any branch is already there.
        args.extend(["--not", not_remote.as_str()]);
    }
    let commits: Vec<(String, String)> = git(dir, &args)?.lines().filter_map(|l| l.split_once('\t')).map(|(h, s)| (h.to_string(), s.to_string())).collect();
    anyhow::ensure!(!commits.is_empty() || set_upstream, "{branch} is up to date with {remote}/{remote_branch}: there is nothing to push");
    Ok(PushPlan { branch, remote, remote_branch, set_upstream, commits })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(dir: &Path, args: &[&str]) {
        git(dir, args).unwrap();
    }

    fn commit(dir: &Path, file: &str, message: &str) {
        std::fs::write(dir.join(file), message).unwrap();
        run(dir, &["add", file]);
        run(dir, &["-c", "user.email=t@t", "-c", "user.name=t", "-c", "commit.gpgsign=false", "commit", "-q", "-m", message]);
    }

    #[test]
    fn plans_pushes_to_the_upstream_or_a_new_branch() {
        let remote = tempfile::tempdir().unwrap();
        run(remote.path(), &["init", "-q", "--bare"]);
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        run(root, &["init", "-q", "-b", "main"]);
        commit(root, "a.txt", "Start");
        assert!(format!("{:#}", plan(root, None).unwrap_err()).contains("no remote"));

        run(root, &["remote", "add", "origin", &remote.path().to_string_lossy()]);
        run(root, &["push", "-q", "-u", "origin", "main"]);
        assert!(format!("{:#}", plan(root, None).unwrap_err()).contains("nothing to push"));

        commit(root, "b.txt", "Add b");
        commit(root, "c.txt", "Add c");
        let p = plan(root, None).unwrap();
        assert_eq!((p.branch.as_str(), p.target(), p.set_upstream), ("main", "origin/main".to_string(), false));
        assert_eq!(p.commits.iter().map(|(_, s)| s.as_str()).collect::<Vec<_>>(), ["Add c", "Add b"]);

        // A branch without upstream: a new branch on origin, with only its own commits.
        run(root, &["push", "-q"]);
        run(root, &["checkout", "-q", "-b", "forge/fix"]);
        commit(root, "d.txt", "Fix d");
        let p = plan(root, None).unwrap();
        assert_eq!((p.target(), p.set_upstream), ("origin/forge/fix".to_string(), true));
        assert_eq!(p.commits.iter().map(|(_, s)| s.as_str()).collect::<Vec<_>>(), ["Fix d"]);
        assert!(format!("{:#}", plan(root, Some("upstream")).unwrap_err()).contains("no remote named upstream"));
    }

    #[test]
    fn reports_the_outcome() {
        let p = PushPlan { branch: "main".into(), remote: "origin".into(), remote_branch: "main".into(), set_upstream: false, commits: vec![("abc1234".into(), "Fix".into())] };
        assert_eq!(report(&p, &PushState::Pushed), Ok("The user pushed 1 commit to origin/main.".into()));
        assert!(report(&p, &PushState::Declined).is_err());
    }
}
