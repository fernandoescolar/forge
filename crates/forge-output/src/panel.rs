//! The Output panel: Forge's own log and each language server's log messages, in a
//! read-only editor (so they can be searched, selected and copied).

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use editor::{Editor, actions::MoveToEnd};
use gpui::{
    Action, App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, InteractiveElement as _, IntoElement,
    ParentElement as _, Pixels, Render, SharedString, Styled as _, Subscription, Task, Window, actions, div, px,
};
use language::LanguageServerId;
use project::{LanguageServerLogType, Project};
use theme::ActiveTheme as _;
use ui::{
    ButtonCommon as _, ButtonLike, ButtonStyle, Clickable as _, Color, ContextMenu, Icon, IconButton, IconName, IconPosition, IconSize, Label,
    LabelCommon as _, LabelSize, PopoverMenu, Toggleable as _, Tooltip, h_flex, v_flex,
};
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

use crate::app_log;

actions!(forge_output, [ToggleFocus, ClearOutput]);

/// Lines kept per language server.
const SERVER_CAPACITY: usize = 5_000;

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<OutputPanel>(window, cx);
        });
    })
    .detach();
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Forge,
    Server(LanguageServerId),
}

pub struct OutputPanel {
    project: Entity<Project>,
    focus_handle: FocusHandle,
    position: DockPosition,
    editor: Entity<Editor>,
    source: Source,
    follow: bool,
    app_lines: Vec<String>,
    app_seen: u64,
    servers: BTreeMap<LanguageServerId, (SharedString, Vec<String>)>,
    _poll: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl OutputPanel {
    pub fn new(workspace: &Workspace, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let project = workspace.project().clone();
        let editor = cx.new(|cx| {
            let mut editor = Editor::multi_line(window, cx);
            editor.set_read_only(true);
            editor.set_show_gutter(false, cx);
            editor
        });
        let subscription = cx.subscribe_in(&project, window, |this, _, event, window, cx| {
            if let project::Event::LanguageServerLog(id, kind, message) = event {
                let prefix = match kind {
                    LanguageServerLogType::Log(typ) => match *typ {
                        lsp::MessageType::ERROR => "error",
                        lsp::MessageType::WARNING => "warn",
                        lsp::MessageType::INFO => "info",
                        _ => "log",
                    },
                    LanguageServerLogType::Trace { .. } => "trace",
                    LanguageServerLogType::Rpc { .. } => return,
                };
                let time = chrono::Local::now().format("%H:%M:%S");
                let lines: Vec<String> = message.lines().map(|line| format!("{time} {prefix:<5} {line}")).collect();
                this.append_server(*id, lines, window, cx);
            }
        });
        // Forge's own log has no event; check for new lines a few times a second.
        let poll = cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(300)).await;
                let Ok(()) = this.update_in(cx, |this, window, cx| {
                    let (lines, seen) = app_log::lines_since(this.app_seen);
                    this.app_seen = seen;
                    if !lines.is_empty() {
                        this.app_lines.extend(lines.iter().cloned());
                        if this.source == Source::Forge {
                            this.append_to_editor(&lines, window, cx);
                        }
                    }
                }) else {
                    break;
                };
            }
        });
        let mut panel = Self {
            project,
            focus_handle: cx.focus_handle(),
            position: forge_ui::dock_position::load(<Self as Panel>::panel_key(), cx).unwrap_or(DockPosition::Bottom),
            editor,
            source: Source::Forge,
            follow: true,
            app_lines: Vec::new(),
            app_seen: 0,
            servers: BTreeMap::new(),
            _poll: poll,
            _subscriptions: vec![subscription],
        };
        panel.show(Source::Forge, window, cx);
        panel
    }

    fn server_name(&self, id: LanguageServerId, cx: &App) -> SharedString {
        self.project
            .read(cx)
            .language_server_statuses(cx)
            .find(|(server, _)| *server == id)
            .map(|(_, status)| SharedString::from(status.name.to_string()))
            .unwrap_or_else(|| format!("Server {id}").into())
    }

    fn append_server(&mut self, id: LanguageServerId, lines: Vec<String>, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.server_name(id, cx);
        let entry = self.servers.entry(id).or_insert_with(|| (name, Vec::new()));
        entry.1.extend(lines.iter().cloned());
        if entry.1.len() > SERVER_CAPACITY {
            let excess = entry.1.len() - SERVER_CAPACITY;
            entry.1.drain(..excess);
        }
        if self.source == Source::Server(id) {
            self.append_to_editor(&lines, window, cx);
        }
        cx.notify();
    }

    fn append_to_editor(&mut self, lines: &[String], window: &mut Window, cx: &mut Context<Self>) {
        let mut text = lines.join("\n");
        text.push('\n');
        let follow = self.follow;
        self.editor.update(cx, |editor, cx| {
            editor.buffer().update(cx, |buffer, cx| {
                let end = buffer.len(cx);
                buffer.edit([(end..end, text)], None, cx);
            });
            if follow {
                editor.move_to_end(&MoveToEnd, window, cx);
            }
        });
    }

    /// Shows `source` in the editor.
    pub fn show(&mut self, source: Source, window: &mut Window, cx: &mut Context<Self>) {
        self.source = source;
        let lines = match source {
            Source::Forge => self.app_lines.clone(),
            Source::Server(id) => self.servers.get(&id).map(|(_, lines)| lines.clone()).unwrap_or_default(),
        };
        let mut text = lines.join("\n");
        if !text.is_empty() {
            text.push('\n');
        }
        let follow = self.follow;
        self.editor.update(cx, |editor, cx| {
            editor.set_text(text, window, cx);
            if follow {
                editor.move_to_end(&MoveToEnd, window, cx);
            }
        });
        cx.notify();
    }

    fn clear(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.source {
            Source::Forge => self.app_lines.clear(),
            Source::Server(id) => {
                if let Some((_, lines)) = self.servers.get_mut(&id) {
                    lines.clear();
                }
            }
        }
        self.show(self.source, window, cx);
    }

    fn source_label(&self, source: Source, cx: &App) -> SharedString {
        match source {
            Source::Forge => "Forge".into(),
            Source::Server(id) => self.servers.get(&id).map(|(name, _)| name.clone()).unwrap_or_else(|| self.server_name(id, cx)),
        }
    }

    /// Every server the project runs, plus those that logged before stopping.
    fn sources(&self, cx: &App) -> Vec<(Source, SharedString)> {
        let mut servers: HashMap<LanguageServerId, SharedString> =
            self.project.read(cx).language_server_statuses(cx).map(|(id, status)| (id, SharedString::from(status.name.to_string()))).collect();
        for (id, (name, _)) in &self.servers {
            servers.entry(*id).or_insert_with(|| name.clone());
        }
        let mut servers: Vec<_> = servers.into_iter().collect();
        servers.sort_by(|a, b| a.1.cmp(&b.1));
        std::iter::once((Source::Forge, "Forge".into())).chain(servers.into_iter().map(|(id, name)| (Source::Server(id), name))).collect()
    }
}

impl Render for OutputPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors().clone();
        let current = self.source;
        let sources = self.sources(cx);
        let this = cx.entity().downgrade();
        let picker = PopoverMenu::new("output-source")
            .trigger(
                ButtonLike::new("output-source-trigger")
                    .style(ButtonStyle::Subtle)
                    .child(
                        h_flex()
                            .gap_1()
                            .child(Label::new(self.source_label(current, cx)).size(LabelSize::Small))
                            .child(Icon::new(IconName::ChevronDown).size(IconSize::XSmall).color(Color::Muted)),
                    )
                    .tooltip(Tooltip::text("Which log to show")),
            )
            .menu(move |window, cx| {
                let (sources, this) = (sources.clone(), this.clone());
                Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                    for (i, (source, name)) in sources.into_iter().enumerate() {
                        if i == 1 {
                            menu = menu.header("Language servers");
                        }
                        let this = this.clone();
                        menu = menu.toggleable_entry(name, source == current, IconPosition::Start, None, move |window, cx| {
                            this.update(cx, |panel, cx| panel.show(source, window, cx)).ok();
                        });
                    }
                    menu
                }))
            });

        let header = h_flex()
            .justify_between()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(colors.border)
            .child(
                h_flex()
                    .gap_2()
                    .child(forge_ui::panel_grip("output-panel-grip", Arc::new(cx.entity()), "Output", Some(IconName::ListTree)))
                    .child(Label::new("Output").weight(FontWeight::BOLD))
                    .child(picker),
            )
            .child(
                h_flex()
                    .gap_0p5()
                    .child(
                        IconButton::new("follow", IconName::ArrowDown)
                            .toggle_state(self.follow)
                            .tooltip(Tooltip::text("Follow new output"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.follow = !this.follow;
                                if this.follow {
                                    this.editor.update(cx, |editor, cx| editor.move_to_end(&MoveToEnd, window, cx));
                                }
                                cx.notify();
                            })),
                    )
                    .child(
                        IconButton::new("clear", IconName::Trash)
                            .tooltip(Tooltip::text("Clear"))
                            .on_click(cx.listener(|this, _, window, cx| this.clear(window, cx))),
                    ),
            );

        v_flex()
            .key_context("ForgeOutputPanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(colors.panel_background)
            .child(header)
            .child(div().flex_1().min_h_0().child(self.editor.clone()))
    }
}

impl Focusable for OutputPanel {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl EventEmitter<PanelEvent> for OutputPanel {}

impl Panel for OutputPanel {
    fn persistent_name() -> &'static str {
        "ForgeOutputPanel"
    }
    fn panel_key() -> &'static str {
        "ForgeOutputPanel"
    }
    fn position(&self, _: &Window, _: &App) -> DockPosition {
        self.position
    }
    fn position_is_valid(&self, _: DockPosition) -> bool {
        true
    }
    fn set_position(&mut self, position: DockPosition, _: &mut Window, cx: &mut Context<Self>) {
        self.position = position;
        forge_ui::dock_position::save(Self::panel_key(), position, cx);
        gpui::BorrowAppContext::update_global::<settings::SettingsStore, _>(cx, |_, _| {});
        cx.notify();
    }
    fn default_size(&self, _: &Window, _: &App) -> Pixels {
        px(260.)
    }
    fn icon(&self, _: &Window, _: &App) -> Option<IconName> {
        Some(IconName::ListTree)
    }
    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        Some("Output")
    }
    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }
    fn activation_priority(&self) -> u32 {
        // Forge panels use 10–19; Zed's use 1–7 and extension slots 8 and 20+.
        12
    }
}
