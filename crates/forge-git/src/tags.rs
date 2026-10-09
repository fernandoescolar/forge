//! Git › Create Tag…: a dialog to tag a commit (HEAD from the menu, any commit from the
//! History panel's or Git panel's commit menu), lightweight or annotated with a message,
//! and optionally push it to the branch's remote.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, anyhow};
use editor::Editor;
use git::Oid;
use git_ui::CommitContextMenuExtension;
use git_ui_core::askpass_modal::AskPassModal;
use gpui::{
    App, AppContext as _, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, InteractiveElement as _, IntoElement, ParentElement as _,
    Render, SharedString, Styled as _, WeakEntity, Window, actions, div, rems,
};
use menu::{Cancel, Confirm};
use project::git_store::Repository;
use theme::ActiveTheme as _;
use ui::{
    Button, ButtonCommon as _, ButtonStyle, Checkbox, Clickable as _, Color, Disableable as _, Headline, HeadlineSize, Icon, IconName, IconSize, Label,
    LabelCommon as _, LabelSize, StyledExt as _, ToggleState, h_flex, v_flex,
};
use workspace::{ModalView, Toast, Workspace, notifications::NotificationId};

actions!(forge_git, [CreateTag]);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &CreateTag, window, cx| {
            let Some(repo) = workspace.project().read(cx).active_repository(cx) else {
                workspace.show_error(anyhow!("Open a folder in a git repository to create a tag."), cx);
                return;
            };
            open(workspace, repo, None, window, cx);
        });
    })
    .detach();
    // "Create Tag…" on any commit, in the commit menus of the History and Git panels.
    cx.set_global(CommitContextMenuExtension(std::sync::Arc::new(|menu, sha, repo, workspace, _, _| {
        menu.separator().entry("Create Tag…", None, move |window, cx| {
            let Some(repo) = repo.as_ref().and_then(|r| r.upgrade()) else { return };
            workspace.update(cx, |ws, cx| open(ws, repo, Some(sha), window, cx)).ok();
        })
    })));
}

/// Opens the dialog to tag `sha` (HEAD when `None`) in `repo`.
pub fn open(workspace: &mut Workspace, repo: Entity<Repository>, sha: Option<Oid>, window: &mut Window, cx: &mut Context<Workspace>) {
    let weak = workspace.weak_handle();
    workspace.toggle_modal(window, cx, |window, cx| CreateTagModal::new(weak, repo, sha, window, cx));
}

pub struct CreateTagModal {
    workspace: WeakEntity<Workspace>,
    repo: Entity<Repository>,
    dir: PathBuf,
    /// The commit to tag; `None`: HEAD.
    sha: Option<Oid>,
    /// What the dialog says it tags: "HEAD (main, 1a2b3c4)" or "1a2b3c4".
    target: SharedString,
    name: Entity<Editor>,
    message: Entity<Editor>,
    /// The remote a push goes to, once known (`None`: the repository has none).
    remote: Option<String>,
    push: bool,
    error: Option<SharedString>,
    busy: bool,
}

impl CreateTagModal {
    fn new(workspace: WeakEntity<Workspace>, repo: Entity<Repository>, sha: Option<Oid>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let (dir, target, upstream) = {
            let r = repo.read(cx);
            let head = r.head_commit.as_ref().map(|c| short(&c.sha));
            let target = match (sha, r.branch.as_ref()) {
                (Some(sha), _) => short(&sha.to_string()),
                (None, Some(branch)) => format!("HEAD ({}{})", branch.name(), head.map(|h| format!(", {h}")).unwrap_or_default()),
                (None, None) => format!("HEAD{}", head.map(|h| format!(" ({h})")).unwrap_or_default()),
            };
            let upstream = r.branch.as_ref().and_then(|b| b.upstream.as_ref()).and_then(|u| u.remote_name()).map(str::to_string);
            (r.work_directory_abs_path.to_path_buf(), target, upstream)
        };
        let name = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("v1.2.0", window, cx);
            editor
        });
        let message = cx.new(|cx| {
            let mut editor = Editor::auto_height(3, 8, window, cx);
            editor.set_placeholder_text("Message (optional: makes it an annotated tag)", window, cx);
            editor
        });
        let remotes_dir = dir.clone();
        cx.spawn(async move |this, cx| {
            let remotes = cx.background_spawn(async move { remotes(&remotes_dir) }).await.unwrap_or_default();
            this.update(cx, |this, cx| {
                this.remote = pick_remote(&remotes, upstream.as_deref());
                cx.notify();
            })
            .ok();
        })
        .detach();
        Self { workspace, repo, dir, sha, target: target.into(), name, message, remote: None, push: false, error: None, busy: false }
    }

    fn cancel(&mut self, _: &Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn confirm(&mut self, _: &Confirm, window: &mut Window, cx: &mut Context<Self>) {
        self.create(window, cx);
    }

    fn create(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let name = self.name.read(cx).text(cx).trim().to_string();
        if name.is_empty() {
            self.error = Some("Enter a name for the tag.".into());
            cx.notify();
            return;
        }
        let message = self.message.read(cx).text(cx).trim().to_string();
        let message = (!message.is_empty()).then_some(message);
        let (dir, sha) = (self.dir.clone(), self.sha.map(|s| s.to_string()));
        let push = self.push.then(|| self.remote.clone()).flatten();
        self.busy = true;
        self.error = None;
        cx.notify();
        let window_handle = window.window_handle();
        cx.spawn_in(window, async move |this, cx| {
            let tag = name.clone();
            let created = cx.background_spawn(async move { create_tag(&dir, &tag, message.as_deref(), sha.as_deref()) }).await;
            if let Err(error) = created {
                this.update(cx, |this, cx| {
                    this.busy = false;
                    this.error = Some(format!("{error:#}").into());
                    cx.notify();
                })
                .ok();
                return;
            }
            let Ok((workspace, repo)) = this.update(cx, |this, cx| {
                cx.emit(DismissEvent);
                (this.workspace.clone(), this.repo.clone())
            }) else {
                return;
            };
            let Some(remote) = push else {
                notify(&workspace, format!("Created tag {name}."), cx);
                return;
            };
            let askpass = askpass_delegate(workspace.clone(), window_handle, format!("git push {remote} {name}"), cx);
            let refspec: SharedString = format!("refs/tags/{name}").into();
            let pushed = repo.update(cx, |repo, cx| repo.push(refspec.clone(), refspec, remote.clone().into(), None, askpass, cx)).await;
            match pushed {
                Ok(Ok(_)) => notify(&workspace, format!("Created tag {name} and pushed it to {remote}."), cx),
                Ok(Err(error)) => {
                    workspace.update(cx, |ws, cx| ws.show_error(error.context(format!("Created tag {name}, but pushing it to {remote} failed")), cx)).ok();
                }
                Err(_) => notify(&workspace, format!("Created tag {name}; the push to {remote} was canceled."), cx),
            }
        })
        .detach();
    }
}

fn notify(workspace: &WeakEntity<Workspace>, message: String, cx: &mut gpui::AsyncWindowContext) {
    struct TagCreated;
    workspace.update(cx, |ws, cx| ws.show_toast(Toast::new(NotificationId::unique::<TagCreated>(), message).autohide(), cx)).ok();
}

/// Asks for the credentials a push needs the way Zed's Git panel does.
fn askpass_delegate(workspace: WeakEntity<Workspace>, window: gpui::AnyWindowHandle, operation: String, cx: &mut gpui::AsyncWindowContext) -> askpass::AskPassDelegate {
    let operation: SharedString = operation.into();
    askpass::AskPassDelegate::new_with_cancellation(cx, move |prompt, tx, cancellation, cx| {
        window
            .update(cx, |_, window, cx| {
                workspace
                    .update(cx, |ws, cx| ws.toggle_modal(window, cx, |window, cx| AskPassModal::new(operation.clone(), prompt.into(), tx, cancellation, window, cx)))
                    .ok();
            })
            .ok();
    })
}

fn short(sha: &str) -> String {
    sha.chars().take(7).collect()
}

fn git(dir: &Path, args: &[&str]) -> Result<std::process::Output> {
    ide_api::std_command("git").arg("-C").arg(dir).args(args).output().context("cannot run git; is it installed?")
}

fn stderr(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).trim().trim_start_matches("fatal: ").trim_start_matches("error: ").to_string()
}

/// Tags `target` (HEAD when `None`) as `name`: annotated when there is a message.
pub(crate) fn create_tag(dir: &Path, name: &str, message: Option<&str>, target: Option<&str>) -> Result<()> {
    if !git(dir, &["check-ref-format", &format!("refs/tags/{name}")])?.status.success() {
        return Err(anyhow!("“{name}” is not a valid tag name."));
    }
    if git(dir, &["rev-parse", "--quiet", "--verify", &format!("refs/tags/{name}")])?.status.success() {
        return Err(anyhow!("A tag named “{name}” already exists."));
    }
    let mut args = vec!["tag"];
    if let Some(message) = message {
        args.extend(["--annotate", "--message", message]);
    }
    args.push(name);
    args.extend(target);
    let output = git(dir, &args)?;
    if !output.status.success() {
        return Err(anyhow!("{}", stderr(&output)));
    }
    Ok(())
}

fn remotes(dir: &Path) -> Result<Vec<String>> {
    let output = git(dir, &["remote"])?;
    Ok(String::from_utf8_lossy(&output.stdout).lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string).collect())
}

/// Where a tag is pushed: the branch's remote, else `origin`, else the only other one.
pub(crate) fn pick_remote(remotes: &[String], upstream: Option<&str>) -> Option<String> {
    upstream
        .filter(|u| remotes.iter().any(|r| r == u))
        .or_else(|| remotes.iter().find(|r| *r == "origin").map(String::as_str))
        .or_else(|| remotes.first().map(String::as_str))
        .map(str::to_string)
}

impl EventEmitter<DismissEvent> for CreateTagModal {}
impl ModalView for CreateTagModal {}

impl Focusable for CreateTagModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.name.focus_handle(cx)
    }
}

impl Render for CreateTagModal {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let push_label = match &self.remote {
            Some(remote) => format!("Push it to {remote}"),
            None => "Push it (the repository has no remote)".into(),
        };
        let field = |label: &'static str, editor: Entity<Editor>, cx: &App| {
            v_flex().gap_1().child(Label::new(label).size(LabelSize::Small).color(Color::Muted)).child(
                div().px_2().py_1().rounded_md().border_1().border_color(cx.theme().colors().border).child(editor),
            )
        };
        v_flex()
            .key_context("CreateTagModal")
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .elevation_2(cx)
            .w(rems(34.))
            .p_3()
            .gap_3()
            .child(
                h_flex()
                    .gap_1p5()
                    .child(Icon::new(IconName::Hash).size(IconSize::Small).color(Color::Muted))
                    .child(Headline::new("Create Tag").size(HeadlineSize::XSmall))
                    .child(Label::new(format!("on {}", self.target)).size(LabelSize::Small).color(Color::Muted)),
            )
            .child(field("Name", self.name.clone(), cx))
            .child(field("Message", self.message.clone(), cx))
            .child(
                Checkbox::new("create-tag-push", if self.push && self.remote.is_some() { ToggleState::Selected } else { ToggleState::Unselected })
                    .label(push_label)
                    .disabled(self.remote.is_none())
                    .on_click(cx.listener(|this, state: &ToggleState, _, cx| {
                        this.push = *state == ToggleState::Selected;
                        cx.notify();
                    })),
            )
            .children(self.error.clone().map(|e| Label::new(e).size(LabelSize::Small).color(Color::Error)))
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(Button::new("create-tag-cancel", "Cancel").style(ButtonStyle::Subtle).on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))))
                    .child(
                        Button::new("create-tag-confirm", if self.busy { "Creating…" } else { "Create Tag" })
                            .style(ButtonStyle::Filled)
                            .disabled(self.busy)
                            .on_click(cx.listener(|this, _, window, cx| this.create(window, cx))),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| assert!(Command::new("git").arg("-C").arg(dir.path()).args(args).output().unwrap().status.success(), "git {args:?}");
        git(&["init", "--quiet", "--initial-branch", "main"]);
        git(&["config", "user.name", "Forge"]);
        git(&["config", "user.email", "forge@example.com"]);
        std::fs::write(dir.path().join("a.txt"), "one").unwrap();
        git(&["add", "."]);
        git(&["commit", "--quiet", "--message", "first"]);
        std::fs::write(dir.path().join("a.txt"), "two").unwrap();
        git(&["commit", "--quiet", "--all", "--message", "second"]);
        dir
    }

    fn rev(dir: &Path, rev: &str) -> String {
        String::from_utf8(git(dir, &["rev-parse", rev]).unwrap().stdout).unwrap().trim().to_string()
    }

    #[test]
    fn creates_lightweight_and_annotated_tags() {
        let dir = repo();
        let first = rev(dir.path(), "HEAD~1");

        create_tag(dir.path(), "v1.0.0", None, None).unwrap();
        assert_eq!(rev(dir.path(), "v1.0.0^{commit}"), rev(dir.path(), "HEAD"), "HEAD by default");
        assert_eq!(String::from_utf8(git(dir.path(), &["cat-file", "-t", "v1.0.0"]).unwrap().stdout).unwrap().trim(), "commit", "lightweight");

        create_tag(dir.path(), "v0.9.0", Some("The first one"), Some(&first)).unwrap();
        assert_eq!(rev(dir.path(), "v0.9.0^{commit}"), first, "the commit asked for");
        assert_eq!(String::from_utf8(git(dir.path(), &["cat-file", "-t", "v0.9.0"]).unwrap().stdout).unwrap().trim(), "tag", "annotated");

        let duplicate = create_tag(dir.path(), "v1.0.0", None, None).unwrap_err();
        assert_eq!(duplicate.to_string(), "A tag named “v1.0.0” already exists.");
        let invalid = create_tag(dir.path(), "bad name..", None, None).unwrap_err();
        assert_eq!(invalid.to_string(), "“bad name..” is not a valid tag name.");
    }

    /// Git › Create Tag… opens the dialog on HEAD of the active repository; a tag needs a
    /// name before anything runs.
    #[gpui::test]
    async fn the_action_opens_the_dialog(cx: &mut gpui::TestAppContext) {
        use gpui::VisualTestContext;
        cx.update(|cx| {
            let params = workspace::AppState::test(cx);
            drop(params);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            init(cx);
        });
        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree("/project", serde_json::json!({ ".git": {}, "a.txt": "one" })).await;
        let project = project::Project::test(fs.clone(), ["/project".as_ref()], cx).await;
        let window = cx.add_window(|window, cx| workspace::MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window.read_with(cx, |mw, _| mw.workspace().clone()).unwrap();
        let cx = &mut VisualTestContext::from_window(window.into(), cx);
        cx.run_until_parked();

        cx.dispatch_action(CreateTag);
        cx.run_until_parked();
        let modal = workspace.read_with(cx, |ws, cx| ws.active_modal::<CreateTagModal>(cx)).expect("the Create Tag dialog");
        assert!(modal.read_with(cx, |m, _| m.target.starts_with("HEAD")), "it tags HEAD");

        modal.update_in(cx, |m, window, cx| m.create(window, cx));
        assert_eq!(modal.read_with(cx, |m, _| m.error.clone()).as_deref(), Some("Enter a name for the tag."));
        assert!(workspace.read_with(cx, |ws, cx| ws.active_modal::<CreateTagModal>(cx)).is_some(), "still open");

        cx.dispatch_action(Cancel);
        cx.run_until_parked();
        assert!(workspace.read_with(cx, |ws, cx| ws.active_modal::<CreateTagModal>(cx)).is_none(), "Escape closes it");
    }

    #[test]
    fn pushes_to_the_branchs_remote_then_origin() {
        let names = |xs: &[&str]| xs.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(pick_remote(&names(&["origin", "fork"]), Some("fork")).as_deref(), Some("fork"));
        assert_eq!(pick_remote(&names(&["fork", "origin"]), None).as_deref(), Some("origin"));
        assert_eq!(pick_remote(&names(&["fork"]), Some("gone")).as_deref(), Some("fork"));
        assert_eq!(pick_remote(&[], None), None);
    }
}
