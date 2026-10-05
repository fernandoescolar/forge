use super::*;
use gpui::{TestAppContext, VisualTestContext};

#[test]
fn environments_merge_and_pick() {
    let mut envs = Environments::default();
    envs.merge(r#"{ "$shared": { "version": "v1" }, "prod": { "host": "https://api" }, "dev": { "host": "http://localhost", "port": 5000 } }"#);
    envs.merge(r#"{ "dev": { "token": "secret" } }"#);
    assert_eq!(envs.pick(None).as_deref(), Some("dev"), "a development environment by default");
    assert_eq!(envs.pick(Some("prod")).as_deref(), Some("prod"));
    assert_eq!(envs.pick(Some("gone")).as_deref(), Some("dev"));
    let vars = envs.variables(Some("dev"));
    assert_eq!((vars["host"].as_str(), vars["port"].as_str(), vars["token"].as_str(), vars["version"].as_str()), ("http://localhost", "5000", "secret", "v1"));
    assert_eq!(Environments::default().pick(None), None);
}

#[test]
fn resolves_variables_from_everywhere() {
    let text = "@base = {{host}}:{{port}}\n\n# @name me\nGET {{base}}/me?id={{$randomInt 5 6}}\nAuthorization: Bearer {{login.response.body.$.token}}\nX-Location: {{login.response.headers.location}}\n";
    let mut envs = Environments::default();
    envs.merge(r#"{ "dev": { "host": "http://localhost", "port": 5000 } }"#);
    let file = Path::new("/p/api.http");
    let mut responses = HashMap::new();
    responses.insert((file.to_path_buf(), "login".to_string()), Stored { headers: vec![("Location".into(), "/home".into())], body: r#"{"token":"t0"}"#.into() });
    let spec = parse::request_at(text, 3).unwrap();
    let resolved = resolve(&spec, text, file, Some("dev"), &envs, &responses).unwrap();
    assert_eq!(resolved.url, "http://localhost:5000/me?id=5");
    assert_eq!(resolved.headers, [("Authorization".to_string(), "Bearer t0".to_string()), ("X-Location".to_string(), "/home".to_string())]);
    assert_eq!(resolved.name.as_deref(), Some("me"));

    let error = resolve(&spec, text, file, None, &Environments::default(), &HashMap::new()).unwrap_err().to_string();
    assert_eq!(error, "undefined: host, port, login.response.body.$.token, login.response.headers.location");
}

#[test]
fn renders_json_responses_with_headers_as_comments() {
    let request = Resolved { name: None, method: "GET".into(), url: "http://x/y".into(), headers: vec![], body: None };
    let exchange = Exchange { status: 200, reason: "OK".into(), elapsed: Duration::from_millis(12), headers: vec![("content-type".into(), "application/json".into())], body: br#"{"a":1}"#.to_vec() };
    let (text, json) = render(&request, Some("dev"), &exchange, None);
    assert!(json);
    assert_eq!(text, "// 200 OK · 12 ms · 7 B\n// GET http://x/y  (environment: dev)\n// \n// content-type: application/json\n\n{\n  \"a\": 1\n}\n");
    let plain = Exchange { body: b"hello".to_vec(), headers: vec![], ..exchange };
    let (text, json) = render(&request, None, &plain, None);
    assert!(!json);
    assert!(text.starts_with("200 OK") && text.ends_with("\nhello\n"), "{text}");
}

/// ⌘↩ in a `.http` file sends the request under the cursor, shows the response beside it
/// and lets the next request use it.
#[gpui::test]
async fn sends_the_request_at_the_cursor(cx: &mut TestAppContext) {
    let params = cx.update(workspace::AppState::test);
    let client = http_client::FakeHttpClient::create(|request| async move {
        let auth = request.headers().get("authorization").map(|v| v.to_str().unwrap().to_string()).unwrap_or_default();
        let body = match request.uri().path() {
            "/login" => r#"{"token":"abc"}"#.to_string(),
            // Never answers, to be cancelled.
            "/slow" => futures::future::pending::<String>().await,
            _ => format!(r#"{{"auth":"{auth}"}}"#),
        };
        Ok(http_client::Response::builder().status(200).header("content-type", "application/json").body(body.into()).unwrap())
    });
    cx.update(|cx| {
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        editor::init(cx);
        init(cx);
        cx.set_http_client(client);
    });
    let text = "# @name login\nPOST http://api.test/login\n\n{}\n\n###\nGET http://api.test/me\nAuthorization: Bearer {{login.response.body.$.token}}\n";
    params.fs.as_fake().insert_tree("/root", serde_json::json!({ "api.http": text })).await;
    let project = project::Project::test(params.fs.clone(), ["/root".as_ref()], cx).await;
    let window = cx.add_window(|window, cx| workspace::MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace = window.read_with(cx, |mw, _| mw.workspace().clone()).unwrap();
    let cx = &mut VisualTestContext::from_window(window.into(), cx);
    let item = workspace.update_in(cx, |ws, window, cx| ws.open_abs_path(PathBuf::from("/root/api.http"), workspace::OpenOptions::default(), window, cx)).await.unwrap();
    let editor = item.downcast::<Editor>().unwrap();

    let send_from_row = |row: u32, cx: &mut VisualTestContext| {
        editor.update_in(cx, |e, window, cx| {
            let point = language::Point::new(row, 0);
            e.change_selections(Default::default(), window, cx, |s| s.select_ranges([point..point]));
        });
        workspace.update_in(cx, |ws, window, cx| send_at_cursor(ws, None, window, cx));
        cx.run_until_parked();
    };
    let response = |cx: &mut VisualTestContext| {
        let view = cx.update(|_, cx| cx.global::<HttpState>().views.get(Path::new("/root/api.http")).and_then(|v| v.upgrade())).expect("a response tab");
        view.read_with(cx, |e, cx| e.buffer().read(cx).as_singleton().unwrap().read(cx).text())
    };

    send_from_row(1, cx);
    assert!(response(cx).ends_with("{\n  \"token\": \"abc\"\n}\n"), "{}", response(cx));
    send_from_row(6, cx);
    assert!(response(cx).ends_with("{\n  \"auth\": \"Bearer abc\"\n}\n"), "the second request used the first one's response: {}", response(cx));
    // The run button on a request line sends that request.
    let click = cx.update(|_, cx| cx.global::<editor::RunIndicatorClick>().0.clone());
    assert!(cx.update(|window, cx| click(Path::new("/root/api.http"), 1, window, cx)), "handled");
    assert!(!cx.update(|window, cx| click(Path::new("/root/main.rs"), 1, window, cx)), "other files' buttons are left alone");
    cx.run_until_parked();
    assert!(response(cx).ends_with("{\n  \"token\": \"abc\"\n}\n"), "{}", response(cx));
    let tabs = workspace.read_with(cx, |ws, cx| ws.panes().iter().map(|p| p.read(cx).items_len()).sum::<usize>());
    assert_eq!(tabs, 2, "one response tab per .http file, reused");
    let active = workspace.read_with(cx, |ws, cx| ws.active_item(cx).and_then(|i| i.downcast::<Editor>()));
    assert_eq!(active, Some(editor), "the .http file keeps the focus");

    // Every request goes into the history, newest last; sending one from it again works.
    let sent: Vec<(String, Option<u16>)> = cx.update(|_, cx| history::entries(cx).iter().map(|e| (e.request.url.clone(), e.status)).collect());
    assert_eq!(sent, [("http://api.test/login".to_string(), Some(200)), ("http://api.test/me".to_string(), Some(200)), ("http://api.test/login".to_string(), Some(200))]);
    let again = cx.update(|_, cx| history::entries(cx)[1].clone());
    workspace.update_in(cx, |ws, window, cx| dispatch(ws, again.file, again.request, again.environment, window, cx));
    cx.run_until_parked();
    assert!(response(cx).ends_with("{\n  \"auth\": \"Bearer abc\"\n}\n"), "{}", response(cx));
    assert_eq!(cx.update(|_, cx| history::entries(cx).len()), 4);

    // A request that doesn't answer can be cancelled.
    let slow = Resolved { name: None, method: "GET".into(), url: "http://api.test/slow".into(), headers: vec![], body: None };
    workspace.update_in(cx, |ws, window, cx| dispatch(ws, PathBuf::from("/root/api.http"), slow, None, window, cx));
    cx.run_until_parked();
    assert!(cx.update(|_, cx| cx.global::<HttpState>().requests.contains_key(Path::new("/root/api.http"))), "still waiting");
    workspace.update_in(cx, |ws, _, cx| cancel_request(ws, cx));
    assert_eq!(response(cx), "// Cancelled.\n");
    assert!(cx.update(|_, cx| cx.global::<HttpState>().requests.is_empty()));
}

/// A body that arrives a few bytes at a time.
struct Trickle(Vec<u8>, usize);

impl futures::AsyncRead for Trickle {
    fn poll_read(mut self: std::pin::Pin<&mut Self>, _: &mut std::task::Context<'_>, buf: &mut [u8]) -> std::task::Poll<std::io::Result<usize>> {
        let start = self.1;
        let end = (start + 4).min(self.0.len()).min(start + buf.len());
        buf[..end - start].copy_from_slice(&self.0[start..end]);
        self.1 = end;
        std::task::Poll::Ready(Ok(end - start))
    }
}

#[test]
fn streams_the_body_as_it_arrives() {
    let client = http_client::FakeHttpClient::create(|_| async move {
        let body = http_client::AsyncBody::from_reader(Trickle(b"data: 1\ndata: 2\n".to_vec(), 0));
        Ok(http_client::Response::builder().status(200).header("content-type", "text/event-stream").body(body).unwrap())
    });
    let request = Resolved { name: None, method: "GET".into(), url: "http://x/events".into(), headers: vec![], body: None };
    let mut seen = Vec::new();
    let exchange = futures::executor::block_on(send_streaming(client, &request, |partial| seen.push(partial.body.len()))).unwrap();
    assert_eq!(seen, [0, 4, 8, 12, 16], "the status and headers first, then each chunk");
    assert_eq!(exchange.body, b"data: 1\ndata: 2\n");
    let partial = Exchange { body: b"data: 1\n".to_vec(), ..exchange };
    let text = render_partial(&request, &partial);
    assert!(text.starts_with("200 OK · ") && text.contains("receiving 8 B…") && text.ends_with("\n\ndata: 1\n"), "{text}");
}

#[test]
fn saves_binary_bodies() {
    let png = Exchange { status: 200, reason: "OK".into(), elapsed: Duration::from_millis(5), headers: vec![("Content-Type".into(), "image/png".into())], body: vec![0x89, b'P', b'N', b'G', 0] };
    assert!(png.is_binary());
    let json = Exchange { headers: vec![("content-type".into(), "application/json; charset=utf-8".into())], body: b"{}".to_vec(), ..png };
    assert!(!json.is_binary());
    let unknown = Exchange { headers: vec![], body: vec![0xff, 0xfe, 0x00], ..json };
    assert!(unknown.is_binary(), "not UTF-8");
    let png = Exchange { headers: vec![("Content-Type".into(), "image/png".into())], body: vec![0x89, b'P', b'N', b'G', 0], ..unknown };
    let path = save_body(Path::new("/p/api.http"), &png).unwrap();
    assert!(path.file_name().unwrap().to_string_lossy().starts_with("api-") && path.extension().unwrap() == "png");
    assert_eq!(std::fs::read(&path).unwrap(), png.body);
    let request = Resolved { name: None, method: "GET".into(), url: "http://x/logo".into(), headers: vec![], body: None };
    let (text, json) = render(&request, None, &png, Some(&path));
    assert!(!json && text.ends_with(&format!("(5 B of image/png, saved to {})\n", path.display())), "{text}");
    std::fs::remove_file(path).ok();
}

/// With Forge's real HTTP client, against a server that sends its body slowly:
/// `FORGE_HTTP_SLOW_URL=http://127.0.0.1:8765/ cargo test -p forge-http -- --ignored --nocapture`
#[test]
#[ignore]
fn streams_from_a_real_server() {
    let Ok(url) = std::env::var("FORGE_HTTP_SLOW_URL") else { return };
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let _guard = runtime.enter();
    let client: Arc<dyn HttpClient> = Arc::new(reqwest_client::ReqwestClient::user_agent("Forge test").unwrap());
    let request = Resolved { name: None, method: "GET".into(), url, headers: vec![], body: None };
    let started = Instant::now();
    let mut seen: Vec<(u128, usize)> = Vec::new();
    let exchange = futures::executor::block_on(send_streaming(client, &request, |partial| seen.push((started.elapsed().as_millis(), partial.body.len())))).unwrap();
    println!("{seen:?}");
    assert!(seen.len() >= 3, "the body came in pieces: {seen:?}");
    assert!(seen[1].0 + 300 < seen.last().unwrap().0, "the first piece came long before the last: {seen:?}");
    assert_eq!(String::from_utf8_lossy(&exchange.body), "one\ntwo\nthree\n");
}
