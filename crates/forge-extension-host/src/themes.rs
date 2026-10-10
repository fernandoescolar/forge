//! Themes and icon themes an extension contributes (`forge.themes`, `forge.iconThemes`).
//!
//! A theme file is a Zed theme family (`{ "themes": [...] }`) or a Forge palette (`{ "ui":
//! ... }`); an icon theme file is a Zed icon theme family, whose icon paths are relative to
//! the extension's folder, as in Zed's extensions. Each entry is a file or a folder of
//! `*.json` files.

use anyhow::{Context as _, Result, bail};
use gpui::App;
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    sync::OnceLock,
};
use theme::{IconThemeFamilyContent, ThemeRegistry};

/// Turns a Forge palette into a Zed theme family. Palettes belong to the app, which sets it.
static PALETTE_EXPANDER: OnceLock<fn(&Value) -> Result<Value>> = OnceLock::new();

pub fn set_palette_expander(expand: fn(&Value) -> Result<Value>) {
    PALETTE_EXPANDER.set(expand).ok();
}

/// The theme names an extension registered, to remove them when it unloads.
#[derive(Debug, Clone, Default)]
pub struct Registered {
    pub themes: Vec<String>,
    pub icon_themes: Vec<String>,
}

impl Registered {
    pub fn is_empty(&self) -> bool {
        self.themes.is_empty() && self.icon_themes.is_empty()
    }
}

/// The `*.json` files `entries` name in the extension at `root`: files as they are, folders
/// by their JSON files (sorted).
pub(crate) fn resolve(root: &Path, entries: &[String]) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for entry in entries {
        let relative = entry.trim_start_matches("./").trim_end_matches('/');
        anyhow::ensure!(Path::new(relative).components().all(|c| matches!(c, std::path::Component::Normal(_))), "`{entry}` must be a path inside the extension");
        let path = root.join(relative);
        if path.is_dir() {
            let mut files: Vec<PathBuf> = std::fs::read_dir(&path)?.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "json")).collect();
            files.sort();
            out.extend(files);
        } else if path.is_file() {
            out.push(path);
        } else {
            bail!("`{entry}` does not exist");
        }
    }
    Ok(out)
}

/// A theme file as a Zed theme family: a palette is expanded, a family is taken as it is.
fn theme_family(text: &str) -> Result<Value> {
    let value: Value = serde_json_lenient::from_str(text).context("not valid JSON")?;
    if value.get("themes").is_some_and(Value::is_array) {
        return Ok(value);
    }
    let expand = PALETTE_EXPANDER.get().context("Forge palettes can't be loaded here")?;
    expand(&value)
}

/// Registers the themes in `themes` and the icon themes in `icon_themes` (icons resolved
/// against `root`), and applies them if one is selected. Returns the names registered, and
/// an error per file that could not be loaded.
pub fn register(root: &Path, themes: &[PathBuf], icon_themes: &[PathBuf], cx: &mut App) -> (Registered, Vec<String>) {
    let registry = ThemeRegistry::global(cx);
    let mut registered = Registered::default();
    let mut errors = Vec::new();
    let name_of = |path: &Path| path.strip_prefix(root).unwrap_or(path).display().to_string();
    for path in themes {
        let loaded = std::fs::read_to_string(path).map_err(anyhow::Error::from).and_then(|text| {
            let family = theme_family(&text)?.to_string();
            let content = theme_settings::deserialize_user_theme(family.as_bytes())?;
            let names: Vec<String> = content.themes.iter().map(|t| t.name.clone()).collect();
            registry.insert_theme_families([theme_settings::refine_theme_family(content)]);
            Ok(names)
        });
        match loaded {
            Ok(names) => registered.themes.extend(names),
            Err(e) => errors.push(format!("theme {}: {e:#}", name_of(path))),
        }
    }
    for path in icon_themes {
        let loaded = std::fs::read_to_string(path).map_err(anyhow::Error::from).and_then(|text| {
            let family: IconThemeFamilyContent = serde_json_lenient::from_str(&text)?;
            let names: Vec<String> = family.themes.iter().map(|t| t.name.clone()).collect();
            registry.load_icon_theme(family, root)?;
            Ok(names)
        });
        match loaded {
            Ok(names) => registered.icon_themes.extend(names),
            Err(e) => errors.push(format!("icon theme {}: {e:#}", name_of(path))),
        }
    }
    reload(&registered, cx);
    (registered, errors)
}

/// Removes what [`register`] registered; a selected theme that goes falls back to the default.
pub fn unregister(registered: &Registered, cx: &mut App) {
    if registered.is_empty() {
        return;
    }
    let registry = ThemeRegistry::global(cx);
    let names = |list: &[String]| list.iter().map(|n| n.clone().into()).collect::<Vec<_>>();
    registry.remove_user_themes(&names(&registered.themes));
    registry.remove_icon_themes(&names(&registered.icon_themes));
    reload(registered, cx);
}

fn reload(registered: &Registered, cx: &mut App) {
    if !registered.themes.is_empty() {
        theme_settings::reload_theme(cx);
    }
    if !registered.icon_themes.is_empty() {
        theme_settings::reload_icon_theme(cx);
    }
}

#[cfg(test)]
mod tests {
    use crate::ExtensionHost;
    use theme::ThemeRegistry;

    const THEME: &str = r#"{ "name": "Ocean", "author": "me", "themes": [{ "name": "Ocean Dark", "appearance": "dark", "style": {} }] }"#;
    const ICONS: &str = r#"{ "name": "Shapes", "author": "me", "themes": [{ "name": "Shapes", "appearance": "dark", "file_icons": { "rust": { "path": "./icons/rust.svg" } } }] }"#;

    /// An extension with no code of its own brings a theme and an icon theme, and takes
    /// them away when it unloads.
    #[gpui::test]
    async fn an_extension_brings_themes_and_icon_themes(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        cx.update(|cx| {
            let store = settings::SettingsStore::test(cx);
            cx.set_global(store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
        let tmp = tempfile::tempdir().unwrap();
        let ext = tmp.path().join("ocean");
        std::fs::create_dir_all(ext.join("themes")).unwrap();
        std::fs::create_dir_all(ext.join("icon_themes")).unwrap();
        std::fs::write(ext.join("package.json"), r#"{"name":"ocean","forge":{"themes":["themes"],"iconThemes":["./icon_themes/shapes.json"]}}"#).unwrap();
        std::fs::write(ext.join("themes/ocean.json"), THEME).unwrap();
        // A Forge palette is a theme too, once the app says how to expand one.
        super::set_palette_expander(|palette| Ok(serde_json::json!({ "name": palette["name"], "author": "", "themes": [{ "name": palette["name"], "appearance": "light", "style": {} }] })));
        std::fs::write(ext.join("themes/sand.json"), r#"{ "name": "Sand", "ui": {} }"#).unwrap();
        std::fs::write(ext.join("icon_themes/shapes.json"), ICONS).unwrap();

        let host = cx.update(|cx| ExtensionHost::init(vec![tmp.path().to_path_buf()], cx)).unwrap();
        let (themes, icon_themes) = host.read_with(cx, |h, _| (h.extensions[0].themes.themes.clone(), h.extensions[0].themes.icon_themes.clone()));
        assert_eq!(themes, ["Ocean Dark", "Sand"]);
        assert_eq!(icon_themes, ["Shapes"]);
        let errors = host.read_with(cx, |h, _| h.errors.clone());
        assert!(errors.is_empty(), "{errors:?}");
        cx.update(|cx| {
            let registry = ThemeRegistry::global(cx);
            assert!(registry.get("Ocean Dark").is_ok());
            let shapes = registry.get_icon_theme("Shapes").unwrap();
            let rust = shapes.file_icons.get("rust").unwrap();
            assert_eq!(std::path::Path::new(rust.path.as_ref()), ext.join("./icons/rust.svg"), "icons are found in the extension");
        });

        cx.update(|cx| host.update(cx, |h, cx| h.unload("ocean", cx)));
        cx.update(|cx| {
            let registry = ThemeRegistry::global(cx);
            assert!(registry.get("Ocean Dark").is_err() && registry.get("Sand").is_err());
            assert!(registry.get_icon_theme("Shapes").is_err());
        });
    }

    /// The icon themes that ship with Forge load, and every icon they name is there.
    #[gpui::test]
    fn bundled_icon_themes_load(cx: &mut gpui::TestAppContext) {
        let extensions = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../extensions");
        let expected: &[(&str, &[&str])] = &[
            ("forge-icons", &["Forge Dark", "Forge Light"]),
            ("modern-icons", &["Modern Icons (Light)", "Modern Icons (Dark)"]),
            ("colored-icons", &["Colored Zed Icons Theme Dark", "Colored Zed Icons Theme Light"]),
            ("vscode-great-icons", &["VSCode Great Icons Theme"]),
            ("seti-icons", &["Seti Icon Theme"]),
        ];
        cx.update(|cx| {
            let store = settings::SettingsStore::test(cx);
            cx.set_global(store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            for (folder, names) in expected {
                let ext = crate::host::read_manifest(&extensions.join(folder)).unwrap().unwrap();
                assert!(ext.main.is_none(), "{folder} has no code");
                let (registered, errors) = super::register(&ext.info.path, &ext.themes, &ext.icon_themes, cx);
                assert!(errors.is_empty(), "{folder}: {errors:?}");
                assert_eq!(registered.icon_themes, *names);
                for name in *names {
                    let theme = ThemeRegistry::global(cx).get_icon_theme(name).unwrap();
                    let paths = theme.file_icons.values().map(|i| &i.path).chain(theme.directory_icons.collapsed.iter()).chain(theme.directory_icons.expanded.iter());
                    for path in paths {
                        assert!(std::path::Path::new(path.as_ref()).is_file(), "{name}: {path} is missing");
                    }
                }
            }
        });
    }

    #[test]
    fn theme_paths_stay_inside_the_extension() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(super::resolve(tmp.path(), &["../elsewhere.json".into()]).is_err());
        assert!(super::resolve(tmp.path(), &["missing.json".into()]).is_err());
    }
}
