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
    /// Its credentials were left out when it was saved (headers such as `Authorization`,
    /// query parameters such as `token`): the history on disk keeps no secrets.
    #[serde(default)]
    pub redacted: bool,
}

/// Names of headers and query parameters that carry credentials.
fn sensitive(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    matches!(name.as_str(), "authorization" | "proxy-authorization" | "cookie" | "set-cookie" | "key" | "sig" | "signature" | "code")
        || ["token", "secret", "password", "passwd", "apikey", "api-key", "api_key", "session", "credential"].iter().any(|word| name.contains(word))
}

/// `entry` without its credentials, as the history is saved.
fn without_secrets(entry: &Entry) -> Entry {
    let mut entry = entry.clone();
    let before = entry.request.headers.len();
    entry.request.headers.retain(|(name, _)| !sensitive(name));
    let mut redacted = entry.request.headers.len() != before;
    if let Some((base, query)) = entry.request.url.clone().split_once('?') {
        let (pairs, fragment) = match query.split_once('#') {
            Some((q, f)) => (q, Some(f)),
            None => (query, None),
        };
        let kept: Vec<&str> = pairs.split('&').filter(|pair| !sensitive(pair.split('=').next().unwrap_or_default())).collect();
        if kept.len() != pairs.split('&').count() {
            redacted = true;
            let mut url = base.to_string();
            if !kept.is_empty() {
                url.push('?');
                url.push_str(&kept.join("&"));
            }
            if let Some(fragment) = fragment {
                url.push('#');
                url.push_str(fragment);
            }
            entry.request.url = url;
        }
    }
    entry.redacted |= redacted;
    entry
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
        redacted: false,
    };
    let entries = entries(cx);
    entries.push(entry);
    let excess = entries.len().saturating_sub(KEEP);
    entries.drain(..excess);
    // In memory for this session as sent; on disk without credentials.
    let saved: Vec<Entry> = entries.iter().map(without_secrets).collect();
    let json = serde_json::to_string(&saved).unwrap_or_default();
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
    let redacted = if entry.redacted { " · credentials not kept" } else { "" };
    format!("{status} · {} ms · {ago} · {file}{redacted}", entry.millis)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saves_requests_without_their_credentials() {
        let entry = Entry {
            file: "api.http".into(),
            request: Resolved {
                name: None,
                method: "GET".into(),
                url: "https://api.example.com/users?page=2&access_token=abc&apiKey=xyz#top".into(),
                headers: vec![("Authorization".into(), "Bearer abc".into()), ("Accept".into(), "application/json".into()), ("X-Api-Key".into(), "k".into())],
                body: None,
            },
            environment: None,
            status: Some(200),
            millis: 12,
            at: 0,
            redacted: false,
        };
        let saved = without_secrets(&entry);
        assert_eq!(saved.request.url, "https://api.example.com/users?page=2#top");
        assert_eq!(saved.request.headers, vec![("Accept".to_string(), "application/json".to_string())]);
        assert!(saved.redacted && describe(&saved, 0).ends_with("credentials not kept"));
        let plain = Entry { request: Resolved { url: "https://x/a?q=1".into(), headers: vec![], ..entry.request.clone() }, ..entry };
        assert!(!without_secrets(&plain).redacted, "nothing to leave out");
    }
}
