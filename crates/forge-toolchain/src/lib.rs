//! The tools a project needs, checked when it opens: a notification names those that
//! aren't installed and offers to install them, and Forge › Check Tools… lists them all.
//! Installing runs the command (the package manager's, or the tool's own installer) in a
//! terminal the user sees, where `sudo` asks for the password; nothing runs unseen. When
//! it is done, Forge says so (and whether the tool is on the PATH yet).
//!
//! netcoredbg, the .NET debugger, isn't here to install: Forge downloads it the first
//! time a .NET program is debugged (`forge_languages::netcoredbg`).

pub mod catalog;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use catalog::{Found, Need, PackageManager, Plan, Tool};
use db::kvp::KeyValueStore;
use forge_ui::pick::{self, Choice};
use gpui::{App, AppContext as _, Context, Entity, Task, Window, actions};
use project::Project;
use util::ResultExt as _;
use workspace::{
    Workspace,
    notifications::{NotificationId, simple_message_notification::MessageNotification},
};

actions!(forge_toolchain, [
    /// Lists the tools the project needs, installed or not, and installs the missing ones.
    CheckTools
]);

struct MissingTools;

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        workspace.register_action(|workspace, _: &CheckTools, window, cx| check_tools(workspace, window, cx));
        if window.is_some() {
            offer_when_missing(workspace, cx);
        }
    })
    .detach();
}

/// A tool as this project sees it.
#[derive(Clone, Debug)]
pub struct Status {
    pub tool: Tool,
    /// Why the project needs it; `None` when it doesn't.
    pub need: Option<Need>,
    pub found: Found,
}

/// Everything needed to check and install: what the project needs, what is installed
/// (with its shell environment's `PATH`), and the package manager.
pub struct Survey {
    pub statuses: Vec<Status>,
    pub manager: Option<PackageManager>,
    path: Option<String>,
    places: Vec<PathBuf>,
}

impl Survey {
    fn missing(&self, required_only: bool) -> Vec<&Status> {
        self.statuses.iter().filter(|s| s.need.is_some() && s.found == Found::Missing && !(required_only && s.tool.optional())).collect()
    }

    fn find(&self, tool: Tool) -> Found {
        catalog::find(tool, self.path.as_deref().map(std::ffi::OsStr::new), &self.places)
    }
}

/// Looks at the project's files and the machine (off the UI thread).
pub fn survey(project: &Entity<Project>, cx: &mut App) -> Task<Survey> {
    let listed = forge_ui::worktree::files(project, cx);
    let root: Option<Arc<Path>> = project.read(cx).visible_worktrees(cx).next().map(|wt| wt.read(cx).abs_path());
    let environment = root.clone().map(|root| project.update(cx, |p, cx| p.environment().update(cx, |env, cx| env.directory_environment(root, cx))));
    cx.spawn(async move |cx| {
        let listed = listed.await;
        let env: HashMap<String, String> = match environment {
            Some(task) => task.await.map(|env| env.into_iter().collect()).unwrap_or_default(),
            None => HashMap::new(),
        };
        cx.background_spawn(async move {
            let path = env.get("PATH").cloned().or_else(|| std::env::var("PATH").ok());
            let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
            let places = catalog::usual_places(&home);
            let manager = PackageManager::detect(std::env::consts::OS, path.as_deref().map(std::ffi::OsStr::new));
            let mut needs: Vec<Need> = Vec::new();
            for (root, files) in &listed {
                for need in catalog::needed(root, files) {
                    if !needs.iter().any(|n| n.tool == need.tool) {
                        needs.push(need);
                    }
                }
            }
            let statuses = Tool::ALL
                .iter()
                .map(|&tool| Status { tool, need: needs.iter().find(|n| n.tool == tool).cloned(), found: catalog::find(tool, path.as_deref().map(std::ffi::OsStr::new), &places) })
                .collect();
            Survey { statuses, manager, path, places }
        })
        .await
    })
}

fn dismissed_key(root: &Path, tools: &[Tool]) -> String {
    let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
    format!("forge-toolchain-dismissed:{}:{}", root.display(), names.join(","))
}

/// "Go and pnpm".
fn listing(tools: &[Tool]) -> String {
    let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
    match names.as_slice() {
        [] => String::new(),
        [one] => one.to_string(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// Once the project has been scanned, names the tools it needs that aren't installed,
/// unless the user already said "Not now" to those for this folder.
fn offer_when_missing(workspace: &Workspace, cx: &mut Context<Workspace>) {
    let project = workspace.project().clone();
    cx.spawn(async move |workspace, cx| {
        let Some(root) = cx.update(|cx| project.read(cx).visible_worktrees(cx).next().map(|wt| wt.read(cx).abs_path())) else { return };
        let survey = cx.update(|cx| survey(&project, cx)).await;
        let missing: Vec<Tool> = survey.missing(true).iter().map(|s| s.tool).collect();
        if missing.is_empty() {
            return;
        }
        let kvp = cx.update(|cx| KeyValueStore::global(cx));
        let key = dismissed_key(&root, &missing);
        if kvp.read_kvp(&key).ok().flatten().is_some() {
            return;
        }
        let verb = if missing.len() == 1 { "isn't" } else { "aren't" };
        let message = format!("This project uses {}, which {verb} installed.", listing(&missing));
        workspace
            .update(cx, |workspace, cx| {
                workspace.show_notification(NotificationId::unique::<MissingTools>(), cx, move |cx| {
                    cx.new(move |cx| {
                        MessageNotification::new(message, cx)
                            .primary_message("Install…")
                            .primary_on_click(|window, cx| {
                                window.dispatch_action(Box::new(CheckTools), cx);
                                cx.emit(gpui::DismissEvent);
                            })
                            .secondary_message("Not now")
                            .secondary_on_click(move |_, cx| {
                                let (kvp, key) = (kvp.clone(), key.clone());
                                cx.background_spawn(async move { kvp.write_kvp(key, "1".into()).await.log_err() }).detach();
                                cx.emit(gpui::DismissEvent);
                            })
                    })
                });
            })
            .ok();
    })
    .detach();
}

/// What picking a row of Check Tools does.
enum Row {
    Install { tools: Vec<Tool>, command: String },
    Open(&'static str),
    Nothing,
}

/// Forge › Check Tools…: the project's tools first, each installed or not (with why it
/// is needed and how it would be installed), then the others.
pub fn check_tools(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let project = workspace.project().clone();
    let weak = cx.entity().downgrade();
    let survey = survey(&project, cx);
    cx.spawn_in(window, async move |_, cx| {
        let survey = survey.await;
        let (os, manager) = (std::env::consts::OS, survey.manager);
        let mut choices = Vec::new();
        let mut rows = Vec::new();
        let missing: Vec<Tool> = survey.missing(false).iter().map(|s| s.tool).collect();
        if missing.len() > 1 {
            if let Some(command) = catalog::combined(&missing, os, manager) {
                choices.push(Choice::new(format!("Install {}", listing(&missing))).detail(command.clone()));
                rows.push(Row::Install { tools: missing.clone(), command });
            }
        }
        let mut statuses: Vec<&Status> = survey.statuses.iter().collect();
        statuses.sort_by_key(|s| (s.need.is_none(), s.tool));
        for status in statuses {
            let why = status.need.as_ref().map(|n| format!(" · used by {}", n.because)).unwrap_or_default();
            let (label, detail, row) = match &status.found {
                Found::OnPath => (format!("✓ {}", status.tool.name()), format!("installed{why}"), Row::Nothing),
                Found::OffPath(at) => (format!("✓ {}", status.tool.name()), format!("installed in {}, which isn't on your PATH yet (log in again){why}", at.parent().unwrap_or(at).display()), Row::Nothing),
                Found::Missing => match catalog::plan(status.tool, os, manager) {
                    Plan::Run { command, admin, note } => {
                        let how = note.map(|n| format!(" — {n}")).unwrap_or_default();
                        let asks = if admin { " (asks for your password)" } else { "" };
                        (format!("Install {}", status.tool.name()), format!("{command}{asks}{how}{why}"), Row::Install { tools: vec![status.tool], command })
                    }
                    Plan::Manual { url } => (format!("Get {}", status.tool.name()), format!("not installed: download it from {url}{why}"), Row::Open(url)),
                },
            };
            choices.push(Choice::new(label).detail(detail));
            rows.push(row);
        }
        if survey.statuses.iter().any(|s| s.tool == Tool::Dotnet && s.need.is_some()) {
            choices.push(Choice::new("✓ netcoredbg (the .NET debugger)").detail("Forge downloads it the first time you debug a .NET program"));
            rows.push(Row::Nothing);
        }
        let survey = Arc::new(survey);
        cx.update(|window, cx| {
            pick::defer_workspace(weak.clone(), window, cx, move |workspace, window, cx| {
                pick::pick(workspace, "Tools this project uses…", choices, window, cx, move |ix, window, cx| match rows.into_iter().nth(ix) {
                    Some(Row::Install { tools, command }) => {
                        pick::defer_workspace(weak, window, cx, move |workspace, window, cx| install(workspace, tools, command, survey, window, cx));
                    }
                    Some(Row::Open(url)) => cx.open_url(url),
                    _ => {}
                });
            });
        })
        .ok();
    })
    .detach();
}

/// Runs `command` in a terminal tab, then watches for `tools` to show up and says when
/// they did (or that they need a new login to be on the PATH).
fn install(workspace: &mut Workspace, tools: Vec<Tool>, command: String, survey: Arc<Survey>, window: &mut Window, cx: &mut Context<Workspace>) {
    let template = task::TaskTemplate {
        label: format!("Install {}", listing(&tools)),
        command: command.clone(),
        use_new_terminal: true,
        allow_concurrent_runs: false,
        reveal: task::RevealStrategy::Always,
        ..task::TaskTemplate::default()
    };
    let Some(resolved) = template.resolve_task("forge-toolchain", &task::TaskContext::default()) else { return };
    workspace.schedule_resolved_task(project::TaskSourceKind::UserInput, resolved, false, window, cx);
    cx.spawn(async move |workspace, cx| {
        // Installing takes a while (downloads, a password): look every few seconds.
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(20 * 60) {
            cx.background_executor().timer(Duration::from_secs(3)).await;
            let found: Vec<Found> = {
                let (survey, tools) = (survey.clone(), tools.clone());
                cx.background_spawn(async move { tools.iter().map(|&t| survey.find(t)).collect() }).await
            };
            if found.iter().all(|f| *f != Found::Missing) {
                let message = if found.iter().any(|f| matches!(f, Found::OffPath(_))) {
                    format!("{} installed. Log out and in again (or restart your terminal) so it is on your PATH, then restart Forge.", listing(&tools))
                } else {
                    format!("{} installed.", listing(&tools))
                };
                workspace
                    .update(cx, |workspace, cx| {
                        workspace.dismiss_notification(&NotificationId::unique::<MissingTools>(), cx);
                        workspace.show_toast(workspace::Toast::new(NotificationId::named("forge-toolchain-installed".into()), message), cx);
                    })
                    .ok();
                return;
            }
        }
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The project's files say what it needs; the rest is listed but not needed.
    #[gpui::test]
    async fn surveys_what_the_project_needs(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let store = settings::SettingsStore::test(cx);
            cx.set_global(store);
        });
        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree("/work", serde_json::json!({ "go.mod": "module x\n", "tool": { "Cargo.toml": "" }, "Makefile": "" })).await;
        let project = Project::test(fs, ["/work".as_ref()], cx).await;
        let survey = cx.update(|cx| survey(&project, cx)).await;
        let needed: Vec<Tool> = survey.statuses.iter().filter(|s| s.need.is_some()).map(|s| s.tool).collect();
        assert_eq!(needed, [Tool::Git, Tool::Rust, Tool::Go, Tool::Make]);
        assert_eq!(survey.statuses.iter().find(|s| s.tool == Tool::Rust).unwrap().need.as_ref().unwrap().because, "tool/Cargo.toml");
        assert_eq!(survey.statuses.len(), Tool::ALL.len(), "every tool is listed");
        assert_eq!(listing(&[Tool::Go, Tool::Pnpm, Tool::Make]), "Go, pnpm and make");
    }
}
