//! GitHub pull requests in Forge, through the `gh` CLI (see `gh`): the checked-out branch's
//! pull request and its checks in the status bar, a picker of the repository's pull
//! requests (check out, open, review with an agent), and review comments shown under the
//! lines they are about.

pub mod gh;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use editor::Editor;
use editor::display_map::{BlockPlacement, BlockProperties, BlockStyle, CustomBlockId};
use gpui::{App, AppContext as _, Context, Entity, EntityId, Global, IntoElement, ParentElement as _, Render, Styled as _, Subscription, Task, WeakEntity, Window, actions};
use project::Project;
use theme::ActiveTheme as _;
use ui::{ButtonCommon as _, ButtonLike, ButtonStyle, Color, ContextMenu, FluentBuilder as _, Icon, IconName, IconSize, Label, LabelCommon as _, LabelSize, PopoverMenu, Tooltip, h_flex, v_flex};
use workspace::{ItemHandle, StatusItemView, Workspace, notifications::NotificationId};

use crate::gh::{PullRequest, ReviewComment};

actions!(forge_github, [
    /// Lists the repository's open pull requests.
    PullRequests,
    /// Asks an agent to review the checked-out branch's pull request.
    ReviewPullRequest,
    /// Shows the review comments of the checked-out branch's pull request in the code.
    ShowReviewComments,
    /// Reads the checked-out branch's pull request and checks again.
    RefreshPullRequest
]);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, cx| {
        let github = cx.new(|cx| GitHub::new(workspace, cx));
        let id = cx.entity_id();
        cx.default_global::<States>().0.insert(id, github.downgrade());
        let keep = github.clone();
        cx.on_release(move |_, _| drop(keep)).detach();
        workspace.register_action(|ws, _: &PullRequests, window, cx| pick_pull_request(ws, window, cx));
        workspace.register_action(|ws, _: &ReviewPullRequest, window, cx| {
            if let Some(pr) = GitHub::for_workspace(cx.entity_id(), cx).and_then(|g| g.read(cx).current.clone()) {
                review_with_agent(&pr, window, cx);
            } else {
                toast(ws, "This branch has no pull request.".into(), cx);
            }
        });
        workspace.register_action(|ws, _: &ShowReviewComments, window, cx| {
            let github = GitHub::for_workspace(cx.entity_id(), cx);
            match github.as_ref().and_then(|g| g.read(cx).current.as_ref().map(|pr| pr.number)) {
                Some(number) => github.unwrap().update(cx, |g, cx| g.show_comments(number, window, cx)),
                None => toast(ws, "This branch has no pull request.".into(), cx),
            }
        });
        workspace.register_action(|_, _: &RefreshPullRequest, _, cx| {
            if let Some(github) = GitHub::for_workspace(cx.entity_id(), cx) {
                github.update(cx, |g, cx| g.refresh(cx));
            }
        });
    })
    .detach();
}

#[derive(Default)]
struct States(HashMap<EntityId, WeakEntity<GitHub>>);
impl Global for States {}

fn toast(workspace: &mut Workspace, message: String, cx: &mut Context<Workspace>) {
    workspace.show_toast(workspace::Toast::new(NotificationId::named("forge-github".into()), message), cx);
}

/// A workspace's view of GitHub.
pub struct GitHub {
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    /// The checked-out branch's pull request.
    pub current: Option<PullRequest>,
    /// Review comment threads by absolute file path, for the pull request `comments_of`.
    comments: HashMap<PathBuf, Vec<Vec<ReviewComment>>>,
    comments_of: Option<u64>,
    /// Bumped when `comments` changes, so editors know theirs are stale.
    generation: usize,
    refresh: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl GitHub {
    pub fn for_workspace(workspace: EntityId, cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<States>()?.0.get(&workspace)?.upgrade()
    }

    fn new(workspace: &Workspace, cx: &mut Context<Self>) -> Self {
        let project = workspace.project().clone();
        let git_store = project.read(cx).git_store().clone();
        let mut subscriptions = vec![cx.subscribe(&git_store, |this, _, event: &project::git_store::GitStoreEvent, cx| {
            use project::git_store::{GitStoreEvent, RepositoryEvent};
            if matches!(event, GitStoreEvent::ActiveRepositoryChanged(_) | GitStoreEvent::RepositoryUpdated(_, RepositoryEvent::HeadChanged, true)) {
                this.refresh(cx);
            }
        })];
        if let Some(ws) = workspace.weak_handle().upgrade() {
            subscriptions.push(cx.subscribe(&ws, |this, _, event: &workspace::Event, cx| {
                if matches!(event, workspace::Event::ActiveItemChanged) {
                    this.decorate_active_editor(cx);
                }
            }));
        }
        let mut this = Self { workspace: workspace.weak_handle(), project, current: None, comments: HashMap::new(), comments_of: None, generation: 0, refresh: None, _subscriptions: subscriptions };
        this.refresh(cx);
        this
    }

    fn root(&self, cx: &App) -> Option<PathBuf> {
        self.project.read(cx).visible_worktrees(cx).next().map(|t| t.read(cx).abs_path().to_path_buf())
    }

    /// The project's shell environment, for running `gh`.
    fn environment(&self, dir: PathBuf, cx: &mut Context<Self>) -> Task<HashMap<String, String>> {
        let task = self.project.update(cx, |p, cx| p.environment().update(cx, |env, cx| env.directory_environment(dir.into(), cx)));
        cx.background_spawn(async move { task.await.map(|env| env.into_iter().collect()).unwrap_or_default() })
    }

    /// Reads the branch's pull request, and again later: every 30 seconds while checks
    /// run, every 5 minutes otherwise.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.root(cx) else { return };
        self.refresh = Some(cx.spawn(async move |this, cx| {
            loop {
                let Ok(env) = this.update(cx, |this, cx| this.environment(root.clone(), cx)) else { break };
                let env = env.await;
                let root = root.clone();
                let result = cx.background_spawn(async move { gh::current(&root, &env).await }).await;
                let pending = this
                    .update(cx, |this, cx| {
                        match result {
                            Ok(pr) => this.current = pr,
                            Err(e) => {
                                log::debug!("no pull request status: {e:#}");
                                this.current = None;
                            }
                        }
                        cx.notify();
                        this.current.as_ref().is_some_and(|pr| pr.checks.pending > 0)
                    })
                    .unwrap_or(false);
                let wait = if pending { Duration::from_secs(30) } else { Duration::from_secs(300) };
                cx.background_executor().timer(wait).await;
                if this.upgrade().is_none() {
                    break;
                }
            }
        }));
    }

    /// Loads the review comments of pull request `number` and shows them in the code; opens
    /// a picker of the threads to jump to one.
    pub fn show_comments(&mut self, number: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(root) = self.root(cx) else { return };
        let env = self.environment(root.clone(), cx);
        cx.spawn_in(window, async move |this, cx| {
            let env = env.await;
            let result = cx
                .background_spawn(async move {
                    let top = ide_api::std_command("git").arg("-C").arg(&root).args(["rev-parse", "--show-toplevel"]).output().ok().filter(|o| o.status.success()).map(|o| PathBuf::from(String::from_utf8_lossy(&o.stdout).trim())).unwrap_or(root.clone());
                    gh::comments(&root, number, &env).await.map(|comments| (top, comments))
                })
                .await;
            this.update_in(cx, |this, window, cx| match result {
                Ok((top, comments)) => {
                    this.set_comments(number, &top, comments, cx);
                    this.pick_comment(window, cx);
                }
                Err(e) => this.notify(format!("Couldn't read the review comments: {e:#}"), cx),
            })
            .ok();
        })
        .detach();
    }

    fn notify(&self, message: String, cx: &mut App) {
        if let Some(workspace) = self.workspace.upgrade() {
            workspace.update(cx, |ws, cx| toast(ws, message, cx));
        }
    }

    pub fn set_comments(&mut self, number: u64, top: &Path, comments: Vec<ReviewComment>, cx: &mut Context<Self>) {
        self.comments.clear();
        for thread in gh::threads(&comments) {
            self.comments.entry(top.join(&thread[0].path)).or_default().push(thread);
        }
        self.comments_of = Some(number);
        self.generation += 1;
        let editors: Vec<Entity<Editor>> = self.workspace.upgrade().map(|ws| ws.read(cx).items_of_type::<Editor>(cx).collect()).unwrap_or_default();
        for editor in editors {
            self.decorate(&editor, cx);
        }
        cx.notify();
    }

    fn decorate_active_editor(&mut self, cx: &mut Context<Self>) {
        let editor = self.workspace.upgrade().and_then(|ws| ws.read(cx).active_item(cx)?.downcast::<Editor>());
        if let Some(editor) = editor {
            self.decorate(&editor, cx);
        }
    }

    /// Shows the review comments on `editor`'s file under their lines, replacing older ones.
    fn decorate(&self, editor: &Entity<Editor>, cx: &mut App) {
        let Some(path) = editor.read(cx).buffer().read(cx).as_singleton().and_then(|b| Some(b.read(cx).file()?.as_local()?.abs_path(cx))) else { return };
        let threads = self.comments.get(&path).cloned().unwrap_or_default();
        let generation = self.generation;
        editor.update(cx, |editor, cx| {
            let previous = editor.addon::<CommentsAddon>().map(|a| (a.generation, a.blocks.clone()));
            if previous.as_ref().is_some_and(|(g, _)| *g == generation) {
                return;
            }
            if let Some((_, blocks)) = previous {
                editor.remove_blocks(blocks.into_iter().collect(), None, cx);
            }
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let max_row = snapshot.max_point().row;
            let blocks: Vec<BlockProperties<editor::Anchor>> = threads
                .into_iter()
                .filter_map(|thread| {
                    let row = thread[0].line?.saturating_sub(1).min(max_row);
                    let anchor = snapshot.anchor_after(language::Point::new(row, snapshot.line_len(multi_buffer::MultiBufferRow(row))));
                    let height = thread.iter().map(|c| 1 + c.body.lines().count().max(1)).sum::<usize>() as u32 + 1;
                    Some(BlockProperties {
                        placement: BlockPlacement::Below(anchor),
                        height: Some(height),
                        style: BlockStyle::Flex,
                        render: Arc::new(move |cx| render_thread(&thread, cx)),
                        priority: 0,
                    })
                })
                .collect();
            let ids = editor.insert_blocks(blocks, None, cx);
            editor.register_addon(CommentsAddon { generation, blocks: ids });
        });
    }

    /// A picker of the comment threads; the chosen one opens at its line.
    fn pick_comment(&self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else { return };
        let mut threads: Vec<(PathBuf, Vec<ReviewComment>)> = self.comments.iter().flat_map(|(path, threads)| threads.iter().map(move |t| (path.clone(), t.clone()))).collect();
        threads.sort_by(|a, b| (&a.0, a.1[0].line).cmp(&(&b.0, b.1[0].line)));
        if threads.is_empty() {
            return self.notify("The pull request has no review comments.".into(), cx);
        }
        let choices = threads
            .iter()
            .map(|(_, thread)| {
                let first = &thread[0];
                let line = first.line.map(|l| format!(":{l}")).unwrap_or_else(|| " (outdated)".into());
                let replies = if thread.len() > 1 { format!(" · {} replies", thread.len() - 1) } else { String::new() };
                forge_ui::pick::Choice::new(first.body.lines().next().unwrap_or_default().to_string()).detail(format!("{}{line} · @{}{replies}", first.path, first.author))
            })
            .collect();
        let weak = workspace.downgrade();
        workspace.update(cx, |ws, cx| {
            forge_ui::pick::pick(ws, "Review comments…", choices, window, cx, move |ix, window, cx| {
                let Some((path, thread)) = threads.get(ix).cloned() else { return };
                let row = thread[0].line.unwrap_or(1).saturating_sub(1);
                weak.update(cx, |ws, cx| {
                    let open = ws.open_abs_path(path, workspace::OpenOptions::default(), window, cx);
                    cx.spawn_in(window, async move |_, cx| {
                        let item = open.await?;
                        if let Some(editor) = item.downcast::<Editor>() {
                            editor.update_in(cx, |editor, window, cx| {
                                let point = language::Point::new(row, 0);
                                editor.change_selections(editor::SelectionEffects::scroll(editor::scroll::Autoscroll::center()), window, cx, |s| s.select_ranges([point..point]));
                            })?;
                        }
                        anyhow::Ok(())
                    })
                    .detach();
                })
                .ok();
            });
        });
    }
}

/// Which comment blocks an editor shows.
struct CommentsAddon {
    generation: usize,
    blocks: Vec<CustomBlockId>,
}

impl editor::Addon for CommentsAddon {
    fn to_any(&self) -> &dyn std::any::Any {
        self
    }
}

fn render_thread(thread: &[ReviewComment], cx: &mut editor::display_map::BlockContext) -> gpui::AnyElement {
    let colors = cx.theme().colors().clone();
    let mut card = v_flex().ml(cx.margins.gutter.width).mr_4().my_0p5().px_2().border_l_2().border_color(colors.border_focused).bg(colors.surface_background);
    for comment in thread {
        card = card.child(h_flex().gap_1().child(Icon::new(IconName::Chat).size(IconSize::XSmall).color(Color::Muted)).child(Label::new(format!("@{}", comment.author)).size(LabelSize::Small).color(Color::Accent)));
        for line in comment.body.lines() {
            card = card.child(Label::new(line.to_string()).size(LabelSize::Small).truncate());
        }
    }
    card.into_any_element()
}

/// The repository's open pull requests; the chosen one can be checked out, opened, reviewed
/// by an agent or have its comments shown.
fn pick_pull_request(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let Some(github) = GitHub::for_workspace(cx.entity_id(), cx) else { return };
    let Some(root) = github.read(cx).root(cx) else { return };
    let env = github.update(cx, |g, cx| g.environment(root.clone(), cx));
    let _ = workspace;
    cx.spawn_in(window, async move |workspace, cx| {
        let env = env.await;
        let (list_root, list_env) = (root.clone(), env.clone());
        let result = cx.background_spawn(async move { gh::list(&list_root, &list_env).await }).await;
        workspace.update_in(cx, |ws, window, cx| {
            let prs = match result {
                Ok(prs) if prs.is_empty() => return toast(ws, "No open pull requests.".into(), cx),
                Ok(prs) => prs,
                Err(e) => return toast(ws, format!("Couldn't list the pull requests: {e:#}"), cx),
            };
            let choices = prs
                .iter()
                .map(|pr| {
                    let draft = if pr.draft { " · draft" } else { "" };
                    forge_ui::pick::Choice::new(format!("#{} {}", pr.number, pr.title)).detail(format!("@{} · {}{draft} · {}", pr.author, pr.branch, pr.checks.summary()))
                })
                .collect();
            let weak = cx.entity().downgrade();
            forge_ui::pick::pick(ws, "Pull requests…", choices, window, cx, move |ix, window, cx| {
                let Some(pr) = prs.get(ix).cloned() else { return };
                let actions = ["Check out", "Open on GitHub", "Review with an agent", "Show review comments in the code"];
                let choices = actions.iter().map(|a| forge_ui::pick::Choice::new(*a)).collect();
                let (weak2, root, env) = (weak.clone(), root.clone(), env.clone());
                forge_ui::pick::defer_workspace(weak, window, cx, move |ws, window, cx| {
                    forge_ui::pick::pick(ws, &format!("#{} {}…", pr.number, pr.title), choices, window, cx, move |action, window, cx| match action {
                        0 => {
                            let number = pr.number.to_string();
                            let task = cx.background_spawn(async move { gh::run(&root, &["pr", "checkout", &number], &env).await });
                            cx.spawn(async move |cx| {
                                let message = match task.await {
                                    Ok(_) => format!("Checked out #{} ({}).", pr.number, pr.branch),
                                    Err(e) => format!("Couldn't check out #{}: {e:#}", pr.number),
                                };
                                weak2.update(cx, |ws, cx| toast(ws, message, cx)).ok();
                            })
                            .detach();
                        }
                        1 => cx.open_url(&pr.url),
                        2 => review_with_agent(&pr, window, cx),
                        _ => {
                            if let Some(github) = weak2.upgrade().and_then(|ws| GitHub::for_workspace(ws.entity_id(), cx)) {
                                github.update(cx, |g, cx| g.show_comments(pr.number, window, cx));
                            }
                        }
                    });
                });
            });
        })
    })
    .detach();
}

/// What an agent is asked to review a pull request.
pub fn review_prompt(pr: &PullRequest) -> String {
    format!(
        "Review pull request #{} \"{}\" by @{} ({} into {}): {}\n\n\
         Read it with `gh pr diff {}`, and `gh pr view {} --comments` for the discussion so far. Don't change any files. \
         Report the problems worth fixing (bugs, missing tests, risky or unclear changes), most serious first, each as `path:line: what and why`. \
         If you find nothing important, say so plainly.",
        pr.number, pr.title, pr.author, pr.branch, if pr.base.is_empty() { "the default branch" } else { &pr.base }, pr.url, pr.number, pr.number
    )
}

fn review_with_agent(pr: &PullRequest, window: &mut Window, cx: &mut App) {
    window.dispatch_action(Box::new(forge_ui::AskAgent { prompt: review_prompt(pr) }), cx);
}

/// The status bar's pull request: number and checks; its menu opens, reviews and lists.
pub struct PullRequestStatus {
    github: Option<Entity<GitHub>>,
    _subscription: Option<Subscription>,
}

impl PullRequestStatus {
    pub fn new(workspace: &Workspace, cx: &mut Context<Self>) -> Self {
        let github = GitHub::for_workspace(workspace.weak_handle().entity_id(), cx);
        let subscription = github.as_ref().map(|g| cx.observe(g, |_, _, cx| cx.notify()));
        Self { github, _subscription: subscription }
    }
}

impl Render for PullRequestStatus {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(pr) = self.github.as_ref().and_then(|g| g.read(cx).current.clone()) else { return h_flex().into_any_element() };
        let (icon, color) = match pr.checks {
            c if c.failed > 0 => (IconName::XCircle, Color::Error),
            c if c.pending > 0 => (IconName::ArrowCircle, Color::Warning),
            c if c.passed > 0 => (IconName::Check, Color::Success),
            _ => (IconName::Circle, Color::Muted),
        };
        let tooltip = format!("#{} {}\nChecks: {}{}", pr.number, pr.title, pr.checks.summary(), pr.review.as_deref().map(|r| format!("\nReview: {}", r.to_lowercase().replace('_', " "))).unwrap_or_default());
        let url = pr.url.clone();
        PopoverMenu::new("forge-pr-status")
            .trigger(
                ButtonLike::new("forge-pr-status-button")
                    .style(ButtonStyle::Subtle)
                    .child(
                        h_flex()
                            .gap_1()
                            .child(Icon::new(IconName::PullRequest).size(IconSize::Small).color(Color::Muted))
                            .child(Label::new(format!("#{}", pr.number)).size(LabelSize::Small))
                            .child(Icon::new(icon).size(IconSize::XSmall).color(color))
                            .when(pr.checks.total() > 0, |el| el.child(Label::new(pr.checks.summary()).size(LabelSize::Small).color(color))),
                    )
                    .tooltip(Tooltip::text(tooltip)),
            )
            .menu(move |window, cx| {
                let url = url.clone();
                Some(ContextMenu::build(window, cx, move |menu, _, _| {
                    menu.entry("Open on GitHub", None, move |_, cx| cx.open_url(&url))
                        .action("Show Review Comments", Box::new(ShowReviewComments))
                        .action("Review with an Agent", Box::new(ReviewPullRequest))
                        .separator()
                        .action("All Pull Requests…", Box::new(PullRequests))
                        .action("Refresh", Box::new(RefreshPullRequest))
                }))
            })
            .into_any_element()
    }
}

impl StatusItemView for PullRequestStatus {
    fn set_active_pane_item(&mut self, _: Option<&dyn ItemHandle>, _: &mut Window, _: &mut Context<Self>) {}

    fn hide_setting(&self, _: &App) -> Option<workspace::HideStatusItem> {
        None
    }
}

#[cfg(test)]
mod tests;
