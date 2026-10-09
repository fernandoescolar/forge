//! Agents helping to set Forge up: they look settings up in the Settings tab's registry
//! (`forge_settings`), key bindings in the keymap (`forge_keybindings`) and how things work
//! in Forge's guide (`forge_guide`), and propose changes (`change_settings`,
//! `change_keybinding`) that the user applies or discards from a card in the thread.
//!
//! Agents can't change their own setup: `agents.json` (which agents run, with which
//! commands, their permissions and MCP servers) is the user's to change.

use gpui::{App, actions};
use serde_json::Value;
use forge_ui::settings_registry::{SettingKind, SettingRow, SettingsFile, SettingsPage, rows, value_at};

actions!(forge_agent, [
    /// Opens a thread with an agent that helps you set Forge up.
    ConfigureForge,
]);

/// What a "set Forge up" thread starts with.
pub const CONFIGURE_PROMPT: &str = "Help me set up Forge. Ask me (with `ask_user`) what I'd like to change or set up, if I haven't said yet. \
Look settings up with `forge_settings`, key bindings with `forge_keybindings`, and how features work with `forge_guide`; then propose \
the changes with `change_settings` and `change_keybinding` for me to apply, explaining briefly what each one does.";

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut workspace::Workspace, _, _| {
        workspace.register_action(|_, _: &ConfigureForge, window, cx| {
            window.dispatch_action(Box::new(forge_ui::AskAgent { prompt: CONFIGURE_PROMPT.into() }), cx);
        });
    })
    .detach();
}

/// Settings listed by a search at most.
const MAX_SETTINGS: usize = 30;
/// Key bindings and actions listed at most.
const MAX_BINDINGS: usize = 40;
/// Forge's guide, as it ships with Forge.
pub const GUIDE: &str = include_str!("../../../docs/GUIDE.md");

/// The file agents may not change (see the module's docs).
fn is_agents_own(file: &SettingsFile) -> bool {
    *file == SettingsFile::Config("agents.json".into())
}

/// How a setting is described to an agent: its type, choices or range.
fn kind_text(kind: &SettingKind) -> String {
    let range = |min: &Option<f64>, max: &Option<f64>| match (min, max) {
        (Some(a), Some(b)) => format!(" from {a} to {b}"),
        (Some(a), None) => format!(" from {a}"),
        (None, Some(b)) => format!(" up to {b}"),
        (None, None) => String::new(),
    };
    match kind {
        SettingKind::Bool => "true or false".into(),
        SettingKind::Integer { min, max } => format!("an integer{}", range(min, max)),
        SettingKind::Number { min, max } => format!("a number{}", range(min, max)),
        SettingKind::Text => "text".into(),
        SettingKind::Choice(choices) => format!("one of {}", choices.iter().map(|(v, _)| v.to_string()).collect::<Vec<_>>().join(", ")),
        SettingKind::Json => "JSON (a list, a map or an object)".into(),
    }
}

fn shown(value: Option<&Value>) -> String {
    value.map(Value::to_string).unwrap_or_else(|| "(not set)".into())
}

/// The settings whose key, title, description, group or page match every word of `query`
/// (all of a page's with `page`), each with its type, default and current value.
pub fn search_settings(pages: &[SettingsPage], query: &str, page: Option<&str>, current: &dyn Fn(&SettingsFile) -> Value) -> String {
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    let mut found = Vec::new();
    for p in pages.iter().filter(|p| page.is_none_or(|id| p.id == id)) {
        let values = current(&p.file);
        for row in rows(p) {
            let haystack = format!("{} {} {} {} {}", row.key(), row.title, row.description, row.group.clone().unwrap_or_default(), p.title).to_lowercase();
            if words.iter().all(|w| haystack.contains(w)) {
                found.push(describe_setting(p, &row, &values));
            }
        }
    }
    if found.is_empty() {
        let pages: Vec<String> = pages.iter().map(|p| format!("{} ({})", p.title, p.id)).collect();
        return format!("No setting matches \"{query}\". The pages are: {}.", pages.join(", "));
    }
    let total = found.len();
    found.truncate(MAX_SETTINGS);
    let more = if total > MAX_SETTINGS { format!("\n… and {} more: narrow the search, or give `page`.", total - MAX_SETTINGS) } else { String::new() };
    format!("{}{more}", found.join("\n\n"))
}

fn describe_setting(page: &SettingsPage, row: &SettingRow, values: &Value) -> String {
    let default = value_at(&page.defaults, &row.path);
    let current = value_at(values, &row.path);
    let note = if is_agents_own(&page.file) { "\n  (agents' own setup: only the user changes it)" } else { "" };
    format!(
        "- `{}` on page {} ({}, in {}): {}\n  {}\n  Type: {}. Default: {}. Now: {}.{note}",
        row.key(),
        page.title,
        page.id,
        page.file.display_name(),
        row.title,
        if row.description.is_empty() { "(no description)" } else { row.description.as_str() },
        kind_text(&row.kind),
        shown(default),
        shown(current.or(default)),
    )
}

/// One setting an agent wants to change.
#[derive(Clone, Debug, PartialEq)]
pub struct SettingChange {
    pub page: String,
    pub file: SettingsFile,
    pub path: Vec<String>,
    pub title: String,
    pub before: Option<Value>,
    /// `None` puts the default back.
    pub after: Option<Value>,
}

/// One key binding an agent wants to add to `keymap.json`.
#[derive(Clone, Debug, PartialEq)]
pub struct BindingChange {
    pub keys: String,
    /// `None` disables what the keys do there.
    pub action: Option<String>,
    pub args: Option<Value>,
    pub context: Option<String>,
    /// What the keys do there now.
    pub before: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ConfigChange {
    Setting(SettingChange),
    Binding(BindingChange),
}

/// Whether `value` fits a setting of `kind` (`Err` says why not).
fn check_value(kind: &SettingKind, value: &Value) -> Result<(), String> {
    let in_range = |n: f64, min: &Option<f64>, max: &Option<f64>| min.is_none_or(|m| n >= m) && max.is_none_or(|m| n <= m);
    let ok = match kind {
        SettingKind::Bool => value.is_boolean(),
        SettingKind::Integer { min, max } => value.as_i64().is_some_and(|n| in_range(n as f64, min, max)),
        SettingKind::Number { min, max } => value.as_f64().is_some_and(|n| in_range(n, min, max)),
        SettingKind::Text => value.is_string(),
        SettingKind::Choice(choices) => choices.iter().any(|(v, _)| v == value),
        SettingKind::Json => true,
    };
    if ok { Ok(()) } else { Err(format!("must be {}", kind_text(kind))) }
}

/// The setting changes an agent asked for (`[{ key, value, page? }]`, `value: null` for the
/// default), checked against the registry: each key must name one setting, and the value
/// fit it.
pub fn plan_setting_changes(pages: &[SettingsPage], changes: &Value, current: &dyn Fn(&SettingsFile) -> Value) -> Result<Vec<SettingChange>, String> {
    let list = changes.as_array().filter(|l| !l.is_empty()).ok_or("`changes` must list at least one `{ key, value }`.")?;
    let mut planned = Vec::new();
    for change in list {
        let key = change.get("key").and_then(Value::as_str).ok_or("Each change needs its `key`, as `forge_settings` shows it.")?;
        let page_id = change.get("page").and_then(Value::as_str);
        let matches: Vec<(&SettingsPage, SettingRow)> = pages
            .iter()
            .filter(|p| page_id.is_none_or(|id| p.id == id))
            .flat_map(|p| rows(p).into_iter().filter(|r| r.key() == key).map(move |r| (p, r)))
            .collect();
        let (page, row) = match matches.as_slice() {
            [] => return Err(format!("No setting is called `{key}`{}: find it with `forge_settings`.", page_id.map(|p| format!(" on page {p}")).unwrap_or_default())),
            [one] => one.clone(),
            several => return Err(format!("`{key}` is on several pages ({}): say which with `page`.", several.iter().map(|(p, _)| p.id.as_str()).collect::<Vec<_>>().join(", "))),
        };
        if is_agents_own(&page.file) {
            return Err(format!("`{key}` is part of agents' own setup (agents.json: which agents run, their permissions and MCP servers): only the user changes it, in Settings › Agents."));
        }
        let after = change.get("value").filter(|v| !v.is_null()).cloned();
        if let Some(value) = &after {
            check_value(&row.kind, value).map_err(|why| format!("`{key}` {why}; {value} doesn't."))?;
        }
        let before = value_at(&current(&page.file), &row.path).cloned();
        planned.push(SettingChange { page: page.title.clone(), file: page.file.clone(), path: row.path.clone(), title: row.title.clone(), before, after });
    }
    Ok(planned)
}

/// `keys` written the way the keymap writes them (`cmd-k cmd-s`), or why they aren't keys.
pub fn normalize_keys(keys: &str) -> Result<String, String> {
    let parts: Vec<String> = keys
        .split_whitespace()
        .map(|part| gpui::Keystroke::parse(part).map(|k| k.unparse()).map_err(|e| format!("`{part}` isn't a key: {e}")))
        .collect::<Result<_, _>>()?;
    if parts.is_empty() {
        return Err("`keys` is empty.".into());
    }
    Ok(parts.join(" "))
}

/// The bindings in force: for `keys` (what they do, in which context), or for actions whose
/// name contains `action` (with their keys; unbound ones are listed too).
pub fn describe_bindings(keys: Option<&str>, action: Option<&str>, cx: &App) -> Result<String, String> {
    let keymap = cx.key_bindings();
    let keymap = keymap.borrow();
    let line = |b: &gpui::KeyBinding| {
        let keys: Vec<String> = b.keystrokes().iter().map(|k| k.inner().unparse()).collect();
        let context = b.predicate().map(|p| format!(" (when {p})")).unwrap_or_default();
        let input = b.action_input().map(|i| format!(" {i}")).unwrap_or_default();
        format!("- {} → {}{input}{context}", keys.join(" "), b.action().name())
    };
    if let Some(keys) = keys {
        let keys = normalize_keys(keys)?;
        let found: Vec<String> = keymap.bindings().rev().filter(|b| b.keystrokes().iter().map(|k| k.inner().unparse()).collect::<Vec<_>>().join(" ") == keys).take(MAX_BINDINGS).map(line).collect();
        return Ok(if found.is_empty() { format!("{keys} does nothing in any context.") } else { format!("{keys} (the first one listed wins where several apply):\n{}", found.join("\n")) });
    }
    let wanted = action.unwrap_or_default().to_lowercase();
    if wanted.is_empty() {
        return Err("Give `keys` (`cmd-shift-p`) or an `action` to look for (`toggle_minimap`, `terminal`).".into());
    }
    let bound: Vec<String> = keymap.bindings().rev().filter(|b| b.action().name().to_lowercase().contains(&wanted)).take(MAX_BINDINGS).map(line).collect();
    let mut unbound: Vec<&str> = cx.all_action_names().iter().copied().filter(|n| n.to_lowercase().contains(&wanted)).filter(|n| !keymap.bindings().any(|b| b.action().name() == *n)).collect();
    unbound.sort();
    unbound.truncate(MAX_BINDINGS);
    let mut text = if bound.is_empty() { format!("No key is bound to an action matching \"{wanted}\".") } else { format!("Bound:\n{}", bound.join("\n")) };
    if !unbound.is_empty() {
        text.push_str(&format!("\nWithout keys: {}", unbound.join(", ")));
    }
    Ok(text)
}

/// The binding an agent asked for (`{ keys, action, args?, context? }`, `action: null` to
/// make the keys do nothing there), checked: real keys, a known action.
pub fn plan_binding_change(args: &Value, cx: &App) -> Result<BindingChange, String> {
    let keys = normalize_keys(args.get("keys").and_then(Value::as_str).ok_or("`keys` is missing (`cmd-shift-t`, or `cmd-k cmd-t` for a sequence).")?)?;
    let action = match args.get("action") {
        Some(Value::String(name)) => {
            if !cx.all_action_names().contains(&name.as_str()) {
                return Err(format!("There is no action `{name}`: find it with `forge_keybindings` and `action`."));
            }
            Some(name.clone())
        }
        Some(Value::Null) => None,
        _ => return Err("`action` is missing (an action's name, or null to make the keys do nothing there).".into()),
    };
    let context = args.get("context").and_then(Value::as_str).map(str::trim).filter(|c| !c.is_empty()).map(str::to_string);
    let before = describe_bindings(Some(&keys), None, cx).ok().filter(|t| !t.ends_with("does nothing in any context.")).and_then(|t| t.lines().nth(1).map(|l| l.trim_start_matches("- ").to_string()));
    Ok(BindingChange { keys, action, args: args.get("args").filter(|a| !a.is_null()).cloned(), context, before })
}

/// `keymap.json`'s text with `change` added as an entry of its own, comments kept.
pub fn add_binding(text: &str, change: &BindingChange) -> Result<String, String> {
    let text = if text.trim().is_empty() { "[\n]\n".to_string() } else { text.to_string() };
    let existing: Value = serde_json_lenient::from_str(&text).map_err(|e| format!("keymap.json doesn't parse ({e}): fix it first."))?;
    let count = existing.as_array().ok_or("keymap.json isn't a list of entries.")?.len();
    let action = match (&change.action, &change.args) {
        (None, _) => Value::Null,
        (Some(name), None) => Value::String(name.clone()),
        (Some(name), Some(args)) => serde_json::json!([name, args]),
    };
    let mut entry = serde_json::Map::new();
    if let Some(context) = &change.context {
        entry.insert("context".into(), Value::String(context.clone()));
    }
    entry.insert("bindings".into(), serde_json::json!({ change.keys.clone(): action }));
    let entry = serde_json::to_string(&Value::Object(entry)).map_err(|e| e.to_string())?;
    let end = text.rfind(']').ok_or("keymap.json isn't a list of entries.")?;
    let (head, tail) = text.split_at(end);
    let head = head.trim_end();
    // A comma after the last entry, unless it already ends with one (JSONC allows it).
    let comma = if count > 0 && !head.ends_with(',') { "," } else { "" };
    Ok(format!("{head}{comma}\n  {entry}\n{tail}"))
}

/// "the tab size and 2 key bindings": what a proposal changes, in a few words.
pub fn summary(changes: &[ConfigChange]) -> String {
    let settings: Vec<&str> = changes.iter().filter_map(|c| if let ConfigChange::Setting(s) = c { Some(s.title.as_str()) } else { None }).collect();
    let bindings = changes.iter().filter(|c| matches!(c, ConfigChange::Binding(_))).count();
    let mut parts = Vec::new();
    match settings.as_slice() {
        [] => {}
        [one] => parts.push(format!("the setting “{one}”")),
        many => parts.push(format!("{} settings", many.len())),
    }
    match bindings {
        0 => {}
        1 => parts.push("a key binding".into()),
        n => parts.push(format!("{n} key bindings")),
    }
    parts.join(" and ")
}

/// Applies the changes: settings through the Settings tab's registry (the file keeps its
/// comments, and Forge picks them up at once), key bindings added to `keymap` (Forge reloads
/// it when it changes). Says what was done, for the agent.
pub fn apply(changes: &[ConfigChange], keymap: &std::path::Path, cx: &mut App) -> Result<String, String> {
    let registry = forge_ui::settings_registry::SettingsRegistry::global(cx);
    let mut done = Vec::new();
    for change in changes {
        match change {
            ConfigChange::Setting(s) => {
                registry
                    .update(cx, |r, cx| r.set(&s.file, &s.path, s.after.clone(), cx))
                    .map_err(|e| format!("Couldn't write {}: {e:#}{}", s.file.display_name(), if done.is_empty() { String::new() } else { format!(" (applied before it: {})", done.join("; ")) }))?;
                done.push(format!("`{}` = {}", s.path.join("."), shown(s.after.as_ref()).replace("(not set)", "its default")));
            }
            ConfigChange::Binding(b) => {
                let text = std::fs::read_to_string(keymap).unwrap_or_default();
                let new_text = add_binding(&text, b)?;
                if let Some(dir) = keymap.parent() {
                    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
                }
                std::fs::write(keymap, new_text).map_err(|e| format!("Couldn't write keymap.json: {e}"))?;
                done.push(format!("{} → {}", b.keys, b.action.as_deref().unwrap_or("nothing")));
            }
        }
    }
    Ok(format!("The user applied the changes: {}. They take effect now.", done.join("; ")))
}

/// Forge's guide: its sections' titles, or the sections about `topic` (every word in them).
pub fn guide(topic: Option<&str>) -> String {
    let sections: Vec<&str> = GUIDE.split("\n## ").skip(1).collect();
    let title = |s: &str| s.lines().next().unwrap_or_default().trim().to_string();
    let Some(topic) = topic.map(str::trim).filter(|t| !t.is_empty()) else {
        return format!("Forge's guide has these sections (ask for one with `topic`):\n{}", sections.iter().map(|s| format!("- {}", title(s))).collect::<Vec<_>>().join("\n"));
    };
    let words: Vec<String> = topic.split_whitespace().map(str::to_lowercase).collect();
    let mut scored: Vec<(usize, &str)> = sections
        .iter()
        .map(|s| {
            let lower = s.to_lowercase();
            let in_title = title(s).to_lowercase();
            let score = words.iter().map(|w| lower.matches(w.as_str()).count() + if in_title.contains(w.as_str()) { 20 } else { 0 }).sum();
            (score, *s)
        })
        .filter(|(score, _)| *score > 0)
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0));
    if scored.is_empty() {
        return format!("The guide says nothing about \"{topic}\". Its sections: {}.", sections.iter().map(|s| title(s)).collect::<Vec<_>>().join(", "));
    }
    // The best section whole, and the next one if it is close.
    let mut text = String::new();
    for (i, (score, section)) in scored.iter().take(2).enumerate() {
        if i == 1 && *score * 2 < scored[0].0 {
            break;
        }
        let section: String = section.chars().take(12_000).collect();
        text.push_str(&format!("## {section}\n\n"));
    }
    text.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pages() -> Vec<SettingsPage> {
        let page = |id: &str, title: &str, file: SettingsFile, schema: Value, defaults: Value| SettingsPage { id: id.into(), title: title.into(), file, schema, keys: None, defaults, order: 0, actions: vec![] };
        vec![
            page(
                "editor",
                "Editor",
                SettingsFile::User,
                json!({ "properties": {
                    "tab_size": { "type": "integer", "minimum": 1, "maximum": 16, "description": "How many columns a tab occupies." },
                    "show_minimap": { "type": "boolean", "description": "Shows a minimap of the file." },
                    "cursor_shape": { "oneOf": [{ "const": "bar" }, { "const": "block" }] }
                } }),
                json!({ "tab_size": 4, "show_minimap": false }),
            ),
            page("agents", "Agents", SettingsFile::Config("agents.json".into()), json!({ "properties": { "review_writes": { "type": "boolean", "description": "Review every write." } } }), json!({ "review_writes": true })),
        ]
    }

    fn current(file: &SettingsFile) -> Value {
        match file {
            SettingsFile::User => json!({ "tab_size": 2 }),
            _ => json!({}),
        }
    }

    #[test]
    fn finds_settings_with_their_values() {
        let found = search_settings(&pages(), "tab", None, &current);
        assert!(found.starts_with("- `tab_size` on page Editor (editor, in settings.json): Tab size"), "{found}");
        assert!(found.contains("Type: an integer from 1 to 16. Default: 4. Now: 2."), "{found}");
        assert!(search_settings(&pages(), "minimap", None, &current).contains("Now: false."), "the default when unset");
        assert!(search_settings(&pages(), "review", None, &current).contains("only the user changes it"));
        assert!(search_settings(&pages(), "nothing like this", None, &current).starts_with("No setting matches"));
    }

    #[test]
    fn checks_the_changes_agents_propose() {
        let plan = plan_setting_changes(&pages(), &json!([{ "key": "tab_size", "value": 8 }, { "key": "cursor_shape", "value": "block" }, { "key": "show_minimap", "value": null }]), &current).unwrap();
        assert_eq!(plan[0], SettingChange { page: "Editor".into(), file: SettingsFile::User, path: vec!["tab_size".into()], title: "Tab size".into(), before: Some(json!(2)), after: Some(json!(8)) });
        assert_eq!(plan[2].after, None, "null puts the default back");
        let error = |changes: Value| plan_setting_changes(&pages(), &changes, &current).unwrap_err();
        assert!(error(json!([{ "key": "tab_size", "value": 40 }])).contains("must be an integer from 1 to 16"));
        assert!(error(json!([{ "key": "cursor_shape", "value": "underline" }])).contains("must be one of \"bar\", \"block\""));
        assert!(error(json!([{ "key": "no_such", "value": 1 }])).contains("No setting is called `no_such`"));
        assert!(error(json!([{ "key": "review_writes", "value": false }])).contains("only the user changes it"), "agents can't loosen their own setup");
        assert!(error(json!([])).contains("at least one"));
    }

    #[test]
    fn adds_bindings_keeping_the_file() {
        let change = BindingChange { keys: "cmd-k cmd-t".into(), action: Some("theme_selector::Toggle".into()), args: None, context: Some("Workspace".into()), before: None };
        let template = "// My bindings\n[\n]\n";
        let one = add_binding(template, &change).unwrap();
        assert_eq!(one, "// My bindings\n[\n  {\"context\":\"Workspace\",\"bindings\":{\"cmd-k cmd-t\":\"theme_selector::Toggle\"}}\n]\n");
        let off = BindingChange { keys: "cmd-w".into(), action: None, args: None, context: None, before: None };
        let two = add_binding(&one, &off).unwrap();
        assert!(two.contains("theme_selector::Toggle\"}},\n  {\"bindings\":{\"cmd-w\":null}}\n]"), "{two}");
        let with_args = BindingChange { keys: "ctrl-1".into(), action: Some("pane::ActivateItem".into()), args: Some(json!(0)), context: None, before: None };
        assert!(add_binding("[]", &with_args).unwrap().contains("[\"pane::ActivateItem\",0]"));
        assert!(add_binding("{ not a list", &off).is_err());
    }

    /// An applied key binding lands in the keymap file given (the user's, in Forge).
    #[gpui::test]
    fn applies_key_bindings_to_the_keymap(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let keymap = dir.path().join("keymap.json");
        std::fs::write(&keymap, "// mine\n[\n]\n").unwrap();
        let change = ConfigChange::Binding(BindingChange { keys: "ctrl-alt-m".into(), action: Some("editor::ToggleMinimap".into()), args: None, context: Some("Editor".into()), before: None });
        let said = cx.update(|cx| apply(&[change], &keymap, cx)).unwrap();
        assert_eq!(said, "The user applied the changes: ctrl-alt-m → editor::ToggleMinimap. They take effect now.");
        assert_eq!(std::fs::read_to_string(&keymap).unwrap(), "// mine\n[\n  {\"context\":\"Editor\",\"bindings\":{\"ctrl-alt-m\":\"editor::ToggleMinimap\"}}\n]\n");
    }

    #[test]
    fn finds_the_guide_sections_about_a_topic() {
        assert!(guide(None).contains("- Agents"));
        let agents = guide(Some("skills prompts"));
        assert!(agents.starts_with("## Agents"), "{}", &agents[..80.min(agents.len())]);
        assert!(guide(Some("zzzz-nothing")).starts_with("The guide says nothing"));
    }
}
