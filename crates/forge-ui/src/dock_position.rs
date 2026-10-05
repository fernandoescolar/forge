//! Remembers where the user docked a panel (left / right / bottom) across restarts, in the
//! same key-value store Zed uses for panel sizes. Zed's own panels keep theirs in
//! settings.json; Forge's panels have no settings section, so they use this.

use db::kvp::KeyValueStore;
use gpui::{App, AppContext as _};
use workspace::dock::DockPosition;

fn key(panel_key: &str) -> String {
    format!("forge-panel-position:{panel_key}")
}

pub fn encode(position: DockPosition) -> &'static str {
    match position {
        DockPosition::Left => "left",
        DockPosition::Right => "right",
        DockPosition::Bottom => "bottom",
    }
}

pub fn decode(value: &str) -> Option<DockPosition> {
    match value {
        "left" => Some(DockPosition::Left),
        "right" => Some(DockPosition::Right),
        "bottom" => Some(DockPosition::Bottom),
        _ => None,
    }
}

/// The saved position, if the user ever moved this panel.
pub fn load(panel_key: &str, cx: &App) -> Option<DockPosition> {
    KeyValueStore::global(cx).read_kvp(&key(panel_key)).ok().flatten().and_then(|v| decode(&v))
}

/// Persists `position` for `panel_key` (in the background).
pub fn save(panel_key: &str, position: DockPosition, cx: &App) {
    let kvp = KeyValueStore::global(cx);
    let key = key(panel_key);
    cx.background_spawn(async move {
        if let Err(e) = kvp.write_kvp(key, encode(position).to_string()).await {
            log::error!("failed to save panel position: {e:#}");
        }
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positions_round_trip() {
        for p in [DockPosition::Left, DockPosition::Right, DockPosition::Bottom] {
            assert_eq!(decode(encode(p)), Some(p));
        }
        assert_eq!(decode("top"), None);
    }
}
