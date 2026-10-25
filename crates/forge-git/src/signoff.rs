//! Signing off commits: the `Signed-off-by:` trailer (`git commit --signoff`) that projects
//! under the Developer Certificate of Origin require.
//!
//! It is a setting (`signoff`, on the Settings tab's Commits page) like any other: in the
//! user's `git.json` for every project, and in a project's `.forge/git.json`, which wins and
//! is shared with whoever works on it. The Git panel's Signoff toggle follows the project's
//! value, and flipping it (or the agent's commit card, or Git › *Sign Off Commits in This
//! Project*) saves it to the project. A project whose repository asks for sign-offs (a DCO
//! file, its contributing guide, most of its recent commits) and that hasn't chosen yet is
//! offered to turn them on.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use db::kvp::KeyValueStore;
use forge_ui::settings_registry::{SettingChanged, SettingsFile, SettingsPage, SettingsRegistry};
use git_ui::git_panel::GitPanel;
use gpui::{App, AppContext as _, Context, Entity, Subscription, WeakEntity, actions};
use project::{Project, git_store::GitStoreEvent};
use serde_json::{Value, json};
use util::ResultExt as _;
use workspace::{
    Toast, Workspace,
    notifications::{NotificationId, simple_message_notification::MessageNotification},
};

actions!(
    forge_git,
    [
        /// Turns signing off commits on or off for the project.
        ToggleSignOff
    ]
);

/// The settings file, in Forge's config folder (the user's) and in a project's `.forge/`.
const FILE: &str = "git.json";
const KEY: &str = "signoff";
const TRAILER: &str = "signed-off-by:";
/// Recent commits looked at to tell whether a repository signs off.
const RECENT_COMMITS: &str = "20";
/// Files whose presence says a repository takes the DCO (the DCO GitHub app reads the first).
const DCO_FILES: &[&str] = &[".github/dco.yml", "DCO", "DCO.md", "DCO.txt"];
const CONTRIBUTING_GUIDES: &[&str] = &["CONTRIBUTING.md", ".github/CONTRIBUTING.md", "docs/CONTRIBUTING.md", "CONTRIBUTING.rst", "CONTRIBUTING"];

pub fn init(cx: &mut App) {
    register_settings(cx);
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        if window.is_none() || !workspace.project().read(cx).is_local() {
            return;
        }
        workspace.register_action(|workspace, _: &ToggleSignOff, _, cx| toggle(workspace, cx));
        let sync = cx.new(|cx| Sync::new(workspace, cx));
        cx.default_global::<Syncs>().syncs.push(sync.downgrade());
        // Loading reads the workspace, which is still being built.
        cx.defer(move |cx| sync.update(cx, |sync, cx| sync.load(cx)));
    })
    .detach();
    // The Git panel loads after its workspace.
    cx.observe_new(|_: &mut GitPanel, _, cx| {
        cx.defer(|cx| {
            let syncs: Vec<Entity<Sync>> = cx.default_global::<Syncs>().syncs.iter().filter_map(WeakEntity::upgrade).collect();
            for sync in syncs {
                sync.update(cx, |sync, cx| sync.attach_panel(cx));
            }
        });
    })
    .detach();
}

/// The Commits page of the Settings tab; a project may set it too.
fn register_settings(cx: &mut App) {
    let page = SettingsPage {
        id: "commits".into(),
        title: "Commits".into(),
        file: SettingsFile::Config(FILE.into()),
        schema: json!({ "properties": { KEY: {
            "type": "boolean",
            "title": "Sign off commits",
            "description": "Add a Signed-off-by line with your name and email (the committer in your git config) to the commits you make, \
as projects under the Developer Certificate of Origin (DCO) require. Set it for a project to share it with whoever works on it."
        } } }),
        keys: None,
        defaults: json!({ KEY: false }),
        order: 30,
        actions: vec![],
        project_keys: Some(vec![KEY.into()]),
    };
    let registry = SettingsRegistry::global(cx);
    registry.update(cx, |registry, cx| registry.register(page, cx));
}

/// The open workspaces' syncs, for the Git panels that load after them.
#[derive(Default)]
struct Syncs {
    syncs: Vec<WeakEntity<Sync>>,
    /// Projects offered to sign off since Forge started: one offer each.
    offered: HashSet<PathBuf>,
}
impl gpui::Global for Syncs {}

/// Keeps a workspace's Git panel toggle and its project's setting together.
struct Sync {
    workspace: WeakEntity<Workspace>,
    /// The project's folder and whether it signs off, once read.
    project: Option<(PathBuf, bool)>,
    panel: Option<WeakEntity<GitPanel>>,
    _subscriptions: Vec<Subscription>,
}

impl Sync {
    fn new(workspace: &Workspace, cx: &mut Context<Self>) -> Self {
        let git_store = workspace.project().read(cx).git_store().clone();
        let registry = SettingsRegistry::global(cx);
        let subscriptions = vec![
            cx.subscribe(&git_store, |this, _, event: &GitStoreEvent, cx| {
                if matches!(event, GitStoreEvent::ActiveRepositoryChanged(_)) {
                    this.load(cx);
                }
            }),
            // Changed on the Settings tab, or by another window's toggle.
            cx.subscribe(&registry, |this, _, event: &SettingChanged, cx| {
                if matches!(&event.file, SettingsFile::Config(name) | SettingsFile::Project { name, .. } if name == FILE) {
                    this.load(cx);
                }
            }),
        ];
        Self { workspace: workspace.weak_handle(), project: None, panel: None, _subscriptions: subscriptions }
    }

    /// Reads the project's setting into the panel, offering sign-offs when the active
    /// repository asks for them and the project hasn't chosen.
    fn load(&mut self, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else { return };
        let project = workspace.read(cx).project().clone();
        let Some(root) = project_root(project.read(cx), cx) else {
            self.project = None;
            return;
        };
        let repo = project.read(cx).active_repository(cx).map(|repo| repo.read(cx).work_directory_abs_path.to_path_buf());
        let kvp = KeyValueStore::global(cx);
        cx.spawn(async move |this, cx| {
            let (choice, asks) = cx
                .background_spawn({
                    let root = root.clone();
                    async move {
                        let choice = configured(&root);
                        let asks = choice.is_none()
                            && kvp.read_kvp(&dismissed_key(&root)).ok().flatten().is_none()
                            && repo.is_some_and(|repo| asks_for_it(&repo));
                        (choice, asks)
                    }
                })
                .await;
            this.update(cx, |this, cx| {
                this.project = Some((root.clone(), choice.unwrap_or(false)));
                this.attach_panel(cx);
                this.apply(cx);
                if asks {
                    this.offer(root, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    fn attach_panel(&mut self, cx: &mut Context<Self>) {
        if self.panel.as_ref().is_some_and(|panel| panel.upgrade().is_some()) {
            return;
        }
        let Some(panel) = self.workspace.upgrade().and_then(|workspace| workspace.read(cx).panel::<GitPanel>(cx)) else { return };
        self.panel = Some(panel.downgrade());
        self._subscriptions.push(cx.observe(&panel, |this, panel, cx| this.panel_changed(panel, cx)));
        self.apply(cx);
    }

    fn apply(&self, cx: &mut App) {
        let (Some((_, on)), Some(panel)) = (&self.project, self.panel.as_ref().and_then(|panel| panel.upgrade())) else { return };
        let on = *on;
        panel.update(cx, |panel, cx| {
            if panel.signoff_enabled() != on {
                panel.set_signoff_enabled(on, cx);
            }
        });
    }

    /// The user flipped the panel's toggle: the project keeps it.
    fn panel_changed(&mut self, panel: Entity<GitPanel>, cx: &mut Context<Self>) {
        let on = panel.read(cx).signoff_enabled();
        let Some((root, current)) = &mut self.project else { return };
        if *current != on {
            *current = on;
            set(root.clone(), on, cx);
        }
    }

    fn offer(&self, root: PathBuf, cx: &mut Context<Self>) {
        if !cx.default_global::<Syncs>().offered.insert(root.clone()) {
            return;
        }
        let Some(workspace) = self.workspace.upgrade() else { return };
        let name = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| root.display().to_string());
        let id = NotificationId::named(format!("forge-signoff-{}", root.display()).into());
        let kvp = KeyValueStore::global(cx);
        workspace.update(cx, |workspace, cx| {
            workspace.show_notification(id, cx, move |cx| {
                let (on, off) = (root.clone(), root.clone());
                let kvp = kvp.clone();
                cx.new(move |cx| {
                    MessageNotification::new(
                        format!(
                            "{name} asks contributors to sign off their commits: a Signed-off-by line with their name and email (the Developer Certificate of Origin). \
Sign off the commits made in it? Forge saves it in the project's {}.",
                            SettingsFile::Config(FILE.into()).in_project(&root).display_name()
                        ),
                        cx,
                    )
                    .with_title("Sign off commits?")
                    .show_suppress_button(false)
                    .primary_message("Sign Off")
                    .primary_on_click(move |_, cx| set(on.clone(), true, cx))
                    .secondary_message("Not Now")
                    .secondary_on_click(move |_, cx| {
                        let (kvp, key) = (kvp.clone(), dismissed_key(&off));
                        cx.background_spawn(async move { kvp.write_kvp(key, "1".into()).await.log_err() }).detach();
                    })
                })
            });
        });
    }
}

fn dismissed_key(root: &Path) -> String {
    format!("forge-signoff-dismissed:{}", root.display())
}

/// The folder whose `.forge/` holds the project's settings: its first folder, as on the
/// Settings tab.
pub fn project_root(project: &Project, cx: &App) -> Option<PathBuf> {
    project.visible_worktrees(cx).next().map(|tree| tree.read(cx).abs_path().to_path_buf())
}

/// Saves whether the project at `root` signs off (in its `.forge/git.json`); every Git
/// panel and the Settings tab follow.
pub fn set(root: PathBuf, on: bool, cx: &mut App) {
    let file = SettingsFile::Config(FILE.into()).in_project(&root);
    let registry = SettingsRegistry::global(cx);
    registry.update(cx, |registry, cx| registry.set(&file, &[KEY.to_string()], Some(json!(on)), cx)).context("saving the sign-off setting").log_err();
}

fn toggle(workspace: &mut Workspace, cx: &mut Context<Workspace>) {
    let Some(root) = project_root(workspace.project().read(cx), cx) else {
        workspace.show_error(anyhow::anyhow!("Open a folder to sign off its commits."), cx);
        return;
    };
    let on = !configured(&root).unwrap_or(false);
    set(root.clone(), on, cx);
    cx.spawn(async move |workspace, cx| {
        let signer = cx.background_spawn(async move { signer(&root) }).await;
        let message = match (on, signer) {
            (true, Some(signer)) => format!("Commits in this project are signed off as {signer}."),
            (true, None) => "Commits in this project are signed off.".to_string(),
            (false, _) => "Commits in this project are no longer signed off.".to_string(),
        };
        workspace.update(cx, |workspace, cx| workspace.show_toast(Toast::new(NotificationId::named("forge-signoff".into()), message), cx)).ok();
    })
    .detach();
}

/// Whether the project at `root` signs off its commits: its own setting, else the user's;
/// `None` when neither says.
pub fn configured(root: &Path) -> Option<bool> {
    let user = SettingsFile::Config(FILE.into());
    choice(&user.in_project(root).read(), &user.read())
}

fn choice(project: &Value, user: &Value) -> Option<bool> {
    [project, user].into_iter().find_map(|file| file.get(KEY).and_then(Value::as_bool))
}

fn git(dir: &Path, args: &[&str]) -> Result<std::process::Output> {
    ide_api::std_command("git").arg("-C").arg(dir).args(args).output().context("cannot run git; is it installed?")
}

/// Who `git commit --signoff` signs as in `dir`: "Name <email>", the committer.
pub fn signer(dir: &Path) -> Option<String> {
    let output = git(dir, &["var", "GIT_COMMITTER_IDENT"]).ok().filter(|o| o.status.success())?;
    let ident = String::from_utf8_lossy(&output.stdout);
    // "Name <email> 1700000000 +0100": the date follows the email.
    let end = ident.rfind('>')?;
    Some(ident[..=end].trim().to_string())
}

/// `message` without `Signed-off-by:` lines: only the user signs off, through git.
pub fn without_trailer(message: &str) -> String {
    let kept: Vec<&str> = message.lines().filter(|line| !line.trim_start().to_lowercase().starts_with(TRAILER)).collect();
    kept.join("\n").trim_end().to_string()
}

/// Whether the repository at `dir` asks for signed-off commits.
pub fn asks_for_it(dir: &Path) -> bool {
    if DCO_FILES.iter().any(|file| dir.join(file).is_file()) {
        return true;
    }
    if CONTRIBUTING_GUIDES.iter().any(|guide| std::fs::read_to_string(dir.join(guide)).is_ok_and(|text| mentions_sign_off(&text))) {
        return true;
    }
    git(dir, &["log", "-n", RECENT_COMMITS, "--no-merges", "--format=%B%x00"])
        .ok()
        .filter(|output| output.status.success())
        .is_some_and(|output| mostly_signed_off(&String::from_utf8_lossy(&output.stdout)))
}

fn mentions_sign_off(text: &str) -> bool {
    let text = text.to_lowercase();
    ["signed-off-by", "developer certificate of origin", "--signoff", "git commit -s"].iter().any(|phrase| text.contains(phrase))
}

/// Whether most of the NUL-separated commit messages are signed off (at least three).
fn mostly_signed_off(messages: &str) -> bool {
    let messages: Vec<&str> = messages.split('\0').filter(|m| !m.trim().is_empty()).collect();
    let signed = messages.iter().filter(|m| m.lines().any(|line| line.trim_start().to_lowercase().starts_with(TRAILER))).count();
    signed >= 3 && signed * 2 > messages.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for args in [&["init", "-q"][..], &["config", "user.name", "Ada Lovelace"], &["config", "user.email", "ada@example.com"], &["config", "commit.gpgsign", "false"]] {
            assert!(git(dir.path(), args).unwrap().status.success());
        }
        dir
    }

    fn commit(dir: &Path, message: &str) {
        assert!(git(dir, &["commit", "-q", "--allow-empty", "-m", message]).unwrap().status.success());
    }

    #[test]
    fn the_project_wins_over_the_user() {
        assert_eq!(choice(&json!({}), &json!({})), None, "nobody chose");
        assert_eq!(choice(&json!({}), &json!({ "signoff": true })), Some(true), "the user's, for every project");
        assert_eq!(choice(&json!({ "signoff": false }), &json!({ "signoff": true })), Some(false));
        assert_eq!(choice(&json!({ "signoff": true }), &json!({})), Some(true));
    }

    #[test]
    fn saves_in_the_projects_forge_folder() {
        let dir = tempfile::tempdir().unwrap();
        let file = SettingsFile::Config(FILE.into()).in_project(dir.path());
        file.write(&[KEY.to_string()], Some(&json!(true))).unwrap();
        assert!(dir.path().join(".forge/git.json").is_file());
        assert_eq!(choice(&file.read(), &json!({})), Some(true));
    }

    #[test]
    fn signs_as_the_committer() {
        let dir = repo();
        assert_eq!(signer(dir.path()).as_deref(), Some("Ada Lovelace <ada@example.com>"));
    }

    #[test]
    fn drops_the_agents_own_sign_offs() {
        assert_eq!(without_trailer("Fix it\n\nBody.\n\nSigned-off-by: Bot <bot@example.com>\n"), "Fix it\n\nBody.");
        assert_eq!(without_trailer("Fix it\n\nsigned-off-by: x\nCo-Authored-By: y"), "Fix it\n\nCo-Authored-By: y");
    }

    #[test]
    fn tells_repositories_that_ask_for_sign_offs() {
        let dir = repo();
        assert!(!asks_for_it(dir.path()), "an empty repository");

        std::fs::write(dir.path().join("CONTRIBUTING.md"), "Run the tests first.").unwrap();
        assert!(!asks_for_it(dir.path()));
        std::fs::write(dir.path().join("CONTRIBUTING.md"), "Commits need a Developer Certificate of Origin sign-off.").unwrap();
        assert!(asks_for_it(dir.path()), "its contributing guide");
        std::fs::remove_file(dir.path().join("CONTRIBUTING.md")).unwrap();

        std::fs::create_dir(dir.path().join(".github")).unwrap();
        std::fs::write(dir.path().join(".github/dco.yml"), "require:\n  members: false\n").unwrap();
        assert!(asks_for_it(dir.path()), "the DCO app's file");
        std::fs::remove_file(dir.path().join(".github/dco.yml")).unwrap();

        for n in 0..3 {
            commit(dir.path(), &format!("Change {n}\n\nSigned-off-by: Ada Lovelace <ada@example.com>"));
        }
        commit(dir.path(), "Unsigned");
        assert!(asks_for_it(dir.path()), "most of its commits");
    }

    #[test]
    fn needs_most_commits_signed_off() {
        let signed = "A\n\nSigned-off-by: a <a@a>\0";
        assert!(!mostly_signed_off(&signed.repeat(2)), "too few to tell");
        assert!(mostly_signed_off(&format!("{}B\0", signed.repeat(3))));
        assert!(!mostly_signed_off(&format!("{}B\0C\0D\0", signed.repeat(3))), "half isn't most");
    }
}
