//! Run's tools for agents: `run_app`, `app_output` and `stop_app` drive the title bar's
//! Run as the user would, and `start_debugging` starts sessions through the launcher
//! registered here (run targets here, tests through the Tests panel).

use std::sync::Arc;

use forge_ui::ide_tools::{DebugWhat, IdeTool, ToolContext, ToolEffect, ToolReply, closed, ide_tools, str_arg, tail};
use gpui::{App, AsyncWindowContext, Entity};
use serde_json::{Value, json};

use crate::{RunController, State};

/// Lines of output `app_output` quotes when not told.
const TAIL_LINES: usize = 80;

pub(crate) fn init(_: &mut App) {
    let tools = ide_tools();
    tools.register(IdeTool::new(
        "run_app",
        "Run the app",
        "Start one of the project's run targets as the title bar's Run button does (a .NET app or Aspire host, a Cargo binary, a Go main package, \
a package.json script, a Python program), in a terminal the user sees. Returns its first seconds of output; then use `app_output` and `stop_app`. \
One thing runs at a time.",
        json!({ "type": "object", "properties": { "target": { "type": "string", "description": "The target's name. Leave it out for the one selected in the title bar; a wrong name lists them." } } }),
        ToolEffect::RunsCode,
        |ide, args, mut cx| Box::pin(async move { run_app(ide, args, &mut cx).await }),
    ));
    tools.register(IdeTool::new(
        "app_output",
        "App output",
        "The latest output of what `run_app` (or the user's Run button) started, and whether it still runs.",
        json!({ "type": "object", "properties": { "lines": { "type": "integer", "minimum": 1, "description": "How many of the last lines (default 80)." } } }),
        ToolEffect::Reads,
        |ide, args, mut cx| Box::pin(async move { app_output(ide, args, &mut cx) }),
    ));
    tools.register(IdeTool::new(
        "stop_app",
        "Stop the app",
        "Stop what `run_app` (or the user's Run button) started.",
        json!({ "type": "object", "properties": {} }),
        ToolEffect::Reads,
        |ide, _, mut cx| Box::pin(async move { stop_app(ide, &mut cx) }),
    ));
    tools.set_debug_launcher(Arc::new(|ide, what, cx| match what {
        DebugWhat::Target(target) => start_target_session(ide, target, cx),
        DebugWhat::Tests { path, name } => forge_tests::agent_tools::start_test_session(ide, path.as_deref(), name.as_deref(), cx),
    }));
}

fn run_controller(ide: &ToolContext, cx: &mut AsyncWindowContext) -> Result<Entity<RunController>, String> {
    let workspace = ide.workspace.upgrade().ok_or_else(closed)?;
    cx.update(|_, cx| RunController::for_workspace(&workspace, cx)).ok().flatten().ok_or_else(|| "Run isn't set up for this window.".to_string())
}

/// The run target's output so far (its terminal), the last `lines`.
fn app_output_text(controller: &Entity<RunController>, lines: usize, cx: &App) -> Option<String> {
    let content = controller.read(cx).terminal()?.read(cx).get_content();
    Some(tail(&content, lines))
}

/// Starts a run target (the title bar's Run) and reports its first output.
async fn run_app(ide: ToolContext, args: Value, cx: &mut AsyncWindowContext) -> ToolReply {
    let controller = run_controller(&ide, cx)?;
    let wanted = str_arg(&args, "target").map(|t| t.to_lowercase());
    let picked = cx
        .update(|_, cx| {
            let c = controller.read(cx);
            let names = || c.targets().iter().map(|t| format!("{} ({})", t.name, t.kind.label())).collect::<Vec<_>>().join(", ");
            if c.targets().is_empty() {
                return Err("Forge found nothing to run in this project.".to_string());
            }
            if c.state(cx) != State::Idle {
                return Err("Something is already running (title bar Run); stop it first with `stop_app`.".to_string());
            }
            let target = match &wanted {
                Some(w) => c.targets().iter().find(|t| t.name.to_lowercase() == *w || t.id().to_lowercase() == *w).ok_or_else(|| format!("No run target is called that. The targets are: {}", names()))?,
                None => c.selected().or(c.targets().first()).unwrap(),
            };
            Ok((target.id(), target.name.clone()))
        })
        .map_err(|_| closed())??;
    let (id, name) = picked;
    cx.update(|window, cx| {
        controller.update(cx, |c, cx| c.select(id, cx));
        let controller = controller.clone();
        // As the Run action does: starting a task updates the workspace.
        window.defer(cx, move |window, cx| controller.update(cx, |c, cx| c.run(window, cx)));
    })
    .map_err(|_| closed())?;
    // Its first output: startup errors, the address it listens on…
    cx.background_executor().timer(std::time::Duration::from_secs(5)).await;
    let (state, output) = cx.update(|_, cx| (controller.read(cx).state(cx), app_output_text(&controller, 40, cx))).map_err(|_| closed())?;
    let status = if state == State::Idle { "It already ended." } else { "It is running; `app_output` reads more of its output, `stop_app` stops it." };
    Ok(format!("Started {name} in a terminal the user can see. {status}\n\nOutput so far:\n{}", output.unwrap_or_default()))
}

/// The output of the run target, while it runs or after.
fn app_output(ide: ToolContext, args: Value, cx: &mut AsyncWindowContext) -> ToolReply {
    let controller = run_controller(&ide, cx)?;
    let lines = args.get("lines").and_then(Value::as_u64).map(|l| l.clamp(1, 1000) as usize).unwrap_or(TAIL_LINES);
    cx.update(|_, cx| {
        let running = controller.read(cx).state(cx) != State::Idle;
        let output = app_output_text(&controller, lines, cx).ok_or("Nothing has run yet.")?;
        Ok(format!("{}\n\n{output}", if running { "Running." } else { "Not running anymore." }))
    })
    .map_err(|_| closed())?
}

fn stop_app(ide: ToolContext, cx: &mut AsyncWindowContext) -> ToolReply {
    let controller = run_controller(&ide, cx)?;
    cx.update(|_, cx| {
        if controller.read(cx).state(cx) == State::Idle {
            return Ok("Nothing is running.".to_string());
        }
        controller.update(cx, |c, cx| c.stop(cx));
        Ok("Stopping it (Ctrl+C).".to_string())
    })
    .map_err(|_| closed())?
}

/// Starts debugging a run target, as the title bar's Debug does.
fn start_target_session(ide: &ToolContext, target: Option<String>, cx: &mut AsyncWindowContext) -> Result<(), String> {
    let controller = run_controller(ide, cx)?;
    let wanted = target.map(|t| t.to_lowercase());
    let id = cx
        .update(|_, cx| {
            let c = controller.read(cx);
            if c.state(cx) != State::Idle {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    fn registers_the_app_tools_and_the_debug_launcher(cx: &mut gpui::TestAppContext) {
        cx.update(init);
        assert_eq!(ide_tools().get("run_app").map(|t| t.effect), Some(ToolEffect::RunsCode));
        assert_eq!(ide_tools().get("stop_app").map(|t| t.effect), Some(ToolEffect::Reads));
        assert!(ide_tools().get("app_output").is_some() && ide_tools().debug_launcher().is_some());
    }
}
