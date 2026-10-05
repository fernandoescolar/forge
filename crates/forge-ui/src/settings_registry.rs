//! Every setting Forge knows about, for the Settings tab.
//!
//! A [`SettingsPage`] is a JSON schema plus the file its values live in. Forge registers
//! pages built from the editor's settings schema (`settings.json`); Forge's crates register
//! their own files (`dotnet.json`, `agents.json`) from their config types' schemas; and
//! extensions declare theirs in `package.json` (`forge.settings`, stored in
//! `extensions.json`). The Settings tab turns any page into rows with [`rows`], and every
//! change goes through [`SettingsRegistry::set`], which edits the file in place (comments
//! and formatting are kept) and emits [`SettingChanged`].

use gpui::{App, AppContext as _, Context, Entity, EventEmitter, Global};
use serde_json::{Map, Value};
use std::path::PathBuf;

/// Where a page's values are stored.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum SettingsFile {
    /// The user's `settings.json`.
    User,
    /// A JSON file in Forge's config folder, by name (`dotnet.json`).
    Config(String),
}

impl SettingsFile {
    pub fn path(&self) -> PathBuf {
        match self {
            SettingsFile::User => paths::settings_file().clone(),
            SettingsFile::Config(name) => paths::config_dir().join(name),
        }
    }

    pub fn display_name(&self) -> String {
        match self {
            SettingsFile::User => "settings.json".into(),
            SettingsFile::Config(name) => name.clone(),
        }
    }

    /// The file's values (an empty object when it is missing or invalid).
    pub fn read(&self) -> Value {
        std::fs::read_to_string(self.path())
            .ok()
            .and_then(|text| serde_json_lenient::from_str::<Value>(&text).ok())
            .filter(Value::is_object)
            .unwrap_or_else(|| Value::Object(Map::new()))
    }

    /// Sets (or with `None`, removes) the value at `path`, keeping the rest of the text.
    pub fn write(&self, path: &[String], value: Option<&Value>) -> anyhow::Result<()> {
        let file = self.path();
        let text = std::fs::read_to_string(&file).unwrap_or_default();
        let new_text = edit_json_text(&text, path, value);
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&file, new_text)?;
        Ok(())
    }
}

/// `text` with the value at `path` replaced, added or (with `None`) removed.
pub fn edit_json_text(text: &str, path: &[String], value: Option<&Value>) -> String {
    let text = if text.trim().is_empty() { "{\n}\n".to_string() } else { text.to_string() };
    let tab_size = settings::infer_json_indent_size(&text).max(2);
    let (range, replacement) = settings::replace_value_in_json_text(&text, path, tab_size, value, None);
    let mut out = text;
    out.replace_range(range, &replacement);
    out
}

/// One section of the Settings tab.
#[derive(Clone, Debug)]
pub struct SettingsPage {
    /// Unique id (`editor`, `dotnet`, `ext:my-extension`).
    pub id: String,
    pub title: String,
    pub file: SettingsFile,
    /// A JSON schema whose `properties` are the page's settings (`$defs` are resolved).
    pub schema: Value,
    /// When set, only these top-level properties, in this order.
    pub keys: Option<Vec<String>>,
    /// Values used when the file doesn't set one.
    pub defaults: Value,
    /// Where the page goes in the list: Forge's pages use 0–99, extensions 100+.
    pub order: i32,
    /// Buttons in the page's header: (label, action name), e.g. a dedicated editor.
    pub actions: Vec<(String, String)>,
}

/// What kind of control edits a setting.
#[derive(Clone, Debug, PartialEq)]
pub enum SettingKind {
    Bool,
    Integer { min: Option<f64>, max: Option<f64> },
    Number { min: Option<f64>, max: Option<f64> },
    Text,
    /// A fixed set of values, each with a label.
    Choice(Vec<(Value, String)>),
    /// Lists, maps and anything else: edited in the file itself.
    Json,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SettingRow {
    pub path: Vec<String>,
    pub title: String,
    pub description: String,
    pub kind: SettingKind,
    /// The object the setting belongs to ("Terminal › Toolbar"), when nested.
    pub group: Option<String>,
}

impl SettingRow {
    pub fn key(&self) -> String {
        self.path.join(".")
    }
}

pub fn value_at<'a>(value: &'a Value, path: &[String]) -> Option<&'a Value> {
    path.iter().try_fold(value, |v, key| v.get(key)).filter(|v| !v.is_null())
}

/// Nested objects are shown this many levels deep; deeper ones are edited in the file.
const MAX_DEPTH: usize = 3;

/// The page's settings, in schema order.
pub fn rows(page: &SettingsPage) -> Vec<SettingRow> {
    let root = &page.schema;
    let Some(properties) = resolve(root, root).get("properties").and_then(Value::as_object) else { return vec![] };
    let keys: Vec<&String> = match &page.keys {
        Some(keys) => keys.iter().filter(|k| properties.contains_key(*k)).collect(),
        None => properties.keys().collect(),
    };
    let mut out = Vec::new();
    for key in keys {
        collect(root, &properties[key], vec![key.clone()], None, &mut out);
    }
    out
}

fn collect(root: &Value, schema: &Value, path: Vec<String>, group: Option<String>, out: &mut Vec<SettingRow>) {
    let resolved = resolve(root, schema);
    let title = schema.get("title").or_else(|| resolved.get("title")).and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| humanize(path.last().unwrap()));
    let description = clean_description(schema.get("description").or_else(|| resolved.get("description")).and_then(Value::as_str).unwrap_or(""));
    if let Some(properties) = resolved.get("properties").and_then(Value::as_object).filter(|p| !p.is_empty() && path.len() < MAX_DEPTH) {
        let group = Some(match &group {
            Some(parent) => format!("{parent} › {title}"),
            None => title,
        });
        for (key, child) in properties {
            let mut child_path = path.clone();
            child_path.push(key.clone());
            collect(root, child, child_path, group.clone(), out);
        }
        return;
    }
    out.push(SettingRow { path, title, description, kind: kind(root, resolved), group });
}

/// Follows `$ref`s and drops the `null` alternative of optional values.
fn resolve<'a>(root: &'a Value, schema: &'a Value) -> &'a Value {
    let mut schema = schema;
    for _ in 0..16 {
        if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
            let Some(target) = reference.strip_prefix("#/").and_then(|p| root.pointer(&format!("/{p}"))) else { break };
            schema = target;
            continue;
        }
        let alternatives = schema.get("anyOf").or_else(|| schema.get("oneOf")).and_then(Value::as_array);
        if let Some(alternatives) = alternatives {
            let non_null: Vec<&Value> = alternatives.iter().filter(|a| a.get("type").and_then(Value::as_str) != Some("null")).collect();
            if non_null.len() == 1 && non_null.len() < alternatives.len() {
                schema = non_null[0];
                continue;
            }
        }
        break;
    }
    schema
}

fn kind(root: &Value, schema: &Value) -> SettingKind {
    if let Some(options) = choices(root, schema) {
        return SettingKind::Choice(options);
    }
    let types: Vec<&str> = match schema.get("type") {
        Some(Value::String(t)) => vec![t.as_str()],
        Some(Value::Array(ts)) => ts.iter().filter_map(Value::as_str).filter(|t| *t != "null").collect(),
        _ => vec![],
    };
    let bound = |key: &str| schema.get(key).and_then(Value::as_f64);
    match types.as_slice() {
        ["boolean"] => SettingKind::Bool,
        ["integer"] => SettingKind::Integer { min: bound("minimum"), max: bound("maximum") },
        ["number"] => SettingKind::Number { min: bound("minimum"), max: bound("maximum") },
        ["string"] => SettingKind::Text,
        _ => SettingKind::Json,
    }
}

/// `enum: [...]`, or alternatives that are each a single constant (with a description).
fn choices(root: &Value, schema: &Value) -> Option<Vec<(Value, String)>> {
    if let Some(values) = schema.get("enum").and_then(Value::as_array) {
        let labels = schema.get("enumDescriptions").and_then(Value::as_array);
        return Some(values.iter().filter(|v| !v.is_null()).enumerate().map(|(i, v)| (v.clone(), label_for(v, labels.and_then(|l| l.get(i))))).collect());
    }
    let alternatives = schema.get("oneOf").or_else(|| schema.get("anyOf")).and_then(Value::as_array)?;
    let mut out = Vec::new();
    for alternative in alternatives {
        let alternative = resolve(root, alternative);
        if alternative.get("type").and_then(Value::as_str) == Some("null") {
            continue;
        }
        if let Some(value) = alternative.get("const") {
            out.push((value.clone(), label_for(value, None)));
        } else if let Some(values) = alternative.get("enum").and_then(Value::as_array) {
            out.extend(values.iter().map(|v| (v.clone(), label_for(v, None))));
        } else {
            return None;
        }
    }
    (!out.is_empty()).then_some(out)
}

fn label_for(value: &Value, description: Option<&Value>) -> String {
    if let Some(d) = description.and_then(Value::as_str).filter(|d| !d.is_empty()) {
        return d.to_string();
    }
    match value {
        // Identifiers (`on_save`, `bar`) read better humanized; names (`Forge Dark`) don't.
        Value::String(s) if s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-') => humanize(s),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// `tab_size` → "Tab size", `includePrerelease` → "Include prerelease", `git.enabled` → "Git enabled".
pub fn humanize(key: &str) -> String {
    let mut words = String::new();
    let mut prev_lower = false;
    for ch in key.chars() {
        if ch == '_' || ch == '-' || ch == '.' || ch == ' ' {
            words.push(' ');
            prev_lower = false;
        } else if ch.is_uppercase() && prev_lower {
            words.push(' ');
            words.extend(ch.to_lowercase());
            prev_lower = false;
        } else {
            words.push(ch);
            prev_lower = ch.is_lowercase() || ch.is_ascii_digit();
        }
    }
    const ACRONYMS: &[&str] = &["ui", "lsp", "json", "url", "mcp", "id", "ai", "dap", "sdk", "cpu", "ssh", "acp"];
    let words: Vec<String> = words
        .to_lowercase()
        .split_whitespace()
        .enumerate()
        .map(|(i, w)| {
            if ACRONYMS.contains(&w) {
                w.to_uppercase()
            } else if i == 0 {
                let mut chars = w.chars();
                chars.next().map(|first| first.to_uppercase().chain(chars).collect()).unwrap_or_default()
            } else {
                w.to_string()
            }
        })
        .collect();
    words.join(" ")
}

/// The description without its "Default: …" line (the tab shows the default itself), and
/// naming the product as Forge.
fn clean_description(text: &str) -> String {
    let kept: Vec<&str> = text.lines().filter(|l| !l.trim_start().starts_with("Default:")).collect();
    let text = kept.join("\n").trim().to_string();
    text.replace("Zed's", "Forge's").replace("Zed", "Forge").replace("zed:", "forge:")
}

/// A setting was changed from the Settings tab.
#[derive(Clone, Debug)]
pub struct SettingChanged {
    pub file: SettingsFile,
    pub path: Vec<String>,
    pub value: Option<Value>,
}

#[derive(Default)]
pub struct SettingsRegistry {
    pages: Vec<SettingsPage>,
}

impl EventEmitter<SettingChanged> for SettingsRegistry {}

struct GlobalRegistry(Entity<SettingsRegistry>);
impl Global for GlobalRegistry {}

impl SettingsRegistry {
    pub fn global(cx: &mut App) -> Entity<SettingsRegistry> {
        if let Some(registry) = cx.try_global::<GlobalRegistry>() {
            return registry.0.clone();
        }
        let registry = cx.new(|_| SettingsRegistry::default());
        cx.set_global(GlobalRegistry(registry.clone()));
        registry
    }

    /// Adds a page, replacing the one with the same id.
    pub fn register(&mut self, page: SettingsPage, cx: &mut Context<Self>) {
        self.pages.retain(|p| p.id != page.id);
        self.pages.push(page);
        self.pages.sort_by(|a, b| a.order.cmp(&b.order).then_with(|| a.title.cmp(&b.title)));
        cx.notify();
    }

    pub fn unregister(&mut self, id: &str, cx: &mut Context<Self>) {
        self.pages.retain(|p| p.id != id);
        cx.notify();
    }

    pub fn pages(&self) -> &[SettingsPage] {
        &self.pages
    }

    pub fn page(&self, id: &str) -> Option<&SettingsPage> {
        self.pages.iter().find(|p| p.id == id)
    }

    /// Writes the value (`None` resets it to its default) and tells listeners.
    pub fn set(&mut self, file: &SettingsFile, path: &[String], value: Option<Value>, cx: &mut Context<Self>) -> anyhow::Result<()> {
        file.write(path, value.as_ref())?;
        cx.emit(SettingChanged { file: file.clone(), path: path.to_vec(), value });
        cx.notify();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn page(schema: Value) -> SettingsPage {
        SettingsPage { id: "t".into(), title: "T".into(), file: SettingsFile::Config("t.json".into()), schema, keys: None, defaults: json!({}), order: 0, actions: vec![] }
    }

    #[test]
    fn rows_from_a_schema() {
        let schema = json!({
            "properties": {
                "tab_size": { "type": ["integer", "null"], "minimum": 1, "maximum": 128, "description": "How many columns a tab should occupy.\n\nDefault: 4" },
                "terminal": { "anyOf": [{ "$ref": "#/$defs/Terminal" }, { "type": "null" }], "description": "Configuration of the terminal in Zed." },
                "cursor_shape": { "anyOf": [{ "$ref": "#/$defs/Shape" }, { "type": "null" }] },
                "file_types": { "type": "object", "additionalProperties": { "type": "array" } }
            },
            "$defs": {
                "Terminal": { "type": "object", "properties": { "blinking": { "type": "boolean" }, "font_size": { "type": "number" } } },
                "Shape": { "oneOf": [{ "const": "bar", "description": "A bar" }, { "const": "block" }] }
            }
        });
        let rows = rows(&page(schema));
        let summary: Vec<(String, SettingKind, Option<String>)> = rows.iter().map(|r| (r.key(), r.kind.clone(), r.group.clone())).collect();
        assert_eq!(
            summary,
            vec![
                ("tab_size".into(), SettingKind::Integer { min: Some(1.), max: Some(128.) }, None),
                ("terminal.blinking".into(), SettingKind::Bool, Some("Terminal".into())),
                ("terminal.font_size".into(), SettingKind::Number { min: None, max: None }, Some("Terminal".into())),
                ("cursor_shape".into(), SettingKind::Choice(vec![(json!("bar"), "Bar".into()), (json!("block"), "Block".into())]), None),
                ("file_types".into(), SettingKind::Json, None),
            ]
        );
        assert_eq!(rows[0].title, "Tab size");
        assert_eq!(rows[0].description, "How many columns a tab should occupy.", "the default line is dropped");
    }

    #[test]
    fn extension_style_schemas() {
        let schema = json!({ "properties": {
            "notes.sort": { "type": "string", "enum": ["name", "date"], "enumDescriptions": ["By name", "By date"], "title": "Sort notes" },
            "notes.maxItems": { "type": "integer", "default": 10 }
        }});
        let rows = rows(&page(schema));
        assert_eq!(rows[0].path, vec!["notes.sort".to_string()], "dotted keys are a single key");
        assert_eq!(rows[0].title, "Sort notes");
        assert_eq!(rows[0].kind, SettingKind::Choice(vec![(json!("name"), "By name".into()), (json!("date"), "By date".into())]));
        assert_eq!(rows[1].title, "Notes max items");
        let names = page(json!({ "properties": { "theme": { "enum": ["Forge Dark", "one_light"] } } }));
        assert_eq!(super::rows(&names)[0].kind, SettingKind::Choice(vec![(json!("Forge Dark"), "Forge Dark".into()), (json!("one_light"), "One light".into())]));
    }

    #[test]
    fn edits_keep_the_rest_of_the_file() {
        let text = "// mine\n{\n  \"a\": 1, // keep\n  \"nuget\": { \"x\": true }\n}\n";
        let path = |p: &[&str]| p.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let set = edit_json_text(text, &path(&["nuget", "includePrerelease"]), Some(&json!(true)));
        assert!(set.starts_with("// mine") && set.contains("// keep"), "{set}");
        let parsed: Value = serde_json_lenient::from_str(&set).unwrap();
        assert_eq!(parsed["nuget"]["includePrerelease"], true);
        assert_eq!(parsed["a"], 1);

        let removed = edit_json_text(&set, &path(&["a"]), None);
        let parsed: Value = serde_json_lenient::from_str(&removed).unwrap();
        assert!(parsed.get("a").is_none());
        assert_eq!(serde_json_lenient::from_str::<Value>(&edit_json_text("", &path(&["k"]), Some(&json!(2)))).unwrap(), json!({ "k": 2 }));
    }

    #[test]
    fn humanizes_keys() {
        assert_eq!(humanize("tab_size"), "Tab size");
        assert_eq!(humanize("includePrerelease"), "Include prerelease");
        assert_eq!(humanize("notes.maxItems"), "Notes max items");
        assert_eq!(humanize("ui_font_size"), "UI font size");
    }
}
