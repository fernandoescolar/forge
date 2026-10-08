//! Forge's debugger tools for agents: breakpoints, a debug session of a run target or of
//! tests, stepping, reading where it stopped (frames and local variables) and evaluating
//! expressions. Everything goes through Zed's debugger, so the user sees the session in
//! the Debug panel and the stopped line in the editor while the agent drives it.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use gpui::{AsyncWindowContext, Entity, EntityId};
use language::{Bias, Point};
use project::debugger::breakpoint_store::{Breakpoint, BreakpointEditAction, BreakpointWithPosition};
use project::debugger::session::{Session, SessionEvent, ThreadId, ThreadStatus};
use serde_json::Value;

use crate::forge_mcp::ToolReply;
use crate::forge_tools::{Ide, line_arg, str_arg};

/// How long a session may take to start (it builds first), and to reach a breakpoint.
const START_TIMEOUT: Duration = Duration::from_secs(180);
const PAUSE_TIMEOUT: Duration = Duration::from_secs(60);
const FRAMES: i64 = 8;
const VARIABLES: usize = 40;
const VALUE_CHARS: usize = 200;

fn closed() -> String {
    "The window is closed.".to_string()
}

/// The breakpoints set, one per line: `path:line [if condition]`.
fn list_breakpoints(ide: &Ide, cx: &mut AsyncWindowContext) -> Result<String, String> {
    let project = ide.project.upgrade().ok_or_else(closed)?;
    cx.update(|_, cx| {
        let store = project.read(cx).breakpoint_store();
        let mut lines = Vec::new();
        for (path, breakpoints) in store.read(cx).all_source_breakpoints(cx) {
            for bp in breakpoints {
                let condition = bp.condition.as_deref().map(|c| format!(" if {c}")).unwrap_or_default();
                lines.push(format!("{}:{}{condition}", ide.show(&path), bp.row + 1));
            }
        }
        if lines.is_empty() { "No breakpoints.".to_string() } else { format!("Breakpoints:\n{}", lines.join("\n")) }
    })
    .map_err(|_| closed())
}

/// Sets (or replaces) a breakpoint at `line` of `path`, with an optional condition.
pub(crate) async fn set_breakpoint(ide: Ide, args: Value, cx: &mut AsyncWindowContext) -> ToolReply {
    let path = str_arg(&args, "path").ok_or("`path` is missing.")?;
    let line = line_arg(&args, "line").ok_or("`line` (1-based) is missing.")?;
    let condition = str_arg(&args, "condition");
    let abs = ide.resolve(&path);
    let project = ide.project.upgrade().ok_or_else(closed)?;
    let buffer = project.update(cx, |p, cx| p.open_local_buffer(&abs, cx)).await.map_err(|e| format!("Can't open {path}: {e:#}"))?;
    cx.update(|_, cx| {
        let store = project.read(cx).breakpoint_store();
        // One per line: a new condition replaces the old breakpoint.
        if let Some((buffer, existing)) = store.read(cx).breakpoint_at_row(&abs, line - 1, cx) {
            store.update(cx, |s, cx| s.toggle_breakpoint(buffer, existing, BreakpointEditAction::Toggle, cx));
        }
        let position = buffer.read(cx).anchor_before(buffer.read(cx).clip_point(Point::new(line - 1, 0), Bias::Left));
        let bp = Breakpoint { condition: condition.clone().map(Into::into), ..Breakpoint::new_standard() };
        store.update(cx, |s, cx| s.toggle_breakpoint(buffer, BreakpointWithPosition { position, bp }, BreakpointEditAction::Toggle, cx));
    })
    .map_err(|_| closed())?;
    let condition = condition.map(|c| format!(" (when {c})")).unwrap_or_default();
    Ok(format!("Breakpoint set at {path}:{line}{condition}.\n{}", list_breakpoints(&ide, cx)?))
}

pub(crate) async fn remove_breakpoint(ide: Ide, args: Value, cx: &mut AsyncWindowContext) -> ToolReply {
    let project = ide.project.upgrade().ok_or_else(closed)?;
    if args.get("all").and_then(Value::as_bool) == Some(true) {
        cx.update(|_, cx| project.read(cx).breakpoint_store().update(cx, |s, cx| s.clear_breakpoints(cx))).map_err(|_| closed())?;
        return Ok("Removed every breakpoint.".into());
    }
    let path = str_arg(&args, "path").ok_or("`path` is missing (or pass `all`).")?;
    let line = line_arg(&args, "line").ok_or("`line` (1-based) is missing.")?;
    let abs = ide.resolve(&path);
    let removed = cx
        .update(|_, cx| {
            let store = project.read(cx).breakpoint_store();
            let existing = store.read(cx).breakpoint_at_row(&abs, line - 1, cx);
            existing.map(|(buffer, bp)| store.update(cx, |s, cx| s.toggle_breakpoint(buffer, bp, BreakpointEditAction::Toggle, cx))).is_some()
        })
        .map_err(|_| closed())?;
    if !removed {
        return Err(format!("There is no breakpoint at {path}:{line}.\n{}", list_breakpoints(&ide, cx)?));
    }
    Ok(format!("Removed the breakpoint at {path}:{line}.\n{}", list_breakpoints(&ide, cx)?))
}

fn sessions(ide: &Ide, cx: &mut AsyncWindowContext) -> Result<Vec<Entity<Session>>, String> {
    let project = ide.project.upgrade().ok_or_else(closed)?;
    cx.update(|_, cx| project.read(cx).dap_store().read(cx).sessions().cloned().collect()).map_err(|_| closed())
}

/// The session the agent works with: the newest one still going.
fn active_session(ide: &Ide, cx: &mut AsyncWindowContext) -> Result<Entity<Session>, String> {
    let all = sessions(ide, cx)?;
    cx.update(|_, cx| all.into_iter().filter(|s| !s.read(cx).is_terminated()).last())
        .map_err(|_| closed())?
        .ok_or_else(|| "Nothing is being debugged; start with `start_debugging`.".to_string())
}

/// How waiting for a session to pause ended.
enum Pause {
    Stopped(Option<ThreadId>),
    Ended,
    StillRunning,
}

/// Waits until the session stops (a breakpoint, a step) or ends. `already`: a session that
/// is stopped now counts (right after it starts).
async fn wait_for_pause(session: &Entity<Session>, timeout: Duration, already: bool, start: impl FnOnce(&mut AsyncWindowContext), cx: &mut AsyncWindowContext) -> Result<Pause, String> {
    let (tx, mut rx) = futures::channel::mpsc::unbounded();
    let _subscription = cx
        .update(|_, cx| {
            cx.subscribe(session, move |_, event: &SessionEvent, _| {
                if let SessionEvent::Stopped(thread) = event {
                    let _ = tx.unbounded_send(*thread);
                }
            })
        })
        .map_err(|_| closed())?;
    start(cx);
    if already && session.read_with(cx, |s, _| s.any_stopped_thread()) {
        return Ok(Pause::Stopped(None));
    }
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(thread) = rx.try_recv() {
            return Ok(Pause::Stopped(thread));
        }
        if session.read_with(cx, |s, _| s.is_terminated()) {
            return Ok(Pause::Ended);
        }
        if Instant::now() > deadline {
            return Ok(Pause::StillRunning);
        }
        cx.background_executor().timer(Duration::from_millis(150)).await;
    }
}

/// The stopped thread: `thread`, else the first the session lists as stopped.
async fn stopped_thread(session: &Entity<Session>, thread: Option<ThreadId>, cx: &mut AsyncWindowContext) -> Option<ThreadId> {
    if thread.is_some() {
        return thread;
    }
    for _ in 0..30 {
        let found = session.update(cx, |s, cx| s.threads(cx).into_iter().find(|(_, status)| *status == ThreadStatus::Stopped).map(|(t, _)| ThreadId(t.id)));
        if found.is_some() {
            return found;
        }
        cx.background_executor().timer(Duration::from_millis(100)).await;
    }
    None
}

fn shorten(value: &str) -> String {
    let one_line = value.replace('\n', " ");
    if one_line.chars().count() > VALUE_CHARS { format!("{}…", one_line.chars().take(VALUE_CHARS).collect::<String>()) } else { one_line }
}

/// Where the session stopped: the top frames, and the local variables of the first one.
async fn describe_stop(ide: &Ide, session: &Entity<Session>, thread: Option<ThreadId>, cx: &mut AsyncWindowContext) -> ToolReply {
    let thread = stopped_thread(session, thread, cx).await.ok_or("The program is paused, but the debugger didn't say which thread stopped.")?;
    let client = session.read_with(cx, |s, _| s.adapter_client()).ok_or("The debug adapter isn't reachable.")?;
    let trace = client
        .request::<dap::requests::StackTrace>(dap::StackTraceArguments { thread_id: thread.0, start_frame: None, levels: Some(FRAMES as _), format: None })
        .await
        .map_err(|e| format!("Couldn't read the stack: {e:#}"))?;
    let Some(top) = trace.stack_frames.first() else { return Ok("Paused, with no stack frames to show.".into()) };
    let place = |f: &dap::StackFrame| match f.source.as_ref().and_then(|s| s.path.as_deref()) {
        Some(path) => format!("{}:{}", ide.show(std::path::Path::new(path)), f.line),
        None => "(no source)".to_string(),
    };
    let mut text = format!("Paused in {} at {}.\n\nStack:\n", top.name, place(top));
    for (i, frame) in trace.stack_frames.iter().enumerate() {
        text.push_str(&format!("#{i} {}  {}\n", frame.name, place(frame)));
    }
    let scopes = client.request::<dap::requests::Scopes>(dap::ScopesArguments { frame_id: top.id }).await.map_err(|e| format!("Couldn't read the variables: {e:#}"))?;
    for scope in scopes.scopes.iter().filter(|s| !s.expensive).take(2) {
        let variables = client
            .request::<dap::requests::Variables>(dap::VariablesArguments { variables_reference: scope.variables_reference, filter: None, start: None, count: None, format: None })
            .await
            .map(|v| v.variables)
            .unwrap_or_default();
        if variables.is_empty() {
            continue;
        }
        text.push_str(&format!("\n{}:\n", scope.name));
        for variable in variables.iter().take(VARIABLES) {
            let ty = variable.type_.as_deref().map(|t| format!(": {t}")).unwrap_or_default();
            text.push_str(&format!("{}{ty} = {}\n", variable.name, shorten(&variable.value)));
        }
        if variables.len() > VARIABLES {
            text.push_str(&format!("… and {} more\n", variables.len() - VARIABLES));
        }
    }
    text.push_str("\nNext: `debug_step` (continue, step_over, step_in, step_out), `debug_evaluate`, or `stop_debugging`.");
    Ok(text)
}

async fn report(ide: &Ide, session: &Entity<Session>, pause: Pause, waited: Duration, cx: &mut AsyncWindowContext) -> ToolReply {
    match pause {
        Pause::Stopped(thread) => describe_stop(ide, session, thread, cx).await,
        Pause::Ended => Ok("The program ran to the end without stopping (no breakpoint was hit).".into()),
        Pause::StillRunning => Ok(format!(
            "Still running after {}s without stopping. Set a breakpoint where it should stop, use `debug_step` with `pause`, or `stop_debugging`.",
            waited.as_secs()
        )),
    }
}

/// Starts a debug session of a run target or of tests, and waits for it to stop.
pub(crate) async fn start_debugging(ide: Ide, args: Value, cx: &mut AsyncWindowContext) -> ToolReply {
    let before: HashSet<EntityId> = sessions(&ide, cx)?.iter().map(Entity::entity_id).collect();
    if active_session(&ide, cx).is_ok() {
        return Err("A debug session is already going; stop it first with `stop_debugging`.".into());
    }
    let test_path = str_arg(&args, "test_path").map(|p| ide.resolve(&p));
    let test_name = str_arg(&args, "test_name");
    if test_path.is_some() || test_name.is_some() {
        start_test_session(&ide, test_path.as_deref(), test_name.as_deref(), cx)?;
    } else {
        start_target_session(&ide, str_arg(&args, "target"), cx)?;
    }
    // It builds first: wait for the new session.
    let started = Instant::now();
    let session = loop {
        if let Some(session) = sessions(&ide, cx)?.into_iter().find(|s| !before.contains(&s.entity_id())) {
            break session;
        }
        if started.elapsed() > START_TIMEOUT {
            return Err("The debug session didn't start (did the build fail? the Debug panel and the terminal say why).".into());
        }
        cx.background_executor().timer(Duration::from_millis(250)).await;
    };
    let pause = wait_for_pause(&session, PAUSE_TIMEOUT, true, |_| {}, cx).await?;
    report(&ide, &session, pause, PAUSE_TIMEOUT, cx).await
}

fn start_test_session(ide: &Ide, path: Option<&std::path::Path>, name: Option<&str>, cx: &mut AsyncWindowContext) -> Result<(), String> {
    use forge_tests::{TestPanel, runner::Scope};
    let panel = ide
        .workspace
        .update_in(cx, |ws, window, cx| {
            ws.open_panel::<TestPanel>(window, cx);
            ws.panel::<TestPanel>(cx)
        })
        .map_err(|_| closed())?
        .ok_or("The Tests panel isn't ready yet; try again in a moment.")?;
    panel
        .update_in(cx, |panel, window, cx| {
            let jobs = crate::forge_tools::select_tests(panel.projects(), path, name).unwrap_or_default();
            let Some((manifest, fqns)) = jobs.into_iter().next() else { return Err("No test matches that path or name.".to_string()) };
            let label = format!("Debug {}", name.unwrap_or("tests"));
            if panel.debug_scope(&manifest, Scope::Methods(fqns), label.into(), window, cx) { Ok(()) } else { Err("Forge can't debug those tests (Jest and Vitest tests aren't supported).".to_string()) }
        })
        .map_err(|_| closed())?
}

fn start_target_session(ide: &Ide, target: Option<String>, cx: &mut AsyncWindowContext) -> Result<(), String> {
    let workspace = ide.workspace.upgrade().ok_or_else(closed)?;
    let controller = cx.update(|_, cx| forge_run::RunController::for_workspace(&workspace, cx)).ok().flatten().ok_or("Run isn't set up for this window.")?;
    let wanted = target.map(|t| t.to_lowercase());
    let id = cx
        .update(|_, cx| {
            let c = controller.read(cx);
            if c.state(cx) != forge_run::State::Idle {
                return Err("Something is already running (title bar Run); stop it first.".to_string());
            }
            let names = || c.targets().iter().map(|t| t.name.clone()).collect::<Vec<_>>().join(", ");
            let target = match &wanted {
                Some(w) => c.targets().iter().find(|t| t.name.to_lowercase() == *w || t.id().to_lowercase() == *w).ok_or_else(|| format!("No run target is called that. The targets are: {}", names()))?,
                None => c.selected().or(c.targets().first()).ok_or("Forge found nothing to run in this project.")?,
            };
            Ok(target.id())
        })
        .map_err(|_| closed())??;
    cx.update(|window, cx| {
        controller.update(cx, |c, cx| c.select(id, cx));
        let controller = controller.clone();
        window.defer(cx, move |window, cx| controller.update(cx, |c, cx| c.debug(window, cx)));
    })
    .map_err(|_| closed())
}

/// Continues, steps or pauses the session, and reports where it stops next.
pub(crate) async fn debug_step(ide: Ide, args: Value, cx: &mut AsyncWindowContext) -> ToolReply {
    let action = str_arg(&args, "action").unwrap_or_else(|| "continue".into());
    let session = active_session(&ide, cx)?;
    if action == "pause" {
        let threads = session.update(cx, |s, cx| s.threads(cx));
        let Some((thread, _)) = threads.into_iter().next() else { return Err("The debugger lists no threads to pause.".into()) };
        let pause = wait_for_pause(&session, Duration::from_secs(10), false, |cx| session.update(cx, |s, cx| s.pause_thread(ThreadId(thread.id), cx)), cx).await?;
        return report(&ide, &session, pause, Duration::from_secs(10), cx).await;
    }
    let thread = stopped_thread(&session, None, cx).await.ok_or("The program isn't paused; `debug_step` with `pause` stops it.")?;
    let granularity = dap::SteppingGranularity::Statement;
    let step = |cx: &mut AsyncWindowContext| {
        session.update(cx, |s, cx| match action.as_str() {
            "step_over" => s.step_over(thread, granularity, cx),
            "step_in" => s.step_in(thread, granularity, cx),
            "step_out" => s.step_out(thread, granularity, cx),
            _ => s.continue_program(thread, cx),
        })
    };
    if !matches!(action.as_str(), "continue" | "step_over" | "step_in" | "step_out") {
        return Err(format!("Unknown action `{action}`: use continue, step_over, step_in, step_out or pause."));
    }
    let pause = wait_for_pause(&session, PAUSE_TIMEOUT, false, step, cx).await?;
    report(&ide, &session, pause, PAUSE_TIMEOUT, cx).await
}

/// Evaluates an expression in the paused program's current frame.
pub(crate) async fn debug_evaluate(ide: Ide, args: Value, cx: &mut AsyncWindowContext) -> ToolReply {
    let expression = str_arg(&args, "expression").ok_or("`expression` is missing.")?;
    let session = active_session(&ide, cx)?;
    let thread = stopped_thread(&session, None, cx).await.ok_or("The program isn't paused.")?;
    let client = session.read_with(cx, |s, _| s.adapter_client()).ok_or("The debug adapter isn't reachable.")?;
    let trace = client
        .request::<dap::requests::StackTrace>(dap::StackTraceArguments { thread_id: thread.0, start_frame: None, levels: Some(1), format: None })
        .await
        .map_err(|e| format!("Couldn't read the stack: {e:#}"))?;
    let frame_id = trace.stack_frames.first().map(|f| f.id);
    let result = client
        .request::<dap::requests::Evaluate>(dap::EvaluateArguments {
            expression: expression.clone(),
            frame_id,
            context: Some(dap::EvaluateArgumentsContext::Repl),
            line: None,
            column: None,
            source: None,
            format: None,
        })
        .await
        .map_err(|e| format!("`{expression}` couldn't be evaluated: {e:#}"))?;
    let ty = result.type_.map(|t| format!(" ({t})")).unwrap_or_default();
    Ok(format!("{expression} = {}{ty}", result.result))
}

pub(crate) async fn stop_debugging(ide: Ide, cx: &mut AsyncWindowContext) -> ToolReply {
    let project = ide.project.upgrade().ok_or_else(closed)?;
    if active_session(&ide, cx).is_err() {
        return Ok("Nothing is being debugged.".into());
    }
    let task = cx.update(|_, cx| project.read(cx).dap_store().update(cx, |s, cx| s.shutdown_sessions(cx))).map_err(|_| closed())?;
    task.await;
    Ok("Stopped debugging.".into())
}
