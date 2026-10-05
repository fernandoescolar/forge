//! GitHub through the `gh` command line tool: it already knows the user's accounts and
//! the repository's remote, so Forge never handles tokens.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use serde_json::Value;

/// The fields Forge reads of a pull request (`gh pr list/view --json`).
pub const FIELDS: &str = "number,title,author,headRefName,baseRefName,isDraft,url,reviewDecision,statusCheckRollup";

#[derive(Clone, Debug, PartialEq)]
pub struct PullRequest {
    pub number: u64,
    pub title: String,
    pub author: String,
    pub branch: String,
    pub base: String,
    pub draft: bool,
    pub url: String,
    /// `APPROVED`, `CHANGES_REQUESTED`, `REVIEW_REQUIRED`, or none.
    pub review: Option<String>,
    pub checks: Checks,
}

/// The pull request's checks (GitHub Actions runs and commit statuses), by outcome.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Checks {
    pub passed: usize,
    pub failed: usize,
    pub pending: usize,
    pub skipped: usize,
}

impl Checks {
    pub fn total(&self) -> usize {
        self.passed + self.failed + self.pending + self.skipped
    }

    /// "2 failing", "3 running", "all 5 passed", "no checks".
    pub fn summary(&self) -> String {
        match self {
            c if c.failed > 0 => format!("{} failing", c.failed),
            c if c.pending > 0 => format!("{} running", c.pending),
            c if c.passed > 0 => format!("all {} passed", c.passed),
            _ => "no checks".into(),
        }
    }
}

fn checks_of(rollup: Option<&Value>) -> Checks {
    let mut checks = Checks::default();
    for item in rollup.and_then(Value::as_array).into_iter().flatten() {
        let field = |k: &str| item.get(k).and_then(Value::as_str).unwrap_or_default().to_ascii_uppercase();
        // Check runs have a status and, once completed, a conclusion; statuses a state.
        let outcome = if item.get("__typename").and_then(Value::as_str) == Some("StatusContext") {
            field("state")
        } else if field("status") != "COMPLETED" {
            "PENDING".into()
        } else {
            field("conclusion")
        };
        match outcome.as_str() {
            "SUCCESS" | "NEUTRAL" => checks.passed += 1,
            "SKIPPED" | "STALE" => checks.skipped += 1,
            "PENDING" | "EXPECTED" | "QUEUED" | "IN_PROGRESS" | "WAITING" | "REQUESTED" | "" => checks.pending += 1,
            _ => checks.failed += 1,
        }
    }
    checks
}

fn pull_request(json: &Value) -> Option<PullRequest> {
    let text = |k: &str| json.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
    Some(PullRequest {
        number: json.get("number")?.as_u64()?,
        title: text("title"),
        author: json.pointer("/author/login").and_then(Value::as_str).unwrap_or_default().to_string(),
        branch: text("headRefName"),
        base: text("baseRefName"),
        draft: json.get("isDraft").and_then(Value::as_bool).unwrap_or(false),
        url: text("url"),
        review: Some(text("reviewDecision")).filter(|r| !r.is_empty()),
        checks: checks_of(json.get("statusCheckRollup")),
    })
}

pub fn parse_list(json: &str) -> Vec<PullRequest> {
    serde_json::from_str::<Value>(json).ok().and_then(|v| v.as_array().map(|prs| prs.iter().filter_map(pull_request).collect())).unwrap_or_default()
}

pub fn parse_view(json: &str) -> Option<PullRequest> {
    pull_request(&serde_json::from_str(json).ok()?)
}

/// A review comment, on a line of a file of the pull request.
#[derive(Clone, Debug, PartialEq)]
pub struct ReviewComment {
    pub id: u64,
    /// Relative to the repository.
    pub path: String,
    /// 1-based, in the pull request's latest version; `None` once the line is outdated.
    pub line: Option<u32>,
    pub author: String,
    pub body: String,
    /// The comment this one answers (threads are a first comment and its replies).
    pub reply_to: Option<u64>,
    pub url: String,
}

/// From `gh api repos/{owner}/{repo}/pulls/{n}/comments` (REST).
pub fn parse_comments(json: &str) -> Vec<ReviewComment> {
    let Ok(Value::Array(items)) = serde_json::from_str::<Value>(json) else { return vec![] };
    items
        .iter()
        .filter_map(|c| {
            Some(ReviewComment {
                id: c.get("id")?.as_u64()?,
                path: c.get("path")?.as_str()?.to_string(),
                line: c.get("line").and_then(Value::as_u64).map(|l| l as u32),
                author: c.pointer("/user/login").and_then(Value::as_str).unwrap_or_default().to_string(),
                body: c.get("body").and_then(Value::as_str).unwrap_or_default().to_string(),
                reply_to: c.get("in_reply_to_id").and_then(Value::as_u64),
                url: c.get("html_url").and_then(Value::as_str).unwrap_or_default().to_string(),
            })
        })
        .collect()
}

/// Comment threads: each first comment with its replies, in order.
pub fn threads(comments: &[ReviewComment]) -> Vec<Vec<ReviewComment>> {
    let mut threads: Vec<Vec<ReviewComment>> = Vec::new();
    for comment in comments {
        match comment.reply_to.and_then(|parent| threads.iter_mut().find(|t| t.iter().any(|c| c.id == parent))) {
            Some(thread) => thread.push(comment.clone()),
            None => threads.push(vec![comment.clone()]),
        }
    }
    threads
}

/// Runs `gh` in `dir` with `env` (the project's shell environment, so `gh` is found when
/// Forge was started from the Dock).
pub async fn run(dir: &Path, args: &[&str], env: &HashMap<String, String>) -> Result<String> {
    let program = env
        .get("PATH")
        .and_then(|path| std::env::split_paths(path).map(|d| d.join("gh")).find(|p| p.is_file()))
        .unwrap_or_else(|| PathBuf::from("gh"));
    let mut command = util::command::new_command(&program);
    command.args(args).envs(env.iter()).env("GH_PROMPT_DISABLED", "1").env("NO_COLOR", "1").current_dir(dir);
    let output = command.output().await.context("cannot run `gh`, the GitHub CLI; install it from https://cli.github.com")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("{}", stderr.trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The pull request of the branch checked out in `dir`, if it has one.
pub async fn current(dir: &Path, env: &HashMap<String, String>) -> Result<Option<PullRequest>> {
    match run(dir, &["pr", "view", "--json", FIELDS], env).await {
        Ok(json) => Ok(parse_view(&json)),
        Err(e) if e.to_string().contains("no pull requests found") => Ok(None),
        Err(e) => Err(e),
    }
}

pub async fn list(dir: &Path, env: &HashMap<String, String>) -> Result<Vec<PullRequest>> {
    Ok(parse_list(&run(dir, &["pr", "list", "--limit", "50", "--json", FIELDS], env).await?))
}

pub async fn comments(dir: &Path, number: u64, env: &HashMap<String, String>) -> Result<Vec<ReviewComment>> {
    let json = run(dir, &["api", "--paginate", "--slurp", &format!("repos/{{owner}}/{{repo}}/pulls/{number}/comments?per_page=100")], env).await?;
    // `--slurp` wraps the pages in an array.
    let pages: Vec<Value> = serde_json::from_str(&json).unwrap_or_default();
    Ok(pages.iter().flat_map(|page| parse_comments(&page.to_string())).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIST: &str = r#"[{"author":{"login":"ana"},"headRefName":"docs/nix","baseRefName":"trunk","isDraft":false,"number":14592,"reviewDecision":"REVIEW_REQUIRED",
        "statusCheckRollup":[
          {"__typename":"CheckRun","conclusion":"SUCCESS","name":"label","status":"COMPLETED"},
          {"__typename":"CheckRun","conclusion":"SKIPPED","name":"close","status":"COMPLETED"},
          {"__typename":"CheckRun","conclusion":"","name":"build","status":"IN_PROGRESS"},
          {"__typename":"CheckRun","conclusion":"FAILURE","name":"lint","status":"COMPLETED"},
          {"__typename":"StatusContext","state":"SUCCESS","context":"ci/legacy"}],
        "title":"docs: recommend nix-shell","url":"https://github.com/cli/cli/pull/14592"},
      {"author":{"login":"bo"},"headRefName":"x","isDraft":true,"number":7,"reviewDecision":"","statusCheckRollup":[],"title":"WIP","url":"u"}]"#;

    #[test]
    fn reads_pull_requests_and_their_checks() {
        let prs = parse_list(LIST);
        assert_eq!(prs.len(), 2);
        let pr = &prs[0];
        assert_eq!((pr.number, pr.author.as_str(), pr.branch.as_str(), pr.base.as_str()), (14592, "ana", "docs/nix", "trunk"));
        assert_eq!(pr.checks, Checks { passed: 2, failed: 1, pending: 1, skipped: 1 });
        assert_eq!(pr.checks.summary(), "1 failing");
        assert_eq!(pr.review.as_deref(), Some("REVIEW_REQUIRED"));
        assert!(prs[1].draft && prs[1].review.is_none());
        assert_eq!(prs[1].checks.summary(), "no checks");
        assert_eq!(Checks { passed: 3, pending: 1, ..Default::default() }.summary(), "1 running");
    }

    #[test]
    fn groups_review_comments_into_threads() {
        let json = r#"[
          {"id":1,"path":"src/a.rs","line":10,"body":"Why?","user":{"login":"ana"},"html_url":"u1"},
          {"id":2,"path":"src/b.rs","line":null,"original_line":4,"body":"Old","user":{"login":"bo"},"html_url":"u2"},
          {"id":3,"path":"src/a.rs","line":10,"body":"Because.","user":{"login":"bo"},"in_reply_to_id":1,"html_url":"u3"}]"#;
        let comments = parse_comments(json);
        assert_eq!(comments[1].line, None, "outdated");
        let threads = threads(&comments);
        assert_eq!(threads.iter().map(|t| t.iter().map(|c| c.id).collect::<Vec<_>>()).collect::<Vec<_>>(), [vec![1, 3], vec![2]]);
    }
}

#[cfg(test)]
mod real {
    use super::*;

    /// Against a real clone: `FORGE_GITHUB_REPO=/path/to/clone cargo test -p forge-github -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn reads_a_real_repository() {
        let Ok(dir) = std::env::var("FORGE_GITHUB_REPO") else { return };
        let env: HashMap<String, String> = std::env::vars().collect();
        let prs = futures::executor::block_on(list(Path::new(&dir), &env)).unwrap();
        assert!(!prs.is_empty());
        for pr in prs.iter().take(5) {
            println!("PR #{} {} @{} [{}] {:?}", pr.number, pr.title, pr.author, pr.checks.summary(), pr.review);
        }
        let with_comments = prs.iter().find_map(|pr| {
            let comments = futures::executor::block_on(comments(Path::new(&dir), pr.number, &env)).unwrap();
            (!comments.is_empty()).then_some((pr.number, comments))
        });
        if let Some((number, comments)) = with_comments {
            println!("#{number}: {} comments in {} threads, first on {}:{:?}", comments.len(), threads(&comments).len(), comments[0].path, comments[0].line);
        }
        println!("current: {:?}", futures::executor::block_on(current(Path::new(&dir), &env)).unwrap().map(|pr| pr.number));
    }
}
