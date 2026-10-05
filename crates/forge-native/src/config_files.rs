//! The user's `settings.json` and `keymap.json`: created with Forge's own templates.
//!
//! The settings store falls back to a built-in template when it writes to a file that
//! doesn't exist yet (choosing a theme, for instance). Creating both files at startup
//! keeps that template, which sets other themes and fonts, out of the user's config.

use std::path::Path;

pub const USER_SETTINGS: &str = r#"// Forge settings
//
// Your settings go here, on top of Forge's defaults. Forge applies them when you save.
// To see every setting and its default value: Forge › Settings › Open Default Settings.
{
}
"#;

pub const KEYMAP: &str = r#"// Forge key bindings
//
// Your bindings go here, on top of Forge's defaults. Forge applies them when you save.
// To see the default bindings: Forge › Settings › Open Default Key Bindings.
//
// Each entry binds keys to actions in a context, for example:
//   { "context": "Editor", "bindings": { "cmd-k cmd-d": "editor::DuplicateLineDown" } }
[
]
"#;

/// Creates the files that don't exist yet, and replaces a template written by an earlier
/// version (left untouched otherwise: only its header comment changes).
pub fn ensure() {
    let settings = settings::initial_user_settings_content();
    let keymap = settings::initial_keymap_content();
    ensure_file(paths::settings_file(), USER_SETTINGS, &settings);
    ensure_file(paths::keymap_file(), KEYMAP, &keymap);
}

fn ensure_file(path: &Path, template: &str, foreign: &str) {
    let current = std::fs::read_to_string(path).unwrap_or_default();
    if let Some(updated) = updated_content(&current, template, foreign) {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(err) = std::fs::write(path, updated) {
            log::warn!("couldn't write {}: {err}", path.display());
        }
    }
}

/// What the file should contain now, if it needs to change.
fn updated_content(current: &str, template: &str, foreign: &str) -> Option<String> {
    if current.trim().is_empty() || current == foreign {
        return Some(template.to_string());
    }
    let header = |text: &str| -> usize { text.lines().take_while(|l| l.trim_start().starts_with("//")).map(|l| l.len() + 1).sum() };
    let foreign_header = &foreign[..header(foreign).min(foreign.len())];
    if !foreign_header.is_empty() && current.starts_with(foreign_header) {
        let template_header = &template[..header(template)];
        return Some(format!("{template_header}{}", &current[foreign_header.len()..]));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_and_rebrands_config_files() {
        let foreign = settings::initial_user_settings_content();
        assert_eq!(updated_content("", USER_SETTINGS, &foreign).as_deref(), Some(USER_SETTINGS), "missing file");
        assert_eq!(updated_content(&foreign, USER_SETTINGS, &foreign).as_deref(), Some(USER_SETTINGS), "untouched template");

        let body = "{\n  \"tab_size\": 2\n}\n";
        let edited = format!("{}{body}", &foreign[..foreign.find('{').unwrap()]);
        let updated = updated_content(&edited, USER_SETTINGS, &foreign).unwrap();
        assert!(updated.starts_with("// Forge settings") && updated.ends_with(body), "the user's settings are kept: {updated}");

        assert_eq!(updated_content(&updated, USER_SETTINGS, &foreign), None, "nothing to do the second time");
        assert_eq!(updated_content("{ \"x\": 1 }", USER_SETTINGS, &foreign), None, "user files without the header");

        let keymap = settings::initial_keymap_content();
        assert!(updated_content(&keymap, KEYMAP, &keymap).unwrap().starts_with("// Forge key bindings"));
        for template in [USER_SETTINGS, KEYMAP] {
            assert!(!template.to_lowercase().contains("zed"));
            serde_json_lenient::from_str::<serde_json::Value>(template).unwrap();
        }
    }
}
