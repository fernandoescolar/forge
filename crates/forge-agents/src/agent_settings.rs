//! The Agents settings page: which agent new threads start with, the agents Forge knows,
//! adding one (a preset or your own command), whether writes wait for review and what
//! agents may do without asking. Every change is saved to `agents.json` straight away.

use editor::Editor;
use gpui::TaskExt as _;
use gpui::prelude::FluentBuilder as _;
use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, InteractiveElement as _, IntoElement, ParentElement as _, Render,
    ScrollHandle, SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, WeakEntity, Window, div, px,
};
use ide_api::AgentSpec;
use theme::ActiveTheme as _;
use ui::{
    Button, ButtonCommon as _, ButtonSize, ButtonStyle, Checkbox, Clickable as _, Color, Disableable as _, Icon, IconButton, IconName, IconSize, Label,
    LabelCommon as _, LabelSize, ToggleState, Tooltip, h_flex, v_flex,
};
use workspace::{
    OpenOptions, Workspace,
    item::{Item, ItemEvent},
};

use crate::permissions::{PermissionMode, Permissions};
use crate::settings::{self, AgentSettings};
use crate::threads_panel::OpenAgentSettings;

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &OpenAgentSettings, window, cx| open(workspace, window, cx));
    })
    .detach();
}

/// Shows the settings page, reusing its tab if it is already open.
pub fn open(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let existing = workspace.items_of_type::<AgentSettingsPage>(cx).next();
    if let Some(page) = existing {
        workspace.activate_item(&page, true, true, window, cx);
        return;
    }
    let weak = workspace.weak_handle();
    let page = cx.new(|cx| AgentSettingsPage::new(weak, window, cx));
    workspace.add_item_to_active_pane(Box::new(page), None, true, window, cx);
}

pub struct AgentSettingsPage {
    workspace: WeakEntity<Workspace>,
    settings: Entity<AgentSettings>,
    focus_handle: FocusHandle,
    scroll: ScrollHandle,
    id: Entity<Editor>,
    command: Entity<Editor>,
    args: Entity<Editor>,
    /// A command prefix to always allow.
    allow_command: Entity<Editor>,
    _subscriptions: Vec<Subscription>,
}

impl AgentSettingsPage {
    pub fn new(workspace: WeakEntity<Workspace>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let settings = settings::global(cx);
        let field = |placeholder: &str, window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| {
                let mut e = Editor::single_line(window, cx);
                e.set_placeholder_text(placeholder, window, cx);
                e
            })
        };
        let id = field("name, e.g. my-agent", window, cx);
        let command = field("command, e.g. npx", window, cx);
        let args = field("arguments, e.g. -y @scope/agent-acp", window, cx);
        let allow_command = field("command, e.g. dotnet build", window, cx);
        let subs = vec![cx.observe(&settings, |_, _, cx| cx.notify())];
        Self { workspace, settings, focus_handle: cx.focus_handle(), scroll: ScrollHandle::new(), id, command, args, allow_command, _subscriptions: subs }
    }

    fn add_custom(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let id = self.id.read(cx).text(cx).trim().to_string();
        let command = self.command.read(cx).text(cx).trim().to_string();
        if id.is_empty() || command.is_empty() {
            return;
        }
        let args = split_args(&self.args.read(cx).text(cx));
        self.settings.update(cx, |s, cx| s.upsert_agent(AgentSpec { id, command, args, env: vec![], cwd: None }, cx));
        for field in [&self.id, &self.command, &self.args] {
            field.update(cx, |e, cx| e.set_text("", window, cx));
        }
    }

    fn update_permissions(&mut self, cx: &mut Context<Self>, f: impl FnOnce(&mut Permissions)) {
        let mut permissions = self.settings.read(cx).config().permissions.clone();
        f(&mut permissions);
        self.settings.update(cx, |s, cx| s.set_permissions(permissions, cx));
    }

    fn add_allowed_command(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let command = self.allow_command.read(cx).text(cx).trim().to_string();
        if command.is_empty() {
            return;
        }
        self.update_permissions(cx, |p| {
            if !p.allow_commands.contains(&command) {
                p.allow_commands.push(command);
            }
        });
        self.allow_command.update(cx, |e, cx| e.set_text("", window, cx));
    }

    fn render_permissions(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let permissions = self.settings.read(cx).config().permissions.clone();
        let super_user = permissions.mode == PermissionMode::SuperUser;
        let modes = PermissionMode::ALL.into_iter().map(|mode| {
            let selected = permissions.mode == mode;
            h_flex()
                .id(SharedString::from(format!("permission-mode-{mode:?}")))
                .py_1p5()
                .gap_3()
                .items_start()
                .cursor_pointer()
                .child(Icon::new(if selected { IconName::Check } else { IconName::Circle }).size(IconSize::Small).color(if selected { Color::Accent } else { Color::Muted }))
                .child(
                    v_flex()
                        .min_w_0()
                        .child(
                            Label::new(mode.label())
                                .weight(if selected { FontWeight::SEMIBOLD } else { FontWeight::NORMAL })
                                .color(if mode == PermissionMode::SuperUser { Color::Warning } else { Color::Default }),
                        )
                        .child(Label::new(mode.description()).size(LabelSize::Small).color(Color::Muted)),
                )
                .on_click(cx.listener(move |this, _, _, cx| this.update_permissions(cx, |p| p.mode = mode)))
        });
        let commands = permissions.allow_commands.iter().map(|command| {
            let remove = command.clone();
            h_flex()
                .gap_1()
                .pl_2()
                .pr_0p5()
                .rounded_md()
                .bg(cx.theme().colors().element_background)
                .child(Label::new(command.clone()).size(LabelSize::Small).buffer_font(cx))
                .child(
                    IconButton::new(SharedString::from(format!("remove-command-{command}")), IconName::Close)
                        .icon_size(IconSize::XSmall)
                        .tooltip(Tooltip::text("Ask again for this command"))
                        .on_click(cx.listener(move |this, _, _, cx| this.update_permissions(cx, |p| p.allow_commands.retain(|c| *c != remove)))),
                )
        });
        v_flex()
            .child(Self::section("Permissions", "What agents may do without asking you first. Anything else shows up in the thread for your decision.", cx))
            .child(v_flex().mt_2().children(modes))
            .child(
                div().mt_3().child(
                    Checkbox::new("outside-workspace", if permissions.may_touch_outside() { ToggleState::Selected } else { ToggleState::Unselected })
                        .label("Let agents read and write files outside the workspace")
                        .disabled(super_user)
                        .on_click(cx.listener(|this, state: &ToggleState, _, cx| {
                            let on = *state == ToggleState::Selected;
                            this.update_permissions(cx, |p| p.files_outside_workspace = on);
                        })),
                ),
            )
            .child(
                v_flex()
                    .mt_4()
                    .gap_1()
                    .child(Label::new("Always allowed commands").weight(FontWeight::SEMIBOLD))
                    .child(
                        Label::new("Commands starting with one of these run without asking, in any mode. Chained or piped commands (&&, ;, |, >) always ask.")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            )
            .child(h_flex().mt_2().gap_1().flex_wrap().children(commands))
            .child(
                h_flex()
                    .mt_2()
                    .gap_2()
                    .child(div().flex_1().child(Self::field(&self.allow_command, 260., cx)))
                    .child(
                        Button::new("add-allowed-command", "Allow")
                            .style(ButtonStyle::Filled)
                            .disabled(self.allow_command.read(cx).text(cx).trim().is_empty())
                            .on_click(cx.listener(|this, _, window, cx| this.add_allowed_command(window, cx))),
                    ),
            )
    }

    fn edit_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let path = crate::config::path();
        if !path.exists() {
            // Write what the page shows so hand edits start from it.
            let config = self.settings.read(cx).config().clone();
            if let Err(e) = crate::config::save(&config) {
                log::error!("Could not create agents.json: {e:#}");
            }
        }
        if let Some(ws) = self.workspace.upgrade() {
            ws.update(cx, |ws, cx| ws.open_abs_path(path, OpenOptions::default(), window, cx).detach_and_log_err(cx));
        }
    }

    fn section(title: &str, hint: &str, cx: &App) -> impl IntoElement {
        v_flex()
            .pt_6()
            .pb_2()
            .gap_0p5()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(Label::new(title.to_string()).weight(FontWeight::SEMIBOLD))
            .child(Label::new(hint.to_string()).size(LabelSize::Small).color(Color::Muted))
    }

    fn render_agent(&self, ix: usize, agent: &AgentSpec, is_default: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let id = agent.id.clone();
        let command_line = std::iter::once(agent.command.as_str()).chain(agent.args.iter().map(String::as_str)).collect::<Vec<_>>().join(" ");
        let dev = crate::config::is_builtin_dev_agent(agent);
        h_flex()
            .id(SharedString::from(format!("agent-{ix}")))
            .group("agent-row")
            .py_2()
            .gap_3()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                IconButton::new(SharedString::from(format!("default-{ix}")), if is_default { IconName::Check } else { IconName::Circle })
                    .icon_size(IconSize::Small)
                    .icon_color(if is_default { Color::Accent } else { Color::Muted })
                    .tooltip(Tooltip::text(if is_default { "New threads start with this agent" } else { "Make this the default" }))
                    .on_click(cx.listener({
                        let id = id.clone();
                        move |this, _, _, cx| this.settings.update(cx, |s, cx| s.set_default(&id, cx))
                    })),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        h_flex()
                            .gap_2()
                            .child(Label::new(agent.id.clone()))
                            .when(is_default, |row| row.child(Label::new("default").size(LabelSize::XSmall).color(Color::Accent)))
                            .when(dev, |row| row.child(Label::new("built in").size(LabelSize::XSmall).color(Color::Muted))),
                    )
                    .child(Label::new(command_line).size(LabelSize::Small).color(Color::Muted).truncate()),
            )
            .when(!dev, |row| {
                row.child(
                    div().invisible().group_hover("agent-row", |s| s.visible()).child(
                        IconButton::new(SharedString::from(format!("remove-{ix}")), IconName::Trash)
                            .icon_size(IconSize::Small)
                            .icon_color(Color::Muted)
                            .tooltip(Tooltip::text("Remove this agent"))
                            .on_click(cx.listener(move |this, _, _, cx| this.settings.update(cx, |s, cx| s.remove_agent(&id, cx)))),
                    ),
                )
            })
    }

    fn field(editor: &Entity<Editor>, width: f32, cx: &App) -> impl IntoElement {
        div()
            .w(px(width))
            .px_2()
            .py_1()
            .rounded_md()
            .border_1()
            .border_color(cx.theme().colors().border)
            .bg(cx.theme().colors().editor_background)
            .child(editor.clone())
    }
}

/// Splits arguments on whitespace, keeping double-quoted parts together.
fn split_args(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    for c in text.chars() {
        match c {
            '"' => quoted = !quoted,
            c if c.is_whitespace() && !quoted => {
                if !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

impl Render for AgentSettingsPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let settings = self.settings.read(cx);
        let agents = settings.agents().to_vec();
        let default_ix = settings.default_index();
        let review = settings.config().review_writes;
        let error = settings.error().map(str::to_string);
        let missing: Vec<AgentSpec> = settings::presets().into_iter().filter(|p| !agents.iter().any(|a| a.id == p.id)).collect();
        let path = crate::config::path();

        let rows: Vec<_> = agents.iter().enumerate().map(|(ix, a)| self.render_agent(ix, a, ix == default_ix, cx).into_any_element()).collect();

        v_flex()
            .id("forge-agent-settings")
            .key_context("ForgeAgentSettings")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .child(
                v_flex()
                    .w_full()
                    .max_w(px(720.))
                    .mx_auto()
                    .px_8()
                    .py_8()
                    .child(
                        h_flex()
                            .justify_between()
                            .child(
                                v_flex()
                                    .gap_1()
                                    .child(Label::new("Agents").size(LabelSize::Large).weight(FontWeight::SEMIBOLD))
                                    .child(Label::new(format!("Saved in {}", path.display())).size(LabelSize::Small).color(Color::Muted)),
                            )
                            .child(
                                h_flex()
                                    .gap_1()
                                    .child(
                                        Button::new("reload", "Reload")
                                            .style(ButtonStyle::Subtle)
                                            .size(ButtonSize::Compact)
                                            .tooltip(Tooltip::text("Read agents.json again after editing it by hand"))
                                            .on_click(cx.listener(|this, _, _, cx| this.settings.update(cx, |s, cx| s.reload(cx)))),
                                    )
                                    .child(
                                        Button::new("edit-file", "Edit agents.json")
                                            .style(ButtonStyle::Subtle)
                                            .size(ButtonSize::Compact)
                                            .on_click(cx.listener(|this, _, window, cx| this.edit_file(window, cx))),
                                    ),
                            ),
                    )
                    .when_some(error, |page, error| {
                        page.child(
                            h_flex()
                                .mt_4()
                                .p_2()
                                .gap_2()
                                .rounded_md()
                                .bg(cx.theme().status().error_background)
                                .child(Icon::new(IconName::XCircle).size(IconSize::Small).color(Color::Error))
                                .child(Label::new(error).size(LabelSize::Small)),
                        )
                    })
                    .child(Self::section("New threads", "The agent marked ✓ starts every new thread. You can still pick another from the ▾ next to New.", cx))
                    .when(rows.is_empty(), |page| page.child(Label::new("No agents yet: add one below.").color(Color::Muted).mt_2()))
                    .children(rows)
                    .child(Self::section("Add an agent", "Any program that speaks the Agent Client Protocol.", cx))
                    .when(!missing.is_empty(), |page| {
                        page.child(h_flex().mt_3().gap_2().flex_wrap().children(missing.into_iter().map(|preset| {
                            let tooltip = std::iter::once(preset.command.clone()).chain(preset.args.clone()).collect::<Vec<_>>().join(" ");
                            Button::new(SharedString::from(format!("preset-{}", preset.id)), format!("Add {}", preset.id))
                                .style(ButtonStyle::Outlined)
                                .tooltip(Tooltip::text(tooltip))
                                .on_click(cx.listener(move |this, _, _, cx| this.settings.update(cx, |s, cx| s.upsert_agent(preset.clone(), cx))))
                        })))
                    })
                    .child(
                        h_flex()
                            .mt_3()
                            .gap_2()
                            .child(Self::field(&self.id, 140., cx))
                            .child(Self::field(&self.command, 140., cx))
                            .child(div().flex_1().child(Self::field(&self.args, 260., cx)))
                            .child(
                                Button::new("add-custom", "Add")
                                    .style(ButtonStyle::Filled)
                                    .disabled(self.id.read(cx).text(cx).trim().is_empty() || self.command.read(cx).text(cx).trim().is_empty())
                                    .on_click(cx.listener(|this, _, window, cx| this.add_custom(window, cx))),
                            ),
                    )
                    .child(Self::section("Changes", "What happens when an agent wants to write a file.", cx))
                    .child(
                        div().mt_3().child(
                            Checkbox::new("review-writes", if review { ToggleState::Selected } else { ToggleState::Unselected })
                                .label("Show each write as a diff and wait for my decision")
                                .on_click({
                                    let settings = self.settings.clone();
                                    move |state, _, cx| {
                                        let on = *state == ToggleState::Selected;
                                        settings.update(cx, |s, cx| s.set_review_writes(on, cx));
                                    }
                                }),
                        ),
                    )
                    .child(self.render_permissions(cx)),
            )
    }
}

impl Focusable for AgentSettingsPage {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<ItemEvent> for AgentSettingsPage {}

impl Item for AgentSettingsPage {
    type Event = ItemEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        "Agents".into()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Settings))
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, VisualTestContext, point, size};
    use project::Project;

    /// The page opens once, adds a custom agent from its fields, and the default follows.
    #[gpui::test]
    async fn adds_an_agent_and_picks_the_default(cx: &mut TestAppContext) {
        let params = cx.update(workspace::AppState::test);
        cx.update(|cx| {
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            crate::init(cx);
        });
        let project = Project::test(params.fs.clone(), [], cx).await;
        let window = cx.add_window(|window, cx| workspace::MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window.read_with(cx, |mw, _| mw.workspace().clone()).unwrap();
        let cx = &mut VisualTestContext::from_window(window.into(), cx);
        let tmp = tempfile::tempdir().unwrap();
        let config = crate::config::AgentsConfig { agents: vec![], review_writes: true, verify_changes: true, mcp_servers: vec![], default_agent: None, permissions: Default::default() };
        cx.update(|_, cx| settings::set_in_memory(config, tmp.path().to_path_buf(), cx));

        workspace.update_in(cx, |ws, window, cx| open(ws, window, cx));
        workspace.update_in(cx, |ws, window, cx| open(ws, window, cx));
        let pages: Vec<_> = workspace.read_with(cx, |ws, cx| ws.items_of_type::<AgentSettingsPage>(cx).collect());
        assert_eq!(pages.len(), 1, "one settings tab");
        let page = pages[0].clone();

        page.update_in(cx, |p, window, cx| {
            p.id.update(cx, |e, cx| e.set_text("mine", window, cx));
            p.command.update(cx, |e, cx| e.set_text("my-agent", window, cx));
            p.args.update(cx, |e, cx| e.set_text("--acp \"x y\"", window, cx));
            p.add_custom(window, cx);
        });
        let settings = cx.update(|_, cx| settings::global(cx));
        settings.update(cx, |s, cx| s.upsert_agent(settings::presets().remove(0), cx));
        let (ids, default) = settings.read_with(cx, |s, _| (s.agents().iter().map(|a| a.id.clone()).collect::<Vec<_>>(), s.default_agent().map(|a| a.id.clone())));
        assert_eq!(ids, ["mine", "claude"]);
        assert_eq!(default.as_deref(), Some("mine"), "the first agent until one is chosen");
        assert_eq!(settings.read_with(cx, |s, _| s.agents()[0].args.clone()), ["--acp", "x y"]);
        assert!(page.read_with(cx, |p, cx| p.id.read(cx).text(cx).is_empty()), "fields cleared");

        settings.update(cx, |s, cx| s.set_default("claude", cx));
        settings.update(cx, |s, cx| s.remove_agent("claude", cx));
        assert_eq!(settings.read_with(cx, |s, _| s.default_agent().map(|a| a.id.clone())).as_deref(), Some("mine"));

        let page = page.clone();
        cx.draw(point(px(0.), px(0.)), size(px(1000.), px(700.)), move |_, _| div().size_full().child(page));
    }

    #[test]
    fn splits_arguments_keeping_quotes_together() {
        assert_eq!(split_args(r#"-y  @scope/agent "a b""#), ["-y", "@scope/agent", "a b"]);
        assert!(split_args("   ").is_empty());
    }
}
