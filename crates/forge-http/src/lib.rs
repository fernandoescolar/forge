//! `.http` files: send the request at the cursor (⌘↩) and see the response in a tab beside
//! it. Variables come from the file (`@name = value`), the selected environment of the
//! nearest `http-client.env.json` (and its `.user` overrides, as in Visual Studio), earlier
//! named responses and dynamic values (`{{$guid}}`, `{{$timestamp}}`, …).

mod agent_tools;
pub mod history;
pub mod parse;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow};
use editor::Editor;
use futures::AsyncReadExt as _;
use gpui::{App, AppContext as _, Context, Entity, Global, WeakEntity, Window, actions};
use http_client::{AsyncBody, HttpClient, HttpRequestExt as _, RedirectPolicy};
use multi_buffer::{MultiBuffer, ToPoint as _};
use serde_json::Value;
use workspace::{Workspace, notifications::NotificationId};

use crate::parse::{Body, RequestSpec, Resolver};

actions!(forge_http, [
    /// Chooses the environment (`http-client.env.json`) `.http` requests use.
    SelectEnvironment,
    /// Stops the request the active `.http` file (or its response tab) is waiting on.
    CancelRequest,
    /// Lists the requests sent lately, to send one again.
    RequestHistory
]);

/// Sends the HTTP request at the cursor in a `.http` file (or on `row`, for the run button).
#[derive(PartialEq, Clone, Default, serde::Deserialize, schemars::JsonSchema, gpui::Action)]
#[action(namespace = forge_http)]
#[serde(deny_unknown_fields)]
pub struct SendRequest {
    #[serde(default)]
    pub row: Option<u32>,
}

/// The environments file, looked for from the `.http` file's folder up.
pub const ENV_FILE: &str = "http-client.env.json";
const TIMEOUT: Duration = Duration::from_secs(120);

/// ⌘↩ sends the request at the cursor. Call after the default keymap: Zed binds ⌘↩ in
/// every editor too, and of two bindings for the same editor the later one wins.
pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        gpui::KeyBinding::new("secondary-enter", SendRequest::default(), Some("Editor && extension == http")),
        gpui::KeyBinding::new("secondary-enter", SendRequest::default(), Some("Editor && extension == rest")),
    ]);
}

pub fn init(cx: &mut App) {
    agent_tools::init(cx);
    cx.set_global(HttpState::default());
    // The run button on a request line sends it (instead of running a task). Other run
    // buttons go to whoever handled them before.
    let previous = cx.try_global::<editor::RunIndicatorClick>().cloned();
    cx.set_global(editor::RunIndicatorClick(Arc::new(move |path, row, window, cx| {
        if matches!(path.extension().and_then(|e| e.to_str()), Some("http" | "rest")) {
            window.defer(cx, move |window, cx| window.dispatch_action(Box::new(SendRequest { row: Some(row) }), cx));
            return true;
        }
        previous.as_ref().is_some_and(|previous| (previous.0)(path, row, window, cx))
    })));
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|ws, action: &SendRequest, window, cx| send_at_cursor(ws, action.row, window, cx));
        workspace.register_action(|ws, _: &SelectEnvironment, window, cx| select_environment(ws, window, cx));
        workspace.register_action(|ws, _: &CancelRequest, _, cx| cancel_request(ws, cx));
        workspace.register_action(|ws, _: &RequestHistory, window, cx| history::pick(ws, window, cx));
    })
    .detach();
}

#[derive(Default)]
struct HttpState {
    /// The environment chosen with `SelectEnvironment` (else a default, see `environment`).
    environment: Option<String>,
    /// Named responses, by `.http` file and request name.
    responses: HashMap<(PathBuf, String), Stored>,
    /// Each `.http` file's response tab.
    views: HashMap<PathBuf, WeakEntity<Editor>>,
    /// The request each `.http` file is waiting on; dropping the task stops it.
    requests: HashMap<PathBuf, gpui::Task<()>>,
    /// Requests sent lately, newest last (see `history`).
    history: Option<Vec<history::Entry>>,
}
impl Global for HttpState {}

#[derive(Clone, Debug)]
pub struct Stored {
    pub headers: Vec<(String, String)>,
    pub body: String,
}

/// An `http-client.env.json`: environment name → variables (`$shared` applies to all).
#[derive(Debug, Default, PartialEq)]
pub struct Environments {
    pub names: Vec<String>,
    values: HashMap<String, HashMap<String, String>>,
}

impl Environments {
    /// The nearest environments file above `dir`, with its `.user` file's overrides.
    pub fn find(dir: &Path) -> Self {
        let Some(file) = dir.ancestors().map(|d| d.join(ENV_FILE)).find(|f| f.is_file()) else { return Self::default() };
        let mut envs = Self::default();
        for path in [file.clone(), file.with_extension("json.user")] {
            if let Ok(text) = std::fs::read_to_string(&path) {
                envs.merge(&text);
            }
        }
        envs
    }

    pub fn merge(&mut self, text: &str) {
        let Ok(Value::Object(json)) = serde_json_lenient::from_str::<Value>(text) else { return };
        for (name, vars) in json {
            let Value::Object(vars) = vars else { continue };
            if name != "$shared" && !self.names.contains(&name) {
                self.names.push(name.clone());
            }
            let target = self.values.entry(name).or_default();
            for (key, value) in vars {
                let value = match value {
                    Value::String(s) => s,
                    other => other.to_string(),
                };
                target.insert(key, value);
            }
        }
    }

    /// `chosen` if this file has it, else one named like a development environment, else
    /// the first.
    pub fn pick(&self, chosen: Option<&str>) -> Option<String> {
        if let Some(chosen) = chosen.filter(|c| self.names.iter().any(|n| n == c)) {
            return Some(chosen.to_string());
        }
        ["dev", "development", "local"]
            .iter()
            .find_map(|wanted| self.names.iter().find(|n| n.eq_ignore_ascii_case(wanted)))
            .or(self.names.first())
            .cloned()
    }

    /// `$shared`, overridden by `environment`'s own.
    pub fn variables(&self, environment: Option<&str>) -> HashMap<String, String> {
        let mut vars = self.values.get("$shared").cloned().unwrap_or_default();
        if let Some(own) = environment.and_then(|e| self.values.get(e)) {
            vars.extend(own.clone());
        }
        vars
    }
}

/// Dynamic values and earlier responses, for one `.http` file.
struct FileResolver<'a> {
    file: &'a Path,
    responses: &'a HashMap<(PathBuf, String), Stored>,
}

impl Resolver for FileResolver<'_> {
    fn dynamic(&self, name: &str, args: &[&str]) -> Option<String> {
        let now = chrono::Utc::now();
        match name {
            "guid" | "uuid" | "randomUUID" => Some(uuid::Uuid::new_v4().to_string()),
            "timestamp" => Some(now.timestamp().to_string()),
            "datetime" | "localDatetime" => {
                let local = name == "localDatetime";
                Some(match args.first().copied().unwrap_or("iso8601") {
                    "rfc1123" if local => chrono::Local::now().to_rfc2822(),
                    "rfc1123" => now.to_rfc2822(),
                    _ if local => chrono::Local::now().to_rfc3339(),
                    _ => now.to_rfc3339(),
                })
            }
            "randomInt" => {
                let min: i64 = args.first().and_then(|a| a.parse().ok()).unwrap_or(0);
                let max: i64 = args.get(1).and_then(|a| a.parse().ok()).unwrap_or(1000).max(min + 1);
                let random = uuid::Uuid::new_v4().as_u128() as i64;
                Some((min + random.rem_euclid(max - min)).to_string())
            }
            "processEnv" => std::env::var(args.first()?).ok(),
            "dotenv" => {
                let name = args.first()?;
                let dir = self.file.parent()?;
                let text = dir.ancestors().map(|d| d.join(".env")).find_map(|f| std::fs::read_to_string(f).ok())?;
                text.lines().find_map(|line| {
                    let (key, value) = line.split_once('=')?;
                    (key.trim().trim_start_matches("export ").trim() == *name).then(|| value.trim().trim_matches('"').to_string())
                })
            }
            _ => None,
        }
    }

    fn response(&self, request: &str, path: &str) -> Option<String> {
        let stored = self.responses.get(&(self.file.to_path_buf(), request.to_string()))?;
        if let Some(header) = path.strip_prefix("headers.") {
            return stored.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(header)).map(|(_, v)| v.clone());
        }
        let body_path = path.strip_prefix("body")?;
        match body_path.strip_prefix('.') {
            None | Some("*") => Some(stored.body.clone()),
            Some(json_path) => parse::json_path(&serde_json::from_str(&stored.body).ok()?, json_path),
        }
    }
}

/// A request ready to send.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Resolved {
    pub name: Option<String>,
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
}

/// Resolves `spec`'s variables. Errors name the variables nothing defines.
pub fn resolve(spec: &RequestSpec, text: &str, file: &Path, environment: Option<&str>, envs: &Environments, responses: &HashMap<(PathBuf, String), Stored>) -> Result<Resolved> {
    let mut variables = envs.variables(environment);
    variables.extend(parse::file_variables(text));
    let resolver = FileResolver { file, responses };
    let mut missing = Vec::new();
    let mut sub = |value: &str| parse::substitute(value, &variables, &resolver).unwrap_or_else(|names| {
        missing.extend(names);
        String::new()
    });
    let url = sub(&spec.url);
    let headers = spec.headers.iter().map(|(k, v)| (k.clone(), sub(v))).collect();
    let body = match &spec.body {
        None => None,
        Some(Body::Text(text)) => Some(sub(text)),
        Some(Body::File(path)) => {
            let path = file.parent().unwrap_or(Path::new(".")).join(path);
            Some(std::fs::read_to_string(&path).with_context(|| format!("cannot read the body from {}", path.display()))?)
        }
    };
    if !missing.is_empty() {
        missing.dedup();
        let hint = match environment {
            Some(env) => format!(" (environment: {env})"),
            None if envs.names.is_empty() => String::new(),
            None => " (no environment selected)".into(),
        };
        return Err(anyhow!("undefined: {}{hint}", missing.join(", ")));
    }
    Ok(Resolved { name: spec.name.clone(), method: spec.method.clone(), url, headers, body })
}

/// What came back.
#[derive(Debug)]
pub struct Exchange {
    pub status: u16,
    pub reason: String,
    pub elapsed: Duration,
    pub headers: Vec<(String, String)>,
    /// The body, or its first [`MAX_BODY`] bytes when it is longer (see `spilled`).
    pub body: Vec<u8>,
    /// How many bytes of body arrived.
    pub received: usize,
    /// Where the whole body went once it outgrew [`MAX_BODY`].
    pub spilled: Option<PathBuf>,
}

/// Body bytes kept in memory and shown, at most: a longer response (a big download, a
/// stream left open) goes to a file as it arrives instead of growing without end.
pub const MAX_BODY: usize = 8 * 1024 * 1024;

pub async fn send(client: Arc<dyn HttpClient>, request: &Resolved) -> Result<Exchange> {
    send_streaming(client, request, |_| {}).await
}

/// Sends `request`, calling `progress` with the response so far (status, headers and the
/// body received) as it arrives, then returns all of it.
pub async fn send_streaming(client: Arc<dyn HttpClient>, request: &Resolved, progress: impl FnMut(&Exchange)) -> Result<Exchange> {
    send_limited(client, request, MAX_BODY, progress).await
}

/// [`send_streaming`], keeping `max_body` bytes of body in memory: past that, the whole
/// body is written to a file in Forge's temporary folder as it arrives.
pub async fn send_limited(client: Arc<dyn HttpClient>, request: &Resolved, max_body: usize, mut progress: impl FnMut(&Exchange)) -> Result<Exchange> {
    use std::io::Write as _;
    let mut builder = http_client::Request::builder().method(request.method.as_str()).uri(request.url.as_str()).follow_redirects(RedirectPolicy::FollowLimit(10)).timeout(TIMEOUT);
    for (key, value) in &request.headers {
        builder = builder.header(key.as_str(), value.as_str());
    }
    let body = request.body.clone().map(AsyncBody::from).unwrap_or_else(AsyncBody::empty);
    let started = Instant::now();
    let mut response = client.send(builder.body(body)?).await?;
    let mut exchange = Exchange {
        status: response.status().as_u16(),
        reason: response.status().canonical_reason().unwrap_or_default().to_string(),
        elapsed: started.elapsed(),
        headers: response.headers().iter().map(|(k, v)| (k.to_string(), v.to_str().unwrap_or_default().to_string())).collect(),
        body: Vec::new(),
        received: 0,
        spilled: None,
    };
    progress(&exchange);
    let mut chunk = vec![0u8; 16 * 1024];
    let mut spill: Option<std::fs::File> = None;
    loop {
        let read = response.body_mut().read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        let data = &chunk[..read];
        exchange.received += read;
        if spill.is_none() && exchange.body.len() + read > max_body {
            let path = body_path(&request_stem(request), &exchange)?;
            let mut file = std::fs::File::create(&path).with_context(|| format!("cannot write {}", path.display()))?;
            file.write_all(&exchange.body)?;
            exchange.spilled = Some(path);
            spill = Some(file);
        }
        let room = max_body.saturating_sub(exchange.body.len());
        exchange.body.extend_from_slice(&data[..read.min(room)]);
        if let Some(file) = &mut spill {
            file.write_all(data).context("cannot write the response to disk")?;
        }
        exchange.elapsed = started.elapsed();
        progress(&exchange);
    }
    exchange.elapsed = started.elapsed();
    Ok(exchange)
}

/// A name for files of `request`'s response: the request's name, or the last part of its
/// URL's path.
fn request_stem(request: &Resolved) -> String {
    let from_url = request.url.split(['?', '#']).next().unwrap_or_default().trim_end_matches('/').rsplit('/').next().unwrap_or_default().to_string();
    let stem = request.name.clone().unwrap_or(from_url);
    let clean: String = stem.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') { c } else { '-' }).take(40).collect();
    if clean.is_empty() { "response".into() } else { clean }
}

/// A new file in Forge's temporary folder for a body: `<stem>-<time>.<extension>`.
fn body_path(stem: &str, exchange: &Exchange) -> Result<PathBuf> {
    let dir = std::env::temp_dir().join("forge-http");
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join(format!("{stem}-{}.{}", chrono::Local::now().format("%Y%m%d-%H%M%S%3f"), exchange.extension())))
}

impl Exchange {
    pub fn content_type(&self) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("content-type")).map(|(_, v)| v.as_str())
    }

    /// Whether the body is something other than text: images, archives, PDFs, or bytes
    /// that aren't UTF-8.
    pub fn is_binary(&self) -> bool {
        let kind = self.content_type().unwrap_or_default().split(';').next().unwrap_or_default().trim().to_ascii_lowercase();
        let textual = kind.starts_with("text/") || ["json", "xml", "javascript", "html", "yaml", "x-www-form-urlencoded", "graphql", "csv"].iter().any(|t| kind.contains(t));
        let binary_kind = ["image/", "audio/", "video/", "font/"].iter().any(|t| kind.starts_with(t))
            || ["application/pdf", "application/zip", "application/gzip", "application/octet-stream", "application/wasm"].contains(&kind.as_str());
        if textual {
            return false;
        }
        binary_kind || self.body.contains(&0) || std::str::from_utf8(&self.body).is_err()
    }

    /// The file extension for the body's type.
    fn extension(&self) -> &'static str {
        let kind = self.content_type().unwrap_or_default().split(';').next().unwrap_or_default().trim().to_ascii_lowercase();
        match kind.as_str() {
            "image/png" => "png",
            "image/jpeg" => "jpg",
            "image/gif" => "gif",
            "image/webp" => "webp",
            "image/svg+xml" => "svg",
            "application/pdf" => "pdf",
            "application/zip" => "zip",
            "application/gzip" => "gz",
            _ => "bin",
        }
    }
}

/// Saves a binary body to Forge's temporary folder (named after `file`); returns where.
/// A body that outgrew memory is already in a file: that one.
pub fn save_body(file: &Path, exchange: &Exchange) -> Result<PathBuf> {
    if let Some(path) = &exchange.spilled {
        return Ok(path.clone());
    }
    let stem = file.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "response".into());
    let path = body_path(&stem, exchange)?;
    std::fs::write(&path, &exchange.body)?;
    Ok(path)
}

/// The response tab's first line while the body is still arriving.
pub fn status_line(exchange: &Exchange) -> String {
    format!("{} {} · {} ms · receiving {}…", exchange.status, exchange.reason, exchange.elapsed.as_millis(), size(exchange.received))
}

/// The response tab when its status and headers arrived: the status line, the request,
/// the headers and a blank line, which the body then follows as it arrives.
pub fn render_head(request: &Resolved, exchange: &Exchange) -> String {
    let mut out = format!("{}\n{} {}\n\n", status_line(exchange), request.method, request.url);
    for (key, value) in &exchange.headers {
        out.push_str(&format!("{key}: {value}\n"));
    }
    out.push('\n');
    out
}

/// The response tab's text, and whether its body is JSON (shown as JSONC, headers as
/// comments). A body that outgrew memory shows its beginning and where the rest is.
pub fn render(request: &Resolved, environment: Option<&str>, exchange: &Exchange, saved: Option<&Path>) -> (String, bool) {
    let text = match (saved, &exchange.spilled) {
        (Some(path), _) => format!("({} of {}, saved to {})", size(exchange.received), exchange.content_type().unwrap_or("binary data"), path.display()),
        (None, Some(path)) => format!(
            "{}\n… ({} more: the response is {}, all of it in {})",
            String::from_utf8_lossy(&exchange.body),
            size(exchange.received - exchange.body.len()),
            size(exchange.received),
            path.display()
        ),
        (None, None) => String::from_utf8_lossy(&exchange.body).into_owned(),
    };
    let complete = saved.is_none() && exchange.spilled.is_none();
    let json = complete.then(|| serde_json::from_str::<Value>(&text).ok()).flatten().filter(|v| v.is_object() || v.is_array());
    let prefix = if json.is_some() { "// " } else { "" };
    let mut out = format!("{prefix}{} {} · {} ms · {}\n", exchange.status, exchange.reason, exchange.elapsed.as_millis(), size(exchange.received));
    out.push_str(&format!("{prefix}{} {}{}\n", request.method, request.url, environment.map(|e| format!("  (environment: {e})")).unwrap_or_default()));
    out.push_str(&format!("{prefix}\n"));
    for (key, value) in &exchange.headers {
        out.push_str(&format!("{prefix}{key}: {value}\n"));
    }
    out.push('\n');
    match &json {
        Some(json) => out.push_str(&serde_json::to_string_pretty(json).unwrap_or(text)),
        None => out.push_str(&text),
    }
    out.push('\n');
    (out, json.is_some())
}

fn size(bytes: usize) -> String {
    match bytes {
        0..1024 => format!("{bytes} B"),
        1024..1_048_576 => format!("{:.1} KB", bytes as f64 / 1024.),
        _ => format!("{:.1} MB", bytes as f64 / 1_048_576.),
    }
}

/// The `.http` file of the active editor, its text and the cursor's row.
fn active_http_file(workspace: &Workspace, cx: &App) -> Option<(PathBuf, String, u32)> {
    let editor = workspace.active_item(cx)?.downcast::<Editor>()?;
    let editor = editor.read(cx);
    let buffer = editor.buffer().read(cx).as_singleton()?;
    let path = buffer.read(cx).file()?.as_local()?.abs_path(cx);
    if !matches!(path.extension().and_then(|e| e.to_str()), Some("http" | "rest")) {
        return None;
    }
    let row = editor.selections.newest_anchor().head().to_point(&editor.buffer().read(cx).snapshot(cx)).row;
    Some((path, buffer.read(cx).text(), row))
}

fn toast(workspace: &mut Workspace, message: impl Into<String>, cx: &mut Context<Workspace>) {
    workspace.show_toast(workspace::Toast::new(NotificationId::named("forge-http".into()), message.into()), cx);
}

/// Sends the request on `row` of the active `.http` file (the cursor's when `None`).
pub fn send_at_cursor(workspace: &mut Workspace, row: Option<u32>, window: &mut Window, cx: &mut Context<Workspace>) {
    let Some((file, text, cursor_row)) = active_http_file(workspace, cx) else {
        return toast(workspace, "Put the cursor on a request in a .http file to send it.", cx);
    };
    let Some(spec) = parse::request_at(&text, row.unwrap_or(cursor_row)) else {
        return toast(workspace, "There is no request here.", cx);
    };
    let envs = Environments::find(file.parent().unwrap_or(Path::new("/")));
    let state = cx.global::<HttpState>();
    let environment = envs.pick(state.environment.as_deref());
    let resolved = match resolve(&spec, &text, &file, environment.as_deref(), &envs, &state.responses) {
        Ok(resolved) => resolved,
        Err(e) => return toast(workspace, format!("Can't send the request: {e:#}"), cx),
    };
    dispatch(workspace, file, resolved, environment, window, cx);
}

/// How a request sent from code (see [`send_request_at`], [`send_adhoc`]) went: what was
/// sent, in which environment, and what came back (or why it failed).
pub struct Outcome {
    pub request: Resolved,
    pub environment: Option<String>,
    pub response: Result<Exchange, String>,
}

/// Sends the request on `row` (zero-based) of the `.http` file `file`, as ⌘↩ there would:
/// with the selected environment, shown in the file's response tab.
pub fn send_request_at(workspace: &mut Workspace, file: &Path, row: u32, window: &mut Window, cx: &mut Context<Workspace>) -> Result<futures::channel::oneshot::Receiver<Outcome>> {
    let text = std::fs::read_to_string(file).with_context(|| format!("can't read {}", file.display()))?;
    let spec = parse::request_at(&text, row).ok_or_else(|| anyhow!("there is no request on line {}", row + 1))?;
    send_spec(workspace, file, &text, spec, window, cx)
}

/// Sends a request that isn't in a `.http` file, resolved as if it were in `file` (its
/// environments and `{{variables}}`), and shown in that file's response tab.
pub fn send_adhoc(workspace: &mut Workspace, file: &Path, method: &str, url: &str, headers: Vec<(String, String)>, body: Option<String>, window: &mut Window, cx: &mut Context<Workspace>) -> Result<futures::channel::oneshot::Receiver<Outcome>> {
    let spec = parse::RequestSpec { name: None, method: method.to_uppercase(), url: url.to_string(), headers, body: body.map(parse::Body::Text), row: 0 };
    send_spec(workspace, file, "", spec, window, cx)
}

fn send_spec(workspace: &mut Workspace, file: &Path, text: &str, spec: parse::RequestSpec, window: &mut Window, cx: &mut Context<Workspace>) -> Result<futures::channel::oneshot::Receiver<Outcome>> {
    let envs = Environments::find(file.parent().unwrap_or(Path::new("/")));
    let state = cx.global::<HttpState>();
    let environment = envs.pick(state.environment.as_deref());
    let resolved = resolve(&spec, text, file, environment.as_deref(), &envs, &state.responses)?;
    let (tx, rx) = futures::channel::oneshot::channel();
    dispatch_reporting(workspace, file.to_path_buf(), resolved, environment, Some(tx), window, cx);
    Ok(rx)
}

/// Sends `resolved` for the `.http` file `file`: its response tab shows the response as
/// it arrives. A request the file was still waiting on is dropped, which stops it.
pub(crate) fn dispatch(workspace: &mut Workspace, file: PathBuf, resolved: Resolved, environment: Option<String>, window: &mut Window, cx: &mut Context<Workspace>) {
    dispatch_reporting(workspace, file, resolved, environment, None, window, cx);
}

/// [`dispatch`], telling `done` how it went.
fn dispatch_reporting(workspace: &mut Workspace, file: PathBuf, resolved: Resolved, environment: Option<String>, done: Option<futures::channel::oneshot::Sender<Outcome>>, window: &mut Window, cx: &mut Context<Workspace>) {
    let client = cx.http_client();
    let view = response_view(workspace, &file, window, cx);
    set_text(&view, format!("// Sending {} {}…\n", resolved.method, resolved.url), false, cx);
    let task_file = file.clone();
    let workspace_handle = cx.entity().downgrade();
    let task = cx.spawn_in(window, async move |_, cx| {
        // The request runs off the UI thread and sends what is new (the status and headers
        // first, then each piece of body); the tab adds it at its end, at most every 100 ms,
        // instead of setting its whole text again.
        let (updates_tx, mut updates) = futures::channel::mpsc::unbounded::<Arrived>();
        let sending = cx.background_spawn({
            let resolved = resolved.clone();
            async move {
                let mut sent = 0;
                send_streaming(client, &resolved, |partial| {
                    let arrived = Arrived::since(partial, sent);
                    sent = partial.body.len();
                    updates_tx.unbounded_send(arrived).ok();
                })
                .await
            }
        });
        let mut shown = Shown::default();
        while let Some(first) = futures::StreamExt::next(&mut updates).await {
            let mut batch = vec![first];
            while let Ok(more) = updates.try_recv() {
                batch.push(more);
            }
            cx.update(|_, cx| shown.show(&view, &resolved, batch, cx)).ok();
            cx.background_executor().timer(Duration::from_millis(100)).await;
        }
        let result = sending.await;
        // Saving a binary body and laying out (and pretty-printing) a large one, off the UI
        // thread too.
        let (result, saved, text, json) = cx
            .background_spawn({
                let (file, resolved, environment) = (file.clone(), resolved.clone(), environment.clone());
                async move {
                    let saved = match &result {
                        Ok(exchange) if exchange.is_binary() => save_body(&file, exchange).map_err(|e| log::error!("cannot save the response: {e:#}")).ok(),
                        _ => None,
                    };
                    let (text, json) = match &result {
                        Ok(exchange) => render(&resolved, environment.as_deref(), exchange, saved.as_deref()),
                        Err(e) => (format!("// {} {}\n// Request failed: {e:#}\n", resolved.method, resolved.url), true),
                    };
                    (result, saved, text, json)
                }
            })
            .await;
        cx.update(|_, cx| {
            if let (Ok(exchange), Some(name)) = (&result, &resolved.name) {
                let stored = Stored { headers: exchange.headers.clone(), body: String::from_utf8_lossy(&exchange.body).into_owned() };
                cx.global_mut::<HttpState>().responses.insert((file.clone(), name.clone()), stored);
            }
            history::record(&file, &resolved, environment.as_deref(), result.as_ref().ok(), cx);
            cx.global_mut::<HttpState>().requests.remove(&file);
            set_text(&view, text, json, cx);
            if let Some(path) = saved {
                let toast = workspace::Toast::new(NotificationId::named("forge-http-saved".into()), format!("The response is binary; saved to {}.", path.display()))
                    .on_click("Open", move |_, cx| cx.open_with_system(&path));
                workspace_handle.update(cx, |ws, cx| ws.show_toast(toast, cx)).ok();
            }
        })
        .ok();
        if let Some(done) = done {
            let _ = done.send(Outcome { request: resolved, environment, response: result.map_err(|e| format!("{e:#}")) });
        }
    });
    cx.global_mut::<HttpState>().requests.insert(task_file, task);
}

/// What arrived since the last update of a response being received.
pub struct Arrived {
    status: String,
    /// The tab's head (status, request, headers), the first time.
    head: Option<Exchange>,
    /// Body bytes new since the last update (none once the shown part is full).
    bytes: Vec<u8>,
    binary: bool,
}

impl Arrived {
    /// What `partial` has beyond the first `sent` bytes of body.
    fn since(partial: &Exchange, sent: usize) -> Self {
        let head = (sent == 0 && partial.received == 0).then(|| Exchange {
            status: partial.status,
            reason: partial.reason.clone(),
            elapsed: partial.elapsed,
            headers: partial.headers.clone(),
            body: Vec::new(),
            received: 0,
            spilled: None,
        });
        Self { status: status_line(partial), head, bytes: partial.body[sent.min(partial.body.len())..].to_vec(), binary: partial.is_binary() }
    }
}

/// What a response tab shows of a response being received.
#[derive(Default)]
struct Shown {
    started: bool,
    /// Bytes of a character split between two pieces, waiting for the rest of it.
    pending: Vec<u8>,
}

impl Shown {
    /// Adds what arrived: the head the first time, then the new text at the end, and the
    /// status line (how much arrived) on the first line.
    fn show(&mut self, view: &Entity<Editor>, request: &Resolved, batch: Vec<Arrived>, cx: &mut App) {
        let Some(last) = batch.last() else { return };
        let (status, binary) = (last.status.clone(), last.binary);
        let mut head = None;
        for arrived in batch {
            head = head.or(arrived.head);
            if !arrived.binary {
                self.pending.extend(arrived.bytes);
            }
        }
        let text = if binary { String::new() } else { self.take_text() };
        let Some(buffer) = view.read(cx).buffer().read(cx).as_singleton() else { return };
        if !self.started {
            let Some(head) = head else { return };
            self.started = true;
            set_text(view, render_head(request, &head) + &text, false, cx);
            buffer.update(cx, |buffer, cx| replace_first_line(buffer, &status, cx));
            return;
        }
        buffer.update(cx, |buffer, cx| {
            if !text.is_empty() {
                let end = buffer.len();
                buffer.edit([(end..end, text)], None, cx);
            }
            replace_first_line(buffer, &status, cx);
        });
    }

    /// The text of the bytes waiting, keeping a character cut in half for later.
    fn take_text(&mut self) -> String {
        let valid = match std::str::from_utf8(&self.pending) {
            Ok(_) => self.pending.len(),
            Err(e) if e.error_len().is_none() => e.valid_up_to(),
            Err(_) => self.pending.len(),
        };
        let text = String::from_utf8_lossy(&self.pending[..valid]).into_owned();
        self.pending.drain(..valid);
        text
    }
}

fn replace_first_line(buffer: &mut language::Buffer, line: &str, cx: &mut Context<language::Buffer>) {
    let end = buffer.text().find('\n').unwrap_or(buffer.len());
    buffer.edit([(0..end, line)], None, cx);
}

/// The `.http` file the active item is about: the file itself, or its response tab.
fn file_of_active_item(workspace: &Workspace, cx: &App) -> Option<PathBuf> {
    if let Some((file, _, _)) = active_http_file(workspace, cx) {
        return Some(file);
    }
    let active = workspace.active_item(cx)?.downcast::<Editor>()?;
    cx.global::<HttpState>().views.iter().find(|(_, view)| view.upgrade().as_ref() == Some(&active)).map(|(file, _)| file.clone())
}

fn cancel_request(workspace: &mut Workspace, cx: &mut Context<Workspace>) {
    let Some(file) = file_of_active_item(workspace, cx) else { return };
    if cx.global_mut::<HttpState>().requests.remove(&file).is_none() {
        return toast(workspace, "No request is running for this file.", cx);
    }
    if let Some(view) = cx.global::<HttpState>().views.get(&file).and_then(|v| v.upgrade()) {
        set_text(&view, "// Cancelled.\n".into(), false, cx);
    }
}

/// The `.http` file's response tab: the existing one, or a new one to the right.
fn response_view(workspace: &mut Workspace, file: &Path, window: &mut Window, cx: &mut Context<Workspace>) -> Entity<Editor> {
    if let Some(view) = cx.global::<HttpState>().views.get(file).and_then(|v| v.upgrade()) {
        if let Some(pane) = workspace.panes().iter().find(|p| p.read(cx).index_for_item(&view).is_some()).cloned() {
            pane.update(cx, |pane, cx| {
                if let Some(ix) = pane.index_for_item(&view) {
                    pane.activate_item(ix, false, false, window, cx);
                }
            });
            return view;
        }
    }
    let project = workspace.project().clone();
    let buffer = project.update(cx, |p, cx| p.create_local_buffer("", None, false, cx));
    let title = format!("Response · {}", file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
    let multibuffer = cx.new(|cx| MultiBuffer::singleton(buffer, cx).with_title(title));
    let view = cx.new(|cx| {
        let mut editor = Editor::for_multibuffer(multibuffer, Some(project), window, cx);
        editor.set_read_only(true);
        editor
    });
    // The response goes beside the request, which keeps the focus.
    let source = workspace.active_item(cx);
    workspace.split_item(workspace::SplitDirection::Right, Box::new(view.clone()), window, cx);
    if let Some(source) = source {
        workspace.activate_item(source.as_ref(), true, true, window, cx);
    }
    cx.global_mut::<HttpState>().views.insert(file.to_path_buf(), view.downgrade());
    view
}

fn set_text(view: &Entity<Editor>, text: String, json: bool, cx: &mut App) {
    let Some(buffer) = view.read(cx).buffer().read(cx).as_singleton() else { return };
    let registry = view.read(cx).project().map(|p| p.read(cx).languages().clone());
    buffer.update(cx, |buffer, cx| buffer.set_text(text, cx));
    let Some(registry) = registry.filter(|_| json) else {
        buffer.update(cx, |buffer, cx| buffer.set_language(None, cx));
        return;
    };
    let buffer = buffer.downgrade();
    cx.spawn(async move |cx| {
        let language = registry.language_for_name("JSONC").await.ok();
        buffer.update(cx, |buffer, cx| buffer.set_language(language, cx)).ok();
    })
    .detach();
}

/// Lets the user choose the environment of the active `.http` file's environments file.
fn select_environment(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let dir = active_http_file(workspace, cx)
        .and_then(|(file, _, _)| file.parent().map(Path::to_path_buf))
        .or_else(|| workspace.visible_worktrees(cx).next().map(|t| t.read(cx).abs_path().to_path_buf()));
    let envs = dir.map(|d| Environments::find(&d)).unwrap_or_default();
    if envs.names.is_empty() {
        return toast(workspace, format!("No {ENV_FILE} with environments next to this file or above it."), cx);
    }
    let current = envs.pick(cx.global::<HttpState>().environment.as_deref());
    let choices = envs
        .names
        .iter()
        .map(|n| forge_ui::pick::Choice::new(n.clone()).detail(if Some(n) == current.as_ref() { "current" } else { "" }))
        .collect();
    let names = envs.names.clone();
    forge_ui::pick::pick(workspace, "Environment for .http requests…", choices, window, cx, move |ix, _, cx| {
        cx.global_mut::<HttpState>().environment = names.get(ix).cloned();
    });
}

#[cfg(test)]
mod tests;
