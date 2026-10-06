//! Aspire's IDE execution protocol (`docs/specs/IDE-execution.md` in dotnet/aspire): the
//! endpoint an app host's orchestrator (DCP) asks to run the app's projects, so Forge can
//! start each one under the debugger. DCP finds it through the `DEBUG_SESSION_*` variables
//! of `Endpoint::env`; it sends `PUT /run_session` to start a project, `DELETE
//! /run_session/<id>` to stop it, and listens on the `/run_session/notify` WebSocket for
//! the sessions' output and ends.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result};
use async_tungstenite::WebSocketStream;
use async_tungstenite::tungstenite::Message;
use async_tungstenite::tungstenite::protocol::Role;
use futures::channel::{mpsc, oneshot};
use futures::{AsyncReadExt as _, AsyncWriteExt as _, FutureExt as _, StreamExt as _};
use serde::Deserialize;
use serde_json::{Value, json};

/// The protocol versions Forge speaks (Aspire 9.x and 13+).
const PROTOCOLS: &[&str] = &["2024-03-03", "2024-04-23", "2025-10-01"];

/// What DCP asks of Forge.
#[derive(Debug, PartialEq)]
pub enum Request {
    Start(RunSession),
    Stop(String),
}

/// A project to run, from a `PUT /run_session`.
#[derive(Debug, PartialEq)]
pub struct RunSession {
    pub id: String,
    /// A `.csproj`, or the `.cs` of a file-based app.
    pub project: PathBuf,
    /// `false` for `NoDebug` runs.
    pub debug: bool,
    pub launch_profile: Option<String>,
    pub disable_launch_profile: bool,
    /// Applied on top of the launch profile's.
    pub env: Vec<(String, String)>,
    /// When present (even empty), replace the launch profile's arguments.
    pub args: Option<Vec<String>>,
}

/// What Forge tells DCP about a session.
#[derive(Debug, PartialEq)]
pub enum Notification {
    Started { pid: u32 },
    Terminated { exit_code: Option<u32> },
    Output { stderr: bool, text: String },
    Error { message: String },
}

impl Notification {
    fn to_json(&self, session_id: &str) -> Value {
        match self {
            Notification::Started { pid } => json!({ "notification_type": "processRestarted", "session_id": session_id, "pid": pid }),
            Notification::Terminated { exit_code } => {
                let mut value = json!({ "notification_type": "sessionTerminated", "session_id": session_id });
                if let Some(code) = exit_code {
                    value["exit_code"] = json!(code);
                }
                value
            }
            Notification::Output { stderr, text } => {
                json!({ "notification_type": "serviceLogs", "session_id": session_id, "is_std_err": stderr, "log_message": text })
            }
            Notification::Error { message } => {
                json!({ "notification_type": "sessionMessage", "session_id": session_id, "level": "error", "code": "ForgeError", "message": message })
            }
        }
    }
}

struct Shared {
    port: u16,
    token: String,
    requests: mpsc::UnboundedSender<Request>,
    subscribers: Mutex<Vec<mpsc::UnboundedSender<String>>>,
    live: Mutex<HashSet<String>>,
    next_id: AtomicU64,
}

/// A running endpoint, served from a thread of its own. Dropping it stops listening.
pub struct Endpoint {
    shared: Arc<Shared>,
    stop: Option<oneshot::Sender<()>>,
}

impl Endpoint {
    /// Listens on a free localhost port; DCP's requests arrive on the returned channel.
    pub fn start() -> Result<(Self, mpsc::UnboundedReceiver<Request>)> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").context("no port for the Aspire endpoint")?;
        let port = listener.local_addr()?.port();
        let listener = async_net::TcpListener::try_from(listener)?;
        let (requests, received) = mpsc::unbounded();
        let shared = Arc::new(Shared {
            port,
            token: uuid::Uuid::new_v4().simple().to_string(),
            requests,
            subscribers: Mutex::default(),
            live: Mutex::default(),
            next_id: AtomicU64::new(1),
        });
        let (stop, stopped) = oneshot::channel::<()>();
        std::thread::Builder::new().name("aspire-endpoint".into()).spawn({
            let shared = shared.clone();
            move || {
                let executor = smol::Executor::new();
                smol::block_on(executor.run(async {
                    let accept = async {
                        while let Ok((stream, _)) = listener.accept().await {
                            let shared = shared.clone();
                            executor
                                .spawn(async move {
                                    if let Err(error) = serve(stream, &shared).await {
                                        log::debug!("Aspire endpoint: {error:#}");
                                    }
                                })
                                .detach();
                        }
                    };
                    futures::future::select(Box::pin(accept), stopped).await;
                }));
            }
        })?;
        Ok((Self { shared, stop: Some(stop) }, received))
    }

    /// The variables that point an app host at this endpoint, with every project run
    /// through it in debug mode.
    pub fn env(&self) -> Vec<(String, String)> {
        let info = json!({ "protocols_supported": PROTOCOLS, "supported_launch_configurations": ["project"] });
        vec![
            ("DEBUG_SESSION_PORT".into(), format!("localhost:{}", self.shared.port)),
            ("DEBUG_SESSION_TOKEN".into(), self.shared.token.clone()),
            ("DEBUG_SESSION_INFO".into(), info.to_string()),
            ("DEBUG_SESSION_RUN_MODE".into(), "Debug".into()),
        ]
    }

    pub fn notify(&self, session_id: &str, notification: Notification) {
        if matches!(notification, Notification::Terminated { .. }) && !self.shared.live.lock().unwrap().remove(session_id) {
            return; // already reported
        }
        let line = notification.to_json(session_id).to_string();
        self.shared.subscribers.lock().unwrap().retain(|subscriber| subscriber.unbounded_send(line.clone()).is_ok());
    }
}

impl Drop for Endpoint {
    /// Closes the notification streams and stops listening.
    fn drop(&mut self) {
        self.shared.subscribers.lock().unwrap().clear();
        if let Some(stop) = self.stop.take() {
            stop.send(()).ok();
        }
    }
}

#[derive(Deserialize)]
struct CreateSession {
    launch_configurations: Vec<Value>,
    #[serde(default)]
    env: Vec<EnvVar>,
    args: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct EnvVar {
    name: String,
    #[serde(default)]
    value: String,
}

#[derive(Deserialize)]
struct ProjectLaunch {
    project_path: PathBuf,
    mode: Option<String>,
    launch_profile: Option<String>,
    #[serde(default)]
    disable_launch_profile: bool,
}

struct HttpRequest {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl HttpRequest {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(key, _)| key.eq_ignore_ascii_case(name)).map(|(_, value)| value.as_str())
    }
}

async fn serve(mut stream: async_net::TcpStream, shared: &Shared) -> Result<()> {
    let request = read_request(&mut stream).await?;
    if request.header("authorization") != Some(&format!("Bearer {}", shared.token)) {
        return respond(&mut stream, "401 Unauthorized", &[], None).await;
    }
    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/info") => {
            let info = json!({ "protocols_supported": PROTOCOLS, "supported_launch_configurations": ["project"] });
            respond(&mut stream, "200 OK", &[], Some(info)).await
        }
        ("GET", "/run_session/notify") => {
            let key = request.header("sec-websocket-key").context("not a WebSocket upgrade")?;
            let accept = async_tungstenite::tungstenite::handshake::derive_accept_key(key.as_bytes());
            let head = format!("HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: {accept}\r\n\r\n");
            stream.write_all(head.as_bytes()).await?;
            let socket = WebSocketStream::from_raw_socket(stream, Role::Server, None).await;
            let (lines, received) = mpsc::unbounded();
            shared.subscribers.lock().unwrap().push(lines);
            notify_loop(socket, received).await
        }
        ("PUT", "/run_session") => match parse_session(&request.body, shared) {
            Ok(session) => {
                let location = format!("http://localhost:{}/run_session/{}", shared.port, session.id);
                shared.live.lock().unwrap().insert(session.id.clone());
                shared.requests.unbounded_send(Request::Start(session)).ok();
                respond(&mut stream, "201 Created", &[("Location", &location)], None).await
            }
            Err(error) => {
                let body = json!({ "error": { "code": "BadRequest", "message": format!("{error:#}") } });
                respond(&mut stream, "400 Bad Request", &[], Some(body)).await
            }
        },
        ("DELETE", path) if path.starts_with("/run_session/") => {
            let id = path.trim_start_matches("/run_session/").to_string();
            if shared.live.lock().unwrap().contains(&id) {
                shared.requests.unbounded_send(Request::Stop(id)).ok();
                respond(&mut stream, "200 OK", &[], None).await
            } else {
                respond(&mut stream, "204 No Content", &[], None).await
            }
        }
        _ => respond(&mut stream, "404 Not Found", &[], None).await,
    }
}

fn parse_session(body: &[u8], shared: &Shared) -> Result<RunSession> {
    let request: CreateSession = serde_json::from_slice(body).context("the request is not a run session")?;
    let project = request
        .launch_configurations
        .into_iter()
        .find(|config| config.get("type").and_then(Value::as_str) == Some("project"))
        .context("Forge only runs project launch configurations")?;
    let project: ProjectLaunch = serde_json::from_value(project).context("invalid project launch configuration")?;
    Ok(RunSession {
        id: shared.next_id.fetch_add(1, Ordering::Relaxed).to_string(),
        project: project.project_path,
        debug: project.mode.as_deref().is_none_or(|mode| !mode.eq_ignore_ascii_case("NoDebug")),
        // DCP sends an empty name for "none given".
        launch_profile: project.launch_profile.filter(|name| !name.is_empty()),
        disable_launch_profile: project.disable_launch_profile,
        env: request.env.into_iter().map(|var| (var.name, var.value)).collect(),
        args: request.args,
    })
}

/// Streams notifications to DCP until either side closes. Reading answers its pings.
async fn notify_loop(socket: WebSocketStream<async_net::TcpStream>, mut lines: mpsc::UnboundedReceiver<String>) -> Result<()> {
    let (mut sink, mut incoming) = socket.split();
    loop {
        futures::select_biased! {
            message = incoming.next().fuse() => match message {
                Some(Ok(Message::Close(_))) | None => return Ok(()),
                Some(Ok(_)) => {}
                Some(Err(error)) => return Err(error.into()),
            },
            line = lines.next().fuse() => match line {
                Some(line) => sink.send(Message::text(line)).await?,
                None => {
                    sink.send(Message::Close(None)).await.ok();
                    return Ok(());
                }
            },
        }
    }
}

async fn read_request(stream: &mut async_net::TcpStream) -> Result<HttpRequest> {
    let mut data = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(end) = data.windows(4).position(|w| w == b"\r\n\r\n") {
            break end;
        }
        anyhow::ensure!(data.len() < 64 * 1024, "request head too large");
        let read = stream.read(&mut chunk).await?;
        anyhow::ensure!(read > 0, "connection closed");
        data.extend_from_slice(&chunk[..read]);
    };
    let head = String::from_utf8_lossy(&data[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut start = lines.next().unwrap_or_default().split_whitespace();
    let method = start.next().unwrap_or_default().to_string();
    let target = start.next().unwrap_or_default();
    let path = target.split('?').next().unwrap_or_default().to_string();
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key.trim().to_string(), value.trim().to_string()))
        .collect();
    let length: usize = headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("content-length")).and_then(|(_, v)| v.parse().ok()).unwrap_or(0);
    anyhow::ensure!(length <= 16 * 1024 * 1024, "request body too large");
    let mut body = data[head_end + 4..].to_vec();
    while body.len() < length {
        let read = stream.read(&mut chunk).await?;
        anyhow::ensure!(read > 0, "connection closed");
        body.extend_from_slice(&chunk[..read]);
    }
    body.truncate(length);
    Ok(HttpRequest { method, path, headers, body })
}

async fn respond(stream: &mut async_net::TcpStream, status: &str, headers: &[(&str, &str)], body: Option<Value>) -> Result<()> {
    let body = body.map(|b| b.to_string()).unwrap_or_default();
    let mut head = format!("HTTP/1.1 {status}\r\nConnection: close\r\nContent-Length: {}\r\n", body.len());
    if !body.is_empty() {
        head.push_str("Content-Type: application/json\r\n");
    }
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body.as_bytes()).await?;
    stream.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::FutureExt;

    async fn http(port: u16, method: &str, path: &str, token: &str, body: &str) -> String {
        let mut stream = async_net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let request = format!(
            "{method} {path}?api-version=2025-10-01 HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\nMicrosoft-Developer-DCP-Instance-ID: abcdef123456\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();
        response
    }

    #[test]
    fn speaks_the_ide_execution_protocol() {
        smol::block_on(speaks_the_protocol());
    }

    async fn speaks_the_protocol() {
        let (endpoint, mut requests) = Endpoint::start().unwrap();
        let env = endpoint.env();
        let var = |name: &str| env.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone()).unwrap();
        let port: u16 = var("DEBUG_SESSION_PORT").strip_prefix("localhost:").unwrap().parse().unwrap();
        let token = var("DEBUG_SESSION_TOKEN");
        assert!(var("DEBUG_SESSION_INFO").contains("\"project\""), "file-based apps need project advertised");

        assert!(http(port, "GET", "/info", "wrong", "").await.starts_with("HTTP/1.1 401"));
        let info = http(port, "GET", "/info", &token, "").await;
        assert!(info.starts_with("HTTP/1.1 200") && info.contains("2025-10-01"), "{info}");

        // DCP subscribes before it creates sessions.
        let stream = async_net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let mut ws_request = async_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(format!("ws://localhost:{port}/run_session/notify?api-version=2025-10-01")).unwrap();
        ws_request.headers_mut().insert("Authorization", format!("Bearer {token}").parse().unwrap());
        let (mut notifications, _) = async_tungstenite::client_async(ws_request, stream).await.unwrap();

        let body = r#"{"launch_configurations": [{"type": "project", "project_path": "/src/Api/Api.csproj", "launch_profile": "https"}],
                       "env": [{"name": "OTEL_SERVICE_NAME", "value": "api"}], "args": []}"#;
        let created = http(port, "PUT", "/run_session", &token, body).await;
        assert!(created.starts_with("HTTP/1.1 201"), "{created}");
        assert!(created.contains(&format!("Location: http://localhost:{port}/run_session/1")), "{created}");
        let Some(Request::Start(session)) = requests.next().await else { panic!("no start request") };
        assert_eq!(
            session,
            RunSession {
                id: "1".into(),
                project: "/src/Api/Api.csproj".into(),
                debug: true,
                launch_profile: Some("https".into()),
                disable_launch_profile: false,
                env: vec![("OTEL_SERVICE_NAME".into(), "api".into())],
                args: Some(vec![]),
            }
        );
        let python = r#"{"launch_configurations": [{"type": "python", "program_path": "/src/app.py"}]}"#;
        assert!(http(port, "PUT", "/run_session", &token, python).await.starts_with("HTTP/1.1 400"));

        endpoint.notify("1", Notification::Output { stderr: false, text: "Now listening".into() });
        let line = notifications.next().await.unwrap().unwrap().into_text().unwrap();
        let json: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(json, json!({ "notification_type": "serviceLogs", "session_id": "1", "is_std_err": false, "log_message": "Now listening" }));

        assert!(http(port, "DELETE", "/run_session/1", &token, "").await.starts_with("HTTP/1.1 200"));
        assert_eq!(requests.next().await, Some(Request::Stop("1".into())));
        endpoint.notify("1", Notification::Terminated { exit_code: None });
        endpoint.notify("1", Notification::Terminated { exit_code: None });
        let line = notifications.next().await.unwrap().unwrap().into_text().unwrap();
        assert_eq!(serde_json::from_str::<Value>(&line).unwrap(), json!({ "notification_type": "sessionTerminated", "session_id": "1" }));
        assert!(http(port, "DELETE", "/run_session/1", &token, "").await.starts_with("HTTP/1.1 204"), "an ended session is gone");

        drop(endpoint);
        assert!(matches!(notifications.next().await, Some(Ok(Message::Close(_))) | None), "the stream closes with the endpoint");
    }

    /// Plays the IDE for a real app host: its orchestrator must ask for its projects
    /// through the endpoint, accept their output and stop them when the app host stops.
    /// `FORGE_ASPIRE_APPHOST=/path/apphost.cs cargo test -p forge-run -- --ignored --nocapture runs_a_real_app_host`
    #[test]
    #[ignore]
    fn runs_a_real_app_host() {
        smol::block_on(run_a_real_app_host());
    }

    async fn run_a_real_app_host() {
        use std::io::BufRead as _;
        let Ok(apphost) = std::env::var("FORGE_ASPIRE_APPHOST") else { return };
        let (endpoint, mut requests) = Endpoint::start().unwrap();
        let apphost = PathBuf::from(apphost);
        let mut host = std::process::Command::new("dotnet")
            .args(["run", "--file", &apphost.to_string_lossy()])
            .current_dir(apphost.parent().unwrap())
            .envs(endpoint.env())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let host_out = host.stdout.take().unwrap();
        std::thread::spawn(move || std::io::BufReader::new(host_out).lines().map_while(Result::ok).for_each(|l| println!("HOST {l}")));

        let lines: Arc<Mutex<Vec<(String, String)>>> = Arc::default();
        let mut services = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(240);
        while services.len() < 2 {
            assert!(std::time::Instant::now() < deadline, "the orchestrator asked for {} projects", services.len());
            let request = futures::select_biased! {
                request = requests.next().fuse() => request.unwrap(),
                _ = FutureExt::fuse(smol::Timer::after(std::time::Duration::from_millis(500))) => continue,
            };
            let Request::Start(session) = request else { panic!("unexpected {request:?}") };
            println!("START {} {:?} debug={} profile={:?} disable={} args={:?}", session.id, session.project, session.debug, session.launch_profile, session.disable_launch_profile, session.args);
            let inherited: HashSet<String> = std::env::vars().map(|(k, _)| k).collect();
            println!("  env {:?}", session.env.iter().filter(|(k, _)| !inherited.contains(k)).collect::<Vec<_>>());
            // What Forge debugs, launched as the .NET debug locator would: the profile it
            // picked, under the request's environment.
            let task = crate::controller::aspire_project_task(&session, Some("http"));
            println!("  {} {}", task.command, task.args.join(" "));
            let mut args: Vec<String> = task.args.iter().map(|a| a.trim_matches('"').to_string()).collect();
            let mut env: Vec<(String, String)> = Vec::new();
            if let Some(at) = args.iter().position(|a| a == "--launch-profile") {
                let name = args.remove(at + 1);
                args[at] = "--no-launch-profile".into();
                env = forge_languages::launch_settings::profile(&session.project, Some(&name)).unwrap().env;
            }
            env.extend(task.env);
            let mut child = std::process::Command::new(&task.command).args(args).current_dir(task.cwd.unwrap()).envs(env).stdout(std::process::Stdio::piped()).spawn().unwrap();
            endpoint.notify(&session.id, Notification::Started { pid: child.id() });
            let (out, id, lines) = (child.stdout.take().unwrap(), session.id.clone(), lines.clone());
            std::thread::spawn(move || std::io::BufReader::new(out).lines().map_while(Result::ok).for_each(|l| lines.lock().unwrap().push((id.clone(), l))));
            services.push((session.id, child));
        }
        let mut seen = HashSet::new();
        while seen.len() < 2 {
            assert!(std::time::Instant::now() < deadline, "projects did not start");
            for (id, line) in lines.lock().unwrap().drain(..) {
                println!("OUT {id} {line}");
                if line.contains("started") {
                    seen.insert(id.clone());
                }
                endpoint.notify(&id, Notification::Output { stderr: false, text: line });
            }
            smol::Timer::after(std::time::Duration::from_millis(200)).await;
        }

        // Stopping the app host (Ctrl+C) makes its orchestrator stop the projects.
        std::process::Command::new("pkill").args(["-INT", "-P", &host.id().to_string()]).status().ok();
        let mut stopped = 0;
        while stopped < services.len() && std::time::Instant::now() < deadline {
            let request = futures::select_biased! {
                request = requests.next().fuse() => request,
                _ = FutureExt::fuse(smol::Timer::after(std::time::Duration::from_millis(500))) => continue,
            };
            match request {
                Some(Request::Stop(id)) => {
                    println!("STOP {id}");
                    if let Some((_, child)) = services.iter_mut().find(|(s, _)| *s == id) {
                        // `dotnet run` and the program it started.
                        std::process::Command::new("pkill").args(["-P", &child.id().to_string()]).status().ok();
                        child.kill().ok();
                    }
                    endpoint.notify(&id, Notification::Terminated { exit_code: Some(0) });
                    stopped += 1;
                }
                other => panic!("unexpected {other:?}"),
            }
        }
        for (_, child) in &mut services {
            child.kill().ok();
        }
        host.wait().ok();
        assert_eq!(stopped, 2, "the orchestrator stopped both projects");
    }
}
