//! Threads in the editor area: one tab per conversation, shown as a document (your
//! messages, the agent's answers, the code it reads and the changes it proposes, editable
//! and accepted or rejected right there), with the file the agent works in alongside while
//! it works. The workspace's threads are listed in the Threads panel and the thread picker.

use std::collections::{HashMap, HashSet};


use editor::{Editor, EditorEvent};
use gpui::prelude::FluentBuilder as _;
use gpui::TaskExt as _;
use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, EntityId, EventEmitter, FocusHandle, Focusable, FontWeight, Global, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, WeakEntity, Window,
    FollowMode, ListAlignment, ListState, Task, actions, div, list, px, relative,
};
use std::rc::Rc;
use theme::ActiveTheme as _;
use ui::{
    Button, ButtonCommon as _, ButtonLike, ButtonSize, ButtonStyle, Clickable as _, Color, ContextMenu, Disableable as _, Icon, IconButton, IconName,
    IconSize, Label, LabelCommon as _, LabelSize, PopoverMenu, Toggleable as _, Tooltip, WithScrollbar as _, h_flex, v_flex,
};
use workspace::{
    Workspace,
    item::{Item, ItemEvent},
};

use crate::{
    mentions::{FileMentions, active_context},
    thread::{AgentLocation, Status, Thread, ThreadEvent},
};
use editor::RowHighlightOptions;
use language::Point;

mod changes;
mod composer;
mod follow;
mod turns;
#[cfg(test)]
pub(crate) use composer::tokens;

actions!(forge_agent, [
    NewThread,
    OpenThreads,
    Send,
    Cancel,
    /// Starts a thread that works in a git worktree of its own.
    NewThreadInWorktree,
    /// Applies or removes the project's thread worktrees.
    ManageWorktrees
]);

/// The threads of one workspace.
pub struct ThreadStore {
    threads: Vec<Entity<Thread>>,
    /// Where notifications about the threads lead.
    window: Option<gpui::AnyWindowHandle>,
    workspace: WeakEntity<Workspace>,
    _subscriptions: Vec<Subscription>,
}

impl ThreadStore {
    pub fn threads(&self) -> &[Entity<Thread>] {
        &self.threads
    }

    /// Drops `thread` from the list (its conversation stays saved).
    pub fn remove(&mut self, thread: &Entity<Thread>, cx: &mut Context<Self>) {
        self.threads.retain(|t| t != thread);
        cx.notify();
    }

    fn add(&mut self, thread: Entity<Thread>, cx: &mut Context<Self>) {
        // Lists show each thread's state; follow their changes.
        self._subscriptions.push(cx.subscribe(&thread, |store, thread, _: &ThreadEvent, cx| {
            crate::presence::thread_updated(&thread, store.window, store.workspace.clone(), cx);
            cx.notify();
        }));
        self.threads.insert(0, thread);
        cx.notify();
    }
}

#[derive(Default)]
struct Stores(HashMap<EntityId, WeakEntity<ThreadStore>>);
impl Global for Stores {}

pub fn init(cx: &mut App) {
    cx.bind_keys([
        gpui::KeyBinding::new("secondary-alt-n", NewThread, None),
        gpui::KeyBinding::new("secondary-shift-a", OpenThreads, None),
        gpui::KeyBinding::new("enter", Send, Some("ForgeAgentInput > Editor")),
        gpui::KeyBinding::new("shift-enter", editor::actions::Newline, Some("ForgeAgentInput > Editor")),
        gpui::KeyBinding::new("escape", Cancel, Some("ForgeThread")),
    ]);
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let window = window.map(|w| w.window_handle());
        let weak = cx.weak_entity();
        let store = cx.new(|_| ThreadStore { threads: Vec::new(), window, workspace: weak, _subscriptions: Vec::new() });
        let id = cx.entity_id();
        cx.default_global::<Stores>().0.insert(id, store.downgrade());
        // Keep the store alive as long as the workspace.
        let keep = store.clone();
        cx.on_release(move |_, _| drop(keep)).detach();

        workspace.register_action(|workspace, _: &NewThread, window, cx| {
            new_thread(workspace, None, window, cx);
        });
        workspace.register_action(|workspace, _: &NewThreadInWorktree, window, cx| new_thread_in_worktree(workspace, window, cx));
        workspace.register_action(|workspace, _: &ManageWorktrees, window, cx| manage_worktrees(workspace, window, cx));
        // Stop the agent of the thread on screen (Agents › Stop the Agent).
        workspace.register_action(|workspace, _: &Cancel, _, cx| {
            let view = workspace.active_item(cx).and_then(|item| item.downcast::<ThreadView>()).or_else(|| workspace.items_of_type::<ThreadView>(cx).next());
            if let Some(view) = view {
                let thread = view.read(cx).thread().clone();
                thread.update(cx, |t, cx| t.cancel(cx));
            }
        });
        // "Fix with agent" and friends go to the thread on screen when it is free, or
        // start a new thread with the default agent.
        workspace.register_action(|workspace, action: &forge_ui::AskAgent, window, cx| ask_agent(workspace, action.prompt.clone(), window, cx));
        // Merge conflicts: Zed's "Resolve with Agent" buttons, answered by Forge's threads.
        workspace.register_action(|workspace, action: &zed_actions::agent::ResolveConflictsWithAgent, window, cx| {
            ask_agent(workspace, crate::conflicts::prompt_for_conflicts(&action.conflicts), window, cx)
        });
        workspace.register_action(|workspace, action: &zed_actions::agent::ResolveConflictedFilesWithAgent, window, cx| {
            let mut paths = action.conflicted_file_paths.clone();
            if paths.is_empty() {
                paths = crate::conflicts::conflicted_paths(workspace.project().read(cx), cx);
            }
            if paths.is_empty() {
                let id = workspace::notifications::NotificationId::named("forge-conflicts".into());
                return workspace.show_toast(workspace::Toast::new(id, "There are no merge conflicts."), cx);
            }
            ask_agent(workspace, crate::conflicts::prompt_for_files(&paths), window, cx)
        });
    })
    .detach();
}

/// Sends `prompt` to the thread on screen when it is free, or to a new thread with the
/// default agent.
fn ask_agent(workspace: &mut Workspace, prompt: String, window: &mut Window, cx: &mut Context<Workspace>) {
    let thread = match visible_idle_thread(workspace, cx) {
        Some(thread) => {
            open_view(workspace, thread.clone(), window, cx);
            thread
        }
        None => new_thread(workspace, None, window, cx),
    };
    thread.update(cx, |t, cx| t.ask(prompt, window, cx));
}

/// A thread shown in one of the panes (its tab is the active one) that isn't working.
fn visible_idle_thread(workspace: &Workspace, cx: &App) -> Option<Entity<Thread>> {
    workspace.panes().iter().find_map(|pane| {
        let view = pane.read(cx).active_item()?.downcast::<ThreadView>()?;
        let thread = view.read(cx).thread().clone();
        (thread.read(cx).status() != Status::Busy).then_some(thread)
    })
}

pub(crate) fn store_for(workspace: EntityId, cx: &App) -> Option<Entity<ThreadStore>> {
    cx.try_global::<Stores>()?.0.get(&workspace)?.upgrade()
}

/// Creates a thread with `agent` (an index into the configured agents; the default agent
/// if `None`), shows it in a tab of its own and returns it.
pub fn new_thread(workspace: &mut Workspace, agent: Option<usize>, window: &mut Window, cx: &mut Context<Workspace>) -> Entity<Thread> {
    let thread = cx.new(|cx| Thread::new(workspace, agent, window, cx));
    add_thread(workspace, thread.clone(), window, cx);
    thread
}

fn first_root(workspace: &Workspace, cx: &App) -> Option<std::path::PathBuf> {
    workspace.visible_worktrees(cx).next().map(|t| t.read(cx).abs_path().to_path_buf())
}

fn toast(workspace: &mut Workspace, message: String, cx: &mut Context<Workspace>) {
    workspace.show_toast(workspace::Toast::new(workspace::notifications::NotificationId::named("forge-worktrees".into()), message), cx);
}

/// Creates a git worktree for a new thread with the default agent, then shows the thread.
pub fn new_thread_in_worktree(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let Some(root) = first_root(workspace, cx) else { return toast(workspace, "Open a project first.".into(), cx) };
    let create = cx.background_spawn(async move { crate::worktree::create(&root) });
    cx.spawn_in(window, async move |workspace, cx| {
        let result = create.await;
        workspace.update_in(cx, |workspace, window, cx| match result {
            Ok(worktree) => {
                let branch = worktree.branch();
                let thread = cx.new(|cx| Thread::new_in(workspace, None, Some(worktree), window, cx));
                thread.update(cx, |t, cx| {
                    t.system(format!("This thread works on branch {branch}, in a worktree started from your last commit: your uncommitted changes aren't in it, and its changes stay out of your files until you apply them."), ui::Color::Muted);
                    cx.notify();
                });
                add_thread(workspace, thread, window, cx);
            }
            Err(e) => toast(workspace, format!("Can't start a thread in a worktree: {e:#}"), cx),
        })
    })
    .detach_and_log_err(cx);
}

/// Lists the project's thread worktrees; applies or removes the one picked.
fn manage_worktrees(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let Some(root) = first_root(workspace, cx) else { return };
    let worktrees = crate::worktree::list(&root);
    if worktrees.is_empty() {
        return toast(workspace, "No thread worktrees. Agents › New Thread in Worktree starts one.".into(), cx);
    }
    let threads: Vec<Entity<Thread>> = store_for(cx.entity_id(), cx).map(|s| s.read(cx).threads.clone()).unwrap_or_default();
    let choices = worktrees
        .iter()
        .map(|w| {
            let open = threads.iter().any(|t| t.read(cx).worktree() == Some(w));
            forge_ui::pick::Choice::new(w.branch()).detail(if open { "its thread is open" } else { "no open thread" })
        })
        .collect();
    let weak = cx.entity().downgrade();
    forge_ui::pick::pick(workspace, "Which worktree?", choices, window, cx, move |ix, window, cx| {
        let Some(worktree) = worktrees.get(ix).cloned() else { return };
        let thread = threads.into_iter().find(|t| t.read(cx).worktree() == Some(&worktree));
        let actions = ["Apply its changes to the project", "Remove it and its branch", "Remove it, keep the branch"];
        let choices = actions.iter().map(|a| forge_ui::pick::Choice::new(*a)).collect();
        forge_ui::pick::defer_workspace(weak.clone(), window, cx, move |workspace, window, cx| {
                forge_ui::pick::pick(workspace, &format!("{}…", worktree.branch()), choices, window, cx, move |action, _, cx| {
                    if let Some(thread) = thread {
                        thread.update(cx, |t, cx| match action {
                            0 => t.apply_worktree(cx),
                            _ => t.remove_worktree(action == 2, cx),
                        });
                        return;
                    }
                    let task = cx.background_spawn(async move {
                        match action {
                            0 => crate::worktree::apply(&worktree).map(|applied| match applied {
                                crate::worktree::Applied::Clean { files } => format!("Applied the changes to {files} file(s); review them in the Git panel."),
                                crate::worktree::Applied::Conflicts(files) => format!("Applied with conflicts in {}.", files.join(", ")),
                            }),
                            _ => crate::worktree::remove(&worktree, action == 2).map(|_| "Removed the worktree.".to_string()),
                        }
                    });
                    cx.spawn(async move |cx| {
                        let message = task.await.unwrap_or_else(|e| format!("{e:#}"));
                        weak.update(cx, |workspace, cx| toast(workspace, message, cx)).ok();
                    })
                    .detach();
                });
        });
    });
}

/// Adds `thread` to the workspace's threads without showing it.
pub fn register_thread(_workspace: &mut Workspace, thread: Entity<Thread>, cx: &mut Context<Workspace>) {
    if let Some(store) = store_for(cx.entity_id(), cx) {
        store.update(cx, |store, cx| store.add(thread, cx));
    }
}

/// Adds `thread` to the workspace's threads and shows it.
pub fn add_thread(workspace: &mut Workspace, thread: Entity<Thread>, window: &mut Window, cx: &mut Context<Workspace>) {
    if let Some(store) = store_for(cx.entity_id(), cx) {
        store.update(cx, |store, cx| store.add(thread.clone(), cx));
    }
    open_view(workspace, thread, window, cx);
}

/// Shows `thread` in its tab, opening one if it has none.
pub fn open_view(workspace: &mut Workspace, thread: Entity<Thread>, window: &mut Window, cx: &mut Context<Workspace>) {
    let existing = workspace.items_of_type::<ThreadView>(cx).find(|view| *view.read(cx).thread() == thread);
    if let Some(view) = existing {
        workspace.activate_item(&view, true, true, window, cx);
        return;
    }
    let view = cx.new(|cx| ThreadView::new(workspace, thread, window, cx));
    workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
}

pub struct ThreadView {
    thread: Entity<Thread>,
    workspace: WeakEntity<Workspace>,
    input: Entity<Editor>,
    /// What the active-file chip showed last: repaint only when it changes.
    active_shown: Option<(std::path::PathBuf, Option<std::ops::Range<u32>>)>,
    /// The conversation, a row per turn (see `turns::render_row`).
    list: ListState,
    focus_handle: FocusHandle,
    include_active: bool,
    /// Images pasted or dropped in the composer, sent with the next message.
    images: Vec<std::sync::Arc<gpui::Image>>,
    seen_entries: usize,
    /// The file the agent is working in, shown next to the thread while it works.
    follow: Option<Follow>,
    /// The write waiting for review in the followed file, shown in the follow pane as a diff
    /// of what the agent proposes: its entry and the view.
    follow_review: Option<(usize, crate::diff::DiffView)>,
    follow_paused: bool,
    follow_hidden: bool,
    /// The changed-files list under the conversation is open (it is, until folded).
    changes_open: bool,
    /// Tool calls and thoughts the user folded or unfolded against their default.
    toggled: HashSet<String>,
    /// Turn file diffs unfolded in the conversation, by message and file.
    turn_diffs: HashMap<(usize, std::path::PathBuf), crate::diff::DiffView>,
    /// Repaints the working time while the agent works.
    _ticker: Option<Task<()>>,
    _thread_subscription: Subscription,
    _subscriptions: Vec<Subscription>,
}

/// Highlights the agent's line in the followed file.
struct AgentLine;

struct Follow {
    location: AgentLocation,
    editor: Option<Entity<Editor>>,
    _open: Option<Task<()>>,
    /// The file before the agent's changes, shown as a diff in the editor.
    diff_base: Option<String>,
    _diff: Option<Task<()>>,
}

impl ThreadView {
    fn new(workspace: &Workspace, thread: Entity<Thread>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let project = workspace.project().clone();
        let input = cx.new(|cx| {
            let mut editor = Editor::auto_height(1, 12, window, cx);
            editor.set_placeholder_text("Ask the agent — @ for files, symbols, @problems, @diff, @terminal; / for commands", window, cx);
            editor.set_completion_provider(Some(Rc::new(FileMentions::new(project.downgrade()).with_commands(thread.downgrade()))));
            editor
        });
        // The input repaints itself (and so this view) as you type; nothing else to do here.
        let mut subscriptions = vec![cx.subscribe(&input, |_, _, event: &EditorEvent, cx| {
            if matches!(event, EditorEvent::Focused | EditorEvent::Blurred) {
                cx.notify();
            }
        })];
        if let Some(ws) = workspace.weak_handle().upgrade() {
            // Keep the active-file chip current.
            subscriptions.push(cx.observe(&ws, |this: &mut Self, ws, cx| {
                let shown = active_context(ws.read(cx), cx).map(|a| (a.path, a.selection.map(|(lines, _)| lines)));
                if shown != this.active_shown {
                    this.active_shown = shown;
                    cx.notify();
                }
            }));
        }
        let thread_subscription = Self::subscribe_thread(&thread, window, cx);
        Self {
            thread,
            workspace: workspace.weak_handle(),
            input,
            active_shown: None,
            list: {
                let list = ListState::new(0, ListAlignment::Top, px(1024.));
                list.set_follow_mode(FollowMode::Tail);
                list
            },
            focus_handle: cx.focus_handle(),
            include_active: true,
            images: Vec::new(),
            seen_entries: 0,
            follow: None,
            follow_review: None,
            follow_paused: false,
            follow_hidden: false,
            changes_open: true,
            toggled: HashSet::new(),
            turn_diffs: HashMap::new(),
            _ticker: None,
            _thread_subscription: thread_subscription,
            _subscriptions: subscriptions,
        }
    }

    fn subscribe_thread(thread: &Entity<Thread>, window: &mut Window, cx: &mut Context<Self>) -> Subscription {
        cx.subscribe_in(thread, window, |this, thread, event, window, cx| match event {
            ThreadEvent::Updated => {
                let count = thread.read(cx).entries.len();
                if count != this.seen_entries {
                    this.seen_entries = count;
                    this.sync_list(cx);
                    this.list.scroll_to_end();
                }
                this.sync_follow(window, cx);
                this.sync_follow_review(window, cx);
                this.tick_while_working(cx);
                cx.emit(ItemEvent::UpdateTab);
                cx.notify();
            }
            ThreadEvent::RestoreInput(text) => {
                this.input.update(cx, |e, cx| {
                    e.set_text(text.clone(), window, cx);
                    e.move_to_end(&editor::actions::MoveToEnd, window, cx);
                });
                window.focus(&this.input.focus_handle(cx), cx);
            }
            ThreadEvent::ChangesUpdated => {
                this.update_follow_diff(window, cx);
                cx.notify();
            }
            ThreadEvent::ShowChanges(focus) => this.open_review(focus.clone(), window, cx),
        })
    }

    pub fn thread(&self) -> &Entity<Thread> {
        &self.thread
    }

    /// Repaints every second while the agent works, so its working time stays current.
    fn tick_while_working(&mut self, cx: &mut Context<Self>) {
        let busy = self.thread.read(cx).status() == Status::Busy;
        if !busy {
            self._ticker = None;
            return;
        }
        if self._ticker.is_none() {
            self._ticker = Some(cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(std::time::Duration::from_secs(1)).await;
                    let busy = this.update(cx, |this, cx| {
                        cx.notify();
                        this.thread.read(cx).status() == Status::Busy
                    });
                    if !matches!(busy, Ok(true)) {
                        break;
                    }
                }
                this.update(cx, |this, _| this._ticker = None).ok();
            }));
        }
    }

    fn render_header(&self, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors().clone();
        let thread = self.thread.read(cx);
        let title = thread.title().unwrap_or_else(|| "New thread".into());
        let (summary, color) = thread.summary();
        let status = thread.status();
        let agents: Vec<(usize, String)> = thread.agents().iter().enumerate().map(|(i, a)| (i, a.id.clone())).collect();
        let agent_label = thread.agent_label();
        let handle = self.thread.downgrade();
        let agent = PopoverMenu::new("thread-agent")
            .trigger(
                ButtonLike::new("thread-agent-trigger")
                    .style(ButtonStyle::Subtle)
                    .disabled(status != Status::Disconnected || agents.len() < 2)
                    .child(
                        h_flex()
                            .gap_1()
                            .child(Icon::from_path("icons/forge_agents.svg").size(IconSize::XSmall).color(Color::Accent))
                            .child(Label::new(agent_label).size(LabelSize::Small))
                            .when(status == Status::Disconnected && agents.len() > 1, |el| el.child(Icon::new(IconName::ChevronDown).size(IconSize::XSmall).color(Color::Muted))),
                    )
                    .tooltip(Tooltip::text("The agent this thread talks to")),
            )
            .menu(move |window, cx| {
                let (agents, handle) = (agents.clone(), handle.clone());
                Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                    for (i, id) in agents {
                        let handle = handle.clone();
                        menu = menu.entry(id, None, move |_, cx| {
                            handle.update(cx, |t, cx| t.select_agent(i, cx)).ok();
                        });
                    }
                    menu
                }))
            });
        h_flex()
            .justify_between()
            .gap_2()
            .px_4()
            .py_2()
            .border_b_1()
            .border_color(colors.border)
            .child(
                h_flex()
                    .gap_2()
                    .min_w_0()
                    .child(Label::new(title).weight(FontWeight::SEMIBOLD).truncate())
                    .child(div().px_2().rounded_full().bg(colors.element_background).child(Label::new(summary).size(LabelSize::XSmall).color(color))),
            )
            .child(
                h_flex()
                    .gap_1()
                    .when(self.follow.is_some() && self.follow_hidden, |el| {
                        el.child(Button::new("follow-open", "Follow agent").style(ButtonStyle::Subtle).size(ButtonSize::Compact).on_click(cx.listener(|this, _, window, cx| this.jump_to_agent(window, cx))))
                    })
                    .child(agent)
                    .when(status == Status::Busy, |el| {
                        el.child(Button::new("thread-stop", "Stop").size(ButtonSize::Compact).start_icon(Icon::new(IconName::Stop).size(IconSize::XSmall)).on_click(cx.listener(|this, _, window, cx| this.cancel(&Cancel, window, cx))))
                    }),
            )
            .into_any_element()
    }

    /// For threads in a worktree: where they work, and applying or removing their changes.
    fn render_worktree(&self, window: &mut Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let thread = self.thread.read(cx);
        let worktree = thread.worktree()?;
        let busy = thread.status() == Status::Busy;
        let colors = cx.theme().colors().clone();
        let _ = window;
        Some(
            h_flex()
                .justify_between()
                .gap_2()
                .px_4()
                .py_1()
                .border_b_1()
                .border_color(colors.border)
                .bg(colors.surface_background)
                .child(
                    h_flex()
                        .gap_2()
                        .min_w_0()
                        .child(Icon::new(IconName::GitBranch).size(IconSize::Small).color(Color::Muted))
                        .child(Label::new(worktree.branch()).size(LabelSize::Small))
                        .child(Label::new("its own copy of the project").size(LabelSize::Small).color(Color::Muted).truncate()),
                )
                .child(
                    h_flex()
                        .gap_1()
                        .child(
                            Button::new("worktree-apply", "Apply to project")
                                .size(ButtonSize::Compact)
                                .disabled(busy)
                                .tooltip(Tooltip::text("Copy the agent's changes into your files, uncommitted, to review"))
                                .on_click(cx.listener(|this, _, _, cx| this.thread.update(cx, |t, cx| t.apply_worktree(cx)))),
                        )
                        .child(
                            Button::new("worktree-remove", "Remove…")
                                .size(ButtonSize::Compact)
                                .style(ButtonStyle::Subtle)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    let answer = window.prompt(
                                        gpui::PromptLevel::Warning,
                                        "Remove this thread's worktree?",
                                        Some("Changes you haven't applied are lost, unless you keep the branch (committed changes only)."),
                                        &["Remove", "Remove, keep the branch", "Cancel"],
                                        cx,
                                    );
                                    let thread = this.thread.downgrade();
                                    cx.spawn(async move |_, cx| {
                                        let answer = answer.await.ok()?;
                                        if answer < 2 {
                                            thread.update(cx, |t, cx| t.remove_worktree(answer == 1, cx)).ok();
                                        }
                                        Some(())
                                    })
                                    .detach();
                                })),
                        ),
                )
                .into_any_element(),
        )
    }

    /// While the agent works: what it is doing, for how long, and Stop. Waiting for you is
    /// shown in the warning colour.
    fn render_working(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let thread = self.thread.read(cx);
        let (activity, waiting) = thread.activity()?;
        let elapsed = thread.checkpoints.last().map(|c| turns::duration(c.started.elapsed())).unwrap_or_default();
        let colors = cx.theme().colors().clone();
        let status = cx.theme().status().clone();
        Some(centered(
            h_flex()
                .w_full()
                .mb_2()
                .px_3()
                .py_1p5()
                .gap_2()
                .rounded_md()
                .border_1()
                .border_color(if waiting { status.warning_border } else { colors.border })
                .bg(if waiting { status.warning_background } else { colors.surface_background })
                .child(if waiting { Icon::new(IconName::Warning).size(IconSize::Small).color(Color::Warning).into_any_element() } else { turns::spinner("working-spinner", IconSize::Small, Color::Accent) })
                .child(div().flex_1().min_w_0().child(Label::new(activity).size(LabelSize::Small).color(if waiting { Color::Warning } else { Color::Default }).truncate()))
                .child(Label::new(elapsed).size(LabelSize::Small).color(Color::Muted))
                .child(
                    Button::new("working-stop", "Stop")
                        .style(ButtonStyle::Subtle)
                        .size(ButtonSize::Compact)
                        .start_icon(Icon::new(IconName::Stop).size(IconSize::XSmall))
                        .on_click(cx.listener(|this, _, window, cx| this.cancel(&Cancel, window, cx))),
                ),
        ))
    }

}

impl ThreadView {
    /// Tells the list how many rows the conversation has now; the latest turn (which
    /// grows as the agent works) is measured again.
    fn sync_list(&mut self, cx: &App) {
        let rows = turns::row_count(&self.thread.read(cx).entries);
        let old = self.list.item_count();
        if rows > old && old > 0 {
            self.list.splice(old - 1..old, rows - old + 1);
        } else if rows != old {
            self.list.reset(rows);
        }
    }

    /// The conversation, or what to ask when it hasn't started.
    fn render_document(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        self.sync_list(cx);
        if self.list.item_count() == 0 {
            return v_flex()
                .flex_1()
                .min_h_0()
                .gap_1()
                .pt_16()
                .px_4()
                .items_center()
                .child(Icon::from_path("icons/forge_agents.svg").size(IconSize::Medium).color(Color::Accent))
                .child(Label::new("What should we work on?").size(LabelSize::Large))
                .child(Label::new("The agent reads and changes your code right here; you review every change before it is written.").color(Color::Muted))
                .into_any_element();
        }
        let list_state = self.list.clone();
        v_flex()
            .flex_1()
            .min_h_0()
            .w_full()
            .child(list(self.list.clone(), cx.processor(|this, ix, window, cx| this.render_row(ix, window, cx))).flex_1().min_h_0().w_full())
            .vertical_scrollbar_for(&list_state, window, cx)
            .into_any_element()
    }
}

impl Render for ThreadView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors().clone();
        let header = self.render_header(cx);
        let worktree = self.render_worktree(window, cx);
        let changes = self.render_changes(window, cx);
        let composer = self.render_composer(cx);
        let follow = self.render_follow(cx);

        let working = self.render_working(cx);
        let document = self.render_document(window, cx);

        h_flex()
            .key_context("ForgeThread")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::send))
            .on_action(cx.listener(Self::cancel))
            .size_full()
            .bg(colors.editor_background)
            .child(v_flex().flex_1().min_w_0().h_full().child(header).children(worktree).child(document).children(working).children(changes).child(composer))
            .children(follow)
    }
}

/// `element` in the conversation's column: as wide as the messages and the input, centered.
fn centered(element: impl IntoElement) -> AnyElement {
    h_flex().w_full().justify_center().px_4().child(v_flex().w_full().min_w_0().max_w(px(880.)).child(element)).into_any_element()
}

impl Focusable for ThreadView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.input.focus_handle(cx)
    }
}

impl EventEmitter<ItemEvent> for ThreadView {}

impl Item for ThreadView {
    type Event = ItemEvent;

    fn tab_content_text(&self, _detail: usize, cx: &App) -> SharedString {
        let title = self.thread.read(cx).title().unwrap_or_else(|| "New thread".into());
        let title: String = title.chars().take(32).collect();
        title.into()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::from_path("icons/forge_agents.svg"))
    }

    fn tab_tooltip_text(&self, cx: &App) -> Option<SharedString> {
        self.thread.read(cx).title().map(SharedString::from)
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}

#[cfg(test)]
mod tests;
