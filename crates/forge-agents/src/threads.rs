//! Threads in the editor area: one tab per conversation, shown as a document (your
//! messages, the agent's answers, the code it reads and the changes it proposes, editable
//! and accepted or rejected right there), with the file the agent works in alongside while
//! it works. The workspace's threads are listed in the Threads panel and the thread picker.

use std::collections::{HashMap, HashSet};

mod turns;


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

    fn follow_visible(&self) -> bool {
        self.follow.is_some() && !self.follow_hidden
    }

    /// Opens `path` beside the thread.
    fn open_file(&mut self, path: std::path::PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else { return };
        workspace.update(cx, |ws, cx| ws.open_abs_path(path, workspace::OpenOptions::default(), window, cx).detach_and_log_err(cx));
    }

    /// A tab beside the thread with `paths` against how they were before the turn that
    /// started at the message `user_ix`.
    fn open_turn_review(&mut self, user_ix: usize, paths: Vec<std::path::PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(turn) = self.thread.read(cx).turn_at(user_ix).cloned() else { return };
        let baselines: Vec<(std::path::PathBuf, String)> = paths.into_iter().filter_map(|p| turn.turn_files.get(&p).map(|(before, _)| (p.clone(), before.clone().unwrap_or_default()))).collect();
        let title = format!("Changes · turn {}", self.thread.read(cx).checkpoints.iter().position(|c| c.entry == user_ix).map(|n| n + 1).unwrap_or(1));
        self.open_diffs(baselines, None, title, window, cx);
    }

    /// Rolls back the turn that started at the message `user_ix` (only `path` if given),
    /// asking first when a later turn changed those files again.
    fn undo_turn(&mut self, user_ix: usize, path: Option<std::path::PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        let later = self.thread.read(cx).later_turns_touching(user_ix, path.as_deref());
        if later.is_empty() {
            self.thread.update(cx, |t, cx| t.undo_turn(user_ix, path, cx));
            return;
        }
        let root = self.thread.read(cx).root().clone();
        let names: Vec<String> = later.iter().map(|p| p.strip_prefix(&root).unwrap_or(p).to_string_lossy().into_owned()).collect();
        let detail = format!("Later turns changed {} again; undoing this turn loses those changes too.", names.join(", "));
        let answer = window.prompt(gpui::PromptLevel::Warning, "Undo this turn's changes?", Some(&detail), &["Undo", "Cancel"], cx);
        let thread = self.thread.downgrade();
        cx.spawn(async move |_, cx| {
            if answer.await.ok() == Some(0) {
                thread.update(cx, |t, cx| t.undo_turn(user_ix, path, cx)).ok();
            }
        })
        .detach();
    }

    /// A tab beside the thread (split to the right) with each file against `baseline`.
    fn open_diffs(&mut self, baselines: Vec<(std::path::PathBuf, String)>, focus: Option<std::path::PathBuf>, title: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else { return };
        let languages = self.thread.read(cx).languages().clone();
        let project = workspace.read(cx).project().clone();
        let opens: Vec<_> = baselines.iter().map(|(path, _)| project.update(cx, |p, cx| p.open_local_buffer(path, cx))).collect();
        cx.spawn_in(window, async move |_, cx| {
            let mut files = Vec::new();
            let mut focus_ix = None;
            for ((path, baseline), open) in baselines.into_iter().zip(opens) {
                let Ok(buffer) = open.await else { continue };
                if focus.as_ref() == Some(&path) {
                    focus_ix = Some(files.len());
                }
                files.push((buffer, baseline));
            }
            workspace
                .update_in(cx, |workspace, window, cx| {
                    let editor = crate::diff::review_editor(files, focus_ix, title, project, languages, window, cx);
                    // Beside the thread, so the conversation and its lists stay in view.
                    workspace.split_item(workspace::SplitDirection::Right, Box::new(editor), window, cx);
                })
                .ok();
        })
        .detach();
    }

    /// Shows the followed file against its content before the agent, when the agent
    /// changed it; back to the plain file once those changes are kept or undone.
    /// Shows the write waiting for review in the followed file, or stops showing it once
    /// it is answered.
    fn sync_follow_review(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let pending = self.follow.as_ref().and_then(|f| self.thread.read(cx).pending_review_for(&f.location.path)).map(|(ix, _, _)| ix);
        match pending {
            Some(ix) if self.follow_review.as_ref().map(|(shown, _)| *shown) != Some(ix) => {
                let thread = self.thread.read(cx);
                let Some(crate::thread::Entry::Review { diff, .. }) = thread.entries.get(ix) else { return };
                // The proposal as the thread's card has it (the user may have edited it there).
                let mut edit = diff.edit.clone();
                if let Some(text) = diff.edited_text(cx) {
                    edit.new_text = text;
                }
                let languages = thread.languages().clone();
                let first_change = crate::diff::hunks(edit.old_text.as_deref().unwrap_or_default(), &edit.new_text).first().map(|h| h.new.start);
                let view = crate::diff::DiffView::whole_file(edit, languages, window, cx);
                if let Some(line) = first_change {
                    let editor = view.editor.clone();
                    // Once the file is in the view: the first change in sight.
                    cx.spawn_in(window, async move |_, cx| {
                        cx.background_executor().timer(std::time::Duration::from_millis(150)).await;
                        cx.update(|window, cx| show_line(&editor, Some(line), window, cx)).ok();
                    })
                    .detach();
                }
                self.follow_review = Some((ix, view));
            }
            Some(_) => {}
            None => self.follow_review = None,
        }
    }

    fn update_follow_diff(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(follow) = self.follow.as_ref() else { return };
        let Some(editor) = follow.editor.clone() else { return };
        let thread = self.thread.read(cx);
        let base = thread.changes.iter().find(|c| c.path == follow.location.path).map(|c| c.original.clone().unwrap_or_default());
        match (base, follow.diff_base.is_some()) {
            (Some(base), _) if follow.diff_base.as_ref() != Some(&base) => {
                let languages = thread.languages().clone();
                let task = crate::diff::show_changes_since(&editor, base.clone(), languages, cx);
                let follow = self.follow.as_mut().unwrap();
                follow.diff_base = Some(base);
                follow._diff = Some(task);
            }
            (None, true) => {
                // Reopen the file without the diff.
                let location = follow.location.clone();
                self.follow = None;
                self.follow_to(location, window, cx);
            }
            _ => {}
        }
    }

    /// Lists the files the agent changed (until kept or undone), like Copilot's working
    /// set: each opens in the review tab (its diff against before the agent), and can be
    /// kept or undone; or all at once.
    fn render_changes(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        // Not the files' contents: this runs on every repaint, keystrokes included.
        let changes: Vec<(std::path::PathBuf, bool, (usize, usize))> = self.thread.read(cx).changes.iter().map(|c| (c.path.clone(), c.original.is_none(), c.stats())).collect();
        if changes.is_empty() {
            return None;
        }
        let colors = cx.theme().colors().clone();
        let root = self.thread.read(cx).root().clone();
        let (added, removed) = changes.iter().map(|(_, _, stats)| *stats).fold((0, 0), |(a, r), (x, y)| (a + x, r + y));
        let count = changes.len();
        let header = h_flex()
            .id("changes-header")
            .flex_wrap()
            .gap_2()
            .px_3()
            .py_1()
            .cursor_pointer()
            .child(Icon::new(if self.changes_open { IconName::ChevronDown } else { IconName::ChevronRight }).size(IconSize::XSmall).color(Color::Muted))
            .child(Icon::new(IconName::FileDiff).size(IconSize::Small).color(Color::Accent))
            .child(Label::new(format!("{count} file{} changed", if count == 1 { "" } else { "s" })).size(LabelSize::Small).weight(FontWeight::SEMIBOLD))
            .child(Label::new(format!("+{added}")).size(LabelSize::XSmall).color(Color::Created))
            .child(Label::new(format!("−{removed}")).size(LabelSize::XSmall).color(Color::Deleted))
            .child(div().flex_1())
            .child(
                // Wraps under the summary as one group when the pane is narrow.
                h_flex().gap_1().child(
                Button::new("changes-review", "Review")
                    .style(ButtonStyle::Subtle)
                    .size(ButtonSize::Compact)
                    .start_icon(Icon::new(IconName::Diff).size(IconSize::XSmall))
                    .tooltip(Tooltip::text("Every change in one tab, against before the agent"))
                    .on_click(cx.listener(|this, _, window, cx| this.open_review(None, window, cx))),
            )
            .child(
                Button::new("changes-keep-all", "Keep all")
                    .style(ButtonStyle::Filled)
                    .size(ButtonSize::Compact)
                    .start_icon(Icon::new(IconName::Check).size(IconSize::XSmall))
                    .tooltip(Tooltip::text("Accept every change: they stop being listed here"))
                    .on_click(cx.listener(|this, _, _, cx| this.thread.update(cx, |t, cx| t.keep_changes(None, cx)))),
            )
            .child(
                Button::new("changes-undo-all", "Undo all")
                    .style(ButtonStyle::Subtle)
                    .size(ButtonSize::Compact)
                    .start_icon(Icon::new(IconName::Undo).size(IconSize::XSmall))
                    .tooltip(Tooltip::text("Put every file back as it was before the agent"))
                    .on_click(cx.listener(|this, _, _, cx| this.thread.update(cx, |t, cx| t.undo_changes(None, cx)))),
            ))
            .on_click(cx.listener(|this, _, _, cx| {
                this.changes_open = !this.changes_open;
                cx.notify();
            }));
        let mut list = v_flex().id("changes-list").max_h(px(220.)).overflow_y_scroll().py_0p5();
        if self.changes_open {
            for (ix, (path, is_new, (a, r))) in changes.iter().enumerate() {
                let shown = path.strip_prefix(&root).unwrap_or(path);
                let name = shown.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                let dir = shown.parent().map(|d| d.to_string_lossy().into_owned()).filter(|d| !d.is_empty());
                let (open_path, keep_path, undo_path) = (path.clone(), path.clone(), path.clone());
                let icon = file_icons::FileIcons::get_icon(path, cx).map(Icon::from_path).unwrap_or_else(|| Icon::new(IconName::File));
                list = list.child(
                    h_flex()
                        .id(("change-row", ix))
                        .gap_2()
                        .px_3()
                        .py_0p5()
                        .cursor_pointer()
                        .hover(|el| el.bg(colors.element_hover))
                        .tooltip(Tooltip::text("Open its diff"))
                        .child(icon.size(IconSize::Small).color(Color::Muted))
                        .child(Label::new(name).size(LabelSize::Small).color(if *is_new { Color::Created } else { Color::Modified }))
                        .children(dir.map(|d| div().min_w_0().child(Label::new(d).size(LabelSize::XSmall).color(Color::Muted).truncate())))
                        .when(*is_new, |el| el.child(Label::new("new").size(LabelSize::XSmall).color(Color::Created)))
                        .child(Label::new(format!("+{a}")).size(LabelSize::XSmall).color(Color::Created))
                        .child(Label::new(format!("−{r}")).size(LabelSize::XSmall).color(Color::Deleted))
                        .child(div().flex_1())
                        .child(
                            IconButton::new(("change-keep", ix), IconName::Check)
                                .icon_size(IconSize::XSmall)
                                .tooltip(Tooltip::text("Keep these changes"))
                                .on_click(cx.listener(move |this, _, _, cx| this.thread.update(cx, |t, cx| t.keep_changes(Some(&keep_path), cx)))),
                        )
                        .child(
                            IconButton::new(("change-undo", ix), IconName::Undo)
                                .icon_size(IconSize::XSmall)
                                .tooltip(Tooltip::text(if *is_new { "Undo: delete the file the agent created" } else { "Undo: back to before the agent" }))
                                .on_click(cx.listener(move |this, _, _, cx| this.thread.update(cx, |t, cx| t.undo_changes(Some(undo_path.clone()), cx)))),
                        )
                        .on_click(cx.listener(move |this, _, window, cx| this.open_review(Some(open_path.clone()), window, cx))),
                );
            }
        }
        Some(centered(
            v_flex()
                .w_full()
                .mb_2()
                .rounded_md()
                .border_1()
                .border_color(colors.border)
                .bg(colors.surface_background)
                .child(header)
                .when(self.changes_open, |el| el.child(div().border_t_1().border_color(colors.border_variant).child(list))),
        ))
    }

    /// The review tab: every changed file against before the agent, at `focus` if given,
    /// beside the thread.
    fn open_review(&mut self, focus: Option<std::path::PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        let thread = self.thread.read(cx);
        let baselines = thread.changes.iter().map(|c| (c.path.clone(), c.original.clone().unwrap_or_default())).collect();
        let title = format!("Changes · {}", thread.title().unwrap_or_else(|| "agent".into()));
        self.open_diffs(baselines, focus, title, window, cx);
    }

    /// Moves the follow pane to where the agent is, unless paused or closed.
    fn sync_follow(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(location) = self.thread.read(cx).location().cloned() else { return };
        if self.follow_paused || self.follow_hidden || self.follow.as_ref().is_some_and(|f| f.location == location) {
            return;
        }
        self.follow_to(location, window, cx);
    }

    fn follow_to(&mut self, location: AgentLocation, window: &mut Window, cx: &mut Context<Self>) {
        let same_file = self.follow.as_ref().is_some_and(|f| f.location.path == location.path && f.editor.is_some());
        if same_file {
            let follow = self.follow.as_mut().unwrap();
            follow.location = location.clone();
            if let Some(editor) = follow.editor.clone() {
                show_line(&editor, location.line, window, cx);
            }
            cx.notify();
            return;
        }
        let Some(workspace) = self.workspace.upgrade() else { return };
        let project = workspace.read(cx).project().clone();
        let open = project.update(cx, |p, cx| p.open_local_buffer(&location.path, cx));
        let line = location.line;
        let task = cx.spawn_in(window, async move |this, cx| {
            let Ok(buffer) = open.await else { return };
            this.update_in(cx, |this, window, cx| {
                let editor = cx.new(|cx| Editor::for_buffer(buffer, Some(project), window, cx));
                show_line(&editor, line, window, cx);
                if let Some(follow) = this.follow.as_mut() {
                    follow.editor = Some(editor);
                }
                this.update_follow_diff(window, cx);
                cx.notify();
            })
            .ok();
        });
        self.follow = Some(Follow { location, editor: None, _open: Some(task), diff_base: None, _diff: None });
        cx.notify();
    }

    #[cfg(test)]
    fn render_follow_visible(&self, _: &App) -> bool {
        self.follow.is_some() && !self.follow_hidden
    }

    fn pause_follow(&mut self, cx: &mut Context<Self>) {
        self.follow_paused = true;
        cx.notify();
    }

    /// Back to where the agent is now.
    fn jump_to_agent(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.follow_paused = false;
        self.follow_hidden = false;
        if let Some(location) = self.thread.read(cx).location().cloned() {
            self.follow_to(location, window, cx);
        }
        cx.notify();
    }

    fn close_follow(&mut self, cx: &mut Context<Self>) {
        self.follow_hidden = true;
        cx.notify();
    }

    fn render_follow(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let follow = self.follow.as_ref().filter(|_| !self.follow_hidden)?;
        let colors = cx.theme().colors().clone();
        let root = self.thread.read(cx).root().clone();
        let shown = follow.location.path.strip_prefix(&root).unwrap_or(&follow.location.path).to_string_lossy().into_owned();
        let agent_at = self.thread.read(cx).location().cloned();
        let review = self.thread.read(cx).pending_review_for(&follow.location.path);
        let line_label = follow.location.line.map(|l| format!("line {}", l + 1)).unwrap_or_default();
        // While a command runs, its terminal; otherwise the file the agent is in.
        let terminal = self.follow_terminal(cx).and_then(|_| {
            let thread = self.thread.read(cx);
            let (_, terminal) = thread.running_terminal()?;
            thread.terminal_view(&terminal)
        });
        // What the pane shows, said as what the agent does: "Editing src/main.rs  line 12",
        // "Running cargo test".
        let what = if terminal.is_some() {
            let running = self.thread.read(cx).running_tool().unwrap_or("a command").to_string();
            h_flex()
                .gap_2()
                .min_w_0()
                .child(Icon::new(IconName::Terminal).size(IconSize::Small).color(Color::Accent))
                .child(Label::new("Running").size(LabelSize::Small).color(Color::Accent))
                .child(Label::new(running).size(LabelSize::Small).truncate())
        } else {
            let verb = match follow.location.kind.as_deref() {
                _ if review.is_some() => "Proposing a change to",
                Some("edit" | "delete" | "move") => "Editing",
                Some("read") => "Reading",
                Some("search") => "Searching in",
                _ => "Looking at",
            };
            h_flex()
                .gap_2()
                .min_w_0()
                .child(Icon::new(IconName::File).size(IconSize::Small).color(Color::Muted))
                .child(div().flex_none().child(Label::new(verb).size(LabelSize::Small).color(if self.follow_paused { Color::Muted } else { Color::Accent })))
                .child(Label::new(shown).size(LabelSize::Small).buffer_font(cx).truncate())
                .child(Label::new(line_label).size(LabelSize::XSmall).color(Color::Muted))
        };
        let status = if self.follow_paused {
            let at = agent_at
                .map(|l| {
                    let file = l.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    match l.line {
                        Some(line) => format!("{file}:{}", line + 1),
                        None => file,
                    }
                })
                .unwrap_or_default();
            h_flex().gap_2().child(Label::new(format!("Paused · the agent is in {at}")).size(LabelSize::XSmall).color(Color::Muted)).child(
                Button::new("follow-jump", "Jump to agent").style(ButtonStyle::Subtle).size(ButtonSize::Compact).on_click(cx.listener(|this, _, window, cx| this.jump_to_agent(window, cx))),
            )
        } else {
            h_flex().child(
                IconButton::new("follow-pause", IconName::DebugPause)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("Stay here (stop following the agent)"))
                    .on_click(cx.listener(|this, _, _, cx| this.pause_follow(cx))),
            )
        };
        let header = h_flex()
            .justify_between()
            .gap_2()
            .px_3()
            .py_1p5()
            .border_b_1()
            .border_color(colors.border)
            .child(what)
            .child(
                h_flex().gap_1().child(status).child(
                    IconButton::new("follow-close", IconName::Close).icon_size(IconSize::Small).tooltip(Tooltip::text("Close (the agent keeps working)")).on_click(cx.listener(|this, _, _, cx| this.close_follow(cx))),
                ),
            );
        // The proposed change, shown as a diff of what it adds and removes, to accept here.
        let proposal = self.follow_review.as_ref().filter(|(ix, _)| review.is_some_and(|(r, _, _)| r == *ix)).map(|(ix, view)| (*ix, view.editor.clone()));
        let banner = review.map(|(ix, added, removed)| {
            let thread = self.thread.downgrade();
            let answer = move |accept: bool| {
                let thread = thread.clone();
                move |_: &gpui::ClickEvent, _: &mut Window, cx: &mut App| {
                    thread.update(cx, |t, cx| t.answer_review(ix, accept, cx)).ok();
                }
            };
            h_flex()
                .gap_2()
                .px_3()
                .py_1()
                .bg(cx.theme().status().info_background)
                .child(Icon::new(IconName::Pencil).size(IconSize::XSmall).color(Color::Info))
                .child(div().flex_1().min_w_0().child(Label::new(format!("The agent proposes this change (+{added} −{removed})")).size(LabelSize::XSmall).truncate()))
                .child(Button::new("follow-accept", "Accept").style(ButtonStyle::Filled).size(ButtonSize::Compact).start_icon(Icon::new(IconName::Check).size(IconSize::XSmall)).on_click(answer(true)))
                .child(Button::new("follow-reject", "Reject").size(ButtonSize::Compact).start_icon(Icon::new(IconName::Close).size(IconSize::XSmall)).on_click(answer(false)))
        });
        // What the agent is doing besides the pane's tool call: waiting for you, thinking…
        let running = self.thread.read(cx).running_tool().map(str::to_string);
        let activity = self.thread.read(cx).activity().filter(|(text, waiting)| *waiting || running.as_deref() != Some(text.as_str())).map(|(text, waiting)| {
            h_flex()
                .gap_2()
                .px_3()
                .py_1()
                .border_b_1()
                .border_color(colors.border_variant)
                .child(if waiting { Icon::new(IconName::Warning).size(IconSize::XSmall).color(Color::Warning).into_any_element() } else { turns::spinner("follow-spinner", IconSize::XSmall, Color::Accent) })
                .child(div().min_w_0().flex_1().child(Label::new(text).size(LabelSize::Small).color(if waiting { Color::Warning } else { Color::Accent }).truncate()))
        });
        let body = match (&terminal, &follow.editor) {
            (Some(view), _) => div().flex_1().min_h_0().p_1().child(view.clone()).into_any_element(),
            (None, _) if proposal.is_some() => div().flex_1().min_h_0().child(proposal.clone().map(|(_, editor)| editor).unwrap()).into_any_element(),
            (None, Some(editor)) => div().flex_1().min_h_0().child(editor.clone()).into_any_element(),
            (None, None) => div().p_4().child(Label::new("Opening…").size(LabelSize::Small).color(Color::Muted)).into_any_element(),
        };
        Some(
            v_flex()
                .w(relative(0.48))
                .flex_shrink_0()
                .h_full()
                .border_l_1()
                .border_color(colors.border)
                .bg(colors.editor_background)
                .child(header)
                .children(activity)
                .when_some(banner.filter(|_| terminal.is_none()), |el, b| el.child(b))
                .child(body)
                .into_any_element(),
        )
    }

    fn send(&mut self, _: &Send, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.input.read(cx).text(cx).trim().to_string();
        if text.is_empty() {
            return;
        }
        let active = if self.include_active { self.workspace.upgrade().and_then(|w| active_context(w.read(cx), cx)) } else { None };
        let status = self.thread.read(cx).status();
        let images = self.images.clone();
        let sent = match status {
            Status::Ready => self.thread.update(cx, |t, cx| t.send_with_images(text, active, images, cx)),
            // Not connected yet: connect the selected agent, then send.
            Status::Disconnected => {
                self.thread.update(cx, |t, cx| t.ask(text, window, cx));
                true
            }
            // Working: the message waits for the end of the turn.
            Status::Connecting | Status::Busy => {
                self.thread.update(cx, |t, cx| t.enqueue(text, active, images, cx));
                true
            }
        };
        if sent {
            self.images.clear();
            self.input.update(cx, |e, cx| e.set_text("", window, cx));
        }
    }

    /// Paste: images become attachments; anything else is pasted as usual.
    fn paste(&mut self, _: &editor::actions::Paste, _: &mut Window, cx: &mut Context<Self>) {
        let images: Vec<gpui::Image> = cx
            .read_from_clipboard()
            .map(|item| item.entries().iter().filter_map(|e| if let gpui::ClipboardEntry::Image(image) = e { Some(image.clone()) } else { None }).collect())
            .unwrap_or_default();
        if images.is_empty() {
            cx.propagate();
            return;
        }
        self.images.extend(images.into_iter().map(std::sync::Arc::new));
        cx.notify();
    }

    /// Files dropped from Finder: images are attached, project files mentioned.
    fn drop_paths(&mut self, paths: &gpui::ExternalPaths, window: &mut Window, cx: &mut Context<Self>) {
        let root = self.thread.read(cx).root().clone();
        let mut mentions = String::new();
        for path in paths.paths() {
            let format = path.extension().and_then(|e| e.to_str()).and_then(|e| match e.to_ascii_lowercase().as_str() {
                "png" => Some(gpui::ImageFormat::Png),
                "jpg" | "jpeg" => Some(gpui::ImageFormat::Jpeg),
                "gif" => Some(gpui::ImageFormat::Gif),
                "webp" => Some(gpui::ImageFormat::Webp),
                _ => None,
            });
            match (format, std::fs::read(path)) {
                (Some(format), Ok(bytes)) => self.images.push(std::sync::Arc::new(gpui::Image::from_bytes(format, bytes))),
                _ => {
                    if let Ok(rel) = path.strip_prefix(&root) {
                        mentions.push_str(&format!("@{} ", rel.to_string_lossy()));
                    }
                }
            }
        }
        if !mentions.is_empty() {
            self.input.update(cx, |e, cx| e.insert(&mentions, window, cx));
        }
        cx.notify();
    }

    fn cancel(&mut self, _: &Cancel, _: &mut Window, cx: &mut Context<Self>) {
        self.thread.update(cx, |t, cx| t.cancel(cx));
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

    fn render_composer(&self, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors().clone();
        let status = self.thread.read(cx).status();
        let root = self.thread.read(cx).root().clone();
        let has_text = !self.input.read(cx).text(cx).trim().is_empty();
        let can_send = matches!(status, Status::Ready | Status::Disconnected) && has_text;
        let queued: Vec<String> = self.thread.read(cx).queue.iter().map(|q| q.text.clone()).collect();
        let handle = self.thread.downgrade();
        let input = self.input.clone();
        let queue = (!queued.is_empty()).then(|| {
            v_flex().gap_0p5().pb_1().children(queued.into_iter().enumerate().map(|(ix, text)| {
                let first_line: String = text.lines().next().unwrap_or_default().chars().take(120).collect();
                let (h1, h2, h3, input) = (handle.clone(), handle.clone(), handle.clone(), input.clone());
                h_flex()
                    .gap_1()
                    .px_1()
                    .rounded_sm()
                    .bg(colors.element_background)
                    .child(Icon::new(IconName::ListTodo).size(IconSize::XSmall).color(Color::Muted))
                    .child(div().flex_1().min_w_0().child(Label::new(first_line).size(LabelSize::Small).color(Color::Muted).truncate()))
                    .child(Label::new("queued").size(LabelSize::XSmall).color(Color::Muted))
                    .child(
                        IconButton::new(("queued-now", ix), IconName::Send)
                            .icon_size(IconSize::XSmall)
                            .tooltip(Tooltip::text("Send now (stops the current turn)"))
                            .on_click(move |_, _, cx| {
                                h1.update(cx, |t, cx| t.send_queued_now(ix, cx)).ok();
                            }),
                    )
                    .child(
                        IconButton::new(("queued-edit", ix), IconName::Pencil)
                            .icon_size(IconSize::XSmall)
                            .tooltip(Tooltip::text("Back to the input to edit it"))
                            .on_click(move |_, window, cx| {
                                if let Some(Some(text)) = h2.update(cx, |t, cx| t.unqueue(ix, cx)).ok() {
                                    input.update(cx, |e, cx| e.set_text(text, window, cx));
                                }
                            }),
                    )
                    .child(
                        IconButton::new(("queued-remove", ix), IconName::Close)
                            .icon_size(IconSize::XSmall)
                            .tooltip(Tooltip::text("Don't send it"))
                            .on_click(move |_, _, cx| {
                                h3.update(cx, |t, cx| t.unqueue(ix, cx)).ok();
                            }),
                    )
            }))
        });
        let active = self.workspace.upgrade().and_then(|w| active_context(w.read(cx), cx));
        let chip = match (&active, self.include_active) {
            (Some(a), true) => Some(
                Button::new("thread-active-context", a.label(&root))
                    .style(ButtonStyle::Subtle)
                    .size(ButtonSize::Compact)
                    .start_icon(Icon::new(IconName::File).size(IconSize::XSmall))
                    .end_icon(Icon::new(IconName::Close).size(IconSize::XSmall))
                    .tooltip(Tooltip::text("Sent with your message. Click to leave it out."))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.include_active = false;
                        cx.notify();
                    }))
                    .into_any_element(),
            ),
            (Some(_), false) => Some(
                Button::new("thread-active-context", "Include the active file")
                    .style(ButtonStyle::Transparent)
                    .size(ButtonSize::Compact)
                    .start_icon(Icon::new(IconName::Plus).size(IconSize::XSmall))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.include_active = true;
                        cx.notify();
                    }))
                    .into_any_element(),
            ),
            (None, _) => None,
        };
        h_flex().w_full().justify_center().px_4().pb_4().child(
            v_flex()
                .w_full()
                .max_w(px(880.))
                .gap_1()
                .p_2()
                .rounded_lg()
                .border_1()
                .border_color(colors.border)
                .bg(colors.elevated_surface_background)
                .children(queue)
                .when(!self.images.is_empty(), |el| {
                    let accepts = self.thread.read(cx).accepts_images() || status == Status::Disconnected;
                    el.child(
                        h_flex()
                            .gap_1()
                            .flex_wrap()
                            .children(self.images.iter().enumerate().map(|(ix, image)| {
                                h_flex()
                                    .gap_0p5()
                                    .p_0p5()
                                    .rounded_sm()
                                    .border_1()
                                    .border_color(colors.border)
                                    .child(gpui::img(image.clone()).h(px(40.)).max_w(px(80.)).rounded_sm())
                                    .child(
                                        IconButton::new(("remove-image", ix), IconName::Close)
                                            .icon_size(IconSize::XSmall)
                                            .tooltip(Tooltip::text("Remove"))
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                if ix < this.images.len() {
                                                    this.images.remove(ix);
                                                }
                                                cx.notify();
                                            })),
                                    )
                            }))
                            .when(!accepts, |el| el.child(Label::new("This agent doesn't take images").size(LabelSize::XSmall).color(Color::Warning))),
                    )
                })
                .when_some(chip, |el, chip| el.child(h_flex().child(chip)))
                .child(
                    h_flex()
                        .gap_2()
                        .items_end()
                        .child(
                            div()
                                .key_context("ForgeAgentInput")
                                .flex_1()
                                .min_w_0()
                                .px_1()
                                .capture_action(cx.listener(Self::paste))
                                .on_drop(cx.listener(|this, paths: &gpui::ExternalPaths, window, cx| this.drop_paths(paths, window, cx)))
                                .child(self.input.clone()),
                        )
                        .when(status == Status::Busy && has_text, |el| {
                            el.child(
                                IconButton::new("thread-queue", IconName::Send)
                                    .icon_color(Color::Muted)
                                    .tooltip(Tooltip::text("Send when the agent finishes (Enter)"))
                                    .on_click(cx.listener(|this, _, window, cx| this.send(&Send, window, cx))),
                            )
                        })
                        .child(match status {
                            Status::Busy => IconButton::new("thread-cancel", IconName::Stop)
                                .icon_color(Color::Error)
                                .tooltip(Tooltip::text("Stop the agent"))
                                .on_click(cx.listener(|this, _, window, cx| this.cancel(&Cancel, window, cx)))
                                .into_any_element(),
                            _ => IconButton::new("thread-send", IconName::Send)
                                .icon_color(Color::Accent)
                                .disabled(!can_send)
                                .tooltip(Tooltip::text("Send (Enter)"))
                                .on_click(cx.listener(|this, _, window, cx| this.send(&Send, window, cx)))
                                .into_any_element(),
                        }),
                )
                .children(self.render_session_bar(cx)),
        )
        .into_any_element()
    }

    /// Under the input: the agent's session settings (mode, model, effort…), then what
    /// the session has used (the plan's limits, the context window).
    fn render_session_bar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let thread = self.thread.read(cx);
        let handle = self.thread.downgrade();
        let mut settings: Vec<AnyElement> = Vec::new();
        for (ix, option) in thread.config_options.iter().enumerate() {
            let tooltip: SharedString = option.description.clone().map(|d| format!("{}: {d}", option.name)).unwrap_or_else(|| option.name.clone()).into();
            match &option.value {
                crate::thread::ConfigValue::Select { current, choices } => {
                    let (id, current, choices, handle) = (option.id.clone(), current.clone(), choices.clone(), handle.clone());
                    let icon = match option.category.as_deref() {
                        Some("mode") => IconName::Settings,
                        Some("model") => IconName::ZedAgent,
                        _ => IconName::Sparkle,
                    };
                    settings.push(
                        PopoverMenu::new(("thread-option", ix))
                            .trigger(
                                ButtonLike::new(("thread-option-trigger", ix))
                                    .style(ButtonStyle::Subtle)
                                    .size(ButtonSize::Compact)
                                    .child(
                                        h_flex()
                                            .gap_1()
                                            .child(Icon::new(icon).size(IconSize::XSmall).color(Color::Muted))
                                            .child(Label::new(option.current_label()).size(LabelSize::XSmall))
                                            .child(Icon::new(IconName::ChevronDown).size(IconSize::XSmall).color(Color::Muted)),
                                    )
                                    .tooltip(Tooltip::text(tooltip)),
                            )
                            .menu(move |window, cx| {
                                let (id, current, choices, handle) = (id.clone(), current.clone(), choices.clone(), handle.clone());
                                Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                                    for (value, name, description) in choices {
                                        let label = match description {
                                            Some(d) if !d.is_empty() => format!("{name} — {d}"),
                                            _ => name,
                                        };
                                        let (id, handle, checked) = (id.clone(), handle.clone(), value == current);
                                        menu = menu.toggleable_entry(label, checked, ui::IconPosition::Start, None, move |_, cx| {
                                            handle.update(cx, |t, cx| t.set_config_option(id.clone(), serde_json::Value::String(value.clone()), cx)).ok();
                                        });
                                    }
                                    menu
                                }))
                            })
                            .into_any_element(),
                    );
                }
                crate::thread::ConfigValue::Boolean(on) => {
                    let (id, on, handle) = (option.id.clone(), *on, handle.clone());
                    settings.push(
                        Button::new(("thread-option-toggle", ix), option.name.clone())
                            .style(if on { ButtonStyle::Filled } else { ButtonStyle::Subtle })
                            .size(ButtonSize::Compact)
                            .label_size(LabelSize::XSmall)
                            .toggle_state(on)
                            .tooltip(Tooltip::text(tooltip))
                            .on_click(move |_, _, cx| {
                                handle.update(cx, |t, cx| t.set_config_option(id.clone(), serde_json::Value::Bool(!on), cx)).ok();
                            })
                            .into_any_element(),
                    );
                }
            }
        }
        // Agents with modes but no settings list (ACP `modes` only).
        let has_mode_setting = thread.config_options.iter().any(|o| o.category.as_deref() == Some("mode"));
        if !has_mode_setting && !thread.modes.is_empty() {
            let modes = thread.modes.clone();
            let current_mode = thread.current_mode.clone();
            let current_name = modes.iter().find(|m| Some(&m.id) == current_mode.as_ref()).map(|m| m.name.clone()).unwrap_or_else(|| "Mode".into());
            let handle = handle.clone();
            settings.push(
                PopoverMenu::new("thread-mode")
                    .trigger(
                        ButtonLike::new("thread-mode-trigger")
                            .style(ButtonStyle::Subtle)
                            .size(ButtonSize::Compact)
                            .child(
                                h_flex()
                                    .gap_1()
                                    .child(Icon::new(IconName::Settings).size(IconSize::XSmall).color(Color::Muted))
                                    .child(Label::new(current_name).size(LabelSize::XSmall))
                                    .child(Icon::new(IconName::ChevronDown).size(IconSize::XSmall).color(Color::Muted)),
                            )
                            .tooltip(Tooltip::text("How much the agent asks before acting")),
                    )
                    .menu(move |window, cx| {
                        let (modes, current_mode, handle) = (modes.clone(), current_mode.clone(), handle.clone());
                        Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                            for mode in modes {
                                let checked = Some(&mode.id) == current_mode.as_ref();
                                let label = match &mode.description {
                                    Some(d) => format!("{} — {d}", mode.name),
                                    None => mode.name.clone(),
                                };
                                let (handle, id) = (handle.clone(), mode.id.clone());
                                menu = menu.toggleable_entry(label, checked, ui::IconPosition::Start, None, move |_, cx| {
                                    handle.update(cx, |t, cx| t.set_mode(id.clone(), cx)).ok();
                                });
                            }
                            menu
                        }))
                    })
                    .into_any_element(),
            );
        }
        let plan = thread.plan_usage(cx).and_then(|plan| render_plan_usage(&plan));
        let usage = render_usage(&thread.usage, cx);
        if settings.is_empty() && plan.is_none() && usage.is_none() {
            return None;
        }
        Some(
            h_flex()
                .gap_1()
                .flex_wrap()
                .pt_1()
                .border_t_1()
                .border_color(cx.theme().colors().border_variant)
                .children(settings)
                .child(div().flex_1())
                .children(plan)
                .children(usage)
                .into_any_element(),
        )
    }
}

/// Scrolls `editor` to `line` and highlights it as the agent's.
fn show_line(editor: &Entity<Editor>, line: Option<u32>, window: &mut Window, cx: &mut App) {
    editor.update(cx, |editor, cx| {
        editor.clear_row_highlights::<AgentLine>();
        let Some(line) = line else { return };
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let line = line.min(snapshot.max_point().row);
        let anchor = snapshot.anchor_before(Point::new(line, 0));
        let color = |cx: &App| cx.theme().colors().border_focused.opacity(0.18);
        editor.highlight_rows::<AgentLine>(anchor..anchor, color, RowHighlightOptions { autoscroll: true, include_gutter: true }, cx);
        editor.go_to_singleton_buffer_point(Point::new(line, 0), window, cx);
    });
}

/// `12.3k`, `1.2M`.
pub(crate) fn tokens(n: u64) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..1_000_000 => format!("{:.1}k", n as f64 / 1_000.0).replace(".0k", "k"),
        _ => format!("{:.1}M", n as f64 / 1_000_000.0).replace(".0M", "M"),
    }
}

/// How much of the account's limits are used (Claude's 5-hour and weekly windows): the
/// tightest one in the header, all of them in the tooltip.
fn render_plan_usage(plan: &crate::plan_usage::PlanUsage) -> Option<AnyElement> {
    let tightest = plan.tightest()?;
    let color = if tightest.percent >= 90.0 {
        Color::Error
    } else if tightest.percent >= 70.0 {
        Color::Warning
    } else {
        Color::Muted
    };
    let mut lines: Vec<String> = Vec::new();
    lines.push(match &plan.subscription {
        Some(s) => format!("Claude {s} plan"),
        None => "Plan usage".into(),
    });
    for limit in &plan.limits {
        let resets = limit.resets.as_ref().map(|r| format!(" · resets {r}")).unwrap_or_default();
        lines.push(format!("{}: {}% used{resets}", limit.label, limit.percent.round()));
    }
    let short = if tightest.label.starts_with("5-hour") { "5h" } else if tightest.label.starts_with("Weekly") { "week" } else { "plan" };
    Some(
        div()
            .id("thread-plan-usage")
            .px_1()
            .child(
                h_flex()
                    .gap_1()
                    .child(Label::new("Plan").size(LabelSize::XSmall).color(Color::Muted))
                    .child(Label::new(format!("{}%", tightest.percent.round())).size(LabelSize::XSmall).color(color))
                    .child(Label::new(short).size(LabelSize::XSmall).color(Color::Muted)),
            )
            .tooltip(Tooltip::text(SharedString::from(lines.join("\n"))))
            .into_any_element(),
    )
}

/// How full the context window is (and, in the tooltip, what the session has spent).
fn render_usage(usage: &crate::thread::Usage, cx: &App) -> Option<AnyElement> {
    if usage.is_empty() {
        return None;
    }
    let mut lines = Vec::new();
    if let (Some(used), Some(size)) = (usage.context_used, usage.context_size) {
        lines.push(format!("Context: {} of {} tokens ({} left)", tokens(used), tokens(size), tokens(size.saturating_sub(used))));
    }
    if usage.turns > 0 {
        lines.push(format!("This session: {} in, {} out", tokens(usage.input), tokens(usage.output)));
        if usage.cached_read > 0 {
            lines.push(format!("Read from cache: {}", tokens(usage.cached_read)));
        }
        if usage.thought > 0 {
            lines.push(format!("Reasoning: {}", tokens(usage.thought)));
        }
        lines.push(format!("{} turn{}", usage.turns, if usage.turns == 1 { "" } else { "s" }));
    }
    if let Some((amount, currency)) = &usage.cost {
        lines.push(format!("Cost: {amount:.2} {currency}"));
    }
    let tooltip: SharedString = lines.join("\n").into();
    let chip = match usage.context_ratio() {
        Some(ratio) => {
            // Nearly full: the agent will soon compact or forget the start of the thread.
            let (color, fill) = if ratio >= 0.9 {
                (Color::Error, cx.theme().status().error)
            } else if ratio >= 0.7 {
                (Color::Warning, cx.theme().status().warning)
            } else {
                (Color::Muted, cx.theme().colors().text_accent)
            };
            h_flex()
                .gap_1()
                .child(
                    div()
                        .w(px(36.))
                        .h(px(4.))
                        .rounded_full()
                        .bg(cx.theme().colors().element_background)
                        .child(div().h_full().rounded_full().w(relative(ratio)).bg(fill)),
                )
                .child(Label::new(format!("{}%", (ratio * 100.0).round() as u32)).size(LabelSize::XSmall).color(color))
                .child(Label::new(format!("{} / {}", tokens(usage.context_used.unwrap_or(0)), tokens(usage.context_size.unwrap_or(0)))).size(LabelSize::XSmall).color(Color::Muted))
        }
        None => h_flex().child(Label::new(format!("{} tokens", tokens(usage.input + usage.output))).size(LabelSize::XSmall).color(Color::Muted)),
    };
    Some(div().id("thread-usage").px_1().child(chip).tooltip(Tooltip::text(tooltip)).into_any_element())
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
mod tests {
    use super::*;
    use crate::thread::Entry;
    use fs::Fs as _;
    use gpui::{TestAppContext, VisualTestContext, point, size};
    use ide_api::AgentSpec;
    use project::Project;
    use serde_json::json;
    use std::time::{Duration, Instant};

    async fn wait_for(cx: &mut VisualTestContext, thread: &Entity<Thread>, what: &str, pred: impl Fn(&Thread) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            cx.run_until_parked();
            if thread.read_with(cx, |t, _| pred(t)) {
                return;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            cx.background_executor.timer(Duration::from_millis(20)).await;
        }
    }

    fn draw(view: &Entity<ThreadView>, cx: &mut VisualTestContext) {
        let view = view.clone();
        cx.draw(point(px(0.), px(0.)), size(px(1280.), px(800.)), move |_, _| div().size_full().child(view));
    }

    /// [`diff_rows`] once both are non-zero (diffs are computed in the background, later on a
    /// busy machine), or whatever they are after 20 seconds, for the test to report.
    async fn changed_rows(editor: &Entity<Editor>, view: &Entity<ThreadView>, cx: &mut VisualTestContext) -> (usize, usize) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            cx.run_until_parked();
            draw(view, cx);
            let (added, deleted) = diff_rows(editor, cx);
            if (added > 0 && deleted > 0) || Instant::now() >= deadline {
                return (added, deleted);
            }
            cx.background_executor.timer(Duration::from_millis(20)).await;
        }
    }

    /// (added, deleted) rows the editor shows as diff, expanded.
    fn diff_rows(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> (usize, usize) {
        editor.update(cx, |e, cx| {
            let snapshot = e.buffer().read(cx).snapshot(cx);
            snapshot.row_infos(multi_buffer::MultiBufferRow(0)).filter_map(|r| r.diff_status).fold((0, 0), |(a, d), s| match s.kind {
                buffer_diff::DiffHunkStatusKind::Added => (a + 1, d),
                buffer_diff::DiffHunkStatusKind::Deleted => (a, d + 1),
                _ => (a, d),
            })
        })
    }

    /// While the agent edits the followed file, the pane shows the change as a diff: the
    /// proposal while it waits for review, then the file against before the agent.
    #[gpui::test]
    async fn the_follow_pane_shows_changes_as_diffs(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let tmp = tempfile::tempdir().unwrap();
        cx.update(workspace::AppState::test);
        cx.update(|cx| {
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            gpui_tokio::init(cx);
            editor::init(cx);
            init(cx);
            crate::thread_picker::init(cx);
            crate::agent_review::init(cx);
        });
        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree("/root", json!({ "a.txt": "old\nsame\n" })).await;
        cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));
        let project = Project::test(fs.clone(), ["/root".as_ref()], cx).await;
        let window = cx.add_window(|window, cx| workspace::MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window.read_with(cx, |mw, _| mw.workspace().clone()).unwrap();
        let cx = &mut VisualTestContext::from_window(window.into(), cx);
        let agent = AgentSpec {
            id: "mock".into(),
            command: "python3".into(),
            args: vec![std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tools/mock-acp-agent.py").to_string_lossy().into_owned()],
            env: vec![],
            cwd: Some(tmp.path().to_path_buf()),
        };
        let config = crate::config::AgentsConfig { instructions_files: crate::config::default_instructions_files(), agents: vec![agent], review_writes: true, verify_changes: false, mcp_servers: vec![], default_agent: None, permissions: Default::default() };
        let thread = workspace.update_in(cx, |ws, window, cx| {
            let thread = cx.new(|cx| Thread::with_config(ws, Some(config), tmp.path().join("history"), window, cx));
            add_thread(ws, thread.clone(), window, cx);
            thread
        });
        let view = workspace.read_with(cx, |ws, cx| ws.items_of_type::<ThreadView>(cx).next()).unwrap();
        draw(&view, cx);
        view.update_in(cx, |v, window, cx| {
            v.input.update(cx, |e, cx| e.set_text("edit a.txt new", window, cx));
            v.send(&Send, window, cx);
        });
        wait_for(cx, &thread, "the change to review", |t| t.pending_reviews() == 1).await;
        cx.run_until_parked();
        draw(&view, cx);
        let proposal = view.read_with(cx, |v, _| v.follow_review.as_ref().map(|(_, d)| d.editor.clone())).expect("the pane shows the proposal");
        let (added, deleted) = changed_rows(&proposal, &view, cx).await;
        assert!(added > 0 && deleted > 0, "the proposal shows added and deleted rows: {added} added, {deleted} deleted");

        let review = thread.read_with(cx, |t, _| t.entries.iter().position(|e| matches!(e, Entry::Review { .. })).unwrap());
        thread.update(cx, |t, cx| t.answer_review(review, true, cx));
        wait_for(cx, &thread, "the turn to end", |t| t.status() == Status::Ready && t.pending_reviews() == 0).await;
        cx.run_until_parked();
        draw(&view, cx);
        assert!(view.read_with(cx, |v, _| v.follow_review.is_none()), "answered: back to the file");
        let editor = view.read_with(cx, |v, _| v.follow.as_ref().and_then(|f| f.editor.clone())).expect("the pane shows the file");
        let (added, deleted) = changed_rows(&editor, &view, cx).await;
        assert!(added > 0 && deleted > 0, "the file shows the agent's change: {added} added, {deleted} deleted");
        // Filled red and green, not git's hollow staged hunks.
        let staged = editor.update(cx, |e, cx| {
            let snapshot = e.buffer().read(cx).snapshot(cx);
            let statuses: Vec<_> = snapshot.row_infos(multi_buffer::MultiBufferRow(0)).filter_map(|r| r.diff_status).collect();
            statuses.iter().map(|s| e.diff_hunk_renderer().render_hunk_as_staged(s, cx)).collect::<Vec<_>>()
        });
        assert!(staged.iter().all(|s| !s), "the follow pane's hunks render as staged (hollow): {staged:?}");
        let staged = proposal.update(cx, |e, cx| {
            let snapshot = e.buffer().read(cx).snapshot(cx);
            let statuses: Vec<_> = snapshot.row_infos(multi_buffer::MultiBufferRow(0)).filter_map(|r| r.diff_status).collect();
            statuses.iter().map(|s| e.diff_hunk_renderer().render_hunk_as_staged(s, cx)).collect::<Vec<_>>()
        });
        assert!(staged.iter().all(|s| !s), "the proposal's hunks render as staged (hollow): {staged:?}");
    }

    /// As with Claude Code in auto mode: the agent reads the file (the pane opens it), then
    /// writes it straight away (no review); the pane shows the change as a diff.
    #[gpui::test]
    async fn the_follow_pane_shows_direct_writes_as_diffs(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let tmp = tempfile::tempdir().unwrap();
        cx.update(workspace::AppState::test);
        cx.update(|cx| {
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            gpui_tokio::init(cx);
            editor::init(cx);
            init(cx);
            crate::thread_picker::init(cx);
            crate::agent_review::init(cx);
        });
        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree("/root", json!({ "a.txt": "old\nsame\n" })).await;
        cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));
        let project = Project::test(fs.clone(), ["/root".as_ref()], cx).await;
        let window = cx.add_window(|window, cx| workspace::MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window.read_with(cx, |mw, _| mw.workspace().clone()).unwrap();
        let cx = &mut VisualTestContext::from_window(window.into(), cx);
        let agent = AgentSpec {
            id: "mock".into(),
            command: "python3".into(),
            args: vec![std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tools/mock-acp-agent.py").to_string_lossy().into_owned()],
            env: vec![],
            cwd: Some(tmp.path().to_path_buf()),
        };
        let config = crate::config::AgentsConfig { instructions_files: crate::config::default_instructions_files(), agents: vec![agent], review_writes: false, verify_changes: false, mcp_servers: vec![], default_agent: None, permissions: Default::default() };
        let thread = workspace.update_in(cx, |ws, window, cx| {
            let thread = cx.new(|cx| Thread::with_config(ws, Some(config), tmp.path().join("history"), window, cx));
            add_thread(ws, thread.clone(), window, cx);
            thread
        });
        let view = workspace.read_with(cx, |ws, cx| ws.items_of_type::<ThreadView>(cx).next()).unwrap();
        draw(&view, cx);
        for message in ["read a.txt 0", "edit a.txt new"] {
            view.update_in(cx, |v, window, cx| {
                v.input.update(cx, |e, cx| e.set_text(message, window, cx));
                v.send(&Send, window, cx);
            });
            wait_for(cx, &thread, "the turn to end", |t| t.status() == Status::Ready && t.entries.iter().filter(|e| matches!(e, Entry::Agent(_))).count() >= 1).await;
            cx.run_until_parked();
            draw(&view, cx);
        }
        wait_for(cx, &thread, "the change", |t| !t.changes.is_empty()).await;
        cx.run_until_parked();
        draw(&view, cx);
        let editor = view.read_with(cx, |v, _| v.follow.as_ref().and_then(|f| f.editor.clone())).expect("the pane shows the file");
        let text = editor.update(cx, |e, cx| e.buffer().read(cx).snapshot(cx).text());
        assert!(text.contains("new"), "the pane shows the written file: {text:?}");
        let (added, deleted) = changed_rows(&editor, &view, cx).await;
        assert!(added > 0 && deleted > 0, "the pane shows the agent's change: {added} added, {deleted} deleted");
    }

    /// Claude Code's adapter writes files itself and only reports the edit (a tool call
    /// with the whole file before and after): the edit still joins the changes, and the
    /// follow pane shows it as a diff.
    #[gpui::test]
    async fn edits_the_agent_writes_itself_show_as_diffs(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let tmp = tempfile::tempdir().unwrap();
        cx.update(workspace::AppState::test);
        cx.update(|cx| {
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            gpui_tokio::init(cx);
            editor::init(cx);
            init(cx);
            crate::thread_picker::init(cx);
            crate::agent_review::init(cx);
        });
        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree("/root", json!({ "a.rs": "fn a() {}\nfn b() {}\n" })).await;
        cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));
        let project = Project::test(fs.clone(), ["/root".as_ref()], cx).await;
        let window = cx.add_window(|window, cx| workspace::MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window.read_with(cx, |mw, _| mw.workspace().clone()).unwrap();
        let cx = &mut VisualTestContext::from_window(window.into(), cx);
        let config = crate::config::AgentsConfig { instructions_files: crate::config::default_instructions_files(), agents: vec![], review_writes: true, verify_changes: false, mcp_servers: vec![], default_agent: None, permissions: Default::default() };
        let thread = workspace.update_in(cx, |ws, window, cx| {
            let thread = cx.new(|cx| Thread::with_config(ws, Some(config), tmp.path().join("history"), window, cx));
            add_thread(ws, thread.clone(), window, cx);
            thread
        });
        let view = workspace.read_with(cx, |ws, cx| ws.items_of_type::<ThreadView>(cx).next()).unwrap();
        let (before, after) = ("fn a() {}\nfn b() {}\n", "fn a() {}\nfn c() {}\n");
        let update = |u: serde_json::Value, cx: &mut VisualTestContext| {
            thread.update_in(cx, |t, window, cx| {
                t.apply_update(u, window, cx);
                t.changed(cx);
            });
            cx.run_until_parked();
        };
        // The edit is announced (the pane opens the file), written by the agent, then done.
        update(json!({ "sessionUpdate": "tool_call", "toolCallId": "t1", "title": "Edit a.rs", "kind": "edit", "status": "pending",
            "locations": [{ "path": "/root/a.rs", "line": 1 }],
            "content": [{ "type": "diff", "path": "/root/a.rs", "oldText": before, "newText": after }] }), cx);
        draw(&view, cx);
        fs.save("/root/a.rs".as_ref(), &after.into(), Default::default()).await.unwrap();
        update(json!({ "sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "completed" }), cx);
        draw(&view, cx);
        let changes = thread.read_with(cx, |t, _| t.changes.iter().map(|c| (c.path.clone(), c.original.clone(), c.current.clone())).collect::<Vec<_>>());
        assert_eq!(changes, [(std::path::PathBuf::from("/root/a.rs"), Some(before.to_string()), after.to_string())]);
        let editor = view.read_with(cx, |v, _| v.follow.as_ref().and_then(|f| f.editor.clone())).expect("the pane shows the file");
        let (added, deleted) = changed_rows(&editor, &view, cx).await;
        assert!(added > 0 && deleted > 0, "the pane shows the agent's change: {added} added, {deleted} deleted");

        // A snippet's diff doesn't say what the file was: not recorded.
        update(json!({ "sessionUpdate": "tool_call", "toolCallId": "t2", "title": "Edit a.rs", "kind": "edit", "status": "pending",
            "content": [{ "type": "diff", "path": "/root/a.rs", "oldText": "fn c() {}", "newText": "fn d() {}" }] }), cx);
        fs.save("/root/a.rs".as_ref(), &"fn a() {}\nfn d() {}\n".into(), Default::default()).await.unwrap();
        update(json!({ "sessionUpdate": "tool_call_update", "toolCallId": "t2", "status": "completed" }), cx);
        let current = thread.read_with(cx, |t, _| t.changes[0].current.clone());
        assert_eq!(current, after, "a snippet's diff isn't taken for the whole file");
    }

    /// Tools extensions offer (`forge.agents.registerTool`) reach agents through Forge's MCP
    /// server: one that only reads runs at once; any other waits for the user in the thread.
    #[gpui::test]
    async fn extension_tools_run_for_agents_with_the_users_leave(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        use forge_ui::agent_tools::{AgentTool, AgentToolReply, AgentToolRunner, agent_tools};
        struct Echo;
        impl AgentToolRunner for Echo {
            fn run(&self, tool: &AgentTool, args: serde_json::Value, _cwd: std::path::PathBuf) -> futures::channel::oneshot::Receiver<AgentToolReply> {
                let (tx, rx) = futures::channel::oneshot::channel();
                tx.send(Ok(format!("{} ran with {args}", tool.tool))).ok();
                rx
            }
        }
        let tool = |name: &str, read_only: bool| AgentTool {
            name: forge_ui::agent_tools::mcp_name("demo-ext", name),
            tool: name.into(),
            extension: "demo-ext".into(),
            title: format!("Demo {name}"),
            description: format!("Does {name}."),
            input_schema: json!({ "type": "object", "properties": {} }),
            read_only,
        };
        agent_tools().register(tool("lookup", true));
        agent_tools().register(tool("change", false));
        agent_tools().set_runner(std::sync::Arc::new(Echo));
        assert!(crate::forge_mcp::instructions(std::path::Path::new("/nowhere")).contains("`demo_ext__lookup` (Demo lookup, from the demo-ext extension)"));
        assert_eq!(crate::forge_mcp::tool_title("mcp__forge__demo_ext__change").as_deref(), Some("Demo change"));

        let tmp = tempfile::tempdir().unwrap();
        let config = crate::config::AgentsConfig { instructions_files: crate::config::default_instructions_files(), agents: vec![], review_writes: true, verify_changes: false, mcp_servers: vec![], default_agent: None, permissions: Default::default() };
        let (thread, _view, mut cx) = thread_tab(config, tmp.path().join("history"), cx).await;
        let cx = &mut cx;
        let call = |name: &str, cx: &mut VisualTestContext| {
            let (reply, answer) = futures::channel::oneshot::channel();
            let request = crate::forge_mcp::ToolRequest { name: name.into(), args: json!({ "q": 1 }), reply };
            thread.update_in(cx, |t, window, cx| t.handle_tool_request(request, window, cx));
            cx.run_until_parked();
            answer
        };
        let mut lookup = call("demo_ext__lookup", cx);
        assert_eq!(lookup.try_recv().unwrap(), Some(Ok("lookup ran with {\"q\":1}".into())), "reading tools run at once");
        assert!(thread.read_with(cx, |t, _| !t.entries.iter().any(|e| matches!(e, Entry::ExtensionTool { .. }))));

        let mut allowed = call("demo_ext__change", cx);
        assert_eq!(allowed.try_recv().unwrap(), None, "it waits for the user");
        assert_eq!(thread.read_with(cx, |t, _| t.pending_reviews()), 1);
        let card = thread.read_with(cx, |t, _| t.entries.iter().position(|e| matches!(e, Entry::ExtensionTool { .. })).unwrap());
        draw(&_view, cx);
        thread.update_in(cx, |t, window, cx| t.answer_extension_tool(card, true, window, cx));
        cx.run_until_parked();
        assert_eq!(allowed.try_recv().unwrap(), Some(Ok("change ran with {\"q\":1}".into())));

        let mut denied = call("demo_ext__change", cx);
        let card = thread.read_with(cx, |t, _| t.entries.iter().rposition(|e| matches!(e, Entry::ExtensionTool { .. })).unwrap());
        thread.update_in(cx, |t, window, cx| t.answer_extension_tool(card, false, window, cx));
        cx.run_until_parked();
        assert!(matches!(denied.try_recv().unwrap(), Some(Err(e)) if e.contains("didn't let Demo change run")));
        agent_tools().unregister_extension("demo-ext");
    }

    /// `remember` keeps a note (as the user edited it) in the project's instructions;
    /// `user_context` says where the user is, though the thread is the active tab.
    #[gpui::test]
    async fn agents_remember_and_see_what_the_user_is_on(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let tmp = tempfile::tempdir().unwrap();
        cx.update(workspace::AppState::test);
        cx.update(|cx| {
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            gpui_tokio::init(cx);
            editor::init(cx);
            init(cx);
        });
        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree("/root", json!({ "AGENTS.md": "# Repo\n", "a.rs": "fn one() {}\nfn two() {}\nfn three() {}\n" })).await;
        cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));
        let project = Project::test(fs.clone(), ["/root".as_ref()], cx).await;
        let window = cx.add_window(|window, cx| workspace::MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window.read_with(cx, |mw, _| mw.workspace().clone()).unwrap();
        let cx = &mut VisualTestContext::from_window(window.into(), cx);

        // The user works in a.rs, with two lines selected; then the thread opens, in front of it.
        let editor = workspace
            .update_in(cx, |ws, window, cx| ws.open_abs_path("/root/a.rs".into(), workspace::OpenOptions::default(), window, cx))
            .await
            .unwrap()
            .downcast::<Editor>()
            .unwrap();
        editor.update_in(cx, |e, window, cx| e.change_selections(Default::default(), window, cx, |s| s.select_ranges([Point::new(1, 0)..Point::new(2, 5)])));
        let config = crate::config::AgentsConfig { instructions_files: vec!["AGENTS.md".into(), ".forge/AGENTS.md".into()], agents: vec![], review_writes: true, verify_changes: false, mcp_servers: vec![], default_agent: None, permissions: Default::default() };
        let thread = workspace.update_in(cx, |ws, window, cx| {
            let thread = cx.new(|cx| Thread::with_config(ws, Some(config), tmp.path().join("history"), window, cx));
            add_thread(ws, thread.clone(), window, cx);
            thread
        });
        cx.run_until_parked();
        let call = |name: &str, args: serde_json::Value, cx: &mut VisualTestContext| {
            let (reply, answer) = futures::channel::oneshot::channel();
            thread.update_in(cx, |t, window, cx| t.handle_tool_request(crate::forge_mcp::ToolRequest { name: name.into(), args, reply }, window, cx));
            cx.run_until_parked();
            answer
        };

        let mut context = call("user_context", json!({}), cx);
        let deadline = Instant::now() + Duration::from_secs(10);
        let text = loop {
            if let Ok(Some(answer)) = context.try_recv() {
                break answer.unwrap();
            }
            assert!(Instant::now() < deadline, "no context");
            cx.run_until_parked();
            cx.background_executor.timer(Duration::from_millis(20)).await;
        };
        assert!(text.contains("The user is in a.rs at line 2."), "{text}");
        assert!(text.contains("They selected lines 2-3:\n```\nfn two() {}\nfn t"), "{text}");
        assert!(text.contains("Open files: a.rs"), "{text}");

        // A note, edited before keeping it, goes to the first instructions file that exists.
        let mut kept = call("remember", json!({ "note": "Run cargo test." }), cx);
        let card = thread.read_with(cx, |t, _| t.entries.iter().position(|e| matches!(e, Entry::Remember { .. })).unwrap());
        let note = thread.read_with(cx, |t, _| match &t.entries[card] {
            Entry::Remember { note, file, .. } => {
                assert_eq!(file, &std::path::PathBuf::from("/root/AGENTS.md"));
                note.clone()
            }
            _ => unreachable!(),
        });
        note.update_in(cx, |e, window, cx| e.set_text("Run cargo test -q before finishing.", window, cx));
        thread.update(cx, |t, cx| t.answer_remember(card, true, cx));
        cx.run_until_parked();
        assert!(matches!(kept.try_recv().unwrap(), Some(Ok(text)) if text.contains("Kept in AGENTS.md")));
        assert_eq!(fs.load("/root/AGENTS.md".as_ref()).await.unwrap(), "# Repo\n\n## Notes from agents\n\n- Run cargo test -q before finishing.\n");

        let mut declined = call("remember", json!({ "note": "Something else." }), cx);
        let card = thread.read_with(cx, |t, _| t.entries.iter().rposition(|e| matches!(e, Entry::Remember { .. })).unwrap());
        thread.update(cx, |t, cx| t.answer_remember(card, false, cx));
        assert!(matches!(declined.try_recv().unwrap(), Some(Err(_))));
    }

    /// Typing in a new thread connects the agent and answers; an edit waits in the thread
    /// for review and is written once accepted; the rail and tab follow along.
    #[gpui::test]
    async fn a_thread_tab_converses_and_reviews_changes(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let tmp = tempfile::tempdir().unwrap();
        cx.update(workspace::AppState::test);
        cx.update(|cx| {
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            gpui_tokio::init(cx);
            editor::init(cx);
            init(cx);
            crate::thread_picker::init(cx);
        });
        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree("/root", json!({ "a.txt": "old\n", "b.txt": "one\ntwo\nthree\n", ".forge": { "AGENTS.md": "Be brief." } })).await;
        cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));
        let project = Project::test(fs.clone(), ["/root".as_ref()], cx).await;
        let window = cx.add_window(|window, cx| workspace::MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window.read_with(cx, |mw, _| mw.workspace().clone()).unwrap();
        let cx = &mut VisualTestContext::from_window(window.into(), cx);

        let agent = AgentSpec {
            id: "mock".into(),
            command: "python3".into(),
            args: vec![std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tools/mock-acp-agent.py").to_string_lossy().into_owned()],
            env: vec![],
            // The project lives in a fake file system; the process needs a real directory.
            cwd: Some(tmp.path().to_path_buf()),
        };
        let config = crate::config::AgentsConfig { instructions_files: crate::config::default_instructions_files(), agents: vec![agent], review_writes: true, verify_changes: true, mcp_servers: vec![], default_agent: None, permissions: Default::default() };
        let thread = workspace.update_in(cx, |ws, window, cx| {
            let thread = cx.new(|cx| Thread::with_config(ws, Some(config), tmp.path().join("history"), window, cx));
            add_thread(ws, thread.clone(), window, cx);
            thread
        });
        let view = workspace.read_with(cx, |ws, cx| ws.items_of_type::<ThreadView>(cx).next()).expect("the thread opened in a tab");
        assert_eq!(view.read_with(cx, |v, _| v.thread().clone()), thread);
        draw(&view, cx);

        // Not connected yet: sending connects first.
        view.update_in(cx, |v, window, cx| {
            v.input.update(cx, |e, cx| e.set_text("hello there", window, cx));
            v.send(&Send, window, cx);
        });
        wait_for(cx, &thread, "an answer", |t| t.status() == Status::Ready && t.entries.iter().any(|e| matches!(e, Entry::Agent(_)))).await;
        assert_eq!(view.read_with(cx, |v, cx| v.tab_content_text(0, cx)), "hello there");
        assert_eq!(thread.read_with(cx, |t, _| t.summary().0), "Answered");
        assert_eq!(view.read_with(cx, |v, cx| v.input.read(cx).text(cx)), "", "the input clears once sent");
        // Forge's default policy puts the agent in its own auto mode.
        assert_eq!(thread.read_with(cx, |t, _| t.current_mode.clone()), Some("auto".to_string()));
        // The project's standing instructions go with the session's first message only.
        let notes = |cx: &mut VisualTestContext| thread.read_with(cx, |t, _| t.entries.iter().filter(|e| matches!(e, Entry::System(text, _) if text == "Sent your instructions from .forge/AGENTS.md.")).count());
        assert_eq!(notes(cx), 1);
        let received = |cx: &mut VisualTestContext| thread.read_with(cx, |t, cx| t.entries.iter().filter(|e| matches!(e, Entry::Agent(md) if md.read(cx).source().contains("Instructions received"))).count());
        assert_eq!(received(cx), 1, "the agent got them");
        draw(&view, cx);

        // Following the agent: the file it reads opens next to the thread, at its line.
        let followed_row = |cx: &mut VisualTestContext| {
            view.update_in(cx, |v, _, cx| {
                let editor = v.follow.as_ref()?.editor.clone()?;
                let path = v.follow.as_ref()?.location.path.clone();
                Some((path, editor.update(cx, |e, cx| e.selections.newest::<Point>(&e.display_snapshot(cx)).head().row)))
            })
        };
        view.update_in(cx, |v, window, cx| {
            v.input.update(cx, |e, cx| e.set_text("read b.txt 2", window, cx));
            v.send(&Send, window, cx);
        });
        wait_for(cx, &thread, "the agent to read b.txt", |t| t.location().is_some_and(|l| l.line == Some(2))).await;
        cx.run_until_parked();
        assert_eq!(followed_row(cx), Some((std::path::PathBuf::from("/root/b.txt"), 2)), "the follow pane shows the agent's line");
        draw(&view, cx);

        // Paused: the agent moves on, the pane stays; jumping catches up.
        view.update(cx, |v, cx| v.pause_follow(cx));
        wait_for(cx, &thread, "the turn to end", |t| t.status() == Status::Ready).await;
        assert_eq!((notes(cx), received(cx)), (1, 1), "not sent again with later messages");
        view.update_in(cx, |v, window, cx| {
            v.input.update(cx, |e, cx| e.set_text("read b.txt 0", window, cx));
            v.send(&Send, window, cx);
        });
        wait_for(cx, &thread, "the agent to move", |t| t.location().is_some_and(|l| l.line == Some(0))).await;
        cx.run_until_parked();
        assert_eq!(followed_row(cx).map(|(_, row)| row), Some(2), "paused: still where the user left it");
        view.update_in(cx, |v, window, cx| v.jump_to_agent(window, cx));
        cx.run_until_parked();
        assert_eq!(followed_row(cx).map(|(_, row)| row), Some(0), "jumped to the agent");
        view.update(cx, |v, cx| v.close_follow(cx));
        assert!(view.read_with(cx, |v, cx| v.render_follow_visible(cx)) == false);
        wait_for(cx, &thread, "the turn to end", |t| t.status() == Status::Ready).await;

        // An edit waits for review in the thread.
        view.update_in(cx, |v, window, cx| {
            v.input.update(cx, |e, cx| e.set_text("edit a.txt new", window, cx));
            v.send(&Send, window, cx);
        });
        wait_for(cx, &thread, "the change to review", |t| t.pending_reviews() == 1).await;
        assert_eq!(thread.read_with(cx, |t, _| t.summary().0), "1 to review");
        assert_eq!(thread.read_with(cx, |t, _| t.activity()), Some(("Waiting for you: review the change to a.txt".to_string(), true)), "the working bar says it waits for you");
        let touched = thread.read_with(cx, |t, _| t.touched_files());
        assert_eq!(touched.iter().map(|(p, _, _)| p.clone()).collect::<Vec<_>>(), [std::path::PathBuf::from("/root/a.txt")]);
        draw(&view, cx);

        // The proposal is editable: the user's version is what gets written.
        let review = thread.read_with(cx, |t, _| t.entries.iter().position(|e| matches!(e, Entry::Review { .. })).unwrap());
        let buffer = thread.read_with(cx, |t, _| match &t.entries[review] {
            Entry::Review { diff, .. } => diff.buffer.clone(),
            _ => unreachable!(),
        });
        let editable = thread.read_with(cx, |t, cx| match &t.entries[review] {
            Entry::Review { diff, .. } => !diff.editor.read(cx).read_only(cx),
            _ => unreachable!(),
        });
        assert!(editable, "a pending change can be edited in the thread");
        buffer.update(cx, |b, cx| {
            let len = b.len();
            b.edit([(0..len, "new, tweaked\n")], None, cx);
        });
        cx.run_until_parked();
        thread.update(cx, |t, cx| t.answer_review(review, true, cx));
        wait_for(cx, &thread, "the turn to end", |t| t.status() == Status::Ready && t.pending_reviews() == 0).await;
        cx.run_until_parked();
        assert_eq!(fs.load("/root/a.txt".as_ref()).await.unwrap(), "new, tweaked\n");
        let outcome = thread.read_with(cx, |t, _| match &t.entries[review] {
            Entry::Review { outcome, .. } => *outcome,
            _ => unreachable!(),
        });
        assert_eq!(outcome, Some("Applied with your edits"));
        draw(&view, cx);

        // The turn remembers what it changed and how long it took; its summary unfolds a
        // file's diff in the conversation.
        let (user_ix, files, took, activity) = thread.read_with(cx, |t, _| {
            let turn = t.checkpoints.last().unwrap();
            (turn.entry, turn.turn_files.clone(), turn.duration, t.activity())
        });
        assert_eq!(files.into_iter().collect::<Vec<_>>(), [(std::path::PathBuf::from("/root/a.txt"), (Some("old\n".to_string()), "new, tweaked\n".to_string()))]);
        assert!(took.is_some() && activity.is_none());
        view.update_in(cx, |v, window, cx| {
            let edit = crate::diff::Edit { path: "/root/a.txt".into(), old_text: Some("old\n".into()), new_text: "new, tweaked\n".into() };
            let languages = v.thread.read(cx).languages().clone();
            v.turn_diffs.insert((user_ix, "/root/a.txt".into()), crate::diff::DiffView::new(edit, languages, window, cx));
        });
        draw(&view, cx);
    }

    fn mock_auth_agent(marker: &std::path::Path) -> AgentSpec {
        AgentSpec {
            id: "mock-auth".into(),
            command: "python3".into(),
            args: vec![std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tools/mock-acp-agent.py").to_string_lossy().into_owned()],
            env: vec![("MOCK_REQUIRE_AUTH".into(), "1".into()), ("MOCK_AUTH_MARKER".into(), marker.to_string_lossy().into_owned())],
            cwd: None,
        }
    }

    fn auth_card(t: &Thread) -> Option<(usize, &crate::thread::AuthState, bool)> {
        t.entries.iter().enumerate().find_map(|(i, e)| match e {
            Entry::Auth { state, terminal, .. } => Some((i, state, terminal.is_some())),
            _ => None,
        })
    }

    /// A thread tab for a fresh workspace without folders, with `config`'s agents.
    /// A thread in a worktree changes the worktree's files, not yours; applying brings its
    /// changes over, and removing the worktree ends the thread.
    /// `/name …` sends the user's prompt from `.forge/prompts/name.md`, with what follows
    /// the command in place of `$ARGUMENTS`; the thread shows what was typed.
    #[gpui::test]
    async fn sends_the_users_prompts(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("project");
        // The prompts are read from disk; the files the agent reads, from the project.
        std::fs::create_dir_all(root.join(".forge/prompts")).unwrap();
        std::fs::write(root.join(".forge/prompts/peek.md"), "---\ndescription: Read a file\n---\nread $ARGUMENTS").unwrap();
        cx.update(workspace::AppState::test);
        cx.update(|cx| {
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            gpui_tokio::init(cx);
            editor::init(cx);
            init(cx);
        });
        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree(&root, json!({ "b.txt": "uno" })).await;
        cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));
        let project = Project::test(fs.clone(), [root.as_path()], cx).await;
        let window = cx.add_window(|window, cx| workspace::MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window.read_with(cx, |mw, _| mw.workspace().clone()).unwrap();
        let cx = &mut VisualTestContext::from_window(window.into(), cx);
        let agent = AgentSpec {
            id: "mock".into(),
            command: "python3".into(),
            args: vec![std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tools/mock-acp-agent.py").to_string_lossy().into_owned()],
            env: vec![],
            cwd: Some(tmp.path().to_path_buf()),
        };
        let config = crate::config::AgentsConfig { instructions_files: crate::config::default_instructions_files(), agents: vec![agent], review_writes: true, verify_changes: true, mcp_servers: vec![], default_agent: None, permissions: Default::default() };
        let thread = workspace.update_in(cx, |ws, window, cx| {
            let thread = cx.new(|cx| Thread::with_config(ws, Some(config), tmp.path().join("history"), window, cx));
            add_thread(ws, thread.clone(), window, cx);
            thread
        });

        let peek = thread.read_with(cx, |t, _| t.commands().into_iter().find(|c| c.name == "peek"));
        assert_eq!(peek.map(|c| (c.description, c.hint)), Some(("Read a file (your prompt)".to_string(), Some("…".to_string()))));

        thread.update_in(cx, |t, window, cx| t.ask("/peek b.txt".into(), window, cx));
        wait_for(cx, &thread, "the prompt's answer", |t| t.status() == Status::Ready && t.entries.iter().any(|e| matches!(e, Entry::Agent(_)))).await;
        let (user, answered) = thread.read_with(cx, |t, cx| {
            let user = t.entries.iter().find_map(|e| match e {
                Entry::User(text, labels) => Some((text.clone(), labels.clone())),
                _ => None,
            });
            let answered = t.entries.iter().any(|e| matches!(e, Entry::Agent(md) if md.read(cx).source().contains("Read: uno")));
            (user, answered)
        });
        assert_eq!(user, Some(("/peek b.txt".to_string(), vec!["prompt .forge/prompts/peek.md".to_string()])));
        assert!(answered, "the agent got the prompt, expanded");
    }

    #[gpui::test]
    async fn a_thread_works_in_its_own_worktree(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let tmp = tempfile::tempdir().unwrap();
        let repo = std::fs::canonicalize(tmp.path()).unwrap().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let git = |args: &[&str]| assert!(std::process::Command::new("git").arg("-C").arg(&repo).args(args).status().unwrap().success(), "git {args:?}");
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.name", "Test"]);
        git(&["config", "user.email", "test@example.com"]);
        std::fs::write(repo.join("lib.txt"), "original\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "init"]);

        cx.update(workspace::AppState::test);
        cx.update(|cx| {
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            gpui_tokio::init(cx);
            editor::init(cx);
            init(cx);
        });
        let fs = fs::RealFs::new(None, cx.executor());
        cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));
        let project = Project::test(fs.clone(), [repo.as_path()], cx).await;
        let window = cx.add_window(|window, cx| workspace::MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window.read_with(cx, |mw, _| mw.workspace().clone()).unwrap();
        let cx = &mut VisualTestContext::from_window(window.into(), cx);

        let worktree = crate::worktree::create(&repo).unwrap();
        let agent = AgentSpec {
            id: "mock".into(),
            command: "python3".into(),
            args: vec![std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tools/mock-acp-agent.py").to_string_lossy().into_owned()],
            env: vec![],
            cwd: Some(tmp.path().to_path_buf()),
        };
        let config = crate::config::AgentsConfig { instructions_files: crate::config::default_instructions_files(), agents: vec![agent], review_writes: false, verify_changes: false, mcp_servers: vec![], default_agent: None, permissions: Default::default() };
        let history = tmp.path().join("history");
        let thread = workspace.update_in(cx, |ws, window, cx| {
            let thread = cx.new(|cx| Thread::with_config_in(ws, Some(config), history, Some(worktree.clone()), window, cx));
            add_thread(ws, thread.clone(), window, cx);
            thread
        });
        assert_eq!(thread.read_with(cx, |t, _| t.root().clone()), worktree.path);
        let view = workspace.read_with(cx, |ws, cx| ws.items_of_type::<ThreadView>(cx).next()).unwrap();
        view.update_in(cx, |v, window, cx| {
            v.input.update(cx, |e, cx| e.set_text("edit lib.txt changed", window, cx));
            v.send(&Send, window, cx);
        });
        let has_system = |t: &Thread, text: &str| t.entries.iter().any(|e| matches!(e, Entry::System(s, _) if s.contains(text)));
        wait_for(cx, &thread, "the edit", |t| t.status() == Status::Ready && t.entries.iter().any(|e| matches!(e, Entry::Agent(_)))).await;
        let read = |path: &std::path::Path| std::fs::read_to_string(path).unwrap();
        assert_eq!(read(&worktree.path.join("lib.txt")), "changed\n", "the agent wrote in its worktree");
        assert_eq!(read(&repo.join("lib.txt")), "original\n", "your files are untouched");
        draw(&view, cx);

        thread.update(cx, |t, cx| t.apply_worktree(cx));
        wait_for(cx, &thread, "the changes to apply", |t| has_system(t, "Applied the changes to 1 file")).await;
        assert_eq!(read(&repo.join("lib.txt")), "changed\n");

        thread.update(cx, |t, cx| t.remove_worktree(false, cx));
        wait_for(cx, &thread, "the worktree to go", |t| has_system(t, "Removed the worktree")).await;
        assert!(!worktree.path.exists());
        assert!(thread.read_with(cx, |t, _| t.worktree().is_none()));
        thread.update_in(cx, |t, window, cx| t.connect(window, cx));
        assert!(thread.read_with(cx, |t, _| has_system(t, "start a new thread")), "nowhere left to work");
    }

    /// The editor's "Resolve with Agent" on a merge conflict asks the thread on screen.
    #[gpui::test]
    async fn conflicts_go_to_the_thread_on_screen(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let tmp = tempfile::tempdir().unwrap();
        let agent = AgentSpec {
            id: "mock".into(),
            command: "python3".into(),
            args: vec![std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tools/mock-acp-agent.py").to_string_lossy().into_owned()],
            env: vec![],
            cwd: Some(tmp.path().to_path_buf()),
        };
        let config = crate::config::AgentsConfig { instructions_files: crate::config::default_instructions_files(), agents: vec![agent], review_writes: true, verify_changes: false, mcp_servers: vec![], default_agent: None, permissions: Default::default() };
        let (thread, view, mut cx) = thread_tab(config, tmp.path().join("history"), cx).await;
        let cx = &mut cx;
        cx.update(|_, cx| crate::conflicts::init(cx));
        let conflict = zed_actions::agent::ConflictContent { file_path: "/p/a.rs".into(), conflict_text: "<<<<<<< HEAD\na\n=======\nb\n>>>>>>> feature\n".into(), ours_branch_name: "HEAD".into(), theirs_branch_name: "feature".into() };
        view.update_in(cx, |v, window, cx| {
            window.focus(&v.focus_handle(cx), cx);
            window.dispatch_action(Box::new(zed_actions::agent::ResolveConflictsWithAgent { conflicts: vec![conflict] }), cx);
        });
        wait_for(cx, &thread, "the conflict to be asked", |t| t.entries.iter().any(|e| matches!(e, Entry::User(text, _) if text.starts_with("Resolve this merge conflict.")))).await;
    }

    async fn thread_tab(config: crate::config::AgentsConfig, history: std::path::PathBuf, cx: &mut TestAppContext) -> (Entity<Thread>, Entity<ThreadView>, VisualTestContext) {
        let params = cx.update(workspace::AppState::test);
        cx.update(|cx| {
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            gpui_tokio::init(cx);
            editor::init(cx);
            init(cx);
        });
        let project = Project::test(params.fs.clone(), [], cx).await;
        let window = cx.add_window(|window, cx| workspace::MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window.read_with(cx, |mw, _| mw.workspace().clone()).unwrap();
        let mut cx = VisualTestContext::from_window(window.into(), cx);
        cx.update(|_, cx| crate::settings::set_in_memory(config.clone(), history.clone(), cx));
        let thread = workspace.update_in(&mut cx, |ws, window, cx| {
            let thread = cx.new(|cx| Thread::with_config(ws, Some(config), history, window, cx));
            add_thread(ws, thread.clone(), window, cx);
            thread
        });
        let view = workspace.read_with(&cx, |ws, cx| ws.items_of_type::<ThreadView>(cx).next()).unwrap();
        (thread, view, cx)
    }

    /// The whole login inside a thread: card appears, login runs in a real Zed terminal, the
    /// agent restarts signed in and the failed prompt is back in the input.
    #[gpui::test]
    async fn signs_in_without_leaving_forge(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let tmp = tempfile::tempdir().unwrap();
        let marker = tmp.path().join("signed-in");
        let config = crate::config::AgentsConfig { instructions_files: crate::config::default_instructions_files(), agents: vec![mock_auth_agent(&marker)], review_writes: true, verify_changes: true, mcp_servers: vec![], default_agent: None, permissions: Default::default() };
        let (thread, view, mut cx) = thread_tab(config, tmp.path().join("history"), cx).await;
        let cx = &mut cx;

        thread.update_in(cx, |t, window, cx| t.connect(window, cx));
        wait_for(cx, &thread, "connection", |t| t.status() == Status::Ready).await;
        wait_for(cx, &thread, "sign-in card", |t| auth_card(t).is_some()).await;
        assert_eq!(thread.read_with(cx, |t, _| t.auth_methods[0].id.clone()), "mock-login");

        // A prompt fails for lack of auth; it is remembered for after the login.
        view.update_in(cx, |v, window, cx| {
            v.input.update(cx, |e, cx| e.set_text("hello after login", window, cx));
            v.send(&Send, window, cx);
        });
        wait_for(cx, &thread, "failed prompt", |t| t.retry_prompt.is_some()).await;

        // Click the method: the login runs in an embedded, interactive Zed terminal.
        let ix = thread.read_with(cx, |t, _| auth_card(t).unwrap().0);
        thread.update_in(cx, |t, window, cx| t.run_auth(ix, 0, window, cx));
        wait_for(cx, &thread, "login terminal", |t| auth_card(t).is_some_and(|(_, _, has_terminal)| has_terminal)).await;
        let terminal = thread.read_with(cx, |t, cx| match &t.entries[ix] {
            Entry::Auth { terminal: Some(view), .. } => view.read(cx).terminal().clone(),
            _ => unreachable!(),
        });
        // Wait for the prompt, then press Enter like the user would.
        let deadline = Instant::now() + Duration::from_secs(20);
        while !terminal.read_with(cx, |t, _| t.get_content().contains("Press Enter")) {
            assert!(Instant::now() < deadline, "login prompt never appeared");
            cx.run_until_parked();
            cx.background_executor.timer(Duration::from_millis(20)).await;
        }
        terminal.update(cx, |t, _| t.input(b"\r".to_vec()));

        wait_for(cx, &thread, "signed in", |t| matches!(auth_card(t), Some((_, crate::thread::AuthState::Done, _)))).await;
        assert!(marker.exists());
        wait_for(cx, &thread, "restarted agent", |t| t.status() == Status::Ready && t.retry_prompt.is_none()).await;
        let input = view.read_with(cx, |v, cx| v.input.read(cx).text(cx));
        let kept = thread.read_with(cx, |t, _| t.entries.iter().any(|e| matches!(e, Entry::User(text, _) if text == "hello after login")));
        assert_eq!(input, "hello after login", "failed prompt is back in the input");
        assert!(kept, "transcript kept across the restart");

        // Now the agent answers.
        view.update_in(cx, |v, window, cx| v.send(&Send, window, cx));
        wait_for(cx, &thread, "answer", |t| t.status() == Status::Ready && t.entries.iter().any(|e| matches!(e, Entry::Agent(_)))).await;
        draw(&view, cx);
    }

    /// `ask` (behind "Fix with agent") connects a disconnected agent and sends the prompt;
    /// the conversation is saved and listed under Earlier in a new thread's rail.
    /// Agents like Copilot CLI refuse `session/new` until signed in: the connection fails,
    /// and the sign-in card still offers the login methods `initialize` listed.
    #[gpui::test]
    async fn a_session_refused_for_lack_of_sign_in_offers_the_login(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let tmp = tempfile::tempdir().unwrap();
        let mut agent = mock_auth_agent(&tmp.path().join("signed-in"));
        agent.env.push(("MOCK_AUTH_ON_NEW_SESSION".into(), "1".into()));
        let config = crate::config::AgentsConfig { instructions_files: crate::config::default_instructions_files(), agents: vec![agent], review_writes: true, verify_changes: true, mcp_servers: vec![], default_agent: None, permissions: Default::default() };
        let (thread, _view, mut cx) = thread_tab(config, tmp.path().join("history"), cx).await;
        let cx = &mut cx;
        thread.update_in(cx, |t, window, cx| t.connect(window, cx));
        wait_for(cx, &thread, "the refused connection", |t| t.status() == Status::Disconnected && t.entries.iter().any(|e| matches!(e, Entry::System(text, _) if text.starts_with("Could not connect")))).await;
        wait_for(cx, &thread, "sign-in card", |t| auth_card(t).is_some()).await;
        assert_eq!(thread.read_with(cx, |t, _| t.auth_methods.iter().map(|m| m.id.clone()).collect::<Vec<_>>()), ["mock-login"]);
    }

    #[gpui::test]
    async fn ask_connects_sends_and_saves(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let tmp = tempfile::tempdir().unwrap();
        let mut agent = mock_auth_agent(&tmp.path().join("unused"));
        agent.env.retain(|(key, _)| key != "MOCK_REQUIRE_AUTH");
        let config = crate::config::AgentsConfig { instructions_files: crate::config::default_instructions_files(), agents: vec![agent], review_writes: true, verify_changes: true, mcp_servers: vec![], default_agent: None, permissions: Default::default() };
        let (thread, view, mut cx) = thread_tab(config, tmp.path().join("history"), cx).await;
        let cx = &mut cx;

        thread.update_in(cx, |t, window, cx| t.ask("why does this test fail?".into(), window, cx));
        wait_for(cx, &thread, "answer to the asked prompt", |t| {
            t.status() == Status::Ready
                && t.entries.iter().any(|e| matches!(e, Entry::User(text, _) if text == "why does this test fail?"))
                && t.entries.iter().any(|e| matches!(e, Entry::Agent(_)))
        })
        .await;
        assert!(thread.read_with(cx, |t, _| t.queued_prompt.is_none()));

        // Closed, it is listed under Earlier in the Threads panel; opening it shows the
        // transcript in a tab of its own.
        let workspace = view.read_with(cx, |v, _| v.workspace.upgrade().unwrap());
        let panel = workspace.update_in(cx, |ws, window, cx| cx.new(|cx| crate::threads_panel::ThreadsPanel::new(ws, window, cx)));
        assert!(panel.read_with(cx, |p, _| p.saved().is_empty()), "open threads aren't listed as saved");
        panel.update_in(cx, |p, window, cx| p.close_thread(&thread, window, cx));
        cx.run_until_parked();
        assert!(workspace.read_with(cx, |ws, cx| ws.items_of_type::<ThreadView>(cx).next().is_none()), "closing a thread closes its tab");
        let saved = panel.update(cx, |p, cx| {
            p.refresh_saved(cx);
            p.saved().to_vec()
        });
        assert_eq!(saved.iter().map(|s| s.title.as_str()).collect::<Vec<_>>(), ["why does this test fail?"]);
        panel.update_in(cx, |p, window, cx| p.open_saved(&saved[0], window, cx));
        cx.run_until_parked();
        let reopened = workspace.read_with(cx, |ws, cx| ws.items_of_type::<ThreadView>(cx).next()).expect("a tab for the saved conversation");
        let reopened_thread = reopened.read_with(cx, |v, _| v.thread().clone());
        assert!(reopened_thread != thread, "a saved conversation opens as a new thread");
        assert_eq!(reopened_thread.read_with(cx, |t, _| t.title()), Some("why does this test fail?".into()));
        let view = reopened;
        draw(&view, cx);

        // Deleting it (once confirmed) closes it and drops it from Earlier for good.
        panel.update_in(cx, |p, window, cx| p.delete_thread(&reopened_thread, window, cx));
        cx.run_until_parked();
        cx.simulate_prompt_answer("Delete");
        cx.run_until_parked();
        assert!(workspace.read_with(cx, |ws, cx| ws.items_of_type::<ThreadView>(cx).next().is_none()), "its tab is closed");
        let saved = panel.update(cx, |p, cx| {
            p.refresh_saved(cx);
            p.saved().to_vec()
        });
        assert!(saved.is_empty(), "the conversation is deleted, not saved");
    }

    /// Permission requests the policy allows are answered for the user (and say so); the
    /// rest wait. The agent's token usage reaches the thread.
    #[gpui::test]
    async fn answers_allowed_requests_and_tracks_usage(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let tmp = tempfile::tempdir().unwrap();
        let mut agent = mock_auth_agent(&tmp.path().join("unused"));
        agent.env.retain(|(key, _)| key != "MOCK_REQUIRE_AUTH");
        let permissions = crate::permissions::Permissions { allow_commands: vec!["echo".into()], ..Default::default() };
        let config = crate::config::AgentsConfig { instructions_files: crate::config::default_instructions_files(), agents: vec![agent], review_writes: true, verify_changes: true, mcp_servers: vec![], default_agent: None, permissions };
        let (thread, _view, mut cx) = thread_tab(config, tmp.path().join("history"), cx).await;
        let cx = &mut cx;
        let permission = |t: &Thread| {
            t.entries.iter().rev().find_map(|e| match e {
                Entry::Permission { title, resolved, .. } => Some((title.clone(), resolved.clone())),
                _ => None,
            })
        };

        thread.update_in(cx, |t, window, cx| t.ask("run echo hola".into(), window, cx));
        wait_for(cx, &thread, "the allowed command to finish", |t| t.status() == Status::Ready && permission(t).is_some()).await;
        let (title, resolved) = thread.read_with(cx, |t, _| permission(t)).unwrap();
        assert!(title.contains("echo hola"));
        assert!(resolved.as_deref().is_some_and(|r| r.starts_with("Allowed automatically")), "answered for the user: {resolved:?}");
        let usage = thread.read_with(cx, |t, _| t.usage.clone());
        assert_eq!(usage.context_size, Some(200_000));
        assert!(usage.context_used.is_some_and(|u| u > 8_000) && usage.turns == 1 && usage.cost.is_some(), "{usage:?}");
        // The agent's modes (Forge's default policy picked its auto mode), and its plan
        // limits from the hidden `/usage` session.
        assert_eq!(thread.read_with(cx, |t, _| (t.modes.len(), t.current_mode.clone())), (4, Some("auto".into())));
        let deadline = Instant::now() + Duration::from_secs(20);
        let plan = loop {
            cx.run_until_parked();
            if let Some(plan) = thread.read_with(cx, |t, cx| t.plan_usage(cx)) {
                break plan;
            }
            assert!(Instant::now() < deadline, "timed out waiting for the plan usage");
            cx.background_executor.timer(Duration::from_millis(20)).await;
        };
        assert_eq!(plan.tightest().map(|l| l.percent), Some(42.0));
        let shown_usage = thread.read_with(cx, |t, cx| {
            t.entries.iter().any(|e| match e {
                Entry::Agent(md) => md.read(cx).source().contains("5-hour"),
                Entry::User(text, _) => text == "/usage",
                _ => false,
            })
        });
        assert!(!shown_usage, "the /usage exchange stays out of the conversation");
        thread.update(cx, |t, cx| t.set_mode("acceptEdits".into(), cx));
        wait_for(cx, &thread, "the mode to change", |t| t.current_mode.as_deref() == Some("acceptEdits")).await;
        // The session settings (model…), changed from Forge, as the agent confirms them.
        let model = |t: &Thread| t.config_options.iter().find(|o| o.id == "model").map(|o| o.current_label());
        assert_eq!(thread.read_with(cx, |t, _| model(t)), Some("Sonnet".into()));
        thread.update(cx, |t, cx| t.set_config_option("model".into(), serde_json::Value::String("opus".into()), cx));
        wait_for(cx, &thread, "the model to change", |t| model(t).as_deref() == Some("Opus")).await;

        thread.update_in(cx, |t, window, cx| t.ask("run ls".into(), window, cx));
        wait_for(cx, &thread, "the permission request", |t| permission(t).is_some_and(|(title, _)| title.contains("`ls`"))).await;
        assert_eq!(thread.read_with(cx, |t, _| permission(t)).unwrap().1, None, "not in the list: it waits for the user");
        assert_eq!(thread.read_with(cx, |t, _| t.status()), Status::Busy);
    }

    /// What the agent writes is listed with its original; undo puts the file back, keep
    /// stops listing it.
    #[gpui::test]
    async fn lists_keeps_and_undoes_the_agents_changes(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let tmp = tempfile::tempdir().unwrap();
        cx.update(workspace::AppState::test);
        cx.update(|cx| {
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            gpui_tokio::init(cx);
            editor::init(cx);
            init(cx);
        });
        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree("/root", json!({ "a.txt": "old\n" })).await;
        cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));
        let project = Project::test(fs.clone(), ["/root".as_ref()], cx).await;
        let window = cx.add_window(|window, cx| workspace::MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window.read_with(cx, |mw, _| mw.workspace().clone()).unwrap();
        let cx = &mut VisualTestContext::from_window(window.into(), cx);
        let mut agent = mock_auth_agent(&tmp.path().join("unused"));
        agent.env.retain(|(key, _)| key != "MOCK_REQUIRE_AUTH");
        agent.cwd = Some(tmp.path().to_path_buf());
        let permissions = crate::permissions::Permissions { mode: crate::permissions::PermissionMode::AllowEdits, ..Default::default() };
        let config = crate::config::AgentsConfig { instructions_files: crate::config::default_instructions_files(), agents: vec![agent], review_writes: true, verify_changes: true, mcp_servers: vec![], default_agent: None, permissions };
        let thread = workspace.update_in(cx, |ws, window, cx| {
            let thread = cx.new(|cx| Thread::with_config(ws, Some(config), tmp.path().join("history"), window, cx));
            add_thread(ws, thread.clone(), window, cx);
            thread
        });
        let view = workspace.read_with(cx, |ws, cx| ws.items_of_type::<ThreadView>(cx).next()).unwrap();

        thread.update_in(cx, |t, window, cx| t.ask("write a.txt brand new".into(), window, cx));
        wait_for(cx, &thread, "the write to be listed", |t| t.status() == Status::Ready && !t.changes.is_empty()).await;
        let change = thread.read_with(cx, |t, _| t.changes[0].clone());
        assert_eq!((change.path.as_path(), change.original.as_deref(), change.current.as_str()), (std::path::Path::new("/root/a.txt"), Some("old\n"), "brand new"));
        assert_eq!(change.stats(), (1, 1));
        // The review tab: the file against before the agent.
        view.update_in(cx, |v, window, cx| v.open_review(Some("/root/a.txt".into()), window, cx));
        cx.run_until_parked();
        let review = workspace.read_with(cx, |ws, cx| ws.active_item(cx).and_then(|i| i.downcast::<editor::Editor>())).expect("a review tab");
        assert_eq!(review.read_with(cx, |e, cx| e.buffer().read(cx).title(cx).to_string()), "Changes · write a.txt brand new");
        draw(&view, cx);

        thread.update(cx, |t, cx| t.undo_changes(None, cx));
        wait_for(cx, &thread, "the undo", |t| t.changes.is_empty()).await;
        assert_eq!(fs.load("/root/a.txt".as_ref()).await.unwrap(), "old\n", "back as before the agent");

        thread.update_in(cx, |t, window, cx| t.ask("write a.txt again".into(), window, cx));
        wait_for(cx, &thread, "the second write", |t| t.status() == Status::Ready && !t.changes.is_empty()).await;
        thread.update(cx, |t, cx| t.keep_changes(None, cx));
        assert!(thread.read_with(cx, |t, _| t.changes.is_empty()));
        assert_eq!(fs.load("/root/a.txt".as_ref()).await.unwrap(), "again", "kept");
    }

    /// A file the agent changed shows its changes in a normal editor, with Keep / Undo per
    /// change; once nothing is left, the editor is back to git's diff.
    #[gpui::test]
    async fn keeps_and_undoes_single_changes_in_the_editor(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let tmp = tempfile::tempdir().unwrap();
        let params = cx.update(workspace::AppState::test);
        cx.update(|cx| {
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            gpui_tokio::init(cx);
            editor::init(cx);
            init(cx);
        });
        cx.update(crate::agent_review::init);
        let fs = params.fs.as_fake();
        fs.insert_tree("/root", json!({ "a.txt": "a\nB\nc\nD\n" })).await;
        let project = Project::test(params.fs.clone(), ["/root".as_ref()], cx).await;
        let window = cx.add_window(|window, cx| workspace::MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window.read_with(cx, |mw, _| mw.workspace().clone()).unwrap();
        let cx = &mut VisualTestContext::from_window(window.into(), cx);
        let agent = AgentSpec { id: "a".into(), command: "true".into(), args: vec![], env: vec![], cwd: None };
        let config = crate::config::AgentsConfig { instructions_files: crate::config::default_instructions_files(), agents: vec![agent], review_writes: true, verify_changes: true, mcp_servers: vec![], default_agent: None, permissions: Default::default() };
        let thread = workspace.update_in(cx, |ws, window, cx| cx.new(|cx| Thread::with_config(ws, Some(config), tmp.path().join("history"), window, cx)));
        // The agent changed lines 2 and 4.
        thread.update(cx, |t, cx| {
            t.changes.push(crate::thread::ChangedFile { path: "/root/a.txt".into(), original: Some("a\nb\nc\nd\n".into()), current: "a\nB\nc\nD\n".into() });
            t.changes_updated(cx);
        });
        let item = workspace.update_in(cx, |ws, window, cx| ws.open_abs_path("/root/a.txt".into(), workspace::OpenOptions::default(), window, cx)).await.unwrap();
        let editor = item.downcast::<editor::Editor>().unwrap();
        cx.run_until_parked();
        let hunk_rows = |cx: &mut VisualTestContext| {
            editor.update(cx, |e, cx| {
                let snapshot = e.buffer().read(cx).snapshot(cx);
                snapshot.diff_hunks().map(|h| h.row_range.start.0).collect::<Vec<_>>()
            })
        };
        let rows = hunk_rows(cx);
        assert_eq!(rows.len(), 2, "both changes show in the editor");

        let at_row = |row: u32, cx: &mut VisualTestContext| {
            editor.update_in(cx, |e, window, cx| {
                e.change_selections(Default::default(), window, cx, |s| s.select_ranges([language::Point::new(row, 0)..language::Point::new(row, 0)]));
            });
        };
        at_row(rows[0], cx);
        workspace.update_in(cx, |_, window, cx| window.dispatch_action(Box::new(crate::agent_review::KeepAgentChange), cx));
        cx.run_until_parked();
        assert_eq!(thread.read_with(cx, |t, _| t.changes[0].original.clone()).as_deref(), Some("a\nB\nc\nd\n"), "kept: part of the baseline");
        let rows = hunk_rows(cx);
        assert_eq!(rows.len(), 1);

        at_row(rows[0], cx);
        workspace.update_in(cx, |_, window, cx| window.dispatch_action(Box::new(crate::agent_review::UndoAgentChange), cx));
        cx.run_until_parked();
        assert_eq!(editor.read_with(cx, |e, cx| e.buffer().read(cx).snapshot(cx).text()), "a\nB\nc\nd\n", "undone: the old line is back");
        assert!(thread.read_with(cx, |t, _| t.changes.is_empty()), "nothing left to review");
        assert_eq!(fs.load("/root/a.txt".as_ref()).await.unwrap(), "a\nB\nc\nd\n", "saved");
        assert!(editor.read_with(cx, |e, _| e.addon::<crate::agent_review::Overlay>().is_none()), "back to git's diff");
    }

    /// Each message is a checkpoint: restoring one puts the files the agent wrote since
    /// back as they were (created files go away), and the changes list follows.
    #[gpui::test]
    async fn restores_checkpoints(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let tmp = tempfile::tempdir().unwrap();
        let params = cx.update(workspace::AppState::test);
        cx.update(|cx| {
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            gpui_tokio::init(cx);
            editor::init(cx);
            init(cx);
            <dyn fs::Fs>::set_global(params.fs.clone(), cx);
        });
        let fs = params.fs.as_fake();
        fs.insert_tree("/root", json!({ "a.txt": "v1" })).await;
        let project = Project::test(params.fs.clone(), ["/root".as_ref()], cx).await;
        let window = cx.add_window(|window, cx| workspace::MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window.read_with(cx, |mw, _| mw.workspace().clone()).unwrap();
        let cx = &mut VisualTestContext::from_window(window.into(), cx);
        let agent = AgentSpec { id: "a".into(), command: "true".into(), args: vec![], env: vec![], cwd: None };
        let config = crate::config::AgentsConfig { instructions_files: crate::config::default_instructions_files(), agents: vec![agent], review_writes: true, verify_changes: true, mcp_servers: vec![], default_agent: None, permissions: Default::default() };
        let thread = workspace.update_in(cx, |ws, window, cx| cx.new(|cx| Thread::with_config(ws, Some(config), tmp.path().join("history"), window, cx)));
        let message = |text: &str, cx: &mut VisualTestContext| {
            thread.update(cx, |t, _| {
                t.entries.push(Entry::User(text.into(), vec![]));
                t.begin_checkpoint();
                t.entries.len() - 1
            })
        };
        let write = |path: &str, old: Option<&str>, new: &str, cx: &mut VisualTestContext| {
            let record = crate::project_fs::WriteRecord { path: path.into(), old_text: old.map(str::to_string), new_text: new.into(), problems_before: None };
            thread.update(cx, |t, cx| t.record_write(record, cx));
        };

        let first = message("one", cx);
        fs.insert_file("/root/a.txt", b"v2".to_vec()).await;
        write("/root/a.txt", Some("v1"), "v2", cx);
        let second = message("two", cx);
        fs.insert_file("/root/a.txt", b"v3".to_vec()).await;
        write("/root/a.txt", Some("v2"), "v3", cx);
        fs.insert_file("/root/b.txt", b"new".to_vec()).await;
        write("/root/b.txt", None, "new", cx);
        assert_eq!(thread.read_with(cx, |t, _| t.checkpoint_at(first).map(|c| c.files.len())), Some(2));
        assert_eq!(thread.read_with(cx, |t, _| t.checkpoint_at(second).map(|c| c.files.len())), Some(2));

        assert_eq!(cx.update(|_, cx| crate::presence::tests::line(&[thread.clone()], cx)).as_deref(), Some("2 files to review"), "the status bar line");
        thread.update(cx, |t, cx| t.restore_checkpoint(second, cx));
        cx.run_until_parked();
        assert_eq!(fs.load("/root/a.txt".as_ref()).await.unwrap(), "v2");
        assert!(!fs.is_file("/root/b.txt".as_ref()).await, "created after the message: removed");
        let changes = thread.read_with(cx, |t, _| t.changes.iter().map(|c| (c.path.clone(), c.original.clone(), c.current.clone())).collect::<Vec<_>>());
        assert_eq!(changes, vec![("/root/a.txt".into(), Some("v1".into()), "v2".into())], "a.txt still differs from before the agent");
        assert!(thread.read_with(cx, |t, _| t.checkpoint_at(second).is_none()), "nothing to restore there any more");

        thread.update(cx, |t, cx| t.restore_checkpoint(first, cx));
        cx.run_until_parked();
        assert_eq!(fs.load("/root/a.txt".as_ref()).await.unwrap(), "v1");
        assert!(thread.read_with(cx, |t, _| t.changes.is_empty()));
    }

    /// A turn's summary rolls the turn back, or one of its files: they go back as they were
    /// before that turn (created files go away), later turns warn first, and the changes
    /// list follows.
    #[gpui::test]
    async fn undoes_a_turn(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let tmp = tempfile::tempdir().unwrap();
        let params = cx.update(workspace::AppState::test);
        cx.update(|cx| {
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            gpui_tokio::init(cx);
            editor::init(cx);
            init(cx);
            <dyn fs::Fs>::set_global(params.fs.clone(), cx);
        });
        let fs = params.fs.as_fake();
        fs.insert_tree("/root", json!({ "a.txt": "v1" })).await;
        let project = Project::test(params.fs.clone(), ["/root".as_ref()], cx).await;
        let window = cx.add_window(|window, cx| workspace::MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window.read_with(cx, |mw, _| mw.workspace().clone()).unwrap();
        let cx = &mut VisualTestContext::from_window(window.into(), cx);
        let agent = AgentSpec { id: "a".into(), command: "true".into(), args: vec![], env: vec![], cwd: None };
        let config = crate::config::AgentsConfig { instructions_files: crate::config::default_instructions_files(), agents: vec![agent], review_writes: true, verify_changes: true, mcp_servers: vec![], default_agent: None, permissions: Default::default() };
        let thread = workspace.update_in(cx, |ws, window, cx| cx.new(|cx| Thread::with_config(ws, Some(config), tmp.path().join("history"), window, cx)));
        let message = |text: &str, cx: &mut VisualTestContext| {
            thread.update(cx, |t, _| {
                t.entries.push(Entry::User(text.into(), vec![]));
                t.begin_checkpoint();
                t.entries.len() - 1
            })
        };
        let write = |path: &str, old: Option<&str>, new: &str, cx: &mut VisualTestContext| {
            let record = crate::project_fs::WriteRecord { path: path.into(), old_text: old.map(str::to_string), new_text: new.into(), problems_before: None };
            thread.update(cx, |t, cx| t.record_write(record, cx));
        };
        let a = std::path::PathBuf::from("/root/a.txt");
        let b = std::path::PathBuf::from("/root/b.txt");

        let first = message("one", cx);
        fs.insert_file("/root/a.txt", b"v2".to_vec()).await;
        write("/root/a.txt", Some("v1"), "v2", cx);
        let second = message("two", cx);
        fs.insert_file("/root/a.txt", b"v3".to_vec()).await;
        write("/root/a.txt", Some("v2"), "v3", cx);
        fs.insert_file("/root/b.txt", b"new".to_vec()).await;
        write("/root/b.txt", None, "new", cx);
        assert_eq!(thread.read_with(cx, |t, _| t.later_turns_touching(first, None)), vec![a.clone()], "the second turn changed a.txt again");
        assert!(thread.read_with(cx, |t, _| t.later_turns_touching(second, None)).is_empty());

        // One file of the last turn.
        thread.update(cx, |t, cx| t.undo_turn(second, Some(b.clone()), cx));
        cx.run_until_parked();
        assert!(!fs.is_file(&b).await, "created in the turn: removed");
        assert_eq!(fs.load(&a).await.unwrap(), "v3", "the rest of the turn stays");
        assert_eq!(thread.read_with(cx, |t, _| t.turn_at(second).map(|c| c.undone.iter().cloned().collect::<Vec<_>>())), Some(vec![b.clone()]));

        // The rest of it.
        thread.update(cx, |t, cx| t.undo_turn(second, None, cx));
        cx.run_until_parked();
        assert_eq!(fs.load(&a).await.unwrap(), "v2");
        let changes = thread.read_with(cx, |t, _| t.changes.iter().map(|c| (c.path.clone(), c.original.clone(), c.current.clone())).collect::<Vec<_>>());
        assert_eq!(changes, vec![(a.clone(), Some("v1".into()), "v2".into())], "a.txt still differs from before the agent");
        assert!(thread.read_with(cx, |t, _| t.later_turns_touching(first, None)).is_empty(), "the later change is undone already");

        // The first turn, from the view: nothing later to warn about, so no prompt.
        let view = workspace.update_in(cx, |ws, window, cx| cx.new(|cx| ThreadView::new(ws, thread.clone(), window, cx)));
        view.update_in(cx, |v, window, cx| v.undo_turn(first, None, window, cx));
        cx.run_until_parked();
        assert!(!cx.has_pending_prompt());
        assert_eq!(fs.load(&a).await.unwrap(), "v1");
        assert!(thread.read_with(cx, |t, _| t.changes.is_empty()));
    }

    /// What you send while the agent works waits for the end of the turn, then goes.
    #[gpui::test]
    async fn queues_messages_while_the_agent_works(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let tmp = tempfile::tempdir().unwrap();
        let mut agent = mock_auth_agent(&tmp.path().join("unused"));
        agent.env.retain(|(key, _)| key != "MOCK_REQUIRE_AUTH");
        agent.cwd = Some(tmp.path().to_path_buf());
        let config = crate::config::AgentsConfig { instructions_files: crate::config::default_instructions_files(), agents: vec![agent], review_writes: true, verify_changes: true, mcp_servers: vec![], default_agent: None, permissions: crate::permissions::Permissions { mode: crate::permissions::PermissionMode::Ask, ..Default::default() } };
        let (thread, view, mut cx) = thread_tab(config, tmp.path().join("history"), cx).await;
        let cx = &mut cx;
        // A write that waits for permission keeps the turn going.
        thread.update_in(cx, |t, window, cx| t.ask("write a.txt hi".into(), window, cx));
        wait_for(cx, &thread, "the permission request", |t| t.pending_reviews() == 1).await;
        view.update_in(cx, |v, window, cx| {
            v.input.update(cx, |e, cx| e.set_text("and then this", window, cx));
            v.send(&Send, window, cx);
        });
        assert_eq!(thread.read_with(cx, |t, _| t.queue.len()), 1, "queued");
        assert_eq!(view.read_with(cx, |v, cx| v.input.read(cx).text(cx)), "", "the input is free again");
        draw(&view, cx);

        let request = thread.read_with(cx, |t, _| {
            t.entries.iter().find_map(|e| match e {
                Entry::Permission { request_id, resolved: None, .. } => Some(request_id.clone()),
                _ => None,
            })
        });
        thread.update(cx, |t, cx| t.answer_permission(request.unwrap(), None, cx));
        wait_for(cx, &thread, "the queued message to be sent and answered", |t| {
            t.queue.is_empty() && t.status() == Status::Ready && t.entries.iter().any(|e| matches!(e, Entry::User(text, _) if text == "and then this"))
        })
        .await;
        let answered = thread.read_with(cx, |t, cx| t.entries.iter().any(|e| matches!(e, Entry::Agent(md) if md.read(cx).source().contains("Echo: and then this"))));
        assert!(answered);

        // A pasted image goes with the next message.
        let png = gpui::Image::from_bytes(gpui::ImageFormat::Png, vec![0x89, b'P', b'N', b'G']);
        cx.write_to_clipboard(gpui::ClipboardItem::new_image(&png));
        view.update_in(cx, |v, window, cx| {
            v.paste(&editor::actions::Paste, window, cx);
            v.input.update(cx, |e, cx| e.set_text("what is this", window, cx));
        });
        assert_eq!(view.read_with(cx, |v, _| v.images.len()), 1, "attached, not pasted as text");
        draw(&view, cx);
        view.update_in(cx, |v, window, cx| v.send(&Send, window, cx));
        wait_for(cx, &thread, "the answer about the image", |t| t.status() == Status::Ready && t.entries.iter().any(|e| matches!(e, Entry::User(text, labels) if text == "what is this" && labels.contains(&"image 1".to_string())))).await;
        let seen = thread.read_with(cx, |t, cx| t.entries.iter().any(|e| matches!(e, Entry::Agent(md) if md.read(cx).source().contains("Images: 1 (image/png)"))));
        assert!(seen, "the agent got the image");
        assert!(view.read_with(cx, |v, _| v.images.is_empty()));

        // A question aside: answered in a hidden session, nothing in the thread.
        let entries = thread.read_with(cx, |t, _| t.entries.len());
        let answer = thread.update(cx, |t, cx| t.ask_aside("on the side".into(), cx)).await.unwrap();
        assert_eq!(answer, "Echo: on the side");
        assert_eq!(thread.read_with(cx, |t, _| t.entries.len()), entries, "the thread shows nothing");

        // Edit: the message goes back to the input.
        let ix = thread.read_with(cx, |t, _| t.entries.iter().position(|e| matches!(e, Entry::User(text, _) if text == "and then this")).unwrap());
        thread.update_in(cx, |t, window, cx| t.edit_message(ix, "and then this".into(), 0, window, cx));
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |v, cx| v.input.read(cx).text(cx)), "and then this");
    }

    /// After a turn that wrote files, the thread lists their problems once the language
    /// servers have reported, and tells whether they are new.
    #[gpui::test]
    async fn checks_the_changed_files_after_a_turn(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let tmp = tempfile::tempdir().unwrap();
        let params = cx.update(workspace::AppState::test);
        cx.update(|cx| {
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            gpui_tokio::init(cx);
            editor::init(cx);
            init(cx);
        });
        let fs = params.fs.as_fake();
        fs.insert_tree("/root", json!({ "a.rs": "fn main() {\n    let x: u8 = \"no\";\n}\n" })).await;
        let project = Project::test(params.fs.clone(), ["/root".as_ref()], cx).await;
        let window = cx.add_window(|window, cx| workspace::MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window.read_with(cx, |mw, _| mw.workspace().clone()).unwrap();
        let cx = &mut VisualTestContext::from_window(window.into(), cx);
        let agent = AgentSpec { id: "a".into(), command: "true".into(), args: vec![], env: vec![], cwd: None };
        let config = crate::config::AgentsConfig { instructions_files: crate::config::default_instructions_files(), agents: vec![agent], review_writes: true, verify_changes: true, mcp_servers: vec![], default_agent: None, permissions: Default::default() };
        let thread = workspace.update_in(cx, |ws, window, cx| {
            let thread = cx.new(|cx| Thread::with_config(ws, Some(config), tmp.path().join("history"), window, cx));
            add_thread(ws, thread.clone(), window, cx);
            thread
        });
        let view = workspace.read_with(cx, |ws, cx| ws.items_of_type::<ThreadView>(cx).next()).unwrap();
        // A turn that wrote a.rs, which now has an error.
        thread.update(cx, |t, _| {
            t.entries.push(Entry::User("change a.rs".into(), vec![]));
            t.begin_checkpoint();
            t.checkpoints.last_mut().unwrap().files.insert("/root/a.rs".into(), Some("fn main() {}\n".into()));
        });
        project.read_with(cx, |p, _| p.lsp_store()).update(cx, |store, cx| {
            let diagnostic = language::Diagnostic {
                severity: language::DiagnosticSeverity::ERROR,
                message: language::DiagnosticMessage::from("mismatched types"),
                source_kind: language::DiagnosticSourceKind::Pushed,
                is_primary: true,
                ..Default::default()
            };
            let at = text::Unclipped(text::PointUtf16::new(1, 16));
            store.update_diagnostic_entries(lsp::LanguageServerId(0), "/root/a.rs".into(), None, None, vec![language::DiagnosticEntry::new(at..at, diagnostic)], cx).unwrap();
        });
        thread.update(cx, |t, cx| t.check_changed_files(cx));
        assert!(thread.read_with(cx, |t, _| matches!(t.entries.last(), Some(Entry::Check(None)))), "checking");
        cx.executor().advance_clock(std::time::Duration::from_secs(3));
        cx.run_until_parked();
        let checks = thread.read_with(cx, |t, _| match t.entries.last() {
            Some(Entry::Check(Some(checks))) => checks.clone(),
            _ => panic!("the check should be done"),
        });
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].problems, vec![crate::verify::Problem { line: 1, error: true, message: "mismatched types".into() }]);
        assert!(checks[0].got_worse(), "a new error");
        draw(&view, cx);

        // The server reports again later (a slow build): the check follows.
        project.read_with(cx, |p, _| p.lsp_store()).update(cx, |store, cx| {
            let warning = language::Diagnostic {
                severity: language::DiagnosticSeverity::WARNING,
                message: language::DiagnosticMessage::from("unused variable"),
                source_kind: language::DiagnosticSourceKind::Pushed,
                is_primary: true,
                ..Default::default()
            };
            let at = text::Unclipped(text::PointUtf16::new(1, 8));
            store.update_diagnostic_entries(lsp::LanguageServerId(0), "/root/a.rs".into(), None, None, vec![language::DiagnosticEntry::new(at..at, warning)], cx).unwrap();
        });
        cx.run_until_parked();
        let problems = thread.read_with(cx, |t, _| match t.entries.last() {
            Some(Entry::Check(Some(checks))) => checks[0].problems.clone(),
            _ => vec![],
        });
        assert_eq!(problems, vec![crate::verify::Problem { line: 1, error: false, message: "unused variable".into() }], "the latest report");
    }

    /// `check_file` lists only the problems the agent's changes brought: the ones the file
    /// had before its first write (even on another line now) are only counted.
    #[gpui::test]
    async fn check_file_reports_only_new_problems(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let tmp = tempfile::tempdir().unwrap();
        let params = cx.update(workspace::AppState::test);
        cx.update(|cx| {
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            gpui_tokio::init(cx);
            editor::init(cx);
            init(cx);
        });
        let fs = params.fs.as_fake();
        fs.insert_tree("/root", json!({ "a.rs": "fn main() {\n    let x = 1;\n}\n", "b.rs": "fn b() {}\n" })).await;
        let project = Project::test(params.fs.clone(), ["/root".as_ref()], cx).await;
        let window = cx.add_window(|window, cx| workspace::MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window.read_with(cx, |mw, _| mw.workspace().clone()).unwrap();
        let cx = &mut VisualTestContext::from_window(window.into(), cx);
        let agent = AgentSpec { id: "a".into(), command: "true".into(), args: vec![], env: vec![], cwd: None };
        let config = crate::config::AgentsConfig { instructions_files: crate::config::default_instructions_files(), agents: vec![agent], review_writes: true, verify_changes: true, mcp_servers: vec![], default_agent: None, permissions: Default::default() };
        let thread = workspace.update_in(cx, |ws, window, cx| {
            let thread = cx.new(|cx| Thread::with_config(ws, Some(config), tmp.path().join("history"), window, cx));
            add_thread(ws, thread.clone(), window, cx);
            thread
        });
        let report = |cx: &mut VisualTestContext, problems: Vec<(u32, language::DiagnosticSeverity, &str)>| {
            project.read_with(cx, |p, _| p.lsp_store()).update(cx, |store, cx| {
                let entries = problems
                    .into_iter()
                    .map(|(row, severity, message)| {
                        let diagnostic = language::Diagnostic { severity, message: language::DiagnosticMessage::from(message), source_kind: language::DiagnosticSourceKind::Pushed, is_primary: true, ..Default::default() };
                        let at = text::Unclipped(text::PointUtf16::new(row, 4));
                        language::DiagnosticEntry::new(at..at, diagnostic)
                    })
                    .collect();
                store.update_diagnostic_entries(lsp::LanguageServerId(0), "/root/a.rs".into(), None, None, entries, cx).unwrap();
            });
            cx.run_until_parked();
        };
        // The file is open with a warning it already had.
        let buffer = project.update(cx, |p, cx| p.open_local_buffer("/root/a.rs", cx)).await.unwrap();
        report(cx, vec![(1, language::DiagnosticSeverity::WARNING, "unused variable `x`")]);

        // The agent writes it; the server then reports the old warning two lines down, and a new error.
        thread.update(cx, |t, cx| {
            t.record_write(crate::project_fs::WriteRecord { path: "/root/a.rs".into(), old_text: Some("fn main() {\n    let x = 1;\n}\n".into()), new_text: "fn main() {\n\n\n    let x = 1;\n    y\n}\n".into(), problems_before: None }, cx)
        });
        buffer.update(cx, |b, cx| b.set_text("fn main() {\n\n\n    let x = 1;\n    y\n}\n", cx));
        report(cx, vec![(3, language::DiagnosticSeverity::WARNING, "unused variable `x`"), (4, language::DiagnosticSeverity::ERROR, "cannot find value `y`")]);

        let call = |cx: &mut VisualTestContext, name: &str, args: serde_json::Value| {
            let (reply, answer) = futures::channel::oneshot::channel();
            let request = crate::forge_mcp::ToolRequest { name: name.into(), args, reply };
            thread.update_in(cx, |t, window, cx| t.handle_tool_request(request, window, cx));
            answer
        };
        let answer = call(cx, "check_file", json!({ "path": "a.rs" }));
        cx.executor().advance_clock(std::time::Duration::from_secs(3));
        cx.run_until_parked();
        let text = answer.await.unwrap().unwrap();
        assert_eq!(text, "a.rs: 1 new problem (1 it already had before your changes remain):\n- line 5 error: cannot find value `y`");

        // Without a path: every file the conversation changed (b.rs wasn't).
        let answer = call(cx, "check_file", json!({}));
        cx.executor().advance_clock(std::time::Duration::from_secs(3));
        cx.run_until_parked();
        assert!(answer.await.unwrap().unwrap().starts_with("a.rs: 1 new problem"));

        // `diagnostics` narrowed to the changed files' errors.
        let answer = call(cx, "diagnostics", json!({ "changed": true, "severity": "error" }));
        cx.executor().advance_clock(std::time::Duration::from_secs(3));
        cx.run_until_parked();
        assert_eq!(answer.await.unwrap().unwrap(), "a.rs:5 error: cannot find value `y`\n(1 error, 0 warnings in 1 file)");

        // Fixed: nothing new left.
        report(cx, vec![(3, language::DiagnosticSeverity::WARNING, "unused variable `x`")]);
        let answer = call(cx, "check_file", json!({ "path": "a.rs" }));
        cx.executor().advance_clock(std::time::Duration::from_secs(3));
        cx.run_until_parked();
        assert_eq!(answer.await.unwrap().unwrap(), "a.rs: no new problems (1 it already had before your changes remain).");
    }

    /// Two threads changing the same file: both say so, once.
    #[gpui::test]
    async fn warns_when_two_threads_change_a_file(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let tmp = tempfile::tempdir().unwrap();
        let params = cx.update(workspace::AppState::test);
        cx.update(|cx| {
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            gpui_tokio::init(cx);
            editor::init(cx);
            init(cx);
        });
        params.fs.as_fake().insert_tree("/root", json!({ "a.txt": "one\n" })).await;
        let project = Project::test(params.fs.clone(), ["/root".as_ref()], cx).await;
        let window = cx.add_window(|window, cx| workspace::MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window.read_with(cx, |mw, _| mw.workspace().clone()).unwrap();
        let cx = &mut VisualTestContext::from_window(window.into(), cx);
        let agent = AgentSpec { id: "a".into(), command: "true".into(), args: vec![], env: vec![], cwd: None };
        let config = crate::config::AgentsConfig { instructions_files: crate::config::default_instructions_files(), agents: vec![agent], review_writes: true, verify_changes: false, mcp_servers: vec![], default_agent: None, permissions: Default::default() };
        let new = |cx: &mut VisualTestContext| {
            let config = config.clone();
            let history = tmp.path().join("history");
            workspace.update_in(cx, |ws, window, cx| cx.new(|cx| Thread::with_config(ws, Some(config), history, window, cx)))
        };
        let (first, second) = (new(cx), new(cx));
        let write = |thread: &Entity<Thread>, old: &str, new: &str, cx: &mut VisualTestContext| {
            let record = crate::project_fs::WriteRecord { path: "/root/a.txt".into(), old_text: Some(old.into()), new_text: new.into(), problems_before: None };
            thread.update(cx, |t, cx| t.record_write(record, cx));
            cx.run_until_parked();
        };
        let warnings = |thread: &Entity<Thread>, cx: &mut VisualTestContext| {
            thread.read_with(cx, |t, _| t.entries.iter().filter(|e| matches!(e, Entry::System(text, _) if text.starts_with("⚠"))).count())
        };
        write(&first, "one\n", "two\n", cx);
        assert_eq!(warnings(&first, cx), 0);
        write(&second, "two\n", "three\n", cx);
        assert_eq!((warnings(&first, cx), warnings(&second, cx)), (1, 1), "both threads say so");
        write(&second, "three\n", "four\n", cx);
        assert_eq!((warnings(&first, cx), warnings(&second, cx)), (1, 1), "once");
    }

    /// New threads start with the default agent; the New menu can pick another.
    #[gpui::test]
    async fn new_threads_use_the_default_agent_unless_told_otherwise(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let tmp = tempfile::tempdir().unwrap();
        let agent = |id: &str| AgentSpec { id: id.into(), command: "true".into(), args: vec![], env: vec![], cwd: None };
        let config = crate::config::AgentsConfig { instructions_files: crate::config::default_instructions_files(), agents: vec![agent("a"), agent("b")], review_writes: true, verify_changes: true, mcp_servers: vec![], default_agent: Some("b".into()), permissions: Default::default() };
        let (_, view, mut cx) = thread_tab(config, tmp.path().join("history"), cx).await;
        let cx = &mut cx;
        let workspace = view.read_with(cx, |v, _| v.workspace.upgrade().unwrap());
        let default = workspace.update_in(cx, |ws, window, cx| new_thread(ws, None, window, cx));
        let picked = workspace.update_in(cx, |ws, window, cx| new_thread(ws, Some(0), window, cx));
        assert_eq!(default.read_with(cx, |t, _| t.agent_label()), "b");
        assert_eq!(picked.read_with(cx, |t, _| t.agent_label()), "a");
        let tabs = workspace.read_with(cx, |ws, cx| ws.items_of_type::<ThreadView>(cx).count());
        assert_eq!(tabs, 3, "one tab per thread");
    }

    /// `cmd-shift-a`: the picker lists a new thread and the open ones, filters as you type
    /// and switches to the chosen thread's tab.
    #[gpui::test]
    async fn the_thread_picker_finds_and_switches_threads(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let tmp = tempfile::tempdir().unwrap();
        let agent = |id: &str| AgentSpec { id: id.into(), command: "true".into(), args: vec![], env: vec![], cwd: None };
        let config = crate::config::AgentsConfig { instructions_files: crate::config::default_instructions_files(), agents: vec![agent("a")], review_writes: true, verify_changes: true, mcp_servers: vec![], default_agent: None, permissions: Default::default() };
        let (first, view, mut cx) = thread_tab(config, tmp.path().join("history"), cx).await;
        let cx = &mut cx;
        let workspace = view.read_with(cx, |v, _| v.workspace.upgrade().unwrap());
        first.update(cx, |t, _| t.entries.push(Entry::User("fix the login bug".into(), vec![])));
        let second = workspace.update_in(cx, |ws, window, cx| new_thread(ws, None, window, cx));
        second.update(cx, |t, _| t.entries.push(Entry::User("write release notes".into(), vec![])));

        workspace.update_in(cx, |ws, window, cx| crate::thread_picker::toggle(ws, window, cx));
        cx.run_until_parked();
        let picker = workspace.read_with(cx, |ws, cx| ws.active_modal::<crate::thread_picker::ThreadPicker>(cx)).expect("picker open");
        let labels = picker.read_with(cx, |p, cx| p.delegate_labels(cx));
        assert_eq!(labels, ["New thread with a", "write release notes", "fix the login bug"], "newest first");

        picker.update_in(cx, |p, window, cx| p.set_query("login", window, cx));
        cx.run_until_parked();
        assert_eq!(picker.read_with(cx, |p, cx| p.delegate_labels(cx)), ["fix the login bug"]);
        let inner = picker.read_with(cx, |p, _| p.picker());
        inner.update_in(cx, |p, window, cx| picker::PickerDelegate::confirm(&mut p.delegate, false, window, cx));
        cx.run_until_parked();

        assert!(workspace.read_with(cx, |ws, cx| ws.active_modal::<crate::thread_picker::ThreadPicker>(cx)).is_none(), "picker closed");
        let active = workspace.read_with(cx, |ws, cx| ws.active_item(cx).and_then(|i| i.downcast::<ThreadView>()).map(|v| v.read(cx).thread.clone())).unwrap();
        assert_eq!(active, first);
    }
}
