//! How a thread's entries look: messages, thoughts, tool calls, plans, permission
//! requests, writes to review and sign-in cards. Shared by every view of a thread; the
//! buttons act on the thread itself.

use std::path::PathBuf;

use gpui::TaskExt as _;
use gpui::prelude::FluentBuilder as _;
use gpui::{AnyElement, App, FontWeight, InteractiveElement as _, StatefulInteractiveElement as _, IntoElement, ParentElement as _, SharedString, Styled as _, WeakEntity, Window, div, px};
use markdown::{MarkdownElement, MarkdownFont, MarkdownStyle};
use theme::ActiveTheme as _;
use ui::{Button, ButtonCommon as _, ButtonSize, ButtonStyle, Clickable as _, Color, Disableable as _, Icon, IconButton, IconName, IconSize, Label, LabelCommon as _, LabelSize, Tooltip, h_flex, v_flex};

use crate::diff::DiffView;
use crate::thread::{AuthState, Entry, Thread};

/// Runs `f` on the thread behind a click.
fn on_thread(thread: &WeakEntity<Thread>, f: impl Fn(&mut Thread, &mut Window, &mut gpui::Context<Thread>) + 'static) -> impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static {
    let thread = thread.clone();
    move |_, window, cx| {
        thread.update(cx, |thread, cx| f(thread, window, cx)).ok();
    }
}

/// One entry of a thread, laid out as part of a full-width document.
/// Text that takes the rest of a row and wraps there instead of pushing the row wider.
fn wrapping(text: impl IntoElement) -> gpui::Div {
    div().flex_1().min_w_0().child(text)
}

pub(crate) fn render_entry(thread: &Thread, handle: &WeakEntity<Thread>, ix: usize, entry: &Entry, window: &Window, cx: &App) -> AnyElement {
    let colors = cx.theme().colors().clone();
    match entry {
        // Your message stands apart from the agent's work, so turns are easy to find when
        // scrolling back.
        Entry::User(text, context) => v_flex()
            .gap_1p5()
            .px_3()
            .py_2()
            .rounded_md()
            .border_l_2()
            .border_color(colors.text_accent)
            .bg(colors.element_background)
            .child(
                h_flex()
                    .justify_between()
                    .child(h_flex().gap_1p5().child(Icon::new(IconName::Person).size(IconSize::XSmall).color(Color::Accent)).child(Label::new("You").size(LabelSize::Small).color(Color::Accent)))
                    .child(div().flex_1())
                    .child({
                        let text = text.clone();
                        let files = thread.checkpoint_at(ix).map(|c| c.files.len()).unwrap_or(0);
                        IconButton::new(("edit-message", ix), IconName::Pencil)
                            .icon_size(IconSize::XSmall)
                            .icon_color(Color::Muted)
                            .tooltip(Tooltip::text("Edit and send again"))
                            .on_click(on_thread(handle, move |thread, window, cx| thread.edit_message(ix, text.clone(), files, window, cx)))
                    })
                    .children(thread.checkpoint_at(ix).map(|checkpoint| {
                        let n = checkpoint.files.len();
                        let detail = format!(
                            "The {n} file{} the agent changed after this message go back to how {} then. Changes you made to {} since are lost too. The conversation stays as it is.",
                            if n == 1 { "" } else { "s" },
                            if n == 1 { "it was" } else { "they were" },
                            if n == 1 { "it" } else { "them" },
                        );
                        Button::new(("restore-checkpoint", ix), "Restore checkpoint")
                            .style(ButtonStyle::Subtle)
                            .size(ButtonSize::Compact)
                            .label_size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .tooltip(Tooltip::text(format!("Put the {n} file{} the agent changed after this message back as they were", if n == 1 { "" } else { "s" })))
                            .on_click(on_thread(handle, move |_, window, cx| {
                                let answer = window.prompt(gpui::PromptLevel::Warning, "Restore this checkpoint?", Some(&detail), &["Restore", "Cancel"], cx);
                                cx.spawn_in(window, async move |this, cx| {
                                    if answer.await == Ok(0) {
                                        this.update(cx, |t, cx| t.restore_checkpoint(ix, cx)).ok();
                                    }
                                })
                                .detach();
                            }))
                    })),
            )
            .child(wrapping(Label::new(text.clone())))
            .when(!context.is_empty(), |el| {
                el.child(h_flex().gap_2().flex_wrap().children(context.iter().map(|c| {
                    h_flex().gap_0p5().child(Icon::new(IconName::File).size(IconSize::XSmall).color(Color::Muted)).child(Label::new(c.clone()).size(LabelSize::Small).color(Color::Muted))
                })))
            })
            .into_any_element(),
        // The turn's header names the agent; its answer reads as plain text.
        Entry::Agent(md) => div().child(MarkdownElement::new(md.clone(), MarkdownStyle::themed(MarkdownFont::Agent, window, cx))).into_any_element(),
        Entry::Thought(md) => v_flex()
            .px_2()
            .py_1()
            .border_l_2()
            .border_color(colors.border)
            .child(h_flex().gap_1().child(Icon::new(IconName::ToolThink).size(IconSize::Small).color(Color::Muted)).child(Label::new("Thinking").size(LabelSize::Small).color(Color::Muted)))
            .child(div().opacity(0.7).child(MarkdownElement::new(md.clone(), MarkdownStyle::themed(MarkdownFont::Agent, window, cx))))
            .into_any_element(),
        Entry::Tool { id, title, kind, status, detail, terminal, diffs, command } => {
            let terminal_view = terminal.as_ref().and_then(|t| thread.terminal_view(t));
            // The permission request about this tool call, shown inside its card.
            let permission = thread.entries.iter().find_map(|e| match e {
                Entry::Permission { request_id, tool_call_id: Some(tid), options, resolved, diffs, .. } if tid == id => Some((request_id, options, resolved, diffs)),
                _ => None,
            });
            let waiting = permission.is_some_and(|(_, _, resolved, _)| resolved.is_none());
            let permission_diffs: Vec<&DiffView> = match permission {
                Some((_, _, _, pdiffs)) if diffs.is_empty() => pdiffs.iter().collect(),
                _ => Vec::new(),
            };
            let (icon, color) = match status.as_str() {
                "completed" => (IconName::Check, Color::Success),
                "failed" => (IconName::XCircle, Color::Error),
                "in_progress" => (IconName::ArrowCircle, Color::Accent),
                _ => (IconName::Circle, Color::Muted),
            };
            v_flex()
                .px_2()
                .py_1()
                .rounded_md()
                .border_1()
                .border_color(if waiting { cx.theme().status().warning_border } else { colors.border })
                .bg(colors.surface_background)
                .child(
                    h_flex()
                        .gap_2()
                        .items_start()
                        .child(if waiting { Icon::new(IconName::Warning).size(IconSize::Small).color(Color::Warning) } else { Icon::new(icon).size(IconSize::Small).color(color) })
                        .child(wrapping(Label::new(title.clone()).size(LabelSize::Small)))
                        .when_some(command.clone(), |el, c| el.child(div().flex_none().max_w(px(320.)).child(Label::new(c).size(LabelSize::XSmall).color(Color::Muted).buffer_font(cx).truncate())))
                        .when_some((!kind.is_empty()).then(|| kind.clone()), |el, k| el.child(div().flex_none().child(Label::new(k).size(LabelSize::XSmall).color(Color::Muted)))),
                )
                .when_some(detail.clone().filter(|_| terminal_view.is_none()), |el, d| el.child(Label::new(d).size(LabelSize::XSmall).color(Color::Muted).buffer_font(cx)))
                .when_some(terminal_view, |el, view| el.child(div().mt_1().rounded_sm().overflow_hidden().child(view)))
                .children(diffs.iter().chain(permission_diffs).map(|d| render_diff(thread, handle, d, cx)))
                .when_some(permission, |el, (request_id, options, resolved, _)| el.child(render_permission_answer(handle, request_id, options, resolved.as_deref())))
                .into_any_element()
        }
        Entry::Plan(items) => v_flex()
            .gap_0p5()
            .px_2()
            .py_1()
            .rounded_md()
            .bg(colors.surface_background)
            .child(Label::new("Plan").size(LabelSize::Small).weight(FontWeight::BOLD))
            .children(items.iter().map(|(text, status)| {
                let (icon, color) = match status.as_str() {
                    "completed" => (IconName::Check, Color::Success),
                    "in_progress" => (IconName::ArrowRight, Color::Accent),
                    _ => (IconName::Circle, Color::Muted),
                };
                h_flex().gap_2().items_start().child(Icon::new(icon).size(IconSize::XSmall).color(color)).child(wrapping(Label::new(text.clone()).size(LabelSize::Small).color(if status == "completed" { Color::Muted } else { Color::Default })))
            }))
            .into_any_element(),
        Entry::Permission { request_id, tool_call_id, title, options, resolved, diffs } => {
            // Shown in its tool call's card when that card is in the thread.
            let in_card = tool_call_id.as_ref().is_some_and(|tid| thread.entries.iter().any(|e| matches!(e, Entry::Tool { id, .. } if id == tid)));
            if in_card {
                return div().into_any_element();
            }
            v_flex()
                .gap_1()
                .px_2()
                .py_1p5()
                .rounded_md()
                .border_1()
                .border_color(if resolved.is_none() { cx.theme().status().warning_border } else { colors.border })
                .bg(colors.surface_background)
                .child(
                    h_flex()
                        .gap_2()
                        .items_start()
                        .child(Icon::new(if resolved.is_none() { IconName::Warning } else { IconName::Check }).size(IconSize::Small).color(if resolved.is_none() { Color::Warning } else { Color::Muted }))
                        .child(wrapping(Label::new(title.clone()).size(LabelSize::Small))),
                )
                .children(diffs.iter().map(|d| render_diff(thread, handle, d, cx)))
                .child(render_permission_answer(handle, request_id, options, resolved.as_deref()))
                .into_any_element()
        }
        Entry::Review { diff, hunks, reply, outcome } => {
            let selected = hunks.iter().filter(|(_, on)| *on).count();
            let pending = reply.is_some();
            let status = match outcome {
                Some(label) => Label::new(format!("→ {label}"))
                    .size(LabelSize::Small)
                    .color(if *label == "Rejected" { Color::Error } else { Color::Success })
                    .into_any_element(),
                None => h_flex()
                    .gap_1()
                    .child(
                        Button::new(("review-accept", ix), if selected == hunks.len() { "Accept".to_string() } else { format!("Accept selected ({selected}/{})", hunks.len()) })
                            .style(ButtonStyle::Filled)
                            .disabled(selected == 0)
                            .start_icon(Icon::new(IconName::Check).size(IconSize::Small))
                            .on_click(on_thread(handle, move |t, _, cx| t.answer_review(ix, true, cx))),
                    )
                    .child(Button::new(("review-reject", ix), "Reject").start_icon(Icon::new(IconName::Close).size(IconSize::Small)).on_click(on_thread(handle, move |t, _, cx| t.answer_review(ix, false, cx))))
                    .into_any_element(),
            };
            let hunk_list = (pending && hunks.len() > 1).then(|| {
                v_flex().gap_0p5().children(hunks.iter().enumerate().map(|(hi, (hunk, on))| {
                    ui::Checkbox::new(("review-hunk", ix * 1000 + hi), ui::ToggleState::from(*on))
                        .label(format!("{}  +{} −{}", hunk.label(), hunk.new.len(), hunk.old.len()))
                        .on_click({
                            let handle = handle.clone();
                            move |_, _, cx| {
                                handle.update(cx, |t, cx| t.toggle_hunk(ix, hi, cx)).ok();
                            }
                        })
                }))
            });
            let diff_el = render_diff(thread, handle, diff, cx);
            v_flex()
                .gap_1()
                .p_2()
                .rounded_md()
                .border_1()
                .border_color(cx.theme().status().info_border)
                .bg(cx.theme().status().info_background)
                .child(h_flex().gap_2().child(Icon::new(IconName::Pencil).size(IconSize::Small).color(Color::Info)).child(Label::new("The agent wants to edit a file").size(LabelSize::Small)))
                .child(diff_el)
                .when_some(hunk_list, |el, list| el.child(list))
                .child(status)
                .into_any_element()
        }
        Entry::Commit { message, files, origin, state, .. } => render_commit(thread, handle, ix, message, files, *origin, state, cx),
        Entry::Push { plan, origin, state, .. } => render_push(handle, ix, plan, *origin, state, cx),
        Entry::LspEdit { plan, state, .. } => render_edit(thread, handle, ix, plan, state, cx),
        Entry::ExtensionTool { tool, args, state, .. } => render_extension_tool(handle, ix, tool, args, state, cx),
        Entry::Remember { note, file, state, .. } => render_remember(thread, handle, ix, note, file, state, cx),
        Entry::Question { question, options, input, answer, .. } => render_question(handle, ix, question, options, input, answer.as_deref(), cx),
        Entry::System(text, color) => h_flex()
            .gap_1p5()
            .items_start()
            .child(Icon::new(match color {
                Color::Error => IconName::XCircle,
                Color::Warning => IconName::Warning,
                Color::Success => IconName::Check,
                _ => IconName::Info,
            }).size(IconSize::XSmall).color(*color))
            .child(wrapping(Label::new(text.clone()).size(LabelSize::Small).color(*color)))
            .into_any_element(),
        Entry::Check(None) => h_flex()
            .gap_1()
            .child(Icon::new(IconName::ArrowCircle).size(IconSize::XSmall).color(Color::Muted))
            .child(Label::new("Checking the changed files…").size(LabelSize::Small).color(Color::Muted))
            .into_any_element(),
        Entry::Check(Some(checks)) => render_check(thread, handle, ix, checks, &colors),
        Entry::Auth { methods, terminal, state } => {
            let name = thread.agent_label();
            let buttons = h_flex().gap_1().flex_wrap().children(methods.iter().enumerate().map(|(mi, m)| {
                let mut b = Button::new(("auth-method", ix * 100 + mi), m.name.clone())
                    .style(if mi == 0 { ButtonStyle::Filled } else { ButtonStyle::Subtle })
                    .start_icon(Icon::new(IconName::Person).size(IconSize::Small))
                    .on_click(on_thread(handle, move |t, window, cx| t.run_auth(ix, mi, window, cx)));
                if let Some(d) = &m.description {
                    b = b.tooltip(Tooltip::text(d.clone()));
                }
                b
            }));
            let body = match state {
                AuthState::Waiting => v_flex().gap_1().child(Label::new("Choose how to sign in. The login runs here, in an embedded terminal.").size(LabelSize::Small).color(Color::Muted)).child(buttons).into_any_element(),
                AuthState::Running(m) => Label::new(format!("{m}: follow the steps below (a browser window may open).")).size(LabelSize::Small).color(Color::Accent).into_any_element(),
                AuthState::Done => Label::new("✓ Signed in").size(LabelSize::Small).color(Color::Success).into_any_element(),
                AuthState::Failed(err) => v_flex().gap_1().child(Label::new(format!("Sign-in failed: {err}")).size(LabelSize::Small).color(Color::Error)).child(buttons).into_any_element(),
            };
            v_flex()
                .gap_1()
                .p_2()
                .rounded_md()
                .border_1()
                .border_color(cx.theme().status().warning_border)
                .bg(cx.theme().status().warning_background)
                .child(h_flex().gap_2().items_start().child(Icon::new(IconName::Lock).size(IconSize::Small).color(Color::Warning)).child(wrapping(Label::new(format!("Sign in to {name}")).weight(FontWeight::BOLD))))
                .child(body)
                .when_some(terminal.clone().filter(|_| *state != AuthState::Done), |el, view| el.child(div().mt_1().rounded_sm().overflow_hidden().child(view)))
                .into_any_element()
        }
    }
}

/// The question's buttons, or how it was answered. Allowing once is the main choice;
/// allowing for the session and rejecting sit next to it, each with its icon.
pub(crate) fn render_permission_answer(handle: &WeakEntity<Thread>, request_id: &str, options: &[crate::thread::PermissionOption], resolved: Option<&str>) -> AnyElement {
    if let Some(answer) = resolved {
        let allowed = !answer.starts_with("Cancel") && !answer.to_lowercase().starts_with("no") && !answer.to_lowercase().contains("reject");
        return h_flex()
            .pt_1()
            .gap_1()
            .child(Icon::new(if allowed { IconName::Check } else { IconName::Close }).size(IconSize::XSmall).color(if allowed { Color::Success } else { Color::Muted }))
            .child(wrapping(Label::new(answer.to_string()).size(LabelSize::XSmall).color(Color::Muted)))
            .into_any_element();
    }
    let mut row = h_flex().pt_1().gap_1().flex_wrap();
    for o in options {
        let (rid, oid, name) = (request_id.to_string(), o.id.clone(), o.name.clone());
        let (style, icon) = match o.kind.as_str() {
            crate::thread::COMMIT_IN_FORGE => (ButtonStyle::Outlined, IconName::GitCommit),
            crate::thread::PUSH_IN_FORGE => (ButtonStyle::Outlined, IconName::ArrowUp),
            "allow_once" => (ButtonStyle::Filled, IconName::Check),
            "allow_always" => (ButtonStyle::Outlined, IconName::CheckDouble),
            k if k.starts_with("reject") => (ButtonStyle::Subtle, IconName::Close),
            _ if o.allow => (ButtonStyle::Outlined, IconName::Check),
            _ => (ButtonStyle::Subtle, IconName::Close),
        };
        row = row.child(
            Button::new(SharedString::from(format!("perm-{request_id}-{}", o.id)), o.name.clone())
                .style(style)
                .size(ButtonSize::Medium)
                .start_icon(Icon::new(icon).size(IconSize::XSmall))
                .on_click(on_thread(handle, move |t, _, cx| t.answer_permission(rid.clone(), Some((oid.clone(), name.clone())), cx))),
        );
    }
    if options.is_empty() {
        let rid = request_id.to_string();
        row = row.child(Button::new(SharedString::from(format!("perm-dismiss-{request_id}")), "Dismiss").on_click(on_thread(handle, move |t, _, cx| t.answer_permission(rid.clone(), None, cx))));
    }
    row.into_any_element()
}

/// A commit the agent proposes: its message and files, and how the user makes it.
#[allow(clippy::too_many_arguments)]
fn render_commit(thread: &Thread, handle: &WeakEntity<Thread>, ix: usize, message: &str, files: &[PathBuf], origin: crate::commit_proposal::Origin, state: &crate::commit_proposal::CommitState, cx: &App) -> AnyElement {
    use crate::commit_proposal::{CommitState, Origin, short};
    let colors = cx.theme().colors().clone();
    let root = thread.root().clone();
    let open = state.is_open();
    let headline = match origin {
        Origin::Tool => "The agent proposes a commit",
        Origin::Command => "The agent tried to run git commit; make it from here",
    };
    let file_rows = files.iter().map(|f| {
        let shown = f.strip_prefix(&root).unwrap_or(f).to_string_lossy().into_owned();
        h_flex().gap_1().child(Icon::new(IconName::File).size(IconSize::XSmall).color(Color::Muted)).child(wrapping(Label::new(shown).size(LabelSize::Small).buffer_font(cx)))
    });
    let footer = match state {
        CommitState::Committed { sha, .. } => h_flex()
            .gap_1()
            .child(Icon::new(IconName::Check).size(IconSize::XSmall).color(Color::Success))
            .child(Label::new(format!("Committed as {}", short(sha))).size(LabelSize::Small).color(Color::Success))
            .into_any_element(),
        CommitState::Declined => Label::new("→ Declined").size(LabelSize::Small).color(Color::Muted).into_any_element(),
        CommitState::Committing => Label::new("Committing…").size(LabelSize::Small).color(Color::Muted).into_any_element(),
        CommitState::InPanel { .. } => h_flex()
            .gap_1()
            .flex_wrap()
            .child(wrapping(Label::new("Staged and waiting in the Git panel: commit there.").size(LabelSize::Small).color(Color::Accent)))
            .child(Button::new(("commit-decline", ix), "Decline").start_icon(Icon::new(IconName::Close).size(IconSize::Small)).on_click(on_thread(handle, move |t, _, cx| t.decline_commit_proposal(ix, cx))))
            .into_any_element(),
        CommitState::Waiting | CommitState::Failed(_) => h_flex()
            .gap_1()
            .flex_wrap()
            .child(
                Button::new(("commit-now", ix), "Commit")
                    .style(ButtonStyle::Filled)
                    .disabled(message.trim().is_empty())
                    .start_icon(Icon::new(IconName::GitCommit).size(IconSize::Small))
                    .on_click(on_thread(handle, move |t, _, cx| t.commit_proposal(ix, cx))),
            )
            // The Git panel shows the project's repository, not a thread's worktree.
            .when(!thread.in_worktree(), |el| {
                el.child(
                    Button::new(("commit-panel", ix), "Edit in Git panel")
                        .style(ButtonStyle::Outlined)
                        .start_icon(Icon::new(IconName::Pencil).size(IconSize::Small))
                        .on_click(on_thread(handle, move |t, window, cx| t.commit_proposal_in_panel(ix, window, cx))),
                )
            })
            .child(Button::new(("commit-decline", ix), "Decline").start_icon(Icon::new(IconName::Close).size(IconSize::Small)).on_click(on_thread(handle, move |t, _, cx| t.decline_commit_proposal(ix, cx))))
            .into_any_element(),
    };
    let shown_message = match state {
        CommitState::Committed { message, .. } => message.clone(),
        _ if message.trim().is_empty() => "(no message: write one in the Git panel)".to_string(),
        _ => message.to_string(),
    };
    v_flex()
        .gap_1()
        .p_2()
        .rounded_md()
        .border_1()
        .border_color(if open { cx.theme().status().info_border } else { colors.border })
        .when(open, |el| el.bg(cx.theme().status().info_background))
        .child(h_flex().gap_2().child(Icon::new(IconName::GitCommit).size(IconSize::Small).color(if open { Color::Info } else { Color::Muted })).child(wrapping(Label::new(headline).size(LabelSize::Small))))
        .child(div().px_2().py_1().rounded_sm().border_1().border_color(colors.border).bg(colors.editor_background).child(Label::new(shown_message).size(LabelSize::Small).buffer_font(cx)))
        .child(v_flex().gap_0p5().children(file_rows))
        .when_some(if let CommitState::Failed(e) = state { Some(e.clone()) } else { None }, |el, e| {
            el.child(h_flex().gap_1().items_start().child(Icon::new(IconName::XCircle).size(IconSize::XSmall).color(Color::Error)).child(wrapping(Label::new(format!("The commit failed: {e}")).size(LabelSize::Small).color(Color::Error))))
        })
        .child(footer)
        .into_any_element()
}

/// A question from the agent: its options as buttons, a box for another answer, and Skip.
fn render_question(handle: &WeakEntity<Thread>, ix: usize, question: &str, options: &[String], input: &gpui::Entity<editor::Editor>, answer: Option<&str>, cx: &App) -> AnyElement {
    let colors = cx.theme().colors().clone();
    let open = answer.is_none();
    let body = match answer {
        Some("") => Label::new("→ Skipped: the agent decides").size(LabelSize::Small).color(Color::Muted).into_any_element(),
        Some(answer) => h_flex().gap_1().child(Icon::new(IconName::Check).size(IconSize::XSmall).color(Color::Success)).child(wrapping(Label::new(answer.to_string()).size(LabelSize::Small))).into_any_element(),
        None => v_flex()
            .gap_1()
            .when(!options.is_empty(), |el| {
                el.child(h_flex().gap_1().flex_wrap().children(options.iter().enumerate().map(|(oi, option)| {
                    let option = option.clone();
                    Button::new(("question-option", ix * 10 + oi), option.clone())
                        .style(if oi == 0 { ButtonStyle::Filled } else { ButtonStyle::Outlined })
                        .on_click(on_thread(handle, move |t, _, cx| t.answer_question(ix, Some(option.clone()), cx)))
                })))
            })
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        div()
                            .id(("question-input", ix))
                            .flex_1()
                            .min_w_0()
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .border_1()
                            .border_color(colors.border)
                            .bg(colors.editor_background)
                            // Enter sends what was written.
                            .on_action({
                                let handle = handle.clone();
                                move |_: &menu::Confirm, _, cx| {
                                    handle.update(cx, |t, cx| t.answer_question(ix, None, cx)).ok();
                                }
                            })
                            .child(input.clone()),
                    )
                    .child(Button::new(("question-send", ix), "Answer").on_click(on_thread(handle, move |t, _, cx| t.answer_question(ix, None, cx))))
                    .child(Button::new(("question-skip", ix), "Skip").style(ButtonStyle::Subtle).tooltip(Tooltip::text("Let the agent decide")).on_click(on_thread(handle, move |t, _, cx| t.answer_question(ix, Some(String::new()), cx)))),
            )
            .into_any_element(),
    };
    v_flex()
        .gap_1p5()
        .p_2()
        .rounded_md()
        .border_1()
        .border_color(if open { cx.theme().status().warning_border } else { colors.border })
        .when(open, |el| el.bg(cx.theme().status().warning_background))
        .child(h_flex().gap_2().items_start().child(Icon::new(IconName::Info).size(IconSize::Small).color(if open { Color::Warning } else { Color::Muted })).child(wrapping(Label::new(question.to_string()).size(LabelSize::Small).weight(FontWeight::MEDIUM))))
        .child(body)
        .into_any_element()
}

/// A note the agent wants kept in the project's instructions: editable until answered.
fn render_remember(thread: &Thread, handle: &WeakEntity<Thread>, ix: usize, note: &gpui::Entity<editor::Editor>, file: &std::path::Path, state: &crate::forge_tools::EditState, cx: &App) -> AnyElement {
    use crate::forge_tools::EditState;
    let colors = cx.theme().colors().clone();
    let open = state.is_open();
    let shown = file.strip_prefix(thread.root()).unwrap_or(file).to_string_lossy().into_owned();
    let footer = match state {
        EditState::Applied => h_flex().gap_1().child(Icon::new(IconName::Check).size(IconSize::XSmall).color(Color::Success)).child(Label::new(format!("Kept in {shown}")).size(LabelSize::Small).color(Color::Success)).into_any_element(),
        EditState::Declined => Label::new("→ Not kept").size(LabelSize::Small).color(Color::Muted).into_any_element(),
        EditState::Applying => Label::new("Saving…").size(LabelSize::Small).color(Color::Muted).into_any_element(),
        EditState::Waiting | EditState::Failed(_) => h_flex()
            .gap_1()
            .child(Button::new(("remember-keep", ix), "Remember").style(ButtonStyle::Filled).start_icon(Icon::new(IconName::Check).size(IconSize::Small)).on_click(on_thread(handle, move |t, _, cx| t.answer_remember(ix, true, cx))))
            .child(Button::new(("remember-skip", ix), "Don't").start_icon(Icon::new(IconName::Close).size(IconSize::Small)).on_click(on_thread(handle, move |t, _, cx| t.answer_remember(ix, false, cx))))
            .into_any_element(),
    };
    v_flex()
        .gap_1()
        .p_2()
        .rounded_md()
        .border_1()
        .border_color(if open { cx.theme().status().info_border } else { colors.border })
        .when(open, |el| el.bg(cx.theme().status().info_background))
        .child(
            h_flex()
                .gap_1()
                .child(Icon::new(IconName::Book).size(IconSize::Small).color(if open { Color::Info } else { Color::Muted }))
                .child(Label::new("The agent wants to remember this for the project, in").size(LabelSize::Small))
                .child(Label::new(shown).size(LabelSize::Small).buffer_font(cx)),
        )
        .child(div().px_2().py_1().rounded_sm().border_1().border_color(colors.border).bg(colors.editor_background).child(note.clone()))
        .when(open, |el| el.child(Label::new("Edit it before keeping it, if you like. Agents read it at the start of every new session.").size(LabelSize::XSmall).color(Color::Muted)))
        .when_some(if let EditState::Failed(e) = state { Some(e.clone()) } else { None }, |el, e| {
            el.child(h_flex().gap_1().items_start().child(Icon::new(IconName::XCircle).size(IconSize::XSmall).color(Color::Error)).child(wrapping(Label::new(e).size(LabelSize::Small).color(Color::Error))))
        })
        .child(footer)
        .into_any_element()
}

/// An agent's call to an extension's tool, waiting for the user: which extension, which
/// tool, and with what.
fn render_extension_tool(handle: &WeakEntity<Thread>, ix: usize, tool: &forge_ui::agent_tools::AgentTool, args: &serde_json::Value, state: &crate::forge_tools::EditState, cx: &App) -> AnyElement {
    use crate::forge_tools::EditState;
    let colors = cx.theme().colors().clone();
    let open = state.is_open();
    // Each argument on a line: `name: value`, long values cut.
    let shown = |v: &serde_json::Value| {
        let text = v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string());
        if text.chars().count() > 300 { format!("{}…", text.chars().take(300).collect::<String>()) } else { text }
    };
    let rows: Vec<(String, String)> = match args {
        serde_json::Value::Object(map) => map.iter().map(|(k, v)| (k.clone(), shown(v))).collect(),
        serde_json::Value::Null => vec![],
        other => vec![(String::new(), shown(other))],
    };
    let footer = match state {
        EditState::Applied => h_flex().gap_1().child(Icon::new(IconName::Check).size(IconSize::XSmall).color(Color::Success)).child(Label::new("Ran").size(LabelSize::Small).color(Color::Success)).into_any_element(),
        EditState::Declined => Label::new("→ Not allowed").size(LabelSize::Small).color(Color::Muted).into_any_element(),
        EditState::Applying => Label::new("Running…").size(LabelSize::Small).color(Color::Muted).into_any_element(),
        EditState::Waiting => h_flex()
            .gap_1()
            .child(Button::new(("ext-tool-allow", ix), "Allow").style(ButtonStyle::Filled).start_icon(Icon::new(IconName::Check).size(IconSize::Small)).on_click(on_thread(handle, move |t, window, cx| t.answer_extension_tool(ix, true, window, cx))))
            .child(Button::new(("ext-tool-deny", ix), "Deny").start_icon(Icon::new(IconName::Close).size(IconSize::Small)).on_click(on_thread(handle, move |t, window, cx| t.answer_extension_tool(ix, false, window, cx))))
            .into_any_element(),
        EditState::Failed(e) => h_flex().gap_1().items_start().child(Icon::new(IconName::XCircle).size(IconSize::XSmall).color(Color::Error)).child(wrapping(Label::new(e.clone()).size(LabelSize::Small).color(Color::Error))).into_any_element(),
    };
    v_flex()
        .gap_1()
        .p_2()
        .rounded_md()
        .border_1()
        .border_color(if open { cx.theme().status().warning_border } else { colors.border })
        .when(open, |el| el.bg(cx.theme().status().warning_background))
        .child(
            h_flex()
                .gap_1()
                .child(Icon::new(IconName::Sparkle).size(IconSize::Small).color(if open { Color::Warning } else { Color::Muted }))
                .child(Label::new("The agent wants to run").size(LabelSize::Small))
                .child(Label::new(tool.title.clone()).size(LabelSize::Small).weight(FontWeight::MEDIUM))
                .child(Label::new(format!("from the {} extension", tool.extension)).size(LabelSize::Small).color(Color::Muted)),
        )
        .when(!rows.is_empty(), |el| {
            el.child(v_flex().px_2().py_1().rounded_sm().border_1().border_color(colors.border).bg(colors.editor_background).children(rows.into_iter().map(|(k, v)| {
                h_flex()
                    .gap_2()
                    .items_start()
                    .when(!k.is_empty(), |el| el.child(div().flex_none().child(Label::new(k).size(LabelSize::Small).color(Color::Muted))))
                    .child(wrapping(Label::new(v).size(LabelSize::Small).buffer_font(cx)))
            })))
        })
        .child(footer)
        .into_any_element()
}

/// A language-server edit the agent asks for: what it does and the files it touches.
fn render_edit(thread: &Thread, handle: &WeakEntity<Thread>, ix: usize, plan: &crate::forge_tools::EditPlan, state: &crate::forge_tools::EditState, cx: &App) -> AnyElement {
    use crate::forge_tools::{EditKind, EditState};
    use crate::push_proposal::plural;
    let colors = cx.theme().colors().clone();
    let open = state.is_open();
    let root = thread.root().clone();
    let files = plan.files.iter().map(|f| {
        let shown = f.strip_prefix(&root).unwrap_or(f).to_string_lossy().into_owned();
        h_flex().gap_1().child(Icon::new(IconName::File).size(IconSize::XSmall).color(Color::Muted)).child(wrapping(Label::new(shown).size(LabelSize::Small).buffer_font(cx)))
    });
    let (headline, button) = match &plan.kind {
        EditKind::Rename { symbol, new_name } => (
            h_flex()
                .gap_1()
                .child(Label::new("The agent wants to rename").size(LabelSize::Small))
                .child(Label::new(symbol.clone()).size(LabelSize::Small).buffer_font(cx))
                .child(Label::new("→").size(LabelSize::Small).color(Color::Muted))
                .child(Label::new(new_name.clone()).size(LabelSize::Small).buffer_font(cx).color(Color::Accent)),
            "Rename",
        ),
        EditKind::CodeAction { title } => (h_flex().gap_1().child(Label::new("The agent wants to apply").size(LabelSize::Small)).child(wrapping(Label::new(title.clone()).size(LabelSize::Small).color(Color::Accent))), "Apply"),
        EditKind::Format => (h_flex().child(Label::new("The agent wants to format this file").size(LabelSize::Small)), "Format"),
    };
    let detail = match plan.places {
        Some(places) => format!("{} in {}, with the language server", plural(places, "place"), plural(plan.files.len(), "file")),
        None => "With the language server; the changes join this conversation's".to_string(),
    };
    let footer = match state {
        EditState::Applied => h_flex().gap_1().child(Icon::new(IconName::Check).size(IconSize::XSmall).color(Color::Success)).child(Label::new("Done: the files are in this conversation's changes").size(LabelSize::Small).color(Color::Success)).into_any_element(),
        EditState::Declined => Label::new("→ Declined").size(LabelSize::Small).color(Color::Muted).into_any_element(),
        EditState::Applying => Label::new("Working…").size(LabelSize::Small).color(Color::Muted).into_any_element(),
        EditState::Waiting | EditState::Failed(_) => h_flex()
            .gap_1()
            .child(Button::new(("edit-apply", ix), button).style(ButtonStyle::Filled).start_icon(Icon::new(IconName::Check).size(IconSize::Small)).on_click(on_thread(handle, move |t, window, cx| t.apply_edit_proposal(ix, window, cx))))
            .child(Button::new(("edit-decline", ix), "Decline").start_icon(Icon::new(IconName::Close).size(IconSize::Small)).on_click(on_thread(handle, move |t, _, cx| t.decline_edit(ix, cx))))
            .into_any_element(),
    };
    v_flex()
        .gap_1()
        .p_2()
        .rounded_md()
        .border_1()
        .border_color(if open { cx.theme().status().info_border } else { colors.border })
        .when(open, |el| el.bg(cx.theme().status().info_background))
        .child(h_flex().gap_2().child(Icon::new(IconName::Pencil).size(IconSize::Small).color(if open { Color::Info } else { Color::Muted })).child(headline))
        .child(Label::new(detail).size(LabelSize::XSmall).color(Color::Muted))
        .child(v_flex().gap_0p5().children(files))
        .when_some(if let EditState::Failed(e) = state { Some(e.clone()) } else { None }, |el, e| {
            el.child(h_flex().gap_1().items_start().child(Icon::new(IconName::XCircle).size(IconSize::XSmall).color(Color::Error)).child(wrapping(Label::new(e).size(LabelSize::Small).color(Color::Error))))
        })
        .child(footer)
        .into_any_element()
}

/// A push the agent proposes: the branch, where it goes and its commits.
fn render_push(handle: &WeakEntity<Thread>, ix: usize, plan: &crate::push_proposal::PushPlan, origin: crate::commit_proposal::Origin, state: &crate::push_proposal::PushState, cx: &App) -> AnyElement {
    use crate::commit_proposal::Origin;
    use crate::push_proposal::{PushState, plural};
    let colors = cx.theme().colors().clone();
    let open = state.is_open();
    let headline = match origin {
        Origin::Tool => "The agent proposes a push",
        Origin::Command => "The agent tried to run git push; make it from here",
    };
    let route = format!("{} → {}{}", plan.branch, plan.target(), if plan.set_upstream { " (new branch, becomes its upstream)" } else { "" });
    let commits = plan.commits.iter().map(|(sha, subject)| {
        h_flex()
            .gap_1p5()
            .child(div().flex_none().child(Label::new(sha.clone()).size(LabelSize::Small).buffer_font(cx).color(Color::Muted)))
            .child(wrapping(Label::new(subject.clone()).size(LabelSize::Small)))
    });
    let summary = match plan.commits.len() {
        0 => "No new commits".to_string(),
        n if n >= 50 => "50 or more commits".to_string(),
        n => plural(n, "commit"),
    };
    let footer = match state {
        PushState::Pushed => h_flex()
            .gap_1()
            .child(Icon::new(IconName::Check).size(IconSize::XSmall).color(Color::Success))
            .child(Label::new(format!("Pushed to {}", plan.target())).size(LabelSize::Small).color(Color::Success))
            .into_any_element(),
        PushState::Declined => Label::new("→ Declined").size(LabelSize::Small).color(Color::Muted).into_any_element(),
        PushState::Pushing => Label::new("Pushing…").size(LabelSize::Small).color(Color::Muted).into_any_element(),
        PushState::Waiting | PushState::Failed(_) => h_flex()
            .gap_1()
            .flex_wrap()
            .child(
                Button::new(("push-now", ix), "Push")
                    .style(ButtonStyle::Filled)
                    .start_icon(Icon::new(IconName::ArrowUp).size(IconSize::Small))
                    .on_click(on_thread(handle, move |t, window, cx| t.push_proposal(ix, window, cx))),
            )
            .child(Button::new(("push-decline", ix), "Decline").start_icon(Icon::new(IconName::Close).size(IconSize::Small)).on_click(on_thread(handle, move |t, _, cx| t.decline_push_proposal(ix, cx))))
            .into_any_element(),
    };
    v_flex()
        .gap_1()
        .p_2()
        .rounded_md()
        .border_1()
        .border_color(if open { cx.theme().status().info_border } else { colors.border })
        .when(open, |el| el.bg(cx.theme().status().info_background))
        .child(h_flex().gap_2().child(Icon::new(IconName::ArrowUp).size(IconSize::Small).color(if open { Color::Info } else { Color::Muted })).child(wrapping(Label::new(headline).size(LabelSize::Small))))
        .child(h_flex().gap_1().child(Icon::new(IconName::GitBranch).size(IconSize::XSmall).color(Color::Muted)).child(wrapping(Label::new(route).size(LabelSize::Small).buffer_font(cx))))
        .child(Label::new(summary).size(LabelSize::XSmall).color(Color::Muted))
        .child(v_flex().gap_0p5().children(commits))
        .when_some(if let PushState::Failed(e) = state { Some(e.clone()) } else { None }, |el, e| {
            el.child(h_flex().gap_1().items_start().child(Icon::new(IconName::XCircle).size(IconSize::XSmall).color(Color::Error)).child(wrapping(Label::new(format!("The push failed: {e}")).size(LabelSize::Small).color(Color::Error))))
        })
        .child(footer)
        .into_any_element()
}

pub(crate) fn render_diff(thread: &Thread, handle: &WeakEntity<Thread>, diff: &DiffView, cx: &App) -> AnyElement {
    let colors = cx.theme().colors().clone();
    let open_path = PathBuf::from(&diff.edit.path);
    let path = std::path::Path::new(&diff.edit.path);
    let shown = path.strip_prefix(thread.root()).unwrap_or(path).to_string_lossy().into_owned();
    let (added, removed) = diff.edit.stats();
    v_flex()
        .mt_1()
        .rounded_md()
        .border_1()
        .border_color(colors.border)
        .overflow_hidden()
        .child(
            h_flex()
                .gap_2()
                .px_2()
                .py_1()
                .bg(colors.surface_background)
                .border_b_1()
                .border_color(colors.border)
                .child(Icon::new(IconName::File).size(IconSize::Small).color(Color::Muted))
                // The path takes the free space (and wraps); the rest keeps its size.
                .child(wrapping(Label::new(shown).size(LabelSize::Small).buffer_font(cx)))
                .when(diff.edit.old_text.is_none(), |el| el.child(div().flex_none().child(Label::new("new file").size(LabelSize::XSmall).color(Color::Accent))))
                .child(div().flex_none().child(Label::new(format!("+{added}")).size(LabelSize::XSmall).color(Color::Created)))
                .child(div().flex_none().child(Label::new(format!("-{removed}")).size(LabelSize::XSmall).color(Color::Deleted)))
                .child(
                    IconButton::new(gpui::ElementId::Name(format!("open-{}", diff.edit.path).into()), IconName::ArrowUpRight)
                        .icon_size(IconSize::XSmall)
                        .tooltip(Tooltip::text("Open in editor"))
                        .on_click({
                            let handle = handle.clone();
                            move |_, window, cx| {
                                let Some(workspace) = handle.upgrade().and_then(|t| t.read(cx).workspace().upgrade()) else { return };
                                workspace.update(cx, |ws, cx| ws.open_abs_path(open_path.clone(), workspace::OpenOptions::default(), window, cx).detach_and_log_err(cx));
                            }
                        }),
                ),
        )
        .child(div().max_h(px(360.)).overflow_hidden().bg(colors.editor_background).child(diff.editor.clone()))
        .into_any_element()
}

/// The problems in the files the agent just changed, each opening its line, and a button
/// to send them back.
fn render_check(thread: &Thread, handle: &WeakEntity<Thread>, ix: usize, checks: &[crate::verify::FileCheck], colors: &theme::ThemeColors) -> AnyElement {
    let (errors, warnings) = checks.iter().fold((0, 0), |(e, w), c| {
        let (ce, cw) = c.counts();
        (e + ce, w + cw)
    });
    let files = checks.len();
    if errors + warnings == 0 {
        return h_flex()
            .gap_1()
            .child(Icon::new(IconName::Check).size(IconSize::XSmall).color(Color::Success))
            .child(Label::new(format!("No errors or warnings in the {files} changed file{}", if files == 1 { "" } else { "s" })).size(LabelSize::Small).color(Color::Muted))
            .into_any_element();
    }
    let worse = checks.iter().any(|c| c.got_worse());
    let plural = |n: usize, word: &str| format!("{n} {word}{}", if n == 1 { "" } else { "s" });
    let summary = match (errors, warnings) {
        (0, w) => plural(w, "warning"),
        (e, 0) => plural(e, "error"),
        (e, w) => format!("{}, {}", plural(e, "error"), plural(w, "warning")),
    };
    let headline = format!("{summary} in the changed files{}", if worse { "" } else { " (there before this turn too)" });
    let root = thread.root().clone();
    let mut rows = Vec::new();
    for (fi, check) in checks.iter().enumerate() {
        let rel = check.path.strip_prefix(&root).unwrap_or(&check.path).to_string_lossy().into_owned();
        for (pi, problem) in check.problems.iter().enumerate() {
            let (path, line) = (check.path.clone(), problem.line);
            rows.push(
                h_flex()
                    .id(("check-problem", ix * 10_000 + fi * 100 + pi))
                    .gap_1()
                    .px_1()
                    .rounded_sm()
                    .cursor_pointer()
                    .hover(|el| el.bg(colors.element_hover))
                    .child(Icon::new(if problem.error { IconName::XCircle } else { IconName::Warning }).size(IconSize::XSmall).color(if problem.error { Color::Error } else { Color::Warning }))
                    .child(Label::new(format!("{rel}:{}", line + 1)).size(LabelSize::Small).color(Color::Muted))
                    .child(wrapping(Label::new(problem.message.clone()).size(LabelSize::Small)))
                    .on_click(on_thread(handle, move |thread, window, cx| thread.open_at(path.clone(), line, window, cx)))
                    .into_any_element(),
            );
        }
    }
    v_flex()
        .gap_1()
        .p_2()
        .rounded_md()
        .border_1()
        .border_color(colors.border)
        .child(
            h_flex()
                .gap_1()
                .child(Icon::new(IconName::Warning).size(IconSize::XSmall).color(if errors > 0 { Color::Error } else { Color::Warning }))
                .child(wrapping(Label::new(headline).size(LabelSize::Small)))
                .child(
                    Button::new(("check-fix", ix), "Ask the agent to fix them")
                        .style(ButtonStyle::Filled)
                        .size(ButtonSize::Compact)
                        .label_size(LabelSize::Small)
                        .on_click(on_thread(handle, move |thread, window, cx| thread.fix_problems(ix, window, cx))),
                ),
        )
        .children(rows)
        .into_any_element()
}
