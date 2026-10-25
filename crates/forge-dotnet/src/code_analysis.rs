//! Code Analysis: every compiler and analyzer diagnostic of the solution's C# files,
//! grouped by rule (CS8019, CA1822, IDE0005…), like Rider's solution-wide analysis. Each
//! rule shows how often it occurs and where, and its severity can be set for the whole
//! solution, in the `.editorconfig` next to it (`dotnet_diagnostic.CA1822.severity = none`),
//! which the language server picks up.
//!
//! The diagnostics are the language server's: Roslyn analyzes the whole solution (see
//! `forge-languages/src/roslyn.rs`), so files that aren't open are listed too.

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;

use dotnet_model::editorconfig;
use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, AppContext as _, ClickEvent, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, IntoElement, ParentElement as _, Render,
    ScrollHandle, SharedString, Styled as _, Subscription, Task, WeakEntity, Window, actions, div, px,
};
use language::{DiagnosticSeverity, ToPoint as _};
use project::Project;
use theme::ActiveTheme as _;
use ui::{
    Button, ButtonCommon as _, ButtonSize, ButtonStyle, Clickable as _, Color, ContextMenu, Icon, IconButton, IconName, IconSize, Label, LabelCommon as _,
    LabelSize, PopoverMenu, Tooltip, h_flex, v_flex,
};
use ui::{InteractiveElement as _, StatefulInteractiveElement as _};
use workspace::{Item, Workspace, item::ItemEvent};

actions!(forge_dotnet, [
    /// Opens Code Analysis: the solution's diagnostics grouped by rule.
    OpenCodeAnalysis
]);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &OpenCodeAnalysis, window, cx| open(workspace, window, cx));
    })
    .detach();
}

/// Opens the view, or shows the one already open.
pub fn open(workspace: &mut Workspace, window: &mut Window, cx: &mut gpui::Context<Workspace>) {
    let existing = workspace.items_of_type::<CodeAnalysis>(cx).next();
    if let Some(existing) = existing {
        workspace.activate_item(&existing, true, true, window, cx);
        return;
    }
    let project = workspace.project().clone();
    let weak = workspace.weak_handle();
    let view = cx.new(|cx| CodeAnalysis::new(weak, project, cx));
    workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
}

/// One occurrence of a rule.
#[derive(Clone, Debug, PartialEq)]
struct Occurrence {
    file: PathBuf,
    /// Zero-based.
    row: u32,
    message: String,
    severity: DiagnosticSeverity,
}

/// A rule and where it occurs.
#[derive(Clone, Debug, PartialEq)]
struct Rule {
    code: String,
    occurrences: Vec<Occurrence>,
}

impl Rule {
    /// The most severe of its occurrences.
    fn severity(&self) -> DiagnosticSeverity {
        self.occurrences.iter().map(|o| o.severity).min().unwrap_or(DiagnosticSeverity::HINT)
    }
}

pub struct CodeAnalysis {
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    focus_handle: FocusHandle,
    rules: Vec<Rule>,
    expanded: HashSet<String>,
    loading: bool,
    /// What the solution's `.editorconfig` sets, by rule.
    configured: BTreeMap<String, String>,
    message: Option<String>,
    scroll: ScrollHandle,
    refresh_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl CodeAnalysis {
    fn new(workspace: WeakEntity<Workspace>, project: Entity<Project>, cx: &mut Context<Self>) -> Self {
        // New diagnostics arrive in bursts while the server analyzes: gather them.
        let subscription = cx.subscribe(&project, |this, _, event, cx| {
            if let project::Event::DiagnosticsUpdated { .. } = event {
                this.schedule_refresh(Duration::from_millis(1500), cx);
            }
        });
        let mut view = Self {
            workspace,
            project,
            focus_handle: cx.focus_handle(),
            rules: Vec::new(),
            expanded: HashSet::new(),
            loading: false,
            configured: BTreeMap::new(),
            message: None,
            scroll: ScrollHandle::new(),
            refresh_task: None,
            _subscriptions: vec![subscription],
        };
        view.schedule_refresh(Duration::ZERO, cx);
        view
    }

    /// The `.editorconfig` severities apply to: the one at the root of the first folder.
    fn editorconfig_path(&self, cx: &App) -> Option<PathBuf> {
        self.project.read(cx).visible_worktrees(cx).next().map(|tree| tree.read(cx).abs_path().join(".editorconfig"))
    }

    fn schedule_refresh(&mut self, delay: Duration, cx: &mut Context<Self>) {
        self.loading = true;
        cx.notify();
        self.refresh_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            let Ok(buffers) = this.update(cx, |this, cx| this.open_buffers_with_problems(cx)) else { return };
            let mut opened = Vec::new();
            for task in buffers {
                if let Ok(buffer) = task.await {
                    opened.push(buffer);
                }
            }
            this.update(cx, |this, cx| {
                this.rules = collect_rules(&opened, cx);
                this.configured = this.read_configured(cx);
                this.loading = false;
                cx.notify();
            })
            .ok();
        }));
    }

    /// The C# files that have errors or warnings, opened to read their diagnostics.
    fn open_buffers_with_problems(&self, cx: &mut App) -> Vec<Task<anyhow::Result<Entity<language::Buffer>>>> {
        let paths: Vec<project::ProjectPath> = self
            .project
            .read(cx)
            .diagnostic_summaries(false, cx)
            .filter(|(path, _, _)| path.path.extension().is_some_and(|e| e == "cs"))
            .map(|(path, _, _)| path)
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        paths.into_iter().map(|path| self.project.update(cx, |project, cx| project.open_buffer(path, cx))).collect()
    }

    fn read_configured(&self, cx: &App) -> BTreeMap<String, String> {
        let Some(path) = self.editorconfig_path(cx) else { return BTreeMap::new() };
        let text = std::fs::read_to_string(path).unwrap_or_default();
        self.rules.iter().filter_map(|rule| Some((rule.code.clone(), editorconfig::severity(&text, &rule.code)?))).collect()
    }

    /// Sets `rule`'s severity in the `.editorconfig`.
    fn configure(&mut self, rule: String, severity: &'static str, cx: &mut Context<Self>) {
        let Some(path) = self.editorconfig_path(cx) else { return };
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        match std::fs::write(&path, editorconfig::set_severity(&text, &rule, severity)) {
            Ok(()) => {
                self.message = Some(format!("{rule} is now “{severity}” in {}", path.display()));
                self.configured.insert(rule, severity.to_string());
            }
            Err(error) => self.message = Some(format!("Could not write {}: {error}", path.display())),
        }
        cx.notify();
    }

    fn open_occurrence(&self, occurrence: Occurrence, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else { return };
        let task = workspace.update(cx, |workspace, cx| workspace.open_abs_path(occurrence.file.clone(), workspace::OpenOptions::default(), window, cx));
        cx.spawn_in(window, async move |_, cx| {
            let item = task.await?;
            if let Some(editor) = item.downcast::<editor::Editor>() {
                editor.update_in(cx, |editor, window, cx| {
                    let point = language::Point::new(occurrence.row, 0);
                    editor.change_selections(Default::default(), window, cx, |s| s.select_ranges([point..point]));
                })?;
            }
            anyhow::Ok(())
        })
        .detach();
    }

    fn render_rule(&self, rule: &Rule, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors().clone();
        let expanded = self.expanded.contains(&rule.code);
        let (icon, color) = severity_icon(rule.severity());
        let configured = self.configured.get(&rule.code).cloned();
        let code = rule.code.clone();
        let toggle = rule.code.clone();
        let first_message = rule.occurrences.first().map(|o| o.message.clone()).unwrap_or_default();
        let mut column = v_flex().child(
            h_flex()
                .id(SharedString::from(format!("rule-{}", rule.code)))
                .w_full()
                .h(px(26.))
                .px_2()
                .gap_2()
                .cursor_pointer()
                .hover(|row| row.bg(colors.element_hover))
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    if !this.expanded.remove(&toggle) {
                        this.expanded.insert(toggle.clone());
                    }
                    cx.notify();
                }))
                .child(Icon::new(if expanded { IconName::ChevronDown } else { IconName::ChevronRight }).size(IconSize::XSmall).color(Color::Muted))
                .child(Icon::new(icon).size(IconSize::Small).color(color))
                .child(Label::new(rule.code.clone()).size(LabelSize::Small).weight(FontWeight::BOLD))
                .child(Label::new(format!("{}×", rule.occurrences.len())).size(LabelSize::XSmall).color(Color::Muted))
                .child(div().flex_1().min_w_0().overflow_hidden().child(Label::new(first_message).size(LabelSize::Small).color(Color::Muted).single_line()))
                .child(self.render_severity_menu(code, configured, cx)),
        );
        if expanded {
            for (index, occurrence) in rule.occurrences.iter().enumerate() {
                let location = format!("{}:{}", occurrence.file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(), occurrence.row + 1);
                let target = occurrence.clone();
                column = column.child(
                    h_flex()
                        .id(SharedString::from(format!("occurrence-{}-{index}", rule.code)))
                        .w_full()
                        .pl(px(40.))
                        .pr_2()
                        .h(px(22.))
                        .gap_2()
                        .cursor_pointer()
                        .hover(|row| row.bg(colors.element_hover))
                        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| this.open_occurrence(target.clone(), window, cx)))
                        .child(Label::new(location).size(LabelSize::XSmall).color(Color::Accent))
                        .child(div().flex_1().min_w_0().overflow_hidden().child(Label::new(occurrence.message.clone()).size(LabelSize::XSmall).single_line())),
                );
            }
        }
        column.into_any_element()
    }

    /// The severity the `.editorconfig` sets (or "Set severity"), and a menu to change it.
    fn render_severity_menu(&self, rule: String, configured: Option<String>, cx: &mut Context<Self>) -> AnyElement {
        let view = cx.entity().downgrade();
        let label = configured.clone().unwrap_or_else(|| "Set severity".into());
        PopoverMenu::new(SharedString::from(format!("severity-{rule}")))
            .trigger(Button::new(SharedString::from(format!("severity-button-{rule}")), label).style(ButtonStyle::Subtle).size(ButtonSize::Compact).label_size(LabelSize::XSmall))
            .menu(move |window, cx| {
                let (rule, configured, view) = (rule.clone(), configured.clone(), view.clone());
                Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                    menu = menu.header(format!("{rule} in .editorconfig"));
                    for severity in editorconfig::SEVERITIES {
                        let (rule, view) = (rule.clone(), view.clone());
                        let checked = configured.as_deref() == Some(severity);
                        menu = menu.toggleable_entry(severity, checked, ui::IconPosition::Start, None, move |_, cx| {
                            view.update(cx, |view, cx| view.configure(rule.clone(), severity, cx)).ok();
                        });
                    }
                    menu
                }))
            })
            .into_any_element()
    }
}

/// The diagnostics of `buffers`, grouped by code, most frequent rules first.
fn collect_rules(buffers: &[Entity<language::Buffer>], cx: &App) -> Vec<Rule> {
    let mut by_code: BTreeMap<String, Vec<Occurrence>> = BTreeMap::new();
    for buffer in buffers {
        let buffer = buffer.read(cx);
        let Some(file) = buffer.file().and_then(|f| f.as_local()).map(|f| f.abs_path(cx)) else { continue };
        let snapshot = buffer.snapshot();
        for entry in buffer.buffer_diagnostics(None) {
            let diagnostic = &entry.diagnostic;
            if !diagnostic.is_primary || diagnostic.severity > DiagnosticSeverity::INFORMATION {
                continue;
            }
            let Some(code) = diagnostic.code.as_ref().map(|code| match code {
                lsp::NumberOrString::Number(n) => n.to_string(),
                lsp::NumberOrString::String(s) => s.clone(),
            }) else {
                continue;
            };
            let occurrence = Occurrence { file: file.clone(), row: entry.range.start.to_point(&snapshot).row, message: diagnostic.message.to_string(), severity: diagnostic.severity };
            let list = by_code.entry(code).or_default();
            // The same problem reported by two sources (e.g. a build and the server).
            if !list.iter().any(|o| o.file == occurrence.file && o.row == occurrence.row) {
                list.push(occurrence);
            }
        }
    }
    let mut rules: Vec<Rule> = by_code.into_iter().map(|(code, occurrences)| Rule { code, occurrences }).collect();
    for rule in &mut rules {
        rule.occurrences.sort_by(|a, b| (&a.file, a.row).cmp(&(&b.file, b.row)));
    }
    rules.sort_by(|a, b| (a.severity(), std::cmp::Reverse(a.occurrences.len()), &a.code).cmp(&(b.severity(), std::cmp::Reverse(b.occurrences.len()), &b.code)));
    rules
}

fn severity_icon(severity: DiagnosticSeverity) -> (IconName, Color) {
    match severity {
        DiagnosticSeverity::ERROR => (IconName::XCircle, Color::Error),
        DiagnosticSeverity::WARNING => (IconName::Warning, Color::Warning),
        _ => (IconName::Info, Color::Muted),
    }
}

impl Render for CodeAnalysis {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors().clone();
        let total: usize = self.rules.iter().map(|r| r.occurrences.len()).sum();
        let rules: Vec<AnyElement> = self.rules.clone().iter().map(|rule| self.render_rule(rule, cx)).collect();
        v_flex()
            .size_full()
            .track_focus(&self.focus_handle)
            .bg(colors.editor_background)
            .child(
                h_flex()
                    .justify_between()
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(colors.border)
                    .child(
                        h_flex()
                            .gap_2()
                            .child(Label::new("Code Analysis").weight(FontWeight::BOLD))
                            .child(Label::new(format!("{} rules · {total} problems", self.rules.len())).size(LabelSize::Small).color(Color::Muted))
                            .when(self.loading, |row| row.child(Label::new("Updating…").size(LabelSize::Small).color(Color::Muted))),
                    )
                    .child(
                        IconButton::new("refresh", IconName::RotateCw)
                            .tooltip(Tooltip::text("Read the diagnostics again"))
                            .on_click(cx.listener(|this, _, _, cx| this.schedule_refresh(Duration::ZERO, cx))),
                    ),
            )
            .when_some(self.message.clone(), |col, message| col.child(div().px_3().py_1().child(Label::new(message).size(LabelSize::Small).color(Color::Accent))))
            .child(
                div()
                    .id("code-analysis-rules")
                    .flex_1()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .when(rules.is_empty() && !self.loading, |list| {
                        list.child(div().p_4().child(Label::new("No errors or warnings in the C# files.").color(Color::Muted)))
                    })
                    .children(rules),
            )
    }
}

impl Focusable for CodeAnalysis {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<ItemEvent> for CodeAnalysis {}

impl Item for CodeAnalysis {
    type Event = ItemEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        "Code Analysis".into()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Warning))
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}
