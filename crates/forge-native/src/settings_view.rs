//! The Settings tab (Forge › Settings, cmd-,): every setting in one place, searchable.
//!
//! Pages come from [`SettingsRegistry`]: the editor's settings (built here from the
//! settings schema, split into sections), Forge's own config files and extensions. Each
//! row edits its value in place; the files stay the source of truth and keep their
//! comments, and lists or maps are edited in the file itself.

use std::collections::HashMap;

use editor::{Editor, EditorEvent};
use forge_ui::settings_registry::{SettingKind, SettingRow, SettingsFile, SettingsPage, SettingsRegistry, rows, value_at};
use gpui::{
    App, AppContext as _, Context, ElementId, Entity, EventEmitter, FocusHandle, Focusable, InteractiveElement as _, IntoElement, ParentElement as _,
    Render, SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, TaskExt as _, WeakEntity, Window, div, prelude::FluentBuilder as _, px,
};
use serde_json::{Value, json};
use settings::SettingsStore;
use theme::{ActiveTheme as _, ThemeRegistry};
use ui::{
    Button, ButtonCommon as _, ButtonLike, ButtonSize, ButtonStyle, Clickable as _, Color, ContextMenu, Headline, HeadlineSize, Icon, IconButton, IconName,
    IconPosition, IconSize, Indicator, Label, LabelCommon as _, LabelSize, PopoverMenu, Switch, ToggleState, Tooltip, h_flex, v_flex,
};
use workspace::{Item, OpenOptions, Workspace, item::ItemEvent};

gpui::actions!(forge, [
    /// Opens the Settings tab.
    OpenSettings
]);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace
            .register_action(|ws, _: &OpenSettings, window, cx| open(ws, None, window, cx))
            .register_action(|ws, action: &forge_ui::OpenSettingsPage, window, cx| open(ws, Some(&action.page), window, cx))
            .register_action(|ws, _: &zed_actions::OpenSettings, window, cx| open(ws, None, window, cx));
    })
    .detach();
}

/// Opens (or focuses) the Settings tab, on `page` if given.
pub fn open(workspace: &mut Workspace, page: Option<&str>, window: &mut Window, cx: &mut Context<Workspace>) {
    register_core_pages(cx);
    let existing = workspace.items_of_type::<SettingsView>(cx).next();
    if let Some(existing) = existing {
        if let Some(page) = page {
            existing.update(cx, |view, cx| view.select_page(page.to_string(), cx));
        }
        workspace.activate_item(&existing, true, true, window, cx);
        return;
    }
    let view = cx.new(|cx| SettingsView::new(workspace.weak_handle(), window, cx));
    if let Some(page) = page {
        view.update(cx, |view, cx| view.select_page(page.to_string(), cx));
    }
    workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
}

/// Sections of `settings.json`: (id, title, top-level keys in order). Keys the schema
/// doesn't have are skipped; the rest of the schema, minus [`HIDDEN`], goes to Advanced.
const CORE_PAGES: &[(&str, &str, &[&str])] = &[
    ("appearance", "Appearance", &[
        "theme", "icon_theme", "ui_font_family", "ui_font_size", "ui_font_weight", "buffer_font_family", "buffer_font_size", "buffer_font_weight",
        "buffer_line_height", "cursor_shape", "cursor_blink", "current_line_highlight", "selection_highlight", "rounded_selection", "show_whitespaces",
        "indent_guides", "show_wrap_guides", "wrap_guides", "colorize_brackets", "unnecessary_code_fade", "minimum_contrast_for_highlights", "text_rendering_mode",
        "reduce_motion",
    ]),
    ("editor", "Editor", &[
        "tab_size", "hard_tabs", "soft_wrap", "preferred_line_length", "auto_indent", "auto_indent_on_paste", "format_on_save", "remove_trailing_whitespace_on_save",
        "ensure_final_newline_on_save", "line_ending", "use_autoclose", "use_auto_surround", "always_treat_brackets_as_autoclosed", "extend_comment_on_newline",
        "extend_list_on_newline", "indent_list_on_tab", "allow_rewrap", "jsx_tag_auto_close", "linked_edits", "use_on_type_format", "show_completions_on_input",
        "show_completion_documentation", "completions", "snippet_sort_order", "auto_signature_help", "show_signature_help_after_edits", "inlay_hints", "code_lens",
        "inline_code_actions", "hover_popover_enabled", "hover_popover_delay", "hover_popover_sticky", "hover_popover_hiding_delay", "relative_line_numbers",
        "gutter", "scrollbar", "minimap", "sticky_scroll", "toolbar", "scroll_beyond_last_line", "vertical_scroll_margin", "horizontal_scroll_margin",
        "scroll_sensitivity", "fast_scroll_sensitivity", "autoscroll_on_clicks", "mouse_wheel_zoom", "multi_cursor_modifier", "middle_click_paste",
        "drag_and_drop_selection", "double_click_in_multibuffer", "hide_mouse", "go_to_definition_fallback", "lsp_document_colors", "diff_view_style",
        "word_diff_enabled", "expand_excerpt_lines", "excerpt_context_lines", "search", "search_wrap", "seed_search_query_from_cursor", "use_smartcase_search",
    ]),
    ("workspace", "Workspace", &[
        "autosave", "restore_on_startup", "restore_on_file_reopen", "confirm_quit", "on_last_window_closed", "when_closing_with_no_tabs", "close_on_file_delete",
        "tabs", "tab_bar", "preview_tabs", "max_tabs", "status_bar", "title_bar", "window_title_format", "window_title_separator", "centered_layout",
        "active_pane_modifiers", "bottom_dock_layout", "resize_all_panels_in_dock", "close_panel_on_toggle", "pane_split_direction_horizontal",
        "pane_split_direction_vertical", "drop_target_size", "use_system_path_prompts", "use_system_prompts", "use_system_window_tabs", "focus_follows_mouse",
        "zoomed_padding", "command_palette", "file_finder", "base_keymap", "vim_mode", "helix_mode", "vim", "which_key",
    ]),
    ("files", "Files & Project", &[
        "project_panel", "outline_panel", "file_scan_exclusions", "file_scan_inclusions", "private_files", "hidden_files", "read_only_files", "file_types",
        "scan_symlinks", "file_scan_depth", "redact_private_values", "image_viewer", "markdown_preview",
    ]),
    ("languages", "Languages", &[
        "enable_language_server", "language_servers", "formatter", "prettier", "code_actions_on_format", "diagnostics", "diagnostics_max_severity",
        "lsp_highlight_debounce", "semantic_tokens", "document_folding_ranges", "document_symbols", "lsp_document_links", "lsp_results_location",
        "global_lsp_settings", "language_detection", "load_direnv", "languages", "lsp", "node",
    ]),
    ("terminal", "Terminal", &["terminal"]),
    ("git", "Git", &["git", "git_panel", "git_hosting_providers"]),
    ("debugger", "Debugging & Tasks", &["debugger", "dap", "debuggers", "tasks"]),
];

/// Settings of features Forge doesn't include (AI, collaboration, remote development,
/// updates) or that only make sense in code.
const HIDDEN: &[&str] = &[
    "edit_predictions", "show_edit_predictions", "edit_predictions_disabled_in", "agent", "agent_servers", "language_models", "agent_ui_font_family",
    "agent_ui_font_size", "agent_buffer_font_family", "agent_buffer_font_size", "disable_ai", "context_servers", "context_server_timeout", "collaboration_panel",
    "calls", "show_call_status_icon", "prevent_sharing_in_public_channels", "audio", "server_url", "credentials_url", "auto_update", "telemetry",
    "feature_flags", "dev", "nightly", "preview", "stable", "macos", "linux", "windows", "profiles", "ssh_connections", "wsl_connections",
    "dev_container_connections", "read_ssh_config", "use_podman", "dev_container_use_buildkit", "jupyter", "repl", "journal", "session", "instrumentation",
    "auto_install_extensions", "auto_update_extensions", "granted_extension_capabilities", "experimental.theme_overrides", "theme_overrides",
    "unstable.ui_density", "log", "proxy", "cli_default_open_behavior", "default_open_behavior", "on_new_window", "accessible_mode", "fullscreen_mode",
    "window_decorations", "modeline_lines", "call_hierarchy", "git_commit_buffer_font_size", "line_indicator_format", "ui_font_fallbacks",
    "ui_font_features", "buffer_font_fallbacks", "buffer_font_features", "command_aliases", "whitespace_map", "cursor_animation", "completion_menu_scrollbar",
    "completion_detail_alignment", "completion_menu_item_kind", "minimum_split_diff_width", "lsp_highlight_debounce", "go_to_definition_scroll_strategy",
];

/// Registers the `settings.json` pages, once (theme names are read when the tab first opens).
fn register_core_pages(cx: &mut App) {
    let registry = SettingsRegistry::global(cx);
    if registry.read(cx).page("editor").is_some() {
        return;
    }
    let themes = ThemeRegistry::global(cx);
    let mut theme_names: Vec<SharedString> = themes.list_names();
    theme_names.sort_by_key(|name| name.to_lowercase());
    let mut icon_theme_names: Vec<SharedString> = themes.list_icon_themes().into_iter().map(|t| t.name).collect();
    icon_theme_names.sort_by_key(|name| name.to_lowercase());
    let empty = Default::default();
    let params = settings::SettingsJsonSchemaParams {
        language_names: &[],
        font_names: &[],
        theme_names: &[],
        icon_theme_names: &[],
        lsp_adapter_names: &[],
        action_names: &[],
        action_documentation: &empty,
        deprecations: &empty,
        deprecation_messages: &empty,
    };
    let mut schema = SettingsStore::json_schema(&params);
    // Themes as choices: `theme` is "a name, or one per appearance"; Forge edits the latter.
    if let Some(properties) = schema.get_mut("properties").and_then(Value::as_object_mut) {
        properties.insert(
            "theme".into(),
            json!({
                "type": "object",
                "description": "The colour theme. Every palette in config/palettes is one.",
                "properties": {
                    "mode": { "enum": ["system", "light", "dark"], "enumDescriptions": ["Follow the system", "Light", "Dark"], "description": "Which of the two themes to use." },
                    "dark": { "enum": theme_names, "title": "Dark theme" },
                    "light": { "enum": theme_names, "title": "Light theme" },
                }
            }),
        );
        properties.insert("icon_theme".into(), json!({ "enum": icon_theme_names, "title": "File icons", "description": "The icons for files and folders." }));
    }
    let defaults = crate::default_settings_value().unwrap_or_else(|_| json!({}));
    let all_keys: Vec<String> = schema.get("properties").and_then(Value::as_object).map(|p| p.keys().cloned().collect()).unwrap_or_default();
    let listed: std::collections::HashSet<&str> = CORE_PAGES.iter().flat_map(|(_, _, keys)| keys.iter().copied()).collect();
    let advanced: Vec<String> = all_keys.into_iter().filter(|k| !listed.contains(k.as_str()) && !HIDDEN.contains(&k.as_str())).collect();
    let mut pages: Vec<SettingsPage> = CORE_PAGES
        .iter()
        .enumerate()
        .map(|(i, (id, title, keys))| SettingsPage {
            id: id.to_string(),
            title: title.to_string(),
            file: SettingsFile::User,
            schema: schema.clone(),
            keys: Some(keys.iter().map(|k| k.to_string()).collect()),
            defaults: defaults.clone(),
            order: i as i32,
            actions: vec![],
        })
        .collect();
    pages.push(SettingsPage { id: "advanced".into(), title: "Advanced".into(), file: SettingsFile::User, schema, keys: Some(advanced), defaults, order: 90, actions: vec![] });
    registry.update(cx, |registry, cx| {
        for page in pages {
            registry.register(page, cx);
        }
    });
}

struct Input {
    editor: Entity<Editor>,
    file: SettingsFile,
    path: Vec<String>,
    kind: SettingKind,
    _blur: Subscription,
}

pub struct SettingsView {
    workspace: WeakEntity<Workspace>,
    registry: Entity<SettingsRegistry>,
    focus_handle: FocusHandle,
    search: Entity<Editor>,
    page: Option<String>,
    rows: HashMap<String, Vec<SettingRow>>,
    values: HashMap<SettingsFile, Value>,
    inputs: HashMap<String, Input>,
    /// The input being typed in: reloads leave it alone.
    editing: Option<String>,
    /// Inputs must show the values just reloaded (done in render, which has the window).
    sync_inputs: bool,
    error: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl SettingsView {
    fn new(workspace: WeakEntity<Workspace>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let registry = SettingsRegistry::global(cx);
        let search = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Search settings", window, cx);
            editor
        });
        let subscriptions = vec![
            cx.subscribe(&search, |_, _, event: &EditorEvent, cx| {
                if let EditorEvent::BufferEdited = event {
                    cx.notify();
                }
            }),
            cx.observe(&registry, |this, _, cx| this.reload(cx)),
            // settings.json edited by hand (or by another window).
            cx.observe_global::<SettingsStore>(|this, cx| this.reload(cx)),
        ];
        let mut this = Self {
            workspace,
            registry,
            focus_handle: cx.focus_handle(),
            search,
            page: None,
            rows: HashMap::new(),
            values: HashMap::new(),
            inputs: HashMap::new(),
            editing: None,
            sync_inputs: false,
            error: None,
            _subscriptions: subscriptions,
        };
        this.reload(cx);
        this
    }

    fn select_page(&mut self, page: String, cx: &mut Context<Self>) {
        self.page = Some(page);
        cx.notify();
    }

    /// Re-reads the pages and their files; inputs not being edited show the new values.
    fn reload(&mut self, cx: &mut Context<Self>) {
        let pages = self.registry.read(cx).pages().to_vec();
        self.rows = pages.iter().map(|p| (p.id.clone(), rows(p))).collect();
        self.values.clear();
        for page in &pages {
            self.values.entry(page.file.clone()).or_insert_with(|| page.file.read());
        }
        self.sync_inputs = true;
        cx.notify();
    }

    fn user_value(&self, file: &SettingsFile, path: &[String]) -> Option<Value> {
        self.values.get(file).and_then(|v| value_at(v, path)).cloned()
    }

    /// What the setting is now: the file's value, else the default.
    fn current(&self, page: &SettingsPage, path: &[String]) -> Option<Value> {
        self.user_value(&page.file, path).or_else(|| value_at(&page.defaults, path).cloned())
    }

    fn set(&mut self, file: SettingsFile, path: Vec<String>, value: Option<Value>, cx: &mut Context<Self>) {
        // `"theme": "Name"` is shorthand for one theme in both appearances: expand it
        // before setting one of its parts.
        if file == SettingsFile::User && path.len() > 1 && path[0] == "theme" {
            if let Some(Value::String(name)) = self.user_value(&file, &path[..1]) {
                let expanded = json!({ "mode": "system", "light": name, "dark": name });
                self.registry.update(cx, |r, cx| r.set(&file, &path[..1], Some(expanded), cx)).ok();
            }
        }
        let result = self.registry.update(cx, |r, cx| r.set(&file, &path, value, cx));
        self.error = result.err().map(|e| format!("Couldn't save {}: {e}", file.display_name()));
        self.reload(cx);
    }

    fn commit_input(&mut self, key: &str, cx: &mut Context<Self>) {
        let Some(input) = self.inputs.get(key) else { return };
        let text = input.editor.read(cx).text(cx);
        let (file, path, kind) = (input.file.clone(), input.path.clone(), input.kind.clone());
        let text = text.trim();
        let value = if text.is_empty() {
            None
        } else {
            match parse_input(text, &kind) {
                Ok(value) => Some(value),
                Err(message) => {
                    self.error = Some(message);
                    cx.notify();
                    return;
                }
            }
        };
        if value == self.user_value(&file, &path) {
            return;
        }
        self.set(file, path, value, cx);
    }

    fn input(&mut self, key: String, page: &SettingsPage, row: &SettingRow, window: &mut Window, cx: &mut Context<Self>) -> Entity<Editor> {
        if let Some(input) = self.inputs.get(&key) {
            return input.editor.clone();
        }
        let text = self.current(page, &row.path).map(|v| value_text(&v)).unwrap_or_default();
        let editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_text(text, window, cx);
            editor
        });
        let blur_key = key.clone();
        let blur = cx.subscribe(&editor, move |this, _, event: &EditorEvent, cx| match event {
            EditorEvent::Focused => this.editing = Some(blur_key.clone()),
            EditorEvent::Blurred => {
                this.editing = None;
                this.commit_input(&blur_key, cx);
            }
            _ => {}
        });
        self.inputs.insert(key, Input { editor: editor.clone(), file: page.file.clone(), path: row.path.clone(), kind: row.kind.clone(), _blur: blur });
        editor
    }

    /// Opens the page's file, with the setting's value selected when it is there.
    fn open_in_file(&self, file: &SettingsFile, path: Vec<String>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else { return };
        let abs = file.path();
        if !abs.exists() {
            let _ = std::fs::write(&abs, if *file == SettingsFile::User { crate::config_files::USER_SETTINGS } else { "{\n}\n" });
        }
        let open = workspace.update(cx, |ws, cx| ws.open_abs_path(abs, OpenOptions::default(), window, cx));
        cx.spawn_in(window, async move |_, cx| {
            let item = open.await?;
            let Some(editor) = item.downcast::<Editor>() else { return anyhow::Ok(()) };
            editor.update_in(cx, |editor, window, cx| {
                let text = editor.text(cx);
                if let Some(range) = settings::find_value_range_in_json_text(&text, &path) {
                    let point = |offset: usize| {
                        let before = &text[..offset];
                        let row = before.matches('\n').count() as u32;
                        let column = (offset - before.rfind('\n').map_or(0, |i| i + 1)) as u32;
                        language::Point::new(row, column)
                    };
                    let (start, end) = (point(range.start), point(range.end));
                    editor.change_selections(editor::SelectionEffects::scroll(editor::scroll::Autoscroll::center()), window, cx, |s| s.select_ranges([start..end]));
                }
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn render_row(&mut self, page: &SettingsPage, row: &SettingRow, window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        let key = format!("{}:{}", page.id, row.key());
        let id = |suffix: &str| ElementId::Name(format!("{key}:{suffix}").into());
        let user = self.user_value(&page.file, &row.path);
        let current = self.current(page, &row.path).unwrap_or(Value::Null);
        let this = cx.weak_entity();
        let (file, path) = (page.file.clone(), row.path.clone());

        let control: gpui::AnyElement = match &row.kind {
            SettingKind::Bool => {
                let (this, file, path) = (this.clone(), file.clone(), path.clone());
                Switch::new(id("switch"), if current.as_bool() == Some(true) { ToggleState::Selected } else { ToggleState::Unselected })
                    .on_click(move |state, _, cx| {
                        let value = json!(*state == ToggleState::Selected);
                        let (file, path) = (file.clone(), path.clone());
                        this.update(cx, |this, cx| this.set(file, path, Some(value), cx)).ok();
                    })
                    .into_any_element()
            }
            SettingKind::Choice(options) => {
                let label = options.iter().find(|(v, _)| *v == current).map(|(_, l)| l.clone()).unwrap_or_else(|| value_text(&current));
                let (options, this, file, path, current) = (options.clone(), this.clone(), file.clone(), path.clone(), current.clone());
                PopoverMenu::new(id("choice"))
                    .trigger(
                        ButtonLike::new(id("choice-trigger")).style(ButtonStyle::Outlined).size(ButtonSize::Compact).child(
                            h_flex()
                                .gap_1()
                                .px_1()
                                .child(Label::new(label).size(LabelSize::Small))
                                .child(Icon::new(IconName::ChevronDown).size(IconSize::XSmall).color(Color::Muted)),
                        ),
                    )
                    .menu(move |window, cx| {
                        let (options, this, file, path, current) = (options.clone(), this.clone(), file.clone(), path.clone(), current.clone());
                        Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                            for (value, label) in options {
                                let (this, file, path, selected) = (this.clone(), file.clone(), path.clone(), value == current);
                                menu = menu.toggleable_entry(label, selected, IconPosition::Start, None, move |_, cx| {
                                    let (file, path, value) = (file.clone(), path.clone(), value.clone());
                                    this.update(cx, |this, cx| this.set(file, path, Some(value), cx)).ok();
                                });
                            }
                            menu
                        }))
                    })
                    .into_any_element()
            }
            SettingKind::Integer { .. } | SettingKind::Number { .. } | SettingKind::Text => {
                let editor = self.input(key.clone(), page, row, window, cx);
                let confirm_key = key.clone();
                div()
                    .w(px(220.))
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .border_1()
                    .border_color(cx.theme().colors().border)
                    .bg(cx.theme().colors().editor_background)
                    .on_action(cx.listener(move |this, _: &menu::Confirm, _, cx| this.commit_input(&confirm_key, cx)))
                    .child(editor)
                    .into_any_element()
            }
            SettingKind::Json => {
                let (file_for_click, path_for_click) = (file.clone(), path.clone());
                Button::new(id("edit"), format!("Edit in {}", file.display_name()))
                    .style(ButtonStyle::Subtle)
                    .size(ButtonSize::Compact)
                    .on_click(cx.listener(move |this, _, window, cx| this.open_in_file(&file_for_click, path_for_click.clone(), window, cx)))
                    .into_any_element()
            }
        };

        let reset = user.is_some().then(|| {
            let (file, path) = (file.clone(), path.clone());
            IconButton::new(id("reset"), IconName::RotateCcw)
                .icon_size(IconSize::XSmall)
                .icon_color(Color::Muted)
                .tooltip(Tooltip::text("Reset to the default"))
                .on_click(cx.listener(move |this, _, _, cx| this.set(file.clone(), path.clone(), None, cx)))
        });

        h_flex()
            .id(id("row"))
            .w_full()
            .px_6()
            .py_2()
            .gap_6()
            .items_start()
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(
                        h_flex()
                            .gap_1p5()
                            .child(Label::new(row.title.clone()))
                            .when(user.is_some(), |el| el.child(div().id(id("modified")).child(Indicator::dot().color(Color::Accent)).tooltip(Tooltip::text("Changed from the default")))),
                    )
                    .child(Label::new(row.key()).size(LabelSize::XSmall).color(Color::Muted).buffer_font(cx))
                    .when(!row.description.is_empty(), |el| el.child(Label::new(row.description.clone()).size(LabelSize::Small).color(Color::Muted))),
            )
            .child(h_flex().flex_none().gap_1().justify_end().child(control).child(div().w(px(20.)).children(reset)))
            .into_any_element()
    }

    fn render_rows(&mut self, page: &SettingsPage, rows: Vec<SettingRow>, window: &mut Window, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        let mut out = Vec::new();
        let mut group: Option<String> = None;
        for row in rows {
            if row.group != group {
                group = row.group.clone();
                if let Some(title) = &group {
                    out.push(
                        div()
                            .px_6()
                            .pt_4()
                            .pb_1()
                            .child(Label::new(title.clone()).size(LabelSize::Small).color(Color::Accent))
                            .into_any_element(),
                    );
                }
            }
            out.push(self.render_row(page, &row, window, cx));
        }
        out
    }
}

/// How a value shows in a text field.
fn value_text(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn parse_input(text: &str, kind: &SettingKind) -> Result<Value, String> {
    let check = |n: f64, min: &Option<f64>, max: &Option<f64>| -> Result<(), String> {
        match (min, max) {
            (Some(min), _) if n < *min => Err(format!("{text} is below the minimum, {min}")),
            (_, Some(max)) if n > *max => Err(format!("{text} is above the maximum, {max}")),
            _ => Ok(()),
        }
    };
    match kind {
        SettingKind::Integer { min, max } => {
            let n: i64 = text.parse().map_err(|_| format!("{text} is not a whole number"))?;
            check(n as f64, min, max)?;
            Ok(json!(n))
        }
        SettingKind::Number { min, max } => {
            let n: f64 = text.parse().map_err(|_| format!("{text} is not a number"))?;
            check(n, min, max)?;
            Ok(json!(n))
        }
        _ => Ok(json!(text)),
    }
}

impl Render for SettingsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors().clone();
        let pages = self.registry.read(cx).pages().to_vec();
        if std::mem::take(&mut self.sync_inputs) {
            let inputs: Vec<(String, Entity<Editor>, Vec<String>)> = self.inputs.iter().map(|(k, i)| (k.clone(), i.editor.clone(), i.path.clone())).collect();
            for (key, editor, path) in inputs {
                let Some(page) = pages.iter().find(|p| key.starts_with(&format!("{}:", p.id))) else { continue };
                if self.editing.as_ref() == Some(&key) {
                    continue;
                }
                let text = self.current(page, &path).map(|v| value_text(&v)).unwrap_or_default();
                if editor.read(cx).text(cx) != text {
                    editor.update(cx, |editor, cx| editor.set_text(text, window, cx));
                }
            }
        }
        let query = self.search.read(cx).text(cx).trim().to_lowercase();
        let selected = self.page.clone().filter(|id| pages.iter().any(|p| &p.id == id)).or_else(|| pages.first().map(|p| p.id.clone()));

        let nav = v_flex()
            .id("settings-nav")
            .w(px(220.))
            .h_full()
            .flex_none()
            .p_2()
            .gap_0p5()
            .border_r_1()
            .border_color(colors.border)
            .overflow_y_scroll()
            .child(div().mb_2().px_2().py_1().rounded_sm().border_1().border_color(colors.border).bg(colors.editor_background).child(self.search.clone()))
            .children(pages.iter().map(|page| {
                let is_selected = query.is_empty() && selected.as_deref() == Some(page.id.as_str());
                let id = page.id.clone();
                h_flex()
                    .id(ElementId::Name(format!("settings-page-{}", page.id).into()))
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .cursor_pointer()
                    .when(is_selected, |el| el.bg(colors.element_selected))
                    .hover(|el| el.bg(colors.element_hover))
                    .child(Label::new(page.title.clone()).size(LabelSize::Small).color(if is_selected { Color::Default } else { Color::Muted }))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.search.update(cx, |s, cx| s.set_text("", window, cx));
                        this.select_page(id.clone(), cx);
                    }))
            }));

        let mut body: Vec<gpui::AnyElement> = Vec::new();
        if query.is_empty() {
            if let Some(page) = pages.iter().find(|p| Some(&p.id) == selected.as_ref()) {
                let file = page.file.clone();
                let actions = page.actions.iter().enumerate().map(|(i, (label, action))| {
                    let action = action.clone();
                    Button::new(("settings-page-action", i), label.clone()).style(ButtonStyle::Outlined).size(ButtonSize::Compact).on_click(move |_, window, cx| {
                        match cx.build_action(&action, None) {
                            Ok(action) => window.dispatch_action(action, cx),
                            Err(e) => log::error!("settings page action {action}: {e}"),
                        }
                    })
                });
                body.push(
                    h_flex()
                        .px_6()
                        .pt_5()
                        .pb_2()
                        .gap_2()
                        .child(Headline::new(page.title.clone()).size(HeadlineSize::Small))
                        .child(div().flex_1())
                        .children(actions)
                        .child(
                            Button::new("settings-open-file", format!("Open {}", file.display_name()))
                                .style(ButtonStyle::Subtle)
                                .size(ButtonSize::Compact)
                                .on_click(cx.listener(move |this, _, window, cx| this.open_in_file(&file, vec![], window, cx))),
                        )
                        .into_any_element(),
                );
                let rows = self.rows.get(&page.id).cloned().unwrap_or_default();
                body.extend(self.render_rows(page, rows, window, cx));
            }
        } else {
            for page in &pages {
                let rows: Vec<SettingRow> = self
                    .rows
                    .get(&page.id)
                    .into_iter()
                    .flatten()
                    .filter(|r| [r.title.to_lowercase(), r.key().to_lowercase(), r.description.to_lowercase()].iter().any(|t| t.contains(&query)))
                    .cloned()
                    .collect();
                if rows.is_empty() {
                    continue;
                }
                body.push(div().px_6().pt_5().pb_1().child(Headline::new(page.title.clone()).size(HeadlineSize::XSmall)).into_any_element());
                body.extend(self.render_rows(page, rows, window, cx));
            }
            if body.is_empty() {
                body.push(div().p_6().child(Label::new("No settings match.").color(Color::Muted)).into_any_element());
            }
        }

        h_flex()
            .key_context("ForgeSettings")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(colors.editor_background)
            .child(nav)
            .child(
                v_flex()
                    .flex_1()
                    .h_full()
                    .min_w_0()
                    .children(self.error.clone().map(|error| {
                        h_flex()
                            .px_6()
                            .py_1()
                            .gap_2()
                            .bg(colors.surface_background)
                            .child(Icon::new(IconName::Warning).size(IconSize::Small).color(Color::Warning))
                            .child(Label::new(error).size(LabelSize::Small))
                    }))
                    .child(v_flex().id("settings-rows").flex_1().min_h_0().pb_8().overflow_y_scroll().children(body)),
            )
    }
}

impl Focusable for SettingsView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<ItemEvent> for SettingsView {}

impl Item for SettingsView {
    type Event = ItemEvent;

    fn tab_content_text(&self, _: usize, _: &App) -> SharedString {
        "Settings".into()
    }

    fn tab_icon(&self, _: &Window, _: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Settings))
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, VisualTestContext};

    /// The tab opens once, lists the editor's settings by section with the right controls,
    /// and hides features Forge doesn't have.
    #[gpui::test]
    async fn opens_with_every_section(cx: &mut TestAppContext) {
        let params = cx.update(workspace::AppState::test);
        cx.update(|cx| {
            theme_settings::init(::theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            init(cx);
        });
        let project = project::Project::test(params.fs.clone(), [], cx).await;
        let window = cx.add_window(|window, cx| workspace::MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window.read_with(cx, |mw, _| mw.workspace().clone()).unwrap();
        let cx = &mut VisualTestContext::from_window(window.into(), cx);

        for _ in 0..2 {
            workspace.update_in(cx, |ws, window, cx| open(ws, Some("editor"), window, cx));
            cx.run_until_parked();
        }
        assert_eq!(workspace.read_with(cx, |ws, cx| ws.items_of_type::<SettingsView>(cx).count()), 1, "one Settings tab");

        let registry = cx.update(|_, cx| SettingsRegistry::global(cx));
        let titles: Vec<String> = registry.read_with(cx, |r, _| r.pages().iter().map(|p| p.title.clone()).collect());
        for title in ["Appearance", "Editor", "Workspace", "Terminal", "Git", "Advanced"] {
            assert!(titles.contains(&title.to_string()), "{title} in {titles:?}");
        }
        let row = |page: &str, key: &str, cx: &mut VisualTestContext| {
            let page = registry.read_with(cx, |r, _| r.page(page).cloned().unwrap());
            rows(&page).into_iter().find(|r| r.key() == key).unwrap_or_else(|| panic!("no {key}"))
        };
        assert!(matches!(row("editor", "tab_size", cx).kind, SettingKind::Integer { min: Some(_), .. }));
        assert!(matches!(row("editor", "format_on_save", cx).kind, SettingKind::Choice(_) | SettingKind::Bool));
        assert_eq!(row("terminal", "terminal.blinking", cx).group.as_deref(), Some("Terminal"));
        let SettingKind::Choice(themes) = row("appearance", "theme.dark", cx).kind else { panic!("themes are a choice") };
        assert!(!themes.is_empty());
        let advanced = registry.read_with(cx, |r, _| r.page("advanced").cloned().unwrap());
        assert!(!rows(&advanced).iter().any(|r| r.path[0] == "agent" || r.path[0] == "edit_predictions"), "AI settings are hidden");
        assert!(!rows(&advanced).iter().any(|r| r.description.contains("Zed")), "descriptions name Forge");
    }
}
