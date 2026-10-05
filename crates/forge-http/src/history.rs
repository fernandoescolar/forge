//! The requests sent lately (from any `.http` file), kept between sessions, to send one
//! again from a picker (Terminal › *HTTP Request History…*).

use std::path::{Path, PathBuf};

use gpui::{App, AppContext as _, Context, Window};
use serde::{Deserialize, Serialize};
use workspace::Workspace;

use crate::{Exchange, HttpState, Resolved};

const KEY: &str = "forge-http-history";
const KEEP: usize = 100;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub file: PathBuf,
    pub request: Resolved,
    pub environment: Option<String>,
    /// `None` when it failed (no response).
    pub status: Option<u16>,
    pub millis: u64,
    /// Unix time it was sent.
    pub at: i64,
}

/// The history, loaded from Forge's database the first time.
pub(crate) fn entries(cx: &mut App) -> &mut Vec<Entry> {
    let state = cx.global::<HttpState>();
    if state.history.is_none() {
        let loaded = db::kvp::KeyValueStore::global(cx).read_kvp(KEY).ok().flatten().and_then(|json| serde_json::from_str(&json).ok()).unwrap_or_default();
        cx.global_mut::<HttpState>().history = Some(loaded);
    }
    cx.global_mut::<HttpState>().history.get_or_insert_with(Vec::new)
}

/// Adds a request that was just sent, and saves the history.
pub(crate) fn record(file: &Path, request: &Resolved, environment: Option<&str>, exchange: Option<&Exchange>, cx: &mut App) {
    let entry = Entry {
        file: file.to_path_buf(),
        request: request.clone(),
        environment: environment.map(String::from),
        status: exchange.map(|e| e.status),
        millis: exchange.map(|e| e.elapsed.as_millis() as u64).unwrap_or(0),
        at: chrono::Utc::now().timestamp(),
    };
    let entries = entries(cx);
    entries.push(entry);
    let excess = entries.len().saturating_sub(KEEP);
    entries.drain(..excess);
    let json = serde_json::to_string(entries).unwrap_or_default();
    let kvp = db::kvp::KeyValueStore::global(cx);
    cx.background_spawn(async move {
        if let Err(e) = kvp.write_kvp(KEY.into(), json).await {
            log::error!("cannot save the HTTP history: {e:#}");
        }
    })
    .detach();
}

/// "200 · 12 ms · 5 min ago · api.http", for the picker.
pub fn describe(entry: &Entry, now: i64) -> String {
    let status = entry.status.map(|s| s.to_string()).unwrap_or_else(|| "failed".into());
    let ago = match (now - entry.at).max(0) {
        d if d < 60 => "just now".to_string(),
        d if d < 3600 => format!("{} min ago", d / 60),
        d if d < 86400 => format!("{} h ago", d / 3600),
        d => format!("{} days ago", d / 86400),
    };
    let file = entry.file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    format!("{status} · {} ms · {ago} · {file}", entry.millis)
}

/// Picks a request from the history, newest first, and sends it again (its response shows
/// in its `.http` file's response tab).
pub fn pick(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let recent: Vec<Entry> = entries(cx).iter().rev().cloned().collect();
    if recent.is_empty() {
        return crate::toast(workspace, "No requests sent yet.", cx);
    }
    let now = chrono::Utc::now().timestamp();
    let choices = recent.iter().map(|e| forge_ui::pick::Choice::new(format!("{} {}", e.request.method, e.request.url)).detail(describe(e, now))).collect();
    let weak = cx.entity().downgrade();
    forge_ui::pick::pick(workspace, "Send again…", choices, window, cx, move |ix, window, cx| {
        let Some(entry) = recent.get(ix).cloned() else { return };
        forge_ui::pick::defer_workspace(weak, window, cx, move |ws, window, cx| {
            crate::dispatch(ws, entry.file, entry.request, entry.environment, window, cx);
        });
    });
}
