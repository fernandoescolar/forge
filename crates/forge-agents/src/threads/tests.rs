use super::*;
use crate::thread::Entry;
use fs::Fs as _;
use gpui::{TestAppContext, VisualTestContext, point, size};
use ide_api::AgentSpec;
use project::Project;
use serde_json::json;
use std::time::{Duration, Instant};

async fn wait_for(cx: &mut VisualTestContext, thread: &Entity<Thread>, what: &str, pred: impl Fn(&Thread) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        cx.run_until_parked();
        if thread.read_with(cx, |t, _| pred(t)) {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        cx.background_executor.timer(Duration::from_millis(20)).await;
    }
}

fn draw(view: &Entity<ThreadView>, cx: &mut VisualTestContext) {
    let view = view.clone();
    cx.draw(point(px(0.), px(0.)), size(px(1280.), px(800.)), move |_, _| div().size_full().child(view));
}

/// (added, deleted) rows the editor shows as diff, expanded.
fn diff_rows(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> (usize, usize) {
    editor.update(cx, |e, cx| {
        let snapshot = e.buffer().read(cx).snapshot(cx);
        snapshot.row_infos(multi_buffer::MultiBufferRow(0)).filter_map(|r| r.diff_status).fold((0, 0), |(a, d), s| match s.kind {
            buffer_diff::DiffHunkStatusKind::Added => (a + 1, d),
            buffer_diff::DiffHunkStatusKind::Deleted => (a, d + 1),
            _ => (a, d),
        })
    })
}

/// While the agent edits the followed file, the pane shows the change as a diff: the
/// proposal while it waits for review, then the file against before the agent.
#[gpui::test]
async fn the_follow_pane_shows_changes_as_diffs(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let tmp = tempfile::tempdir().unwrap();
    cx.update(workspace::AppState::test);
    cx.update(|cx| {
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        gpui_tokio::init(cx);
        editor::init(cx);
        init(cx);
        crate::thread_picker::init(cx);
        crate::agent_review::init(cx);
    });
    let fs = fs::FakeFs::new(cx.executor());
    fs.insert_tree("/root", json!({ "a.txt": "old\nsame\n" })).await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));
    let project = Project::test(fs.clone(), ["/root".as_ref()], cx).await;
    let window = cx.add_window(|window, cx| workspace::MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace = window.read_with(cx, |mw, _| mw.workspace().clone()).unwrap();
    let cx = &mut VisualTestContext::from_window(window.into(), cx);
    let agent = AgentSpec {
        id: "mock".into(),
        command: "python3".into(),
        args: vec![std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tools/mock-acp-agent.py").to_string_lossy().into_owned()],
        env: vec![],
        cwd: Some(tmp.path().to_path_buf()),
    };
    let config = crate::config::AgentsConfig { instructions_files: crate::config::default_instructions_files(), agents: vec![agent], review_writes: true, verify_changes: false, mcp_servers: vec![], default_agent: None, permissions: Default::default() };
    let thread = workspace.update_in(cx, |ws, window, cx| {
        let thread = cx.new(|cx| Thread::with_config(ws, Some(config), tmp.path().join("history"), window, cx));
        add_thread(ws, thread.clone(), window, cx);
        thread
    });
    let view = workspace.read_with(cx, |ws, cx| ws.items_of_type::<ThreadView>(cx).next()).unwrap();
    draw(&view, cx);
    view.update_in(cx, |v, window, cx| {
        v.input.update(cx, |e, cx| e.set_text("edit a.txt new", window, cx));
        v.send(&Send, window, cx);
    });
    wait_for(cx, &thread, "the change to review", |t| t.pending_reviews() == 1).await;
    cx.run_until_parked();
    draw(&view, cx);
    let proposal = view.read_with(cx, |v, _| v.follow_review.as_ref().map(|(_, d)| d.editor.clone())).expect("the pane shows the proposal");
    let (added, deleted) = diff_rows(&proposal, cx);
    assert!(added > 0 && deleted > 0, "the proposal shows added and deleted rows: {added} added, {deleted} deleted");

    let review = thread.read_with(cx, |t, _| t.entries.iter().position(|e| matches!(e, Entry::Review { .. })).unwrap());
    thread.update(cx, |t, cx| t.answer_review(review, true, cx));
    wait_for(cx, &thread, "the turn to end", |t| t.status() == Status::Ready && t.pending_reviews() == 0).await;
    cx.run_until_parked();
    draw(&view, cx);
    assert!(view.read_with(cx, |v, _| v.follow_review.is_none()), "answered: back to the file");
    let editor = view.read_with(cx, |v, _| v.follow.as_ref().and_then(|f| f.editor.clone())).expect("the pane shows the file");
    let (added, deleted) = diff_rows(&editor, cx);
    assert!(added > 0 && deleted > 0, "the file shows the agent's change: {added} added, {deleted} deleted");
    // Filled red and green, not git's hollow staged hunks.
    let staged = editor.update(cx, |e, cx| {
        let snapshot = e.buffer().read(cx).snapshot(cx);
        let statuses: Vec<_> = snapshot.row_infos(multi_buffer::MultiBufferRow(0)).filter_map(|r| r.diff_status).collect();
        statuses.iter().map(|s| e.diff_hunk_renderer().render_hunk_as_staged(s, cx)).collect::<Vec<_>>()
    });
    assert!(staged.iter().all(|s| !s), "the follow pane's hunks render as staged (hollow): {staged:?}");
    let staged = proposal.update(cx, |e, cx| {
        let snapshot = e.buffer().read(cx).snapshot(cx);
        let statuses: Vec<_> = snapshot.row_infos(multi_buffer::MultiBufferRow(0)).filter_map(|r| r.diff_status).collect();
        statuses.iter().map(|s| e.diff_hunk_renderer().render_hunk_as_staged(s, cx)).collect::<Vec<_>>()
    });
    assert!(staged.iter().all(|s| !s), "the proposal's hunks render as staged (hollow): {staged:?}");
}

/// As with Claude Code in auto mode: the agent reads the file (the pane opens it), then
/// writes it straight away (no review); the pane shows the change as a diff.
#[gpui::test]
async fn the_follow_pane_shows_direct_writes_as_diffs(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let tmp = tempfile::tempdir().unwrap();
    cx.update(workspace::AppState::test);
    cx.update(|cx| {
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        gpui_tokio::init(cx);
        editor::init(cx);
        init(cx);
        crate::thread_picker::init(cx);
        crate::agent_review::init(cx);
    });
    let fs = fs::FakeFs::new(cx.executor());
    fs.insert_tree("/root", json!({ "a.txt": "old\nsame\n" })).await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));
    let project = Project::test(fs.clone(), ["/root".as_ref()], cx).await;
    let window = cx.add_window(|window, cx| workspace::MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace = window.read_with(cx, |mw, _| mw.workspace().clone()).unwrap();
    let cx = &mut VisualTestContext::from_window(window.into(), cx);
    let agent = AgentSpec {
        id: "mock".into(),
        command: "python3".into(),
        args: vec![std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tools/mock-acp-agent.py").to_string_lossy().into_owned()],
        env: vec![],
        cwd: Some(tmp.path().to_path_buf()),
    };
    let config = crate::config::AgentsConfig { instructions_files: crate::config::default_instructions_files(), agents: vec![agent], review_writes: false, verify_changes: false, mcp_servers: vec![], default_agent: None, permissions: Default::default() };
    let thread = workspace.update_in(cx, |ws, window, cx| {
        let thread = cx.new(|cx| Thread::with_config(ws, Some(config), tmp.path().join("history"), window, cx));
        add_thread(ws, thread.clone(), window, cx);
        thread
    });
    let view = workspace.read_with(cx, |ws, cx| ws.items_of_type::<ThreadView>(cx).next()).unwrap();
    draw(&view, cx);
    for message in ["read a.txt 0", "edit a.txt new"] {
        view.update_in(cx, |v, window, cx| {
            v.input.update(cx, |e, cx| e.set_text(message, window, cx));
            v.send(&Send, window, cx);
        });
        wait_for(cx, &thread, "the turn to end", |t| t.status() == Status::Ready && t.entries.iter().filter(|e| matches!(e, Entry::Agent(_))).count() >= 1).await;
        cx.run_until_parked();
        draw(&view, cx);
    }
    wait_for(cx, &thread, "the change", |t| !t.changes.is_empty()).await;
    cx.run_until_parked();
    draw(&view, cx);
    let editor = view.read_with(cx, |v, _| v.follow.as_ref().and_then(|f| f.editor.clone())).expect("the pane shows the file");
    let text = editor.update(cx, |e, cx| e.buffer().read(cx).snapshot(cx).text());
    assert!(text.contains("new"), "the pane shows the written file: {text:?}");
    let (added, deleted) = diff_rows(&editor, cx);
    assert!(added > 0 && deleted > 0, "the pane shows the agent's change: {added} added, {deleted} deleted");
}

/// Claude Code's adapter writes files itself and only reports the edit (a tool call
/// with the whole file before and after): the edit still joins the changes, and the
/// follow pane shows it as a diff.
#[gpui::test]
async fn edits_the_agent_writes_itself_show_as_diffs(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let tmp = tempfile::tempdir().unwrap();
    cx.update(workspace::AppState::test);
    cx.update(|cx| {
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        gpui_tokio::init(cx);
        editor::init(cx);
        init(cx);
        crate::thread_picker::init(cx);
        crate::agent_review::init(cx);
    });
    let fs = fs::FakeFs::new(cx.executor());
    fs.insert_tree("/root", json!({ "a.rs": "fn a() {}\nfn b() {}\n" })).await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));
    let project = Project::test(fs.clone(), ["/root".as_ref()], cx).await;
    let window = cx.add_window(|window, cx| workspace::MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace = window.read_with(cx, |mw, _| mw.workspace().clone()).unwrap();
    let cx = &mut VisualTestContext::from_window(window.into(), cx);
    let config = crate::config::AgentsConfig { instructions_files: crate::config::default_instructions_files(), agents: vec![], review_writes: true, verify_changes: false, mcp_servers: vec![], default_agent: None, permissions: Default::default() };
    let thread = workspace.update_in(cx, |ws, window, cx| {
        let thread = cx.new(|cx| Thread::with_config(ws, Some(config), tmp.path().join("history"), window, cx));
        add_thread(ws, thread.clone(), window, cx);
        thread
    });
    let view = workspace.read_with(cx, |ws, cx| ws.items_of_type::<ThreadView>(cx).next()).unwrap();
    let (before, after) = ("fn a() {}\nfn b() {}\n", "fn a() {}\nfn c() {}\n");
    let update = |u: serde_json::Value, cx: &mut VisualTestContext| {
        thread.update_in(cx, |t, window, cx| {
            t.apply_update(u, window, cx);
            t.changed(cx);
        });
        cx.run_until_parked();
    };
    // The edit is announced (the pane opens the file), written by the agent, then done.
    update(json!({ "sessionUpdate": "tool_call", "toolCallId": "t1", "title": "Edit a.rs", "kind": "edit", "status": "pending",
        "locations": [{ "path": "/root/a.rs", "line": 1 }],
        "content": [{ "type": "diff", "path": "/root/a.rs", "oldText": before, "newText": after }] }), cx);
    draw(&view, cx);
    fs.save("/root/a.rs".as_ref(), &after.into(), Default::default()).await.unwrap();
    update(json!({ "sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "completed" }), cx);
    draw(&view, cx);
    let changes = thread.read_with(cx, |t, _| t.changes.iter().map(|c| (c.path.clone(), c.original.clone(), c.current.clone())).collect::<Vec<_>>());
    assert_eq!(changes, [(std::path::PathBuf::from("/root/a.rs"), Some(before.to_string()), after.to_string())]);
    let editor = view.read_with(cx, |v, _| v.follow.as_ref().and_then(|f| f.editor.clone())).expect("the pane shows the file");
    let (added, deleted) = diff_rows(&editor, cx);
    assert!(added > 0 && deleted > 0, "the pane shows the agent's change: {added} added, {deleted} deleted");

    // A snippet's diff doesn't say what the file was: not recorded.
    update(json!({ "sessionUpdate": "tool_call", "toolCallId": "t2", "title": "Edit a.rs", "kind": "edit", "status": "pending",
        "content": [{ "type": "diff", "path": "/root/a.rs", "oldText": "fn c() {}", "newText": "fn d() {}" }] }), cx);
    fs.save("/root/a.rs".as_ref(), &"fn a() {}\nfn d() {}\n".into(), Default::default()).await.unwrap();
    update(json!({ "sessionUpdate": "tool_call_update", "toolCallId": "t2", "status": "completed" }), cx);
    let current = thread.read_with(cx, |t, _| t.changes[0].current.clone());
    assert_eq!(current, after, "a snippet's diff isn't taken for the whole file");
}

/// Tools extensions offer (`forge.agents.registerTool`) reach agents through Forge's MCP
/// server: one that only reads runs at once; any other waits for the user in the thread.
#[gpui::test]
async fn extension_tools_run_for_agents_with_the_users_leave(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    use forge_ui::agent_tools::{AgentTool, AgentToolReply, AgentToolRunner, agent_tools};
    struct Echo;
    impl AgentToolRunner for Echo {
        fn run(&self, tool: &AgentTool, args: serde_json::Value, _cwd: std::path::PathBuf) -> futures::channel::oneshot::Receiver<AgentToolReply> {
            let (tx, rx) = futures::channel::oneshot::channel();
            tx.send(Ok(format!("{} ran with {args}", tool.tool))).ok();
            rx
        }
    }
    let tool = |name: &str, read_only: bool| AgentTool {
        name: forge_ui::agent_tools::mcp_name("demo-ext", name),
        tool: name.into(),
        extension: "demo-ext".into(),
        title: format!("Demo {name}"),
        description: format!("Does {name}."),
        input_schema: json!({ "type": "object", "properties": {} }),
        read_only,
    };
    agent_tools().register(tool("lookup", true));
    agent_tools().register(tool("change", false));
    agent_tools().set_runner(std::sync::Arc::new(Echo));
    assert!(crate::forge_mcp::instructions(std::path::Path::new("/nowhere")).contains("`demo_ext__lookup` (Demo lookup, from the demo-ext extension)"));
    assert_eq!(crate::forge_mcp::tool_title("mcp__forge__demo_ext__change").as_deref(), Some("Demo change"));

    let tmp = tempfile::tempdir().unwrap();
    let config = crate::config::AgentsConfig { instructions_files: crate::config::default_instructions_files(), agents: vec![], review_writes: true, verify_changes: false, mcp_servers: vec![], default_agent: None, permissions: Default::default() };
    let (thread, _view, mut cx) = thread_tab(config, tmp.path().join("history"), cx).await;
    let cx = &mut cx;
    let call = |name: &str, cx: &mut VisualTestContext| {
        let (reply, answer) = futures::channel::oneshot::channel();
        let request = crate::forge_mcp::ToolRequest { name: name.into(), args: json!({ "q": 1 }), reply };
        thread.update_in(cx, |t, window, cx| t.handle_tool_request(request, window, cx));
        cx.run_until_parked();
        answer
    };
    let mut lookup = call("demo_ext__lookup", cx);
    assert_eq!(lookup.try_recv().unwrap(), Some(Ok("lookup ran with {\"q\":1}".into())), "reading tools run at once");
    assert!(thread.read_with(cx, |t, _| !t.entries.iter().any(|e| matches!(e, Entry::ExtensionTool { .. }))));

    let mut allowed = call("demo_ext__change", cx);
    assert_eq!(allowed.try_recv().unwrap(), None, "it waits for the user");
    assert_eq!(thread.read_with(cx, |t, _| t.pending_reviews()), 1);
    let card = thread.read_with(cx, |t, _| t.entries.iter().position(|e| matches!(e, Entry::ExtensionTool { .. })).unwrap());
    draw(&_view, cx);
    thread.update_in(cx, |t, window, cx| t.answer_extension_tool(card, true, window, cx));
    cx.run_until_parked();
    assert_eq!(allowed.try_recv().unwrap(), Some(Ok("change ran with {\"q\":1}".into())));

    let mut denied = call("demo_ext__change", cx);
    let card = thread.read_with(cx, |t, _| t.entries.iter().rposition(|e| matches!(e, Entry::ExtensionTool { .. })).unwrap());
    thread.update_in(cx, |t, window, cx| t.answer_extension_tool(card, false, window, cx));
    cx.run_until_parked();
    assert!(matches!(denied.try_recv().unwrap(), Some(Err(e)) if e.contains("didn't let Demo change run")));
    agent_tools().unregister_extension("demo-ext");
}

/// Forge's tools that reach the network or run code wait for the user (unless the mode
/// lets them run), and the agent hears when the user says no.
#[gpui::test]
async fn forge_tools_that_run_code_ask_first(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let tmp = tempfile::tempdir().unwrap();
    let permissions = crate::permissions::Permissions { mode: crate::permissions::PermissionMode::Ask, ..Default::default() };
    let config = crate::config::AgentsConfig { instructions_files: crate::config::default_instructions_files(), agents: vec![], review_writes: true, verify_changes: false, mcp_servers: vec![], default_agent: None, permissions };
    let (thread, view, mut cx) = thread_tab(config, tmp.path().join("history"), cx).await;
    let cx = &mut cx;
    // A tool another crate registers (as the HTTP client does `http_request`).
    use forge_ui::ide_tools::{IdeTool, ToolEffect, ide_tools};
    ide_tools().register(IdeTool::new("test_send", "Send a test request", "Sends.", json!({}), ToolEffect::Network, |_, args, _| Box::pin(async move { Ok(format!("sent {args}")) })));
    assert_eq!(crate::forge_mcp::tool_title("mcp__forge__test_send").as_deref(), Some("Send a test request"));
    let (reply, mut answer) = futures::channel::oneshot::channel();
    let request = crate::forge_mcp::ToolRequest { name: "test_send".into(), args: json!({ "url": "https://example.com/?k={{$processEnv HOME}}" }), reply };
    thread.update_in(cx, |t, window, cx| t.handle_tool_request(request, window, cx));
    cx.run_until_parked();
    assert_eq!(answer.try_recv().unwrap(), None, "it waits for the user");
    assert_eq!(thread.read_with(cx, |t, _| t.pending_reviews()), 1);
    let card = thread.read_with(cx, |t, _| t.entries.iter().position(|e| matches!(e, Entry::ForgeTool { .. })).unwrap());
    draw(&view, cx);
    thread.update_in(cx, |t, window, cx| t.answer_forge_tool(card, false, window, cx));
    cx.run_until_parked();
    assert!(matches!(answer.try_recv().unwrap(), Some(Err(e)) if e.contains("didn't let you send a test request")));

    // Allowed: it runs, and the agent gets its answer.
    let (reply, mut answer) = futures::channel::oneshot::channel();
    let request = crate::forge_mcp::ToolRequest { name: "test_send".into(), args: json!({ "url": "x" }), reply };
    thread.update_in(cx, |t, window, cx| t.handle_tool_request(request, window, cx));
    cx.run_until_parked();
    let card = thread.read_with(cx, |t, _| t.entries.iter().rposition(|e| matches!(e, Entry::ForgeTool { .. })).unwrap());
    thread.update_in(cx, |t, window, cx| t.answer_forge_tool(card, true, window, cx));
    cx.run_until_parked();
    assert_eq!(answer.try_recv().unwrap(), Some(Ok("sent {\"url\":\"x\"}".into())));
}

/// `remember` keeps a note (as the user edited it) in the project's instructions;
/// `user_context` says where the user is, though the thread is the active tab.
#[gpui::test]
async fn agents_remember_and_see_what_the_user_is_on(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let tmp = tempfile::tempdir().unwrap();
    cx.update(workspace::AppState::test);
    cx.update(|cx| {
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        gpui_tokio::init(cx);
        editor::init(cx);
        init(cx);
    });
    let fs = fs::FakeFs::new(cx.executor());
    fs.insert_tree("/root", json!({ "AGENTS.md": "# Repo\n", "a.rs": "fn one() {}\nfn two() {}\nfn three() {}\n" })).await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));
    let project = Project::test(fs.clone(), ["/root".as_ref()], cx).await;
    let window = cx.add_window(|window, cx| workspace::MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace = window.read_with(cx, |mw, _| mw.workspace().clone()).unwrap();
    let cx = &mut VisualTestContext::from_window(window.into(), cx);

    // The user works in a.rs, with two lines selected; then the thread opens, in front of it.
    let editor = workspace
        .update_in(cx, |ws, window, cx| ws.open_abs_path("/root/a.rs".into(), workspace::OpenOptions::default(), window, cx))
        .await
        .unwrap()
        .downcast::<Editor>()
        .unwrap();
    editor.update_in(cx, |e, window, cx| e.change_selections(Default::default(), window, cx, |s| s.select_ranges([Point::new(1, 0)..Point::new(2, 5)])));
    let config = crate::config::AgentsConfig { instructions_files: vec!["AGENTS.md".into(), ".forge/AGENTS.md".into()], agents: vec![], review_writes: true, verify_changes: false, mcp_servers: vec![], default_agent: None, permissions: Default::default() };
    let thread = workspace.update_in(cx, |ws, window, cx| {
        let thread = cx.new(|cx| Thread::with_config(ws, Some(config), tmp.path().join("history"), window, cx));
        add_thread(ws, thread.clone(), window, cx);
        thread
    });
    cx.run_until_parked();
    let call = |name: &str, args: serde_json::Value, cx: &mut VisualTestContext| {
        let (reply, answer) = futures::channel::oneshot::channel();
        thread.update_in(cx, |t, window, cx| t.handle_tool_request(crate::forge_mcp::ToolRequest { name: name.into(), args, reply }, window, cx));
        cx.run_until_parked();
        answer
    };

    let mut context = call("user_context", json!({}), cx);
    let deadline = Instant::now() + Duration::from_secs(10);
    let text = loop {
        if let Ok(Some(answer)) = context.try_recv() {
            break answer.unwrap();
        }
        assert!(Instant::now() < deadline, "no context");
        cx.run_until_parked();
        cx.background_executor.timer(Duration::from_millis(20)).await;
    };
    assert!(text.contains("The user is in a.rs at line 2."), "{text}");
    assert!(text.contains("They selected lines 2-3:\n```\nfn two() {}\nfn t"), "{text}");
    assert!(text.contains("Open files: a.rs"), "{text}");

    // A note, edited before keeping it, goes to the first instructions file that exists.
    let mut kept = call("remember", json!({ "note": "Run cargo test." }), cx);
    let card = thread.read_with(cx, |t, _| t.entries.iter().position(|e| matches!(e, Entry::Remember { .. })).unwrap());
    let note = thread.read_with(cx, |t, _| match &t.entries[card] {
        Entry::Remember { note, file, .. } => {
            assert_eq!(file, &std::path::PathBuf::from("/root/AGENTS.md"));
            note.clone()
        }
        _ => unreachable!(),
    });
    note.update_in(cx, |e, window, cx| e.set_text("Run cargo test -q before finishing.", window, cx));
    thread.update(cx, |t, cx| t.answer_remember(card, true, cx));
    cx.run_until_parked();
    assert!(matches!(kept.try_recv().unwrap(), Some(Ok(text)) if text.contains("Kept in AGENTS.md")));
    assert_eq!(fs.load("/root/AGENTS.md".as_ref()).await.unwrap(), "# Repo\n\n## Notes from agents\n\n- Run cargo test -q before finishing.\n");

    let mut declined = call("remember", json!({ "note": "Something else." }), cx);
    let card = thread.read_with(cx, |t, _| t.entries.iter().rposition(|e| matches!(e, Entry::Remember { .. })).unwrap());
    thread.update(cx, |t, cx| t.answer_remember(card, false, cx));
    assert!(matches!(declined.try_recv().unwrap(), Some(Err(_))));
}

/// Typing in a new thread connects the agent and answers; an edit waits in the thread
/// for review and is written once accepted; the rail and tab follow along.
#[gpui::test]
async fn a_thread_tab_converses_and_reviews_changes(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let tmp = tempfile::tempdir().unwrap();
    cx.update(workspace::AppState::test);
    cx.update(|cx| {
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        gpui_tokio::init(cx);
        editor::init(cx);
        init(cx);
        crate::thread_picker::init(cx);
    });
    let fs = fs::FakeFs::new(cx.executor());
    fs.insert_tree("/root", json!({ "a.txt": "old\n", "b.txt": "one\ntwo\nthree\n", ".forge": { "AGENTS.md": "Be brief." } })).await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));
    let project = Project::test(fs.clone(), ["/root".as_ref()], cx).await;
    let window = cx.add_window(|window, cx| workspace::MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace = window.read_with(cx, |mw, _| mw.workspace().clone()).unwrap();
    let cx = &mut VisualTestContext::from_window(window.into(), cx);

    let agent = AgentSpec {
        id: "mock".into(),
        command: "python3".into(),
        args: vec![std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tools/mock-acp-agent.py").to_string_lossy().into_owned()],
        env: vec![],
        // The project lives in a fake file system; the process needs a real directory.
        cwd: Some(tmp.path().to_path_buf()),
    };
    let config = crate::config::AgentsConfig { instructions_files: crate::config::default_instructions_files(), agents: vec![agent], review_writes: true, verify_changes: true, mcp_servers: vec![], default_agent: None, permissions: Default::default() };
    let thread = workspace.update_in(cx, |ws, window, cx| {
        let thread = cx.new(|cx| Thread::with_config(ws, Some(config), tmp.path().join("history"), window, cx));
        add_thread(ws, thread.clone(), window, cx);
        thread
    });
    let view = workspace.read_with(cx, |ws, cx| ws.items_of_type::<ThreadView>(cx).next()).expect("the thread opened in a tab");
    assert_eq!(view.read_with(cx, |v, _| v.thread().clone()), thread);
    draw(&view, cx);

    // Not connected yet: sending connects first.
    view.update_in(cx, |v, window, cx| {
        v.input.update(cx, |e, cx| e.set_text("hello there", window, cx));
        v.send(&Send, window, cx);
    });
    wait_for(cx, &thread, "an answer", |t| t.status() == Status::Ready && t.entries.iter().any(|e| matches!(e, Entry::Agent(_)))).await;
    assert_eq!(view.read_with(cx, |v, cx| v.tab_content_text(0, cx)), "hello there");
    assert_eq!(thread.read_with(cx, |t, _| t.summary().0), "Answered");
    assert_eq!(view.read_with(cx, |v, cx| v.input.read(cx).text(cx)), "", "the input clears once sent");
    // Forge's default policy puts the agent in its own auto mode.
    assert_eq!(thread.read_with(cx, |t, _| t.current_mode.clone()), Some("auto".to_string()));
    // The project's standing instructions go with the session's first message only.
    let notes = |cx: &mut VisualTestContext| thread.read_with(cx, |t, _| t.entries.iter().filter(|e| matches!(e, Entry::System(text, _) if text == "Sent your instructions from .forge/AGENTS.md.")).count());
    assert_eq!(notes(cx), 1);
    let received = |cx: &mut VisualTestContext| thread.read_with(cx, |t, cx| t.entries.iter().filter(|e| matches!(e, Entry::Agent(md) if md.read(cx).source().contains("Instructions received"))).count());
    assert_eq!(received(cx), 1, "the agent got them");
    draw(&view, cx);

    // Following the agent: the file it reads opens next to the thread, at its line.
    let followed_row = |cx: &mut VisualTestContext| {
        view.update_in(cx, |v, _, cx| {
            let editor = v.follow.as_ref()?.editor.clone()?;
            let path = v.follow.as_ref()?.location.path.clone();
            Some((path, editor.update(cx, |e, cx| e.selections.newest::<Point>(&e.display_snapshot(cx)).head().row)))
        })
    };
    view.update_in(cx, |v, window, cx| {
        v.input.update(cx, |e, cx| e.set_text("read b.txt 2", window, cx));
        v.send(&Send, window, cx);
    });
    wait_for(cx, &thread, "the agent to read b.txt", |t| t.location().is_some_and(|l| l.line == Some(2))).await;
    cx.run_until_parked();
    assert_eq!(followed_row(cx), Some((std::path::PathBuf::from("/root/b.txt"), 2)), "the follow pane shows the agent's line");
    draw(&view, cx);

    // Paused: the agent moves on, the pane stays; jumping catches up.
    view.update(cx, |v, cx| v.pause_follow(cx));
    wait_for(cx, &thread, "the turn to end", |t| t.status() == Status::Ready).await;
    assert_eq!((notes(cx), received(cx)), (1, 1), "not sent again with later messages");
    view.update_in(cx, |v, window, cx| {
        v.input.update(cx, |e, cx| e.set_text("read b.txt 0", window, cx));
        v.send(&Send, window, cx);
    });
    wait_for(cx, &thread, "the agent to move", |t| t.location().is_some_and(|l| l.line == Some(0))).await;
    cx.run_until_parked();
    assert_eq!(followed_row(cx).map(|(_, row)| row), Some(2), "paused: still where the user left it");
    view.update_in(cx, |v, window, cx| v.jump_to_agent(window, cx));
    cx.run_until_parked();
    assert_eq!(followed_row(cx).map(|(_, row)| row), Some(0), "jumped to the agent");
    view.update(cx, |v, cx| v.close_follow(cx));
    assert!(view.read_with(cx, |v, cx| v.render_follow_visible(cx)) == false);
    wait_for(cx, &thread, "the turn to end", |t| t.status() == Status::Ready).await;

    // An edit waits for review in the thread.
    view.update_in(cx, |v, window, cx| {
        v.input.update(cx, |e, cx| e.set_text("edit a.txt new", window, cx));
        v.send(&Send, window, cx);
    });
    wait_for(cx, &thread, "the change to review", |t| t.pending_reviews() == 1).await;
    assert_eq!(thread.read_with(cx, |t, _| t.summary().0), "1 to review");
    assert_eq!(thread.read_with(cx, |t, _| t.activity()), Some(("Waiting for you: review the change to a.txt".to_string(), true)), "the working bar says it waits for you");
    let touched = thread.read_with(cx, |t, _| t.touched_files());
    assert_eq!(touched.iter().map(|(p, _, _)| p.clone()).collect::<Vec<_>>(), [std::path::PathBuf::from("/root/a.txt")]);
    draw(&view, cx);

    // The proposal is editable: the user's version is what gets written.
    let review = thread.read_with(cx, |t, _| t.entries.iter().position(|e| matches!(e, Entry::Review { .. })).unwrap());
    let buffer = thread.read_with(cx, |t, _| match &t.entries[review] {
        Entry::Review { diff, .. } => diff.buffer.clone(),
        _ => unreachable!(),
    });
    let editable = thread.read_with(cx, |t, cx| match &t.entries[review] {
        Entry::Review { diff, .. } => !diff.editor.read(cx).read_only(cx),
        _ => unreachable!(),
    });
    assert!(editable, "a pending change can be edited in the thread");
    buffer.update(cx, |b, cx| {
        let len = b.len();
        b.edit([(0..len, "new, tweaked\n")], None, cx);
    });
    cx.run_until_parked();
    thread.update(cx, |t, cx| t.answer_review(review, true, cx));
    wait_for(cx, &thread, "the turn to end", |t| t.status() == Status::Ready && t.pending_reviews() == 0).await;
    cx.run_until_parked();
    assert_eq!(fs.load("/root/a.txt".as_ref()).await.unwrap(), "new, tweaked\n");
    let outcome = thread.read_with(cx, |t, _| match &t.entries[review] {
        Entry::Review { outcome, .. } => *outcome,
        _ => unreachable!(),
    });
    assert_eq!(outcome, Some("Applied with your edits"));
    draw(&view, cx);

    // The turn remembers what it changed and how long it took; its summary unfolds a
    // file's diff in the conversation.
    let (user_ix, files, took, activity) = thread.read_with(cx, |t, _| {
        let turn = t.checkpoints.last().unwrap();
        (turn.entry, turn.turn_files.clone(), turn.duration, t.activity())
    });
    assert_eq!(files.into_iter().collect::<Vec<_>>(), [(std::path::PathBuf::from("/root/a.txt"), (Some("old\n".to_string()), "new, tweaked\n".to_string()))]);
    assert!(took.is_some() && activity.is_none());
    view.update_in(cx, |v, window, cx| {
        let edit = crate::diff::Edit { path: "/root/a.txt".into(), old_text: Some("old\n".into()), new_text: "new, tweaked\n".into() };
        let languages = v.thread.read(cx).languages().clone();
        v.turn_diffs.insert((user_ix, "/root/a.txt".into()), crate::diff::DiffView::new(edit, languages, window, cx));
    });
    draw(&view, cx);
}

fn mock_auth_agent(marker: &std::path::Path) -> AgentSpec {
    AgentSpec {
        id: "mock-auth".into(),
        command: "python3".into(),
        args: vec![std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tools/mock-acp-agent.py").to_string_lossy().into_owned()],
        env: vec![("MOCK_REQUIRE_AUTH".into(), "1".into()), ("MOCK_AUTH_MARKER".into(), marker.to_string_lossy().into_owned())],
        cwd: None,
    }
}

fn auth_card(t: &Thread) -> Option<(usize, &crate::thread::AuthState, bool)> {
    t.entries.iter().enumerate().find_map(|(i, e)| match e {
        Entry::Auth { state, terminal, .. } => Some((i, state, terminal.is_some())),
        _ => None,
    })
}

/// A thread tab for a fresh workspace without folders, with `config`'s agents.
/// A thread in a worktree changes the worktree's files, not yours; applying brings its
/// changes over, and removing the worktree ends the thread.
/// `/name …` sends the user's prompt from `.forge/prompts/name.md`, with what follows
/// the command in place of `$ARGUMENTS`; the thread shows what was typed.
#[gpui::test]
async fn sends_the_users_prompts(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("project");
    // The prompts are read from disk; the files the agent reads, from the project.
    std::fs::create_dir_all(root.join(".forge/prompts")).unwrap();
    std::fs::write(root.join(".forge/prompts/peek.md"), "---\ndescription: Read a file\n---\nread $ARGUMENTS").unwrap();
    cx.update(workspace::AppState::test);
    cx.update(|cx| {
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        gpui_tokio::init(cx);
        editor::init(cx);
        init(cx);
    });
    let fs = fs::FakeFs::new(cx.executor());
    fs.insert_tree(&root, json!({ "b.txt": "uno" })).await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));
    let project = Project::test(fs.clone(), [root.as_path()], cx).await;
    let window = cx.add_window(|window, cx| workspace::MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace = window.read_with(cx, |mw, _| mw.workspace().clone()).unwrap();
    let cx = &mut VisualTestContext::from_window(window.into(), cx);
    let agent = AgentSpec {
        id: "mock".into(),
        command: "python3".into(),
        args: vec![std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tools/mock-acp-agent.py").to_string_lossy().into_owned()],
        env: vec![],
        cwd: Some(tmp.path().to_path_buf()),
    };
    let config = crate::config::AgentsConfig { instructions_files: crate::config::default_instructions_files(), agents: vec![agent], review_writes: true, verify_changes: true, mcp_servers: vec![], default_agent: None, permissions: Default::default() };
    let thread = workspace.update_in(cx, |ws, window, cx| {
        let thread = cx.new(|cx| Thread::with_config(ws, Some(config), tmp.path().join("history"), window, cx));
        add_thread(ws, thread.clone(), window, cx);
        thread
    });

    let peek = thread.read_with(cx, |t, _| t.commands().into_iter().find(|c| c.name == "peek"));
    assert_eq!(peek.map(|c| (c.description, c.hint)), Some(("Read a file (your prompt)".to_string(), Some("…".to_string()))));

    thread.update_in(cx, |t, window, cx| t.ask("/peek b.txt".into(), window, cx));
    wait_for(cx, &thread, "the prompt's answer", |t| t.status() == Status::Ready && t.entries.iter().any(|e| matches!(e, Entry::Agent(_)))).await;
    let (user, answered) = thread.read_with(cx, |t, cx| {
        let user = t.entries.iter().find_map(|e| match e {
            Entry::User(text, labels) => Some((text.clone(), labels.clone())),
            _ => None,
        });
        let answered = t.entries.iter().any(|e| matches!(e, Entry::Agent(md) if md.read(cx).source().contains("Read: uno")));
        (user, answered)
    });
    assert_eq!(user, Some(("/peek b.txt".to_string(), vec!["prompt .forge/prompts/peek.md".to_string()])));
    assert!(answered, "the agent got the prompt, expanded");
}

#[gpui::test]
async fn a_thread_works_in_its_own_worktree(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let tmp = tempfile::tempdir().unwrap();
    let repo = std::fs::canonicalize(tmp.path()).unwrap().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let git = |args: &[&str]| assert!(std::process::Command::new("git").arg("-C").arg(&repo).args(args).status().unwrap().success(), "git {args:?}");
    git(&["init", "-q", "-b", "main"]);
    git(&["config", "user.name", "Test"]);
    git(&["config", "user.email", "test@example.com"]);
    std::fs::write(repo.join("lib.txt"), "original\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "-qm", "init"]);

    cx.update(workspace::AppState::test);
    cx.update(|cx| {
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        gpui_tokio::init(cx);
        editor::init(cx);
        init(cx);
    });
    let fs = fs::RealFs::new(None, cx.executor());
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));
    let project = Project::test(fs.clone(), [repo.as_path()], cx).await;
    let window = cx.add_window(|window, cx| workspace::MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace = window.read_with(cx, |mw, _| mw.workspace().clone()).unwrap();
    let cx = &mut VisualTestContext::from_window(window.into(), cx);

    let worktree = crate::worktree::create(&repo).unwrap();
    let agent = AgentSpec {
        id: "mock".into(),
        command: "python3".into(),
        args: vec![std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tools/mock-acp-agent.py").to_string_lossy().into_owned()],
        env: vec![],
        cwd: Some(tmp.path().to_path_buf()),
    };
    let config = crate::config::AgentsConfig { instructions_files: crate::config::default_instructions_files(), agents: vec![agent], review_writes: false, verify_changes: false, mcp_servers: vec![], default_agent: None, permissions: Default::default() };
    let history = tmp.path().join("history");
    let thread = workspace.update_in(cx, |ws, window, cx| {
        let thread = cx.new(|cx| Thread::with_config_in(ws, Some(config), history, Some(worktree.clone()), window, cx));
        add_thread(ws, thread.clone(), window, cx);
        thread
    });
    assert_eq!(thread.read_with(cx, |t, _| t.root().clone()), worktree.path);
    let view = workspace.read_with(cx, |ws, cx| ws.items_of_type::<ThreadView>(cx).next()).unwrap();
    view.update_in(cx, |v, window, cx| {
        v.input.update(cx, |e, cx| e.set_text("edit lib.txt changed", window, cx));
        v.send(&Send, window, cx);
    });
    let has_system = |t: &Thread, text: &str| t.entries.iter().any(|e| matches!(e, Entry::System(s, _) if s.contains(text)));
    wait_for(cx, &thread, "the edit", |t| t.status() == Status::Ready && t.entries.iter().any(|e| matches!(e, Entry::Agent(_)))).await;
    let read = |path: &std::path::Path| std::fs::read_to_string(path).unwrap();
    assert_eq!(read(&worktree.path.join("lib.txt")), "changed\n", "the agent wrote in its worktree");
    assert_eq!(read(&repo.join("lib.txt")), "original\n", "your files are untouched");
    draw(&view, cx);

    thread.update(cx, |t, cx| t.apply_worktree(cx));
    wait_for(cx, &thread, "the changes to apply", |t| has_system(t, "Applied the changes to 1 file")).await;
    assert_eq!(read(&repo.join("lib.txt")), "changed\n");

    thread.update(cx, |t, cx| t.remove_worktree(false, cx));
    wait_for(cx, &thread, "the worktree to go", |t| has_system(t, "Removed the worktree")).await;
    assert!(!worktree.path.exists());
    assert!(thread.read_with(cx, |t, _| t.worktree().is_none()));
    thread.update_in(cx, |t, window, cx| t.connect(window, cx));
    assert!(thread.read_with(cx, |t, _| has_system(t, "start a new thread")), "nowhere left to work");
}

/// The editor's "Resolve with Agent" on a merge conflict asks the thread on screen.
#[gpui::test]
async fn conflicts_go_to_the_thread_on_screen(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let tmp = tempfile::tempdir().unwrap();
    let agent = AgentSpec {
        id: "mock".into(),
        command: "python3".into(),
        args: vec![std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tools/mock-acp-agent.py").to_string_lossy().into_owned()],
        env: vec![],
        cwd: Some(tmp.path().to_path_buf()),
    };
    let config = crate::config::AgentsConfig { instructions_files: crate::config::default_instructions_files(), agents: vec![agent], review_writes: true, verify_changes: false, mcp_servers: vec![], default_agent: None, permissions: Default::default() };
    let (thread, view, mut cx) = thread_tab(config, tmp.path().join("history"), cx).await;
    let cx = &mut cx;
    cx.update(|_, cx| crate::conflicts::init(cx));
    let conflict = zed_actions::agent::ConflictContent { file_path: "/p/a.rs".into(), conflict_text: "<<<<<<< HEAD\na\n=======\nb\n>>>>>>> feature\n".into(), ours_branch_name: "HEAD".into(), theirs_branch_name: "feature".into() };
    view.update_in(cx, |v, window, cx| {
        window.focus(&v.focus_handle(cx), cx);
        window.dispatch_action(Box::new(zed_actions::agent::ResolveConflictsWithAgent { conflicts: vec![conflict] }), cx);
    });
    wait_for(cx, &thread, "the conflict to be asked", |t| t.entries.iter().any(|e| matches!(e, Entry::User(text, _) if text.starts_with("Resolve this merge conflict.")))).await;
}

async fn thread_tab(config: crate::config::AgentsConfig, history: std::path::PathBuf, cx: &mut TestAppContext) -> (Entity<Thread>, Entity<ThreadView>, VisualTestContext) {
    let params = cx.update(workspace::AppState::test);
    cx.update(|cx| {
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        gpui_tokio::init(cx);
        editor::init(cx);
        init(cx);
    });
    let project = Project::test(params.fs.clone(), [], cx).await;
    let window = cx.add_window(|window, cx| workspace::MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace = window.read_with(cx, |mw, _| mw.workspace().clone()).unwrap();
    let mut cx = VisualTestContext::from_window(window.into(), cx);
    cx.update(|_, cx| crate::settings::set_in_memory(config.clone(), history.clone(), cx));
    let thread = workspace.update_in(&mut cx, |ws, window, cx| {
        let thread = cx.new(|cx| Thread::with_config(ws, Some(config), history, window, cx));
        add_thread(ws, thread.clone(), window, cx);
        thread
    });
    let view = workspace.read_with(&cx, |ws, cx| ws.items_of_type::<ThreadView>(cx).next()).unwrap();
    (thread, view, cx)
}

/// The whole login inside a thread: card appears, login runs in a real Zed terminal, the
/// agent restarts signed in and the failed prompt is back in the input.
#[gpui::test]
async fn signs_in_without_leaving_forge(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let tmp = tempfile::tempdir().unwrap();
    let marker = tmp.path().join("signed-in");
    let config = crate::config::AgentsConfig { instructions_files: crate::config::default_instructions_files(), agents: vec![mock_auth_agent(&marker)], review_writes: true, verify_changes: true, mcp_servers: vec![], default_agent: None, permissions: Default::default() };
    let (thread, view, mut cx) = thread_tab(config, tmp.path().join("history"), cx).await;
    let cx = &mut cx;

    thread.update_in(cx, |t, window, cx| t.connect(window, cx));
    wait_for(cx, &thread, "connection", |t| t.status() == Status::Ready).await;
    wait_for(cx, &thread, "sign-in card", |t| auth_card(t).is_some()).await;
    assert_eq!(thread.read_with(cx, |t, _| t.auth_methods[0].id.clone()), "mock-login");

    // A prompt fails for lack of auth; it is remembered for after the login.
    view.update_in(cx, |v, window, cx| {
        v.input.update(cx, |e, cx| e.set_text("hello after login", window, cx));
        v.send(&Send, window, cx);
    });
    wait_for(cx, &thread, "failed prompt", |t| t.retry_prompt.is_some()).await;

    // Click the method: the login runs in an embedded, interactive Zed terminal.
    let ix = thread.read_with(cx, |t, _| auth_card(t).unwrap().0);
    thread.update_in(cx, |t, window, cx| t.run_auth(ix, 0, window, cx));
    wait_for(cx, &thread, "login terminal", |t| auth_card(t).is_some_and(|(_, _, has_terminal)| has_terminal)).await;
    let terminal = thread.read_with(cx, |t, cx| match &t.entries[ix] {
        Entry::Auth { terminal: Some(view), .. } => view.read(cx).terminal().clone(),
        _ => unreachable!(),
    });
    // Wait for the prompt, then press Enter like the user would.
    let deadline = Instant::now() + Duration::from_secs(20);
    while !terminal.read_with(cx, |t, _| t.get_content().contains("Press Enter")) {
        assert!(Instant::now() < deadline, "login prompt never appeared");
        cx.run_until_parked();
        cx.background_executor.timer(Duration::from_millis(20)).await;
    }
    terminal.update(cx, |t, _| t.input(b"\r".to_vec()));

    wait_for(cx, &thread, "signed in", |t| matches!(auth_card(t), Some((_, crate::thread::AuthState::Done, _)))).await;
    assert!(marker.exists());
    wait_for(cx, &thread, "restarted agent", |t| t.status() == Status::Ready && t.retry_prompt.is_none()).await;
    let input = view.read_with(cx, |v, cx| v.input.read(cx).text(cx));
    let kept = thread.read_with(cx, |t, _| t.entries.iter().any(|e| matches!(e, Entry::User(text, _) if text == "hello after login")));
    assert_eq!(input, "hello after login", "failed prompt is back in the input");
    assert!(kept, "transcript kept across the restart");

    // Now the agent answers.
    view.update_in(cx, |v, window, cx| v.send(&Send, window, cx));
    wait_for(cx, &thread, "answer", |t| t.status() == Status::Ready && t.entries.iter().any(|e| matches!(e, Entry::Agent(_)))).await;
    draw(&view, cx);
}

/// `ask` (behind "Fix with agent") connects a disconnected agent and sends the prompt;
/// the conversation is saved and listed under Earlier in a new thread's rail.
/// Agents like Copilot CLI refuse `session/new` until signed in: the connection fails,
/// and the sign-in card still offers the login methods `initialize` listed.
#[gpui::test]
async fn a_session_refused_for_lack_of_sign_in_offers_the_login(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let tmp = tempfile::tempdir().unwrap();
    let mut agent = mock_auth_agent(&tmp.path().join("signed-in"));
    agent.env.push(("MOCK_AUTH_ON_NEW_SESSION".into(), "1".into()));
    let config = crate::config::AgentsConfig { instructions_files: crate::config::default_instructions_files(), agents: vec![agent], review_writes: true, verify_changes: true, mcp_servers: vec![], default_agent: None, permissions: Default::default() };
    let (thread, _view, mut cx) = thread_tab(config, tmp.path().join("history"), cx).await;
    let cx = &mut cx;
    thread.update_in(cx, |t, window, cx| t.connect(window, cx));
    wait_for(cx, &thread, "the refused connection", |t| t.status() == Status::Disconnected && t.entries.iter().any(|e| matches!(e, Entry::System(text, _) if text.starts_with("Could not connect")))).await;
    wait_for(cx, &thread, "sign-in card", |t| auth_card(t).is_some()).await;
    assert_eq!(thread.read_with(cx, |t, _| t.auth_methods.iter().map(|m| m.id.clone()).collect::<Vec<_>>()), ["mock-login"]);
}

#[gpui::test]
async fn ask_connects_sends_and_saves(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let tmp = tempfile::tempdir().unwrap();
    let mut agent = mock_auth_agent(&tmp.path().join("unused"));
    agent.env.retain(|(key, _)| key != "MOCK_REQUIRE_AUTH");
    let config = crate::config::AgentsConfig { instructions_files: crate::config::default_instructions_files(), agents: vec![agent], review_writes: true, verify_changes: true, mcp_servers: vec![], default_agent: None, permissions: Default::default() };
    let (thread, view, mut cx) = thread_tab(config, tmp.path().join("history"), cx).await;
    let cx = &mut cx;

    thread.update_in(cx, |t, window, cx| t.ask("why does this test fail?".into(), window, cx));
    wait_for(cx, &thread, "answer to the asked prompt", |t| {
        t.status() == Status::Ready
            && t.entries.iter().any(|e| matches!(e, Entry::User(text, _) if text == "why does this test fail?"))
            && t.entries.iter().any(|e| matches!(e, Entry::Agent(_)))
    })
    .await;
    assert!(thread.read_with(cx, |t, _| t.queued_prompt.is_none()));

    // Closed, it is listed under Earlier in the Threads panel; opening it shows the
    // transcript in a tab of its own.
    let workspace = view.read_with(cx, |v, _| v.workspace.upgrade().unwrap());
    let panel = workspace.update_in(cx, |ws, window, cx| cx.new(|cx| crate::threads_panel::ThreadsPanel::new(ws, window, cx)));
    assert!(panel.read_with(cx, |p, _| p.saved().is_empty()), "open threads aren't listed as saved");
    panel.update_in(cx, |p, window, cx| p.close_thread(&thread, window, cx));
    cx.run_until_parked();
    assert!(workspace.read_with(cx, |ws, cx| ws.items_of_type::<ThreadView>(cx).next().is_none()), "closing a thread closes its tab");
    let saved = panel.update(cx, |p, cx| {
        p.refresh_saved(cx);
        p.saved().to_vec()
    });
    assert_eq!(saved.iter().map(|s| s.title.as_str()).collect::<Vec<_>>(), ["why does this test fail?"]);
    panel.update_in(cx, |p, window, cx| p.open_saved(&saved[0], window, cx));
    cx.run_until_parked();
    let reopened = workspace.read_with(cx, |ws, cx| ws.items_of_type::<ThreadView>(cx).next()).expect("a tab for the saved conversation");
    let reopened_thread = reopened.read_with(cx, |v, _| v.thread().clone());
    assert!(reopened_thread != thread, "a saved conversation opens as a new thread");
    assert_eq!(reopened_thread.read_with(cx, |t, _| t.title()), Some("why does this test fail?".into()));
    let view = reopened;
    draw(&view, cx);

    // Deleting it (once confirmed) closes it and drops it from Earlier for good.
    panel.update_in(cx, |p, window, cx| p.delete_thread(&reopened_thread, window, cx));
    cx.run_until_parked();
    cx.simulate_prompt_answer("Delete");
    cx.run_until_parked();
    assert!(workspace.read_with(cx, |ws, cx| ws.items_of_type::<ThreadView>(cx).next().is_none()), "its tab is closed");
    let saved = panel.update(cx, |p, cx| {
        p.refresh_saved(cx);
        p.saved().to_vec()
    });
    assert!(saved.is_empty(), "the conversation is deleted, not saved");
}

/// Permission requests the policy allows are answered for the user (and say so); the
/// rest wait. The agent's token usage reaches the thread.
#[gpui::test]
async fn answers_allowed_requests_and_tracks_usage(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let tmp = tempfile::tempdir().unwrap();
    let mut agent = mock_auth_agent(&tmp.path().join("unused"));
    agent.env.retain(|(key, _)| key != "MOCK_REQUIRE_AUTH");
    let permissions = crate::permissions::Permissions { allow_commands: vec!["echo".into()], ..Default::default() };
    let config = crate::config::AgentsConfig { instructions_files: crate::config::default_instructions_files(), agents: vec![agent], review_writes: true, verify_changes: true, mcp_servers: vec![], default_agent: None, permissions };
    let (thread, _view, mut cx) = thread_tab(config, tmp.path().join("history"), cx).await;
    let cx = &mut cx;
    let permission = |t: &Thread| {
        t.entries.iter().rev().find_map(|e| match e {
            Entry::Permission { title, resolved, .. } => Some((title.clone(), resolved.clone())),
            _ => None,
        })
    };

    thread.update_in(cx, |t, window, cx| t.ask("run echo hola".into(), window, cx));
    wait_for(cx, &thread, "the allowed command to finish", |t| t.status() == Status::Ready && permission(t).is_some()).await;
    let (title, resolved) = thread.read_with(cx, |t, _| permission(t)).unwrap();
    assert!(title.contains("echo hola"));
    assert!(resolved.as_deref().is_some_and(|r| r.starts_with("Allowed automatically")), "answered for the user: {resolved:?}");
    let usage = thread.read_with(cx, |t, _| t.usage.clone());
    assert_eq!(usage.context_size, Some(200_000));
    assert!(usage.context_used.is_some_and(|u| u > 8_000) && usage.turns == 1 && usage.cost.is_some(), "{usage:?}");
    // The agent's modes (Forge's default policy picked its auto mode), and its plan
    // limits from the hidden `/usage` session.
    assert_eq!(thread.read_with(cx, |t, _| (t.modes.len(), t.current_mode.clone())), (4, Some("auto".into())));
    let deadline = Instant::now() + Duration::from_secs(20);
    let plan = loop {
        cx.run_until_parked();
        if let Some(plan) = thread.read_with(cx, |t, cx| t.plan_usage(cx)) {
            break plan;
        }
        assert!(Instant::now() < deadline, "timed out waiting for the plan usage");
        cx.background_executor.timer(Duration::from_millis(20)).await;
    };
    assert_eq!(plan.tightest().map(|l| l.percent), Some(42.0));
    let shown_usage = thread.read_with(cx, |t, cx| {
        t.entries.iter().any(|e| match e {
            Entry::Agent(md) => md.read(cx).source().contains("5-hour"),
            Entry::User(text, _) => text == "/usage",
            _ => false,
        })
    });
    assert!(!shown_usage, "the /usage exchange stays out of the conversation");
    thread.update(cx, |t, cx| t.set_mode("acceptEdits".into(), cx));
    wait_for(cx, &thread, "the mode to change", |t| t.current_mode.as_deref() == Some("acceptEdits")).await;
    // The session settings (model…), changed from Forge, as the agent confirms them.
    let model = |t: &Thread| t.config_options.iter().find(|o| o.id == "model").map(|o| o.current_label());
    assert_eq!(thread.read_with(cx, |t, _| model(t)), Some("Sonnet".into()));
    thread.update(cx, |t, cx| t.set_config_option("model".into(), serde_json::Value::String("opus".into()), cx));
    wait_for(cx, &thread, "the model to change", |t| model(t).as_deref() == Some("Opus")).await;

    thread.update_in(cx, |t, window, cx| t.ask("run ls".into(), window, cx));
    wait_for(cx, &thread, "the permission request", |t| permission(t).is_some_and(|(title, _)| title.contains("`ls`"))).await;
    assert_eq!(thread.read_with(cx, |t, _| permission(t)).unwrap().1, None, "not in the list: it waits for the user");
    assert_eq!(thread.read_with(cx, |t, _| t.status()), Status::Busy);
}

/// What the agent writes is listed with its original; undo puts the file back, keep
/// stops listing it.
#[gpui::test]
async fn lists_keeps_and_undoes_the_agents_changes(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let tmp = tempfile::tempdir().unwrap();
    cx.update(workspace::AppState::test);
    cx.update(|cx| {
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        gpui_tokio::init(cx);
        editor::init(cx);
        init(cx);
    });
    let fs = fs::FakeFs::new(cx.executor());
    fs.insert_tree("/root", json!({ "a.txt": "old\n" })).await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));
    let project = Project::test(fs.clone(), ["/root".as_ref()], cx).await;
    let window = cx.add_window(|window, cx| workspace::MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace = window.read_with(cx, |mw, _| mw.workspace().clone()).unwrap();
    let cx = &mut VisualTestContext::from_window(window.into(), cx);
    let mut agent = mock_auth_agent(&tmp.path().join("unused"));
    agent.env.retain(|(key, _)| key != "MOCK_REQUIRE_AUTH");
    agent.cwd = Some(tmp.path().to_path_buf());
    let permissions = crate::permissions::Permissions { mode: crate::permissions::PermissionMode::AllowEdits, ..Default::default() };
    let config = crate::config::AgentsConfig { instructions_files: crate::config::default_instructions_files(), agents: vec![agent], review_writes: true, verify_changes: true, mcp_servers: vec![], default_agent: None, permissions };
    let thread = workspace.update_in(cx, |ws, window, cx| {
        let thread = cx.new(|cx| Thread::with_config(ws, Some(config), tmp.path().join("history"), window, cx));
        add_thread(ws, thread.clone(), window, cx);
        thread
    });
    let view = workspace.read_with(cx, |ws, cx| ws.items_of_type::<ThreadView>(cx).next()).unwrap();

    thread.update_in(cx, |t, window, cx| t.ask("write a.txt brand new".into(), window, cx));
    wait_for(cx, &thread, "the write to be listed", |t| t.status() == Status::Ready && !t.changes.is_empty()).await;
    let change = thread.read_with(cx, |t, _| t.changes[0].clone());
    assert_eq!((change.path.as_path(), change.original.as_deref(), change.current.as_str()), (std::path::Path::new("/root/a.txt"), Some("old\n"), "brand new"));
    assert_eq!(change.stats(), (1, 1));
    // The review tab: the file against before the agent.
    view.update_in(cx, |v, window, cx| v.open_review(Some("/root/a.txt".into()), window, cx));
    cx.run_until_parked();
    let review = workspace.read_with(cx, |ws, cx| ws.active_item(cx).and_then(|i| i.downcast::<editor::Editor>())).expect("a review tab");
    assert_eq!(review.read_with(cx, |e, cx| e.buffer().read(cx).title(cx).to_string()), "Changes · write a.txt brand new");
    draw(&view, cx);

    thread.update(cx, |t, cx| t.undo_changes(None, cx));
    wait_for(cx, &thread, "the undo", |t| t.changes.is_empty()).await;
    assert_eq!(fs.load("/root/a.txt".as_ref()).await.unwrap(), "old\n", "back as before the agent");

    thread.update_in(cx, |t, window, cx| t.ask("write a.txt again".into(), window, cx));
    wait_for(cx, &thread, "the second write", |t| t.status() == Status::Ready && !t.changes.is_empty()).await;
    thread.update(cx, |t, cx| t.keep_changes(None, cx));
    assert!(thread.read_with(cx, |t, _| t.changes.is_empty()));
    assert_eq!(fs.load("/root/a.txt".as_ref()).await.unwrap(), "again", "kept");
}

/// A file the agent changed shows its changes in a normal editor, with Keep / Undo per
/// change; once nothing is left, the editor is back to git's diff.
#[gpui::test]
async fn keeps_and_undoes_single_changes_in_the_editor(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let tmp = tempfile::tempdir().unwrap();
    let params = cx.update(workspace::AppState::test);
    cx.update(|cx| {
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        gpui_tokio::init(cx);
        editor::init(cx);
        init(cx);
    });
    cx.update(crate::agent_review::init);
    let fs = params.fs.as_fake();
    fs.insert_tree("/root", json!({ "a.txt": "a\nB\nc\nD\n" })).await;
    let project = Project::test(params.fs.clone(), ["/root".as_ref()], cx).await;
    let window = cx.add_window(|window, cx| workspace::MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace = window.read_with(cx, |mw, _| mw.workspace().clone()).unwrap();
    let cx = &mut VisualTestContext::from_window(window.into(), cx);
    let agent = AgentSpec { id: "a".into(), command: "true".into(), args: vec![], env: vec![], cwd: None };
    let config = crate::config::AgentsConfig { instructions_files: crate::config::default_instructions_files(), agents: vec![agent], review_writes: true, verify_changes: true, mcp_servers: vec![], default_agent: None, permissions: Default::default() };
    let thread = workspace.update_in(cx, |ws, window, cx| cx.new(|cx| Thread::with_config(ws, Some(config), tmp.path().join("history"), window, cx)));
    // The agent changed lines 2 and 4.
    thread.update(cx, |t, cx| {
        t.changes.push(crate::thread::ChangedFile { path: "/root/a.txt".into(), original: Some("a\nb\nc\nd\n".into()), current: "a\nB\nc\nD\n".into() });
        t.changes_updated(cx);
    });
    let item = workspace.update_in(cx, |ws, window, cx| ws.open_abs_path("/root/a.txt".into(), workspace::OpenOptions::default(), window, cx)).await.unwrap();
    let editor = item.downcast::<editor::Editor>().unwrap();
    cx.run_until_parked();
    let hunk_rows = |cx: &mut VisualTestContext| {
        editor.update(cx, |e, cx| {
            let snapshot = e.buffer().read(cx).snapshot(cx);
            snapshot.diff_hunks().map(|h| h.row_range.start.0).collect::<Vec<_>>()
        })
    };
    let rows = hunk_rows(cx);
    assert_eq!(rows.len(), 2, "both changes show in the editor");

    let at_row = |row: u32, cx: &mut VisualTestContext| {
        editor.update_in(cx, |e, window, cx| {
            e.change_selections(Default::default(), window, cx, |s| s.select_ranges([language::Point::new(row, 0)..language::Point::new(row, 0)]));
        });
    };
    at_row(rows[0], cx);
    workspace.update_in(cx, |_, window, cx| window.dispatch_action(Box::new(crate::agent_review::KeepAgentChange), cx));
    cx.run_until_parked();
    assert_eq!(thread.read_with(cx, |t, _| t.changes[0].original.clone()).as_deref(), Some("a\nB\nc\nd\n"), "kept: part of the baseline");
    let rows = hunk_rows(cx);
    assert_eq!(rows.len(), 1);

    at_row(rows[0], cx);
    workspace.update_in(cx, |_, window, cx| window.dispatch_action(Box::new(crate::agent_review::UndoAgentChange), cx));
    cx.run_until_parked();
    assert_eq!(editor.read_with(cx, |e, cx| e.buffer().read(cx).snapshot(cx).text()), "a\nB\nc\nd\n", "undone: the old line is back");
    assert!(thread.read_with(cx, |t, _| t.changes.is_empty()), "nothing left to review");
    assert_eq!(fs.load("/root/a.txt".as_ref()).await.unwrap(), "a\nB\nc\nd\n", "saved");
    assert!(editor.read_with(cx, |e, _| e.addon::<crate::agent_review::Overlay>().is_none()), "back to git's diff");
}

/// Each message is a checkpoint: restoring one puts the files the agent wrote since
/// back as they were (created files go away), and the changes list follows.
#[gpui::test]
async fn restores_checkpoints(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let tmp = tempfile::tempdir().unwrap();
    let params = cx.update(workspace::AppState::test);
    cx.update(|cx| {
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        gpui_tokio::init(cx);
        editor::init(cx);
        init(cx);
        <dyn fs::Fs>::set_global(params.fs.clone(), cx);
    });
    let fs = params.fs.as_fake();
    fs.insert_tree("/root", json!({ "a.txt": "v1" })).await;
    let project = Project::test(params.fs.clone(), ["/root".as_ref()], cx).await;
    let window = cx.add_window(|window, cx| workspace::MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace = window.read_with(cx, |mw, _| mw.workspace().clone()).unwrap();
    let cx = &mut VisualTestContext::from_window(window.into(), cx);
    let agent = AgentSpec { id: "a".into(), command: "true".into(), args: vec![], env: vec![], cwd: None };
    let config = crate::config::AgentsConfig { instructions_files: crate::config::default_instructions_files(), agents: vec![agent], review_writes: true, verify_changes: true, mcp_servers: vec![], default_agent: None, permissions: Default::default() };
    let thread = workspace.update_in(cx, |ws, window, cx| cx.new(|cx| Thread::with_config(ws, Some(config), tmp.path().join("history"), window, cx)));
    let message = |text: &str, cx: &mut VisualTestContext| {
        thread.update(cx, |t, _| {
            t.entries.push(Entry::User(text.into(), vec![]));
            t.begin_checkpoint();
            t.entries.len() - 1
        })
    };
    let write = |path: &str, old: Option<&str>, new: &str, cx: &mut VisualTestContext| {
        let record = crate::project_fs::WriteRecord { path: path.into(), old_text: old.map(str::to_string), new_text: new.into() };
        thread.update(cx, |t, cx| t.record_write(record, cx));
    };

    let first = message("one", cx);
    fs.insert_file("/root/a.txt", b"v2".to_vec()).await;
    write("/root/a.txt", Some("v1"), "v2", cx);
    let second = message("two", cx);
    fs.insert_file("/root/a.txt", b"v3".to_vec()).await;
    write("/root/a.txt", Some("v2"), "v3", cx);
    fs.insert_file("/root/b.txt", b"new".to_vec()).await;
    write("/root/b.txt", None, "new", cx);
    assert_eq!(thread.read_with(cx, |t, _| t.checkpoint_at(first).map(|c| c.files.len())), Some(2));
    assert_eq!(thread.read_with(cx, |t, _| t.checkpoint_at(second).map(|c| c.files.len())), Some(2));

    assert_eq!(cx.update(|_, cx| crate::presence::tests::line(&[thread.clone()], cx)).as_deref(), Some("2 files to review"), "the status bar line");
    thread.update(cx, |t, cx| t.restore_checkpoint(second, cx));
    cx.run_until_parked();
    assert_eq!(fs.load("/root/a.txt".as_ref()).await.unwrap(), "v2");
    assert!(!fs.is_file("/root/b.txt".as_ref()).await, "created after the message: removed");
    let changes = thread.read_with(cx, |t, _| t.changes.iter().map(|c| (c.path.clone(), c.original.clone(), c.current.clone())).collect::<Vec<_>>());
    assert_eq!(changes, vec![("/root/a.txt".into(), Some("v1".into()), "v2".into())], "a.txt still differs from before the agent");
    assert!(thread.read_with(cx, |t, _| t.checkpoint_at(second).is_none()), "nothing to restore there any more");

    thread.update(cx, |t, cx| t.restore_checkpoint(first, cx));
    cx.run_until_parked();
    assert_eq!(fs.load("/root/a.txt".as_ref()).await.unwrap(), "v1");
    assert!(thread.read_with(cx, |t, _| t.changes.is_empty()));
}

/// A turn's summary rolls the turn back, or one of its files: they go back as they were
/// before that turn (created files go away), later turns warn first, and the changes
/// list follows.
#[gpui::test]
async fn undoes_a_turn(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let tmp = tempfile::tempdir().unwrap();
    let params = cx.update(workspace::AppState::test);
    cx.update(|cx| {
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        gpui_tokio::init(cx);
        editor::init(cx);
        init(cx);
        <dyn fs::Fs>::set_global(params.fs.clone(), cx);
    });
    let fs = params.fs.as_fake();
    fs.insert_tree("/root", json!({ "a.txt": "v1" })).await;
    let project = Project::test(params.fs.clone(), ["/root".as_ref()], cx).await;
    let window = cx.add_window(|window, cx| workspace::MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace = window.read_with(cx, |mw, _| mw.workspace().clone()).unwrap();
    let cx = &mut VisualTestContext::from_window(window.into(), cx);
    let agent = AgentSpec { id: "a".into(), command: "true".into(), args: vec![], env: vec![], cwd: None };
    let config = crate::config::AgentsConfig { instructions_files: crate::config::default_instructions_files(), agents: vec![agent], review_writes: true, verify_changes: true, mcp_servers: vec![], default_agent: None, permissions: Default::default() };
    let thread = workspace.update_in(cx, |ws, window, cx| cx.new(|cx| Thread::with_config(ws, Some(config), tmp.path().join("history"), window, cx)));
    let message = |text: &str, cx: &mut VisualTestContext| {
        thread.update(cx, |t, _| {
            t.entries.push(Entry::User(text.into(), vec![]));
            t.begin_checkpoint();
            t.entries.len() - 1
        })
    };
    let write = |path: &str, old: Option<&str>, new: &str, cx: &mut VisualTestContext| {
        let record = crate::project_fs::WriteRecord { path: path.into(), old_text: old.map(str::to_string), new_text: new.into() };
        thread.update(cx, |t, cx| t.record_write(record, cx));
    };
    let a = std::path::PathBuf::from("/root/a.txt");
    let b = std::path::PathBuf::from("/root/b.txt");

    let first = message("one", cx);
    fs.insert_file("/root/a.txt", b"v2".to_vec()).await;
    write("/root/a.txt", Some("v1"), "v2", cx);
    let second = message("two", cx);
    fs.insert_file("/root/a.txt", b"v3".to_vec()).await;
    write("/root/a.txt", Some("v2"), "v3", cx);
    fs.insert_file("/root/b.txt", b"new".to_vec()).await;
    write("/root/b.txt", None, "new", cx);
    assert_eq!(thread.read_with(cx, |t, _| t.later_turns_touching(first, None)), vec![a.clone()], "the second turn changed a.txt again");
    assert!(thread.read_with(cx, |t, _| t.later_turns_touching(second, None)).is_empty());

    // One file of the last turn.
    thread.update(cx, |t, cx| t.undo_turn(second, Some(b.clone()), cx));
    cx.run_until_parked();
    assert!(!fs.is_file(&b).await, "created in the turn: removed");
    assert_eq!(fs.load(&a).await.unwrap(), "v3", "the rest of the turn stays");
    assert_eq!(thread.read_with(cx, |t, _| t.turn_at(second).map(|c| c.undone.iter().cloned().collect::<Vec<_>>())), Some(vec![b.clone()]));

    // The rest of it.
    thread.update(cx, |t, cx| t.undo_turn(second, None, cx));
    cx.run_until_parked();
    assert_eq!(fs.load(&a).await.unwrap(), "v2");
    let changes = thread.read_with(cx, |t, _| t.changes.iter().map(|c| (c.path.clone(), c.original.clone(), c.current.clone())).collect::<Vec<_>>());
    assert_eq!(changes, vec![(a.clone(), Some("v1".into()), "v2".into())], "a.txt still differs from before the agent");
    assert!(thread.read_with(cx, |t, _| t.later_turns_touching(first, None)).is_empty(), "the later change is undone already");

    // The first turn, from the view: nothing later to warn about, so no prompt.
    let view = workspace.update_in(cx, |ws, window, cx| cx.new(|cx| ThreadView::new(ws, thread.clone(), window, cx)));
    view.update_in(cx, |v, window, cx| v.undo_turn(first, None, window, cx));
    cx.run_until_parked();
    assert!(!cx.has_pending_prompt());
    assert_eq!(fs.load(&a).await.unwrap(), "v1");
    assert!(thread.read_with(cx, |t, _| t.changes.is_empty()));
}

/// What you send while the agent works waits for the end of the turn, then goes.
#[gpui::test]
async fn queues_messages_while_the_agent_works(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let tmp = tempfile::tempdir().unwrap();
    let mut agent = mock_auth_agent(&tmp.path().join("unused"));
    agent.env.retain(|(key, _)| key != "MOCK_REQUIRE_AUTH");
    agent.cwd = Some(tmp.path().to_path_buf());
    let config = crate::config::AgentsConfig { instructions_files: crate::config::default_instructions_files(), agents: vec![agent], review_writes: true, verify_changes: true, mcp_servers: vec![], default_agent: None, permissions: crate::permissions::Permissions { mode: crate::permissions::PermissionMode::Ask, ..Default::default() } };
    let (thread, view, mut cx) = thread_tab(config, tmp.path().join("history"), cx).await;
    let cx = &mut cx;
    // A write that waits for permission keeps the turn going.
    thread.update_in(cx, |t, window, cx| t.ask("write a.txt hi".into(), window, cx));
    wait_for(cx, &thread, "the permission request", |t| t.pending_reviews() == 1).await;
    view.update_in(cx, |v, window, cx| {
        v.input.update(cx, |e, cx| e.set_text("and then this", window, cx));
        v.send(&Send, window, cx);
    });
    assert_eq!(thread.read_with(cx, |t, _| t.queue.len()), 1, "queued");
    assert_eq!(view.read_with(cx, |v, cx| v.input.read(cx).text(cx)), "", "the input is free again");
    draw(&view, cx);

    let request = thread.read_with(cx, |t, _| {
        t.entries.iter().find_map(|e| match e {
            Entry::Permission { request_id, resolved: None, .. } => Some(request_id.clone()),
            _ => None,
        })
    });
    thread.update(cx, |t, cx| t.answer_permission(request.unwrap(), None, cx));
    wait_for(cx, &thread, "the queued message to be sent and answered", |t| {
        t.queue.is_empty() && t.status() == Status::Ready && t.entries.iter().any(|e| matches!(e, Entry::User(text, _) if text == "and then this"))
    })
    .await;
    let answered = thread.read_with(cx, |t, cx| t.entries.iter().any(|e| matches!(e, Entry::Agent(md) if md.read(cx).source().contains("Echo: and then this"))));
    assert!(answered);

    // A pasted image goes with the next message.
    let png = gpui::Image::from_bytes(gpui::ImageFormat::Png, vec![0x89, b'P', b'N', b'G']);
    cx.write_to_clipboard(gpui::ClipboardItem::new_image(&png));
    view.update_in(cx, |v, window, cx| {
        v.paste(&editor::actions::Paste, window, cx);
        v.input.update(cx, |e, cx| e.set_text("what is this", window, cx));
    });
    assert_eq!(view.read_with(cx, |v, _| v.images.len()), 1, "attached, not pasted as text");
    draw(&view, cx);
    view.update_in(cx, |v, window, cx| v.send(&Send, window, cx));
    wait_for(cx, &thread, "the answer about the image", |t| t.status() == Status::Ready && t.entries.iter().any(|e| matches!(e, Entry::User(text, labels) if text == "what is this" && labels.contains(&"image 1".to_string())))).await;
    let seen = thread.read_with(cx, |t, cx| t.entries.iter().any(|e| matches!(e, Entry::Agent(md) if md.read(cx).source().contains("Images: 1 (image/png)"))));
    assert!(seen, "the agent got the image");
    assert!(view.read_with(cx, |v, _| v.images.is_empty()));

    // A question aside: answered in a hidden session, nothing in the thread.
    let entries = thread.read_with(cx, |t, _| t.entries.len());
    let answer = thread.update(cx, |t, cx| t.ask_aside("on the side".into(), cx)).await.unwrap();
    assert_eq!(answer, "Echo: on the side");
    assert_eq!(thread.read_with(cx, |t, _| t.entries.len()), entries, "the thread shows nothing");

    // Edit: the message goes back to the input.
    let ix = thread.read_with(cx, |t, _| t.entries.iter().position(|e| matches!(e, Entry::User(text, _) if text == "and then this")).unwrap());
    thread.update_in(cx, |t, window, cx| t.edit_message(ix, "and then this".into(), 0, window, cx));
    cx.run_until_parked();
    assert_eq!(view.read_with(cx, |v, cx| v.input.read(cx).text(cx)), "and then this");
}

/// After a turn that wrote files, the thread lists their problems once the language
/// servers have reported, and tells whether they are new.
#[gpui::test]
async fn checks_the_changed_files_after_a_turn(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let tmp = tempfile::tempdir().unwrap();
    let params = cx.update(workspace::AppState::test);
    cx.update(|cx| {
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        gpui_tokio::init(cx);
        editor::init(cx);
        init(cx);
    });
    let fs = params.fs.as_fake();
    fs.insert_tree("/root", json!({ "a.rs": "fn main() {\n    let x: u8 = \"no\";\n}\n" })).await;
    let project = Project::test(params.fs.clone(), ["/root".as_ref()], cx).await;
    let window = cx.add_window(|window, cx| workspace::MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace = window.read_with(cx, |mw, _| mw.workspace().clone()).unwrap();
    let cx = &mut VisualTestContext::from_window(window.into(), cx);
    let agent = AgentSpec { id: "a".into(), command: "true".into(), args: vec![], env: vec![], cwd: None };
    let config = crate::config::AgentsConfig { instructions_files: crate::config::default_instructions_files(), agents: vec![agent], review_writes: true, verify_changes: true, mcp_servers: vec![], default_agent: None, permissions: Default::default() };
    let thread = workspace.update_in(cx, |ws, window, cx| {
        let thread = cx.new(|cx| Thread::with_config(ws, Some(config), tmp.path().join("history"), window, cx));
        add_thread(ws, thread.clone(), window, cx);
        thread
    });
    let view = workspace.read_with(cx, |ws, cx| ws.items_of_type::<ThreadView>(cx).next()).unwrap();
    // A turn that wrote a.rs, which now has an error.
    thread.update(cx, |t, _| {
        t.entries.push(Entry::User("change a.rs".into(), vec![]));
        t.begin_checkpoint();
        t.checkpoints.last_mut().unwrap().files.insert("/root/a.rs".into(), Some("fn main() {}\n".into()));
    });
    project.read_with(cx, |p, _| p.lsp_store()).update(cx, |store, cx| {
        let diagnostic = language::Diagnostic {
            severity: language::DiagnosticSeverity::ERROR,
            message: language::DiagnosticMessage::from("mismatched types"),
            source_kind: language::DiagnosticSourceKind::Pushed,
            is_primary: true,
            ..Default::default()
        };
        let at = text::Unclipped(text::PointUtf16::new(1, 16));
        store.update_diagnostic_entries(lsp::LanguageServerId(0), "/root/a.rs".into(), None, None, vec![language::DiagnosticEntry::new(at..at, diagnostic)], cx).unwrap();
    });
    thread.update(cx, |t, cx| t.check_changed_files(cx));
    assert!(thread.read_with(cx, |t, _| matches!(t.entries.last(), Some(Entry::Check(None)))), "checking");
    cx.executor().advance_clock(std::time::Duration::from_secs(3));
    cx.run_until_parked();
    let checks = thread.read_with(cx, |t, _| match t.entries.last() {
        Some(Entry::Check(Some(checks))) => checks.clone(),
        _ => panic!("the check should be done"),
    });
    assert_eq!(checks.len(), 1);
    assert_eq!(checks[0].problems, vec![crate::verify::Problem { line: 1, error: true, message: "mismatched types".into() }]);
    assert!(checks[0].got_worse(), "a new error");
    draw(&view, cx);

    // The server reports again later (a slow build): the check follows.
    project.read_with(cx, |p, _| p.lsp_store()).update(cx, |store, cx| {
        let warning = language::Diagnostic {
            severity: language::DiagnosticSeverity::WARNING,
            message: language::DiagnosticMessage::from("unused variable"),
            source_kind: language::DiagnosticSourceKind::Pushed,
            is_primary: true,
            ..Default::default()
        };
        let at = text::Unclipped(text::PointUtf16::new(1, 8));
        store.update_diagnostic_entries(lsp::LanguageServerId(0), "/root/a.rs".into(), None, None, vec![language::DiagnosticEntry::new(at..at, warning)], cx).unwrap();
    });
    cx.run_until_parked();
    let problems = thread.read_with(cx, |t, _| match t.entries.last() {
        Some(Entry::Check(Some(checks))) => checks[0].problems.clone(),
        _ => vec![],
    });
    assert_eq!(problems, vec![crate::verify::Problem { line: 1, error: false, message: "unused variable".into() }], "the latest report");
}

/// Two threads changing the same file: both say so, once.
#[gpui::test]
async fn warns_when_two_threads_change_a_file(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let tmp = tempfile::tempdir().unwrap();
    let params = cx.update(workspace::AppState::test);
    cx.update(|cx| {
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        gpui_tokio::init(cx);
        editor::init(cx);
        init(cx);
    });
    params.fs.as_fake().insert_tree("/root", json!({ "a.txt": "one\n" })).await;
    let project = Project::test(params.fs.clone(), ["/root".as_ref()], cx).await;
    let window = cx.add_window(|window, cx| workspace::MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace = window.read_with(cx, |mw, _| mw.workspace().clone()).unwrap();
    let cx = &mut VisualTestContext::from_window(window.into(), cx);
    let agent = AgentSpec { id: "a".into(), command: "true".into(), args: vec![], env: vec![], cwd: None };
    let config = crate::config::AgentsConfig { instructions_files: crate::config::default_instructions_files(), agents: vec![agent], review_writes: true, verify_changes: false, mcp_servers: vec![], default_agent: None, permissions: Default::default() };
    let new = |cx: &mut VisualTestContext| {
        let config = config.clone();
        let history = tmp.path().join("history");
        workspace.update_in(cx, |ws, window, cx| cx.new(|cx| Thread::with_config(ws, Some(config), history, window, cx)))
    };
    let (first, second) = (new(cx), new(cx));
    let write = |thread: &Entity<Thread>, old: &str, new: &str, cx: &mut VisualTestContext| {
        let record = crate::project_fs::WriteRecord { path: "/root/a.txt".into(), old_text: Some(old.into()), new_text: new.into() };
        thread.update(cx, |t, cx| t.record_write(record, cx));
        cx.run_until_parked();
    };
    let warnings = |thread: &Entity<Thread>, cx: &mut VisualTestContext| {
        thread.read_with(cx, |t, _| t.entries.iter().filter(|e| matches!(e, Entry::System(text, _) if text.starts_with("⚠"))).count())
    };
    write(&first, "one\n", "two\n", cx);
    assert_eq!(warnings(&first, cx), 0);
    write(&second, "two\n", "three\n", cx);
    assert_eq!((warnings(&first, cx), warnings(&second, cx)), (1, 1), "both threads say so");
    write(&second, "three\n", "four\n", cx);
    assert_eq!((warnings(&first, cx), warnings(&second, cx)), (1, 1), "once");
}

/// New threads start with the default agent; the New menu can pick another.
#[gpui::test]
async fn new_threads_use_the_default_agent_unless_told_otherwise(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let tmp = tempfile::tempdir().unwrap();
    let agent = |id: &str| AgentSpec { id: id.into(), command: "true".into(), args: vec![], env: vec![], cwd: None };
    let config = crate::config::AgentsConfig { instructions_files: crate::config::default_instructions_files(), agents: vec![agent("a"), agent("b")], review_writes: true, verify_changes: true, mcp_servers: vec![], default_agent: Some("b".into()), permissions: Default::default() };
    let (_, view, mut cx) = thread_tab(config, tmp.path().join("history"), cx).await;
    let cx = &mut cx;
    let workspace = view.read_with(cx, |v, _| v.workspace.upgrade().unwrap());
    let default = workspace.update_in(cx, |ws, window, cx| new_thread(ws, None, window, cx));
    let picked = workspace.update_in(cx, |ws, window, cx| new_thread(ws, Some(0), window, cx));
    assert_eq!(default.read_with(cx, |t, _| t.agent_label()), "b");
    assert_eq!(picked.read_with(cx, |t, _| t.agent_label()), "a");
    let tabs = workspace.read_with(cx, |ws, cx| ws.items_of_type::<ThreadView>(cx).count());
    assert_eq!(tabs, 3, "one tab per thread");
}

/// `cmd-shift-a`: the picker lists a new thread and the open ones, filters as you type
/// and switches to the chosen thread's tab.
#[gpui::test]
async fn the_thread_picker_finds_and_switches_threads(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let tmp = tempfile::tempdir().unwrap();
    let agent = |id: &str| AgentSpec { id: id.into(), command: "true".into(), args: vec![], env: vec![], cwd: None };
    let config = crate::config::AgentsConfig { instructions_files: crate::config::default_instructions_files(), agents: vec![agent("a")], review_writes: true, verify_changes: true, mcp_servers: vec![], default_agent: None, permissions: Default::default() };
    let (first, view, mut cx) = thread_tab(config, tmp.path().join("history"), cx).await;
    let cx = &mut cx;
    let workspace = view.read_with(cx, |v, _| v.workspace.upgrade().unwrap());
    first.update(cx, |t, _| t.entries.push(Entry::User("fix the login bug".into(), vec![])));
    let second = workspace.update_in(cx, |ws, window, cx| new_thread(ws, None, window, cx));
    second.update(cx, |t, _| t.entries.push(Entry::User("write release notes".into(), vec![])));

    workspace.update_in(cx, |ws, window, cx| crate::thread_picker::toggle(ws, window, cx));
    cx.run_until_parked();
    let picker = workspace.read_with(cx, |ws, cx| ws.active_modal::<crate::thread_picker::ThreadPicker>(cx)).expect("picker open");
    let labels = picker.read_with(cx, |p, cx| p.delegate_labels(cx));
    assert_eq!(labels, ["New thread with a", "write release notes", "fix the login bug"], "newest first");

    picker.update_in(cx, |p, window, cx| p.set_query("login", window, cx));
    cx.run_until_parked();
    assert_eq!(picker.read_with(cx, |p, cx| p.delegate_labels(cx)), ["fix the login bug"]);
    let inner = picker.read_with(cx, |p, _| p.picker());
    inner.update_in(cx, |p, window, cx| picker::PickerDelegate::confirm(&mut p.delegate, false, window, cx));
    cx.run_until_parked();

    assert!(workspace.read_with(cx, |ws, cx| ws.active_modal::<crate::thread_picker::ThreadPicker>(cx)).is_none(), "picker closed");
    let active = workspace.read_with(cx, |ws, cx| ws.active_item(cx).and_then(|i| i.downcast::<ThreadView>()).map(|v| v.read(cx).thread.clone())).unwrap();
    assert_eq!(active, first);
}
