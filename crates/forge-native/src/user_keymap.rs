//! The user's `keymap.json`: loaded over Zed's and Forge's bindings at startup, and again
//! each time it changes (by hand, or when an agent's change is applied).

use std::sync::Arc;

use fs::Fs;
use futures::StreamExt as _;
use gpui::App;
use settings::{KeybindSource, KeymapFile, KeymapFileLoadResult};

pub fn init(fs: Arc<dyn Fs>, cx: &mut App) {
    let (mut contents, watcher) = settings::watch_config_file(cx.background_executor(), fs, paths::keymap_file().clone());
    cx.spawn(async move |cx| {
        let _watcher = watcher;
        while let Some(text) = contents.next().await {
            cx.update(|cx| apply(&text, cx));
        }
    })
    .detach();
}

/// Replaces the user's bindings with those in `text`; every other binding stays.
pub fn apply(text: &str, cx: &mut App) {
    let user = KeybindSource::User.meta();
    let kept: Vec<gpui::KeyBinding> = cx.key_bindings().borrow().bindings().filter(|b| b.meta() != Some(user)).cloned().collect();
    let mut bindings = match KeymapFile::load(text, cx) {
        KeymapFileLoadResult::Success { key_bindings } => key_bindings,
        KeymapFileLoadResult::SomeFailedToLoad { key_bindings, error_message } => {
            log::warn!("some bindings in keymap.json couldn't be loaded: {error_message}");
            key_bindings
        }
        KeymapFileLoadResult::JsonParseFailure { error } => {
            // Half-typed JSON: keep the bindings the file had until it parses again.
            log::warn!("keymap.json doesn't parse: {error:#}");
            return;
        }
    };
    for binding in &mut bindings {
        binding.set_meta(user);
    }
    cx.clear_key_bindings();
    cx.bind_keys(kept);
    // Last, so they win over the defaults for the same keys and context.
    cx.bind_keys(bindings);
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    gpui::actions!(test_keymap, [Base, Mine, Other]);

    fn action_for(keys: &str, cx: &mut App) -> Option<String> {
        let keymap = cx.key_bindings();
        let keymap = keymap.borrow();
        let (bindings, _) = keymap.bindings_for_input(&[gpui::Keystroke::parse(keys).unwrap()], &[]);
        bindings.first().map(|b| b.action().name().to_string())
    }

    /// The file's bindings win over the others, and editing it replaces them (not adds).
    #[gpui::test]
    fn user_bindings_replace_each_other(cx: &mut TestAppContext) {
        cx.update(|cx| {
            settings::init(cx);
            cx.bind_keys([gpui::KeyBinding::new("ctrl-b", Base, None), gpui::KeyBinding::new("ctrl-o", Other, None)]);
            apply(r#"[{ "bindings": { "ctrl-b": "test_keymap::Mine" } }]"#, cx);
            assert_eq!(action_for("ctrl-b", cx).as_deref(), Some("test_keymap::Mine"), "the user's binding wins");
            assert_eq!(action_for("ctrl-o", cx).as_deref(), Some("test_keymap::Other"), "the others stay");

            // The binding is gone from the file: back to the default.
            apply("// nothing here\n[]", cx);
            assert_eq!(action_for("ctrl-b", cx).as_deref(), Some("test_keymap::Base"));
            // Unparsable (being typed): what was there stays.
            apply(r#"[{ "bindings": { "ctrl-b": "test_keymap::Mine" } }]"#, cx);
            apply("[{ \"bindings\": ", cx);
            assert_eq!(action_for("ctrl-b", cx).as_deref(), Some("test_keymap::Mine"));
        });
    }
}
