//! Git › Write Commit Message (alt-tab in the commit box): the default agent writes the
//! message for the staged changes (all changes when nothing is staged), in the style of
//! the repository's recent commits, straight into the Git panel's commit box. It asks in a
//! hidden session of a connected thread, starting one in the background if needed.

use std::{path::Path, time::Duration};

use gpui::{App, AppContext as _, AsyncWindowContext, Entity, WeakEntity, Window};
use workspace::{Toast, Workspace, notifications::NotificationId};

use crate::thread::{Status, Thread};
use crate::threads::{register_thread, store_for};

gpui::actions!(forge_agent, [
    /// Asks the default agent for a commit message for the staged changes.
    WriteCommitMessage
]);

const MAX_DIFF_BYTES: usize = 60_000;

struct CommitMessageToast;

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|ws, _: &WriteCommitMessage, window, cx| write(ws, window, cx));
    })
    .detach();
}

/// alt-tab in the Git panel's commit box, as in Zed (bound after the default keymap).
pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        gpui::KeyBinding::new("alt-tab", WriteCommitMessage, Some("GitPanel")),
        gpui::KeyBinding::new("alt-tab", WriteCommitMessage, Some("GitCommit > Editor")),
    ]);
}

fn toast(workspace: &WeakEntity<Workspace>, text: impl Into<String>, cx: &mut App) {
    let text = text.into();
    workspace
        .update(cx, |ws, cx| ws.show_toast(Toast::new(NotificationId::unique::<CommitMessageToast>(), text).autohide(), cx))
        .ok();
}

fn write(workspace: &mut Workspace, window: &mut Window, cx: &mut gpui::Context<Workspace>) {
    let weak = workspace.weak_handle();
    let Some(root) = workspace.visible_worktrees(cx).next().map(|w| w.read(cx).abs_path().to_path_buf()) else { return };
    // A thread whose agent is connected, or a new one connecting in the background.
    let connected = store_for(cx.entity_id(), cx).and_then(|store| store.read(cx).threads().iter().find(|t| matches!(t.read(cx).status(), Status::Ready | Status::Busy)).cloned());
    let thread = match connected {
        Some(thread) => thread,
        None => {
            let thread = cx.new(|cx| Thread::new(workspace, None, window, cx));
            register_thread(workspace, thread.clone(), cx);
            thread.update(cx, |t, cx| t.connect(window, cx));
            thread
        }
    };
    toast(&weak, "The agent is writing the commit message…", cx);
    window
        .spawn(cx, async move |cx| {
            if let Err(e) = write_with(thread, &root, weak.clone(), cx).await {
                cx.update(|_, cx| toast(&weak, format!("Couldn't write the commit message: {e:#}"), cx)).ok();
            }
        })
        .detach();
}

async fn write_with(thread: Entity<Thread>, root: &Path, workspace: WeakEntity<Workspace>, cx: &mut AsyncWindowContext) -> anyhow::Result<()> {
    // Wait for the agent (at most a minute: it may be starting or signing in).
    for _ in 0..240 {
        match thread.read_with(cx, |t, _| t.status()) {
            Status::Ready | Status::Busy => break,
            _ => cx.background_executor().timer(Duration::from_millis(250)).await,
        }
    }
    let root = root.to_path_buf();
    let (diff, log) = cx.background_spawn(async move { (changes(&root), recent_subjects(&root)) }).await;
    let diff = diff?;
    let prompt = prompt(&diff, &log);
    let answer = thread.update(cx, |t, cx| t.ask_aside(prompt, cx)).await?;
    let message = clean(&answer);
    anyhow::ensure!(!message.is_empty(), "the agent answered nothing");
    workspace.update_in(cx, |ws, window, cx| {
        let Some(panel) = ws.panel::<git_ui::git_panel::GitPanel>(cx) else { return };
        let buffer = panel.read(cx).commit_message_buffer(cx);
        buffer.update(cx, |b, cx| b.set_text(message, cx));
        ws.focus_panel::<git_ui::git_panel::GitPanel>(window, cx);
    })?;
    Ok(())
}

fn git(root: &Path, args: &[&str]) -> anyhow::Result<String> {
    let out = ide_api::std_command("git").args(args).current_dir(root).output()?;
    anyhow::ensure!(out.status.success(), "git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The staged diff, or every change when nothing is staged.
fn changes(root: &Path) -> anyhow::Result<String> {
    let mut diff = git(root, &["diff", "--cached", "--no-color"])?;
    if diff.trim().is_empty() {
        diff = git(root, &["diff", "HEAD", "--no-color"]).or_else(|_| git(root, &["diff", "--no-color"]))?;
    }
    anyhow::ensure!(!diff.trim().is_empty(), "there are no changes to commit");
    if diff.len() > MAX_DIFF_BYTES {
        let mut cut = MAX_DIFF_BYTES;
        while !diff.is_char_boundary(cut) {
            cut -= 1;
        }
        diff.truncate(cut);
        diff.push_str("\n… (cut)");
    }
    Ok(diff)
}

fn recent_subjects(root: &Path) -> String {
    git(root, &["log", "-12", "--format=%s"]).unwrap_or_default()
}

fn prompt(diff: &str, log: &str) -> String {
    let style = if log.trim().is_empty() { String::new() } else { format!("Recent commit subjects in this repository (follow their style and language):\n{log}\n") };
    format!(
        "Write a git commit message for the changes below. {style}\
         Reply with only the message: a subject line of at most 72 characters, then, if the change needs explaining, a blank line and a short body. \
         No code fences, no quotes, no preamble. Don't run tools.\n\n<diff>\n{diff}\n</diff>"
    )
}

/// The message without fences, quotes or a "Here is…" line agents sometimes add.
fn clean(answer: &str) -> String {
    let mut lines: Vec<&str> = answer.trim().lines().collect();
    if lines.first().is_some_and(|l| {
        let l = l.to_lowercase();
        (l.starts_with("here") || l.starts_with("commit message")) && l.trim_end().ends_with(':')
    }) {
        lines.remove(0);
    }
    if lines.first().is_some_and(|l| l.trim_start().starts_with("```")) {
        lines.remove(0);
        if lines.last().is_some_and(|l| l.trim_start().starts_with("```")) {
            lines.pop();
        }
    }
    lines.join("\n").trim().trim_matches('"').trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleans_answers() {
        assert_eq!(clean("Fix the parser\n\nIt skipped tabs."), "Fix the parser\n\nIt skipped tabs.");
        assert_eq!(clean("Here is the commit message:\n```\nFix the parser\n```"), "Fix the parser");
        assert_eq!(clean("\"Add a test\""), "Add a test");
    }

    #[test]
    fn reads_the_changes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git(root, &["init", "-q"]).unwrap();
        git(root, &["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-q", "--allow-empty", "-m", "Start the project"]).unwrap();
        assert!(changes(root).is_err(), "nothing to commit");
        std::fs::write(root.join("a.txt"), "hello\n").unwrap();
        git(root, &["add", "a.txt"]).unwrap();
        assert!(changes(root).unwrap().contains("+hello"));
        let prompt = prompt(&changes(root).unwrap(), &recent_subjects(root));
        assert!(prompt.contains("Start the project") && prompt.contains("+hello"));
    }
}
