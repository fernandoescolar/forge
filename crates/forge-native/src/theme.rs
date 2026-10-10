//! Forge colour palettes.
//!
//! A palette names ~30 colours (see `assets/palettes/*.json`). [`expand`] turns it into a
//! full Zed theme by overriding Zed's One Dark, so any key a palette doesn't mention still
//! has a sensible value. Every `<config>/palettes/*.json` becomes a selectable theme and is
//! reloaded whenever it is saved. Zed theme files in `<config>/themes` and Zed icon themes
//! in `<config>/icon_themes` load the same way.

use anyhow::{Context as _, Result, bail};
use fs::Fs;
use futures::StreamExt as _;
use gpui::{App, AsyncApp};
use serde_json::{Map, Value, json};
use std::{path::Path, sync::Arc};
use theme::ThemeRegistry;
use util::ResultExt as _;

/// Palettes shipped with Forge, written to the palettes dir on first run.
pub const BUILTIN_PALETTES: &[(&str, &str)] = &[
    ("dracula.json", include_str!("../assets/palettes/dracula.json")),
    ("forge-dark.json", include_str!("../assets/palettes/forge-dark.json")),
    ("forge-light.json", include_str!("../assets/palettes/forge-light.json")),
];
const BASE_THEME: &str = include_str!("../../../vendor/zed/assets/themes/one/one.json");

pub fn init(fs: Arc<dyn Fs>, cx: &mut App) {
    // Extensions can bring palettes too.
    forge_extension_host::themes::set_palette_expander(expand);
    migrate_icon_theme_setting().log_err();
    let dir = paths::config_dir().join("palettes");
    install_builtins(&dir).log_err();
    // Register built-ins synchronously so the first frame already uses the right colours;
    // the user's copies (possibly edited) replace them right after.
    for (name, text) in BUILTIN_PALETTES {
        register(text, cx).with_context(|| format!("built-in palette {name} is invalid")).log_err();
    }
    watch_dir(fs.clone(), dir, |text, cx| register(text, cx).map(|_| ()), cx);
    // Plain Zed theme files keep working too, as in Zed.
    watch_dir(fs.clone(), paths::themes_dir().clone(), |text, cx| {
        theme_settings::load_user_theme(&ThemeRegistry::global(cx), text.as_bytes())?;
        theme_settings::reload_theme(cx);
        Ok(())
    }, cx);
    watch_dir(fs, icon_themes_dir(), load_user_icon_theme, cx);
}

/// The icon theme that was called "Forge" (Zed's icons) became "Forge Dark" and "Forge
/// Light" (the Forge Icons extension): a `settings.json` that names it gets the new names.
fn migrate_icon_theme_setting() -> Result<()> {
    use forge_ui::settings_registry::SettingsFile;
    let Some(current) = SettingsFile::User.read().get("icon_theme").cloned() else { return Ok(()) };
    if let Some(renamed) = renamed_icon_theme(&current) {
        SettingsFile::User.write(&["icon_theme".into()], Some(&renamed))?;
    }
    Ok(())
}

fn renamed_icon_theme(value: &Value) -> Option<Value> {
    const OLD: &str = "Forge";
    match value {
        Value::String(name) if name == OLD => Some(json!({ "mode": "system", "dark": "Forge Dark", "light": "Forge Light" })),
        Value::Object(selection) => {
            let mut renamed = selection.clone();
            for (key, new) in [("dark", "Forge Dark"), ("light", "Forge Light")] {
                if renamed.get(key).and_then(Value::as_str) == Some(OLD) {
                    renamed.insert(key.into(), json!(new));
                }
            }
            (&renamed != selection).then_some(Value::Object(renamed))
        }
        _ => None,
    }
}

/// Zed icon theme files the user adds; their icon paths are relative to this folder.
pub fn icon_themes_dir() -> std::path::PathBuf {
    paths::config_dir().join("icon_themes")
}

fn load_user_icon_theme(text: &str, cx: &mut App) -> Result<()> {
    let family: theme::IconThemeFamilyContent = serde_json_lenient::from_str(text).context("not a Zed icon theme")?;
    ThemeRegistry::global(cx).load_icon_theme(family, &icon_themes_dir())?;
    theme_settings::reload_icon_theme(cx);
    Ok(())
}

/// Syntax highlighting maps tree-sitter captures through the active theme. The language
/// registry must be told about the theme (and every change of it); otherwise every token
/// renders in the plain text colour.
pub fn connect_syntax_highlighting(languages: Arc<language::LanguageRegistry>, cx: &mut App) {
    use ::theme::ActiveTheme as _;
    languages.set_theme(cx.theme().clone());
    cx.observe_global::<::theme::GlobalTheme>(move |cx| languages.set_theme(cx.theme().clone())).detach();
}

fn install_builtins(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    // Earlier versions used a single config/theme.json; keep the user's edits.
    let legacy = paths::config_dir().join("theme.json");
    if legacy.exists() && !dir.join("forge-dark.json").exists() {
        std::fs::rename(&legacy, dir.join("forge-dark.json"))?;
    }
    for (name, text) in BUILTIN_PALETTES {
        let path = dir.join(name);
        // Write missing palettes, and refresh ones still exactly as an earlier Forge wrote
        // them; a palette the user edited is never touched.
        let refresh = match std::fs::read_to_string(&path) {
            Err(_) => true,
            Ok(current) => fingerprint(&current) != fingerprint(text) && is_pristine(name, &current),
        };
        if refresh {
            std::fs::write(path, text)?;
        }
    }
    Ok(())
}

/// Built-in palettes as earlier Forge versions shipped them: a user's copy still equal to
/// one of these (comments and formatting aside) was never edited and gets the new version.
const PREVIOUS_BUILTINS: &[(&str, &str)] = &[
    // Near-black backgrounds (#0e1014 window, #13161c editor).
    ("forge-dark.json", include_str!("../assets/palettes/previous/forge-dark-v1.json")),
    // Before it was softened: brighter text, more saturated colours.
    ("forge-dark.json", include_str!("../assets/palettes/previous/forge-dark-v2.json")),
    // Before it moved to One Dark's lighter greys and colours.
    ("forge-dark.json", include_str!("../assets/palettes/previous/forge-dark-v3.json")),
];

fn is_pristine(name: &str, text: &str) -> bool {
    let print = fingerprint(text);
    PREVIOUS_BUILTINS.iter().any(|(n, old)| *n == name && fingerprint(old) == print)
}

/// What a palette says, ignoring comments and formatting (its JSON, re-serialized).
fn fingerprint(text: &str) -> u64 {
    let canonical = serde_json_lenient::from_str::<Value>(text).map(|v| v.to_string()).unwrap_or_else(|_| text.to_string());
    canonical.bytes().fold(0xcbf29ce484222325, |hash, byte| (hash ^ byte as u64).wrapping_mul(0x100000001b3))
}

fn register(palette_text: &str, cx: &mut App) -> Result<String> {
    let palette: Value = serde_json_lenient::from_str(palette_text).context("not valid JSON")?;
    let family = expand(&palette)?;
    let name = family["themes"][0]["name"].as_str().unwrap_or_default().to_string();
    theme_settings::load_user_theme(&ThemeRegistry::global(cx), family.to_string().as_bytes())?;
    theme_settings::reload_theme(cx);
    Ok(name)
}

/// Loads every `*.json` in `dir` with `load`, then reloads files as they change.
fn watch_dir(fs: Arc<dyn Fs>, dir: std::path::PathBuf, load: fn(&str, &mut App) -> Result<()>, cx: &mut App) {
    cx.spawn(async move |cx: &mut AsyncApp| {
        fs.create_dir(&dir).await.log_err();
        let apply = async |path: &Path, cx: &mut AsyncApp| {
            if path.extension().is_none_or(|e| e != "json") {
                return;
            }
            let Some(text) = fs.load(path).await.ok() else { return };
            match cx.update(|cx| load(&text, cx)) {
                Ok(()) => log::info!("loaded {}", path.display()),
                Err(e) => log::error!("invalid {}: {e:#}", path.display()),
            }
        };
        if let Some(mut entries) = fs.read_dir(&dir).await.log_err() {
            while let Some(Ok(path)) = entries.next().await {
                apply(&path, cx).await;
            }
        }
        let (mut events, _watcher) = fs.watch(&dir, std::time::Duration::from_millis(100)).await;
        while let Some(batch) = events.next().await {
            for event in batch {
                apply(&event.path, cx).await;
            }
        }
    })
    .detach();
}

/// Expands a Forge palette into a Zed theme family JSON document.
pub fn expand(palette: &Value) -> Result<Value> {
    let base: Value = serde_json_lenient::from_str(BASE_THEME)?;
    let appearance = palette.get("appearance").and_then(Value::as_str).unwrap_or("dark");
    let base_index = usize::from(appearance == "light");
    let mut style = base["themes"][base_index]["style"].clone();
    let s = style.as_object_mut().context("base theme has no style")?;

    let ui = palette.get("ui").and_then(Value::as_object).cloned().unwrap_or_default();
    let color = |key: &str| -> Result<Option<Hex>> { ui.get(key).and_then(Value::as_str).map(Hex::parse).transpose() };
    let mut set = |keys: &[&str], value: Option<String>| {
        if let Some(v) = value {
            for k in keys {
                s.insert((*k).to_string(), json!(v));
            }
        }
    };

    let window = color("window")?;
    let editor = color("editor")?;
    let surface = color("surface")?;
    let border = color("border")?;
    let text = color("text")?;
    let muted = color("muted")?;
    let faint = color("faint")?;
    let accent = color("accent")?;
    let selection = color("selection")?.or_else(|| accent.map(|a| a.alpha(0x33)));
    let active_line = color("active_line")?;
    let hex = |c: Option<Hex>| c.map(|c| c.to_string());
    let alpha = |c: Option<Hex>, a: u8| c.map(|c| c.alpha(a).to_string());

    set(&["background", "title_bar.background", "title_bar.inactive_background", "status_bar.background", "tab_bar.background", "tab.inactive_background", "panel.background"], hex(window));
    set(&["editor.background", "editor.gutter.background", "editor.subheader.background", "tab.active_background", "toolbar.background", "terminal.background"], hex(editor));
    set(&["surface.background", "elevated_surface.background", "element.background"], hex(surface));
    set(&["border", "border.variant", "scrollbar.track.border", "editor.wrap_guide"], hex(border));
    set(&["border.focused", "border.selected", "text.accent", "icon.accent", "link_text.hover"], hex(accent));
    set(&["pane.focused_border", "panel.focused_border"], alpha(accent, 0x80));
    set(&["element.hover", "ghost_element.hover", "drop_target.background"], alpha(accent, 0x1f));
    set(&["element.selected", "ghost_element.selected", "element.active", "ghost_element.active"], alpha(accent, 0x33));
    set(&["search.match_background", "editor.document_highlight.read_background"], alpha(accent, 0x2e));
    set(&["search.active_match_background", "editor.document_highlight.write_background"], alpha(accent, 0x55));
    set(&["scrollbar.thumb.background"], alpha(muted, 0x40));
    set(&["scrollbar.thumb.hover_background"], alpha(muted, 0x70));
    set(&["text", "icon", "editor.foreground", "editor.active_line_number", "terminal.foreground", "terminal.bright_foreground"], hex(text));
    set(&["text.muted", "icon.muted", "editor.hover_line_number"], hex(muted));
    set(&["text.placeholder", "text.disabled", "icon.disabled", "icon.placeholder", "editor.line_number", "terminal.dim_foreground", "editor.invisible", "hidden", "ignored"], hex(faint));
    set(&["editor.active_line.background", "editor.highlighted_line.background"], hex(active_line));

    for (status, aliases) in [
        ("error", &["error", "deleted", "version_control.deleted"][..]),
        ("warning", &["warning", "modified", "conflict", "version_control.modified"][..]),
        ("success", &["success", "created", "version_control.added"][..]),
        ("info", &["info", "hint", "renamed"][..]),
    ] {
        let c = color(status)?;
        for name in aliases {
            set(&[name], hex(c));
            if !name.starts_with("version_control") {
                set(&[&format!("{name}.background")], alpha(c, 0x1a));
                set(&[&format!("{name}.border")], alpha(c, 0x66));
            }
        }
    }

    if let (Some(a), Some(sel)) = (accent, selection) {
        s.insert("players".into(), json!([{ "cursor": a.to_string(), "background": a.to_string(), "selection": sel.to_string() }]));
    }

    apply_syntax(s, palette.get("syntax"))?;
    apply_terminal(s, palette.get("terminal"))?;

    if let Some(overrides) = palette.get("overrides").and_then(Value::as_object) {
        for (k, v) in overrides {
            s.insert(k.clone(), v.clone());
        }
    }

    let name = palette.get("name").and_then(Value::as_str).unwrap_or("Forge Dark");
    Ok(json!({
        "name": name,
        "author": "Forge",
        "themes": [{ "name": name, "appearance": appearance, "style": style }]
    }))
}

/// Each base scope takes the palette entry with the longest matching prefix
/// (`string.special.symbol` → `string.special` → `string`).
fn apply_syntax(style: &mut Map<String, Value>, syntax: Option<&Value>) -> Result<()> {
    let Some(palette) = syntax.and_then(Value::as_object) else { return Ok(()) };
    let normalize = |v: &Value| -> Result<Value> {
        Ok(match v {
            Value::String(c) => json!({ "color": Hex::parse(c)?.to_string(), "font_style": null, "font_weight": null }),
            Value::Object(o) => {
                let mut o = o.clone();
                if let Some(c) = o.get("color").and_then(Value::as_str) {
                    o.insert("color".into(), json!(Hex::parse(c)?.to_string()));
                }
                Value::Object(o)
            }
            other => bail!("syntax value must be a colour or object, got {other}"),
        })
    };
    let target = style.entry("syntax").or_insert_with(|| json!({})).as_object_mut().context("syntax is not an object")?;
    let mut scopes: Vec<String> = target.keys().cloned().collect();
    scopes.extend(palette.keys().cloned());
    for scope in scopes {
        let mut candidate = scope.as_str();
        loop {
            if let Some(v) = palette.get(candidate) {
                target.insert(scope.clone(), normalize(v)?);
                break;
            }
            match candidate.rsplit_once('.') {
                Some((parent, _)) => candidate = parent,
                None => break,
            }
        }
    }
    Ok(())
}

fn apply_terminal(style: &mut Map<String, Value>, terminal: Option<&Value>) -> Result<()> {
    let Some(colors) = terminal.and_then(Value::as_object) else { return Ok(()) };
    for (name, value) in colors {
        let Some(c) = value.as_str() else { continue };
        let c = Hex::parse(c)?;
        style.insert(format!("terminal.ansi.{name}"), json!(c.to_string()));
        style.insert(format!("terminal.ansi.bright_{name}"), json!(c.mix([255, 255, 255], 0.25).to_string()));
        style.insert(format!("terminal.ansi.dim_{name}"), json!(c.mix([0, 0, 0], 0.35).to_string()));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Hex([u8; 4]);

impl Hex {
    fn parse(s: &str) -> Result<Self> {
        let h = s.strip_prefix('#').with_context(|| format!("colour {s:?} must start with #"))?;
        let expanded: String = match h.len() {
            3 | 4 => h.chars().flat_map(|c| [c, c]).collect(),
            6 | 8 => h.to_string(),
            _ => bail!("colour {s:?} must be #rgb, #rrggbb or #rrggbbaa"),
        };
        let mut out = [0, 0, 0, 0xff];
        for (i, byte) in out.iter_mut().enumerate().take(expanded.len() / 2) {
            *byte = u8::from_str_radix(&expanded[i * 2..i * 2 + 2], 16).with_context(|| format!("colour {s:?} is not hex"))?;
        }
        Ok(Self(out))
    }

    fn alpha(self, a: u8) -> Self {
        Self([self.0[0], self.0[1], self.0[2], a])
    }

    fn mix(self, other: [u8; 3], t: f32) -> Self {
        let m = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t).round() as u8;
        Self([m(self.0[0], other[0]), m(self.0[1], other[1]), m(self.0[2], other[2]), self.0[3]])
    }
}

impl std::fmt::Display for Hex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let [r, g, b, a] = self.0;
        write!(f, "#{r:02x}{g:02x}{b:02x}{a:02x}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn palette(file: &str) -> Value {
        let text = BUILTIN_PALETTES.iter().find(|(n, _)| *n == file).unwrap().1;
        serde_json_lenient::from_str(text).unwrap()
    }

    fn default_palette() -> Value {
        palette("forge-dark.json")
    }

    #[test]
    fn every_builtin_palette_is_a_valid_zed_theme() {
        for (file, _) in BUILTIN_PALETTES {
            let family = expand(&palette(file)).unwrap();
            theme_settings::deserialize_user_theme(family.to_string().as_bytes()).unwrap_or_else(|e| panic!("{file}: {e}"));
        }
        let dracula = expand(&palette("dracula.json")).unwrap();
        assert_eq!(dracula["themes"][0]["name"], "Dracula");
        assert_eq!(dracula["themes"][0]["style"]["syntax"]["keyword"]["color"], "#ff79c6ff");
    }

    #[test]
    fn refreshes_only_untouched_old_palettes() {
        let old = include_str!("../assets/palettes/previous/forge-dark-v1.json");
        assert!(is_pristine("forge-dark.json", old), "the previous Forge Dark is recognised");
        assert!(is_pristine("forge-dark.json", &format!("{old}\n  ")), "whitespace doesn't matter");
        assert!(!is_pristine("forge-dark.json", &old.replace("#ff8a3d", "#00ff00")), "edited palettes are kept");
        assert!(!is_pristine("dracula.json", old));
        let v2 = include_str!("../assets/palettes/previous/forge-dark-v2.json");
        assert!(is_pristine("forge-dark.json", v2), "the bright Forge Dark is refreshed too");
        let with_old_comments = v2.replace("into a full theme", "into a full Zed theme");
        assert!(is_pristine("forge-dark.json", &with_old_comments), "comments don't matter");
        let v3 = include_str!("../assets/palettes/previous/forge-dark-v3.json");
        assert!(is_pristine("forge-dark.json", v3), "the darker Forge Dark is refreshed too");
        assert!(!is_pristine("forge-dark.json", BUILTIN_PALETTES.iter().find(|(n, _)| *n == "forge-dark.json").unwrap().1), "the current one is not an old one");
    }

    #[test]
    fn default_palette_expands_to_a_valid_zed_theme() {
        let family = expand(&default_palette()).unwrap();
        let theme = theme_settings::deserialize_user_theme(family.to_string().as_bytes()).unwrap();
        assert_eq!(theme.themes[0].name, "Forge Dark");
        let style = &family["themes"][0]["style"];
        assert_eq!(style["editor.background"], "#282c34ff");
        assert_eq!(style["players"][0]["selection"], "#3e4451ff");
        assert_eq!(style["terminal.ansi.red"], "#e06c75ff");
    }

    #[test]
    fn syntax_scopes_inherit_from_prefix() {
        let family = expand(&default_palette()).unwrap();
        let syntax = &family["themes"][0]["style"]["syntax"];
        assert_eq!(syntax["string"]["color"], "#98c379ff");
        assert_eq!(syntax["string.special"]["color"], "#98c379ff", "inherits string");
        assert_eq!(syntax["string.escape"]["color"], "#56b6c2ff", "own entry wins");
        assert_eq!(syntax["comment"]["font_style"], "italic");
    }

    /// An icon theme in config/icon_themes finds its icons next to it.
    #[gpui::test]
    fn loads_user_icon_themes(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let store = settings::SettingsStore::test(cx);
            cx.set_global(store);
            theme_settings::init(::theme::LoadThemes::JustBase, cx);
            let text = r#"{ "name": "Shapes", "author": "me", "themes": [{ "name": "Shapes", "appearance": "dark", "file_icons": { "rust": { "path": "icons/rust.svg" } } }] }"#;
            load_user_icon_theme(text, cx).unwrap();
            let shapes = ThemeRegistry::global(cx).get_icon_theme("Shapes").unwrap();
            assert_eq!(Path::new(shapes.file_icons["rust"].path.as_ref()), icon_themes_dir().join("icons/rust.svg"));
            assert!(load_user_icon_theme("{ \"name\": \"not an icon theme\" }", cx).is_err());
        });
    }

    #[test]
    fn renames_the_old_forge_icon_theme() {
        assert_eq!(renamed_icon_theme(&json!("Forge")), Some(json!({ "mode": "system", "dark": "Forge Dark", "light": "Forge Light" })));
        assert_eq!(renamed_icon_theme(&json!({ "mode": "light", "light": "Forge", "dark": "Forge" })), Some(json!({ "mode": "light", "light": "Forge Light", "dark": "Forge Dark" })));
        assert_eq!(renamed_icon_theme(&json!({ "mode": "dark", "light": "Seti Icon Theme", "dark": "Forge" })), Some(json!({ "mode": "dark", "light": "Seti Icon Theme", "dark": "Forge Dark" })));
        assert_eq!(renamed_icon_theme(&json!("Zed (Default)")), None);
        assert_eq!(renamed_icon_theme(&json!({ "light": "Forge Light", "dark": "Forge Dark" })), None);
    }

    #[test]
    fn overrides_and_errors() {
        let mut p = default_palette();
        p["overrides"] = json!({ "editor.wrap_guide": "#ff0000ff" });
        assert_eq!(expand(&p).unwrap()["themes"][0]["style"]["editor.wrap_guide"], "#ff0000ff");
        p["ui"]["accent"] = json!("orange");
        assert!(expand(&p).unwrap_err().to_string().contains("must start with #"));
    }

    /// Regression test for "everything is grey": grammars must load and captures must
    /// resolve to palette colours once the theme is connected.
    #[gpui::test]
    async fn rust_keywords_get_palette_colours(cx: &mut gpui::TestAppContext) {
        use gpui::UpdateGlobal as _;
        cx.update(|cx| {
            let store = settings::SettingsStore::test(cx);
            cx.set_global(store);
            theme_settings::init(::theme::LoadThemes::JustBase, cx);
            let family = expand(&palette("dracula.json")).unwrap();
            theme_settings::load_user_theme(&ThemeRegistry::global(cx), family.to_string().as_bytes()).unwrap();
            settings::SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |s| s.theme.theme = Some(settings::ThemeSelection::Static(settings::ThemeName("Dracula".into()))));
            });
            theme_settings::reload_theme(cx);
        });
        let languages = Arc::new(language::LanguageRegistry::test(cx.executor()));
        let fs = fs::FakeFs::new(cx.executor());
        cx.update(|cx| {
            languages::init(languages.clone(), fs, node_runtime::NodeRuntime::unavailable(), cx);
            connect_syntax_highlighting(languages.clone(), cx);
        });
        let rust = languages.language_for_name("Rust").await.unwrap();
        let grammar = rust.grammar().expect("tree-sitter grammar for Rust is loaded");
        let id = grammar.highlight_id_for_name("keyword").expect("`keyword` capture is mapped to a theme style");
        let color = cx.update(|cx| {
            use ::theme::ActiveTheme as _;
            assert_eq!(cx.theme().name, "Dracula");
            cx.theme().syntax().get(id).and_then(|s| s.color)
        });
        let expected: gpui::Hsla = gpui::rgb(0xff79c6).into();
        let color = color.expect("keyword has a colour");
        assert!((color.h - expected.h).abs() < 0.01 && (color.l - expected.l).abs() < 0.01, "{color:?} != {expected:?}");
    }

    #[test]
    fn hex_parsing() {
        assert_eq!(Hex::parse("#abc").unwrap().to_string(), "#aabbccff");
        assert_eq!(Hex::parse("#11223344").unwrap().to_string(), "#11223344");
        assert!(Hex::parse("#12345").is_err());
    }
}
