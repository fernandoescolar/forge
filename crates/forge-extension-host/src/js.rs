//! QuickJS on a dedicated thread.
//!
//! All extensions share one JS context (and therefore one React), loaded with
//! `packages/forge-api/dist/runtime.js`. The GPUI side talks to it with [`ToJs`] messages
//! and receives [`FromJs`] events; nothing JS-related ever touches the UI thread.

use crate::tree::Op;
use futures::channel::mpsc::UnboundedSender;
use rquickjs::{
    CatchResultExt, Context, Function, Object, Runtime,
    function::{Func, IntoArgs},
};
use serde_json::Value;
use std::{
    cell::RefCell,
    collections::BTreeSet,
    rc::Rc,
    sync::mpsc::{self, RecvTimeoutError},
    thread,
    time::{Duration, Instant},
};

const MEMORY_LIMIT: usize = 512 * 1024 * 1024;

#[derive(Debug)]
pub enum ToJs {
    /// Evaluate an extension bundle and call its `activate`.
    Load { id: String, path: String, code: String },
    /// Deactivate an extension and undo everything it registered.
    Unload { id: String },
    Dispatch { node: u32, event: String, payload: String },
    Resolve { call: u64, ok: bool, json: String },
    RunCommand { id: String },
    /// An agent called an extension's tool; it answers with `agents.toolResult` and `call`.
    RunTool { call: u64, extension: String, tool: String, args: String, context: String },
    /// A webview page posted a message to its extension.
    WebviewMessage { panel: String, json: String },
    /// An extension setting changed (`json` is its new value, `null` once reset).
    SettingChanged { key: String, json: String },
    /// Something an extension listens to happened (`api::ACTIVE_FILE_CHANGED`, …).
    Event { name: String, json: String },
}

#[derive(Debug, PartialEq)]
pub enum FromJs {
    Commit { panel: String, ops: Vec<Op> },
    Call { method: String, args: Value, id: u64 },
    Log { level: String, message: String },
}

#[derive(Clone)]
pub struct JsHost {
    tx: mpsc::Sender<ToJs>,
}

impl JsHost {
    pub fn spawn(runtime_js: String, out: UnboundedSender<FromJs>) -> anyhow::Result<Self> {
        let (tx, rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        thread::Builder::new()
            .name("forge-extensions".into())
            .stack_size(8 * 1024 * 1024)
            .spawn(move || match JsThread::new(&runtime_js, out) {
                Ok(js) => {
                    let _ = ready_tx.send(Ok(()));
                    js.run(rx);
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                }
            })?;
        ready_rx.recv()??;
        Ok(Self { tx })
    }

    pub fn send(&self, msg: ToJs) {
        if self.tx.send(msg).is_err() {
            log::error!("extension runtime is not running");
        }
    }
}

type Timers = Rc<RefCell<BTreeSet<(Instant, u32)>>>;

struct JsThread {
    rt: Runtime,
    ctx: Context,
    timers: Timers,
    out: UnboundedSender<FromJs>,
}

impl JsThread {
    fn new(runtime_js: &str, out: UnboundedSender<FromJs>) -> anyhow::Result<Self> {
        let rt = Runtime::new()?;
        rt.set_memory_limit(MEMORY_LIMIT);
        rt.set_max_stack_size(4 * 1024 * 1024);
        let ctx = Context::full(&rt)?;
        let timers: Timers = Rc::default();
        let this = Self { rt, ctx, timers, out };
        this.install_native()?;
        this.ctx.with(|ctx| -> anyhow::Result<()> {
            ctx.eval::<(), _>(runtime_js).catch(&ctx).map_err(|e| anyhow::anyhow!("runtime.js failed: {e}"))?;
            Ok(())
        })?;
        this.drain_jobs();
        Ok(this)
    }

    /// Installs `globalThis.__forgeNative` (see packages/forge-api/src/native.ts).
    fn install_native(&self) -> anyhow::Result<()> {
        self.ctx.with(|ctx| -> rquickjs::Result<()> {
            let native = Object::new(ctx.clone())?;

            let out = self.out.clone();
            native.set(
                "commit",
                Func::from(move |panel: String, ops: String| match serde_json::from_str::<Vec<Op>>(&ops) {
                    Ok(ops) => {
                        let _ = out.unbounded_send(FromJs::Commit { panel, ops });
                    }
                    Err(e) => log::error!("invalid ops from panel {panel}: {e}"),
                }),
            )?;

            let out = self.out.clone();
            native.set(
                "call",
                Func::from(move |method: String, args: String, id: f64| {
                    let args = serde_json::from_str(&args).unwrap_or(Value::Null);
                    let _ = out.unbounded_send(FromJs::Call { method, args, id: id as u64 });
                }),
            )?;

            let timers = self.timers.clone();
            native.set(
                "setTimer",
                Func::from(move |id: u32, ms: f64| {
                    let at = Instant::now() + Duration::from_millis(ms.max(0.0) as u64);
                    timers.borrow_mut().insert((at, id));
                }),
            )?;

            let timers = self.timers.clone();
            native.set("clearTimer", Func::from(move |id: u32| timers.borrow_mut().retain(|(_, t)| *t != id)))?;

            let out = self.out.clone();
            native.set(
                "log",
                Func::from(move |level: String, message: String| {
                    let _ = out.unbounded_send(FromJs::Log { level, message });
                }),
            )?;

            ctx.globals().set("__forgeNative", native)?;
            Ok(())
        })?;
        Ok(())
    }

    fn run(self, rx: mpsc::Receiver<ToJs>) {
        loop {
            let wait = self.timers.borrow().first().map(|(at, _)| at.saturating_duration_since(Instant::now())).unwrap_or(Duration::from_secs(3600));
            match rx.recv_timeout(wait) {
                Ok(msg) => self.handle(msg),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            self.fire_due_timers();
            self.drain_jobs();
        }
    }

    fn handle(&self, msg: ToJs) {
        match msg {
            ToJs::Load { id, path, code } => {
                self.call_forge("prepare", (id.clone(), path.clone()));
                let code = scoped(&id, &code);
                let result = self.ctx.with(|ctx| ctx.eval::<(), _>(code).catch(&ctx).map_err(|e| e.to_string()));
                match result {
                    Ok(()) => self.call_forge("activate", (id, path)),
                    Err(e) => {
                        self.log("error", format!("failed to load extension {id}: {e}"));
                        self.call_forge("deactivate", (id,));
                    }
                }
            }
            ToJs::Unload { id } => self.call_forge("deactivate", (id,)),
            ToJs::Dispatch { node, event, payload } => self.call_forge("dispatch", (node, event, payload)),
            ToJs::Resolve { call, ok, json } => self.call_forge("resolve", (call as f64, ok, json)),
            ToJs::RunCommand { id } => self.call_forge("runCommand", (id,)),
            ToJs::RunTool { call, extension, tool, args, context } => self.call_forge("runTool", (call as f64, extension, tool, args, context)),
            ToJs::WebviewMessage { panel, json } => self.call_forge("webviewMessage", (panel, json)),
            ToJs::SettingChanged { key, json } => self.call_forge("settingChanged", (key, json)),
            ToJs::Event { name, json } => self.call_forge("event", (name, json)),
        }
    }

    fn fire_due_timers(&self) {
        loop {
            let due = {
                let mut timers = self.timers.borrow_mut();
                match timers.first().copied() {
                    Some((at, id)) if at <= Instant::now() => {
                        timers.remove(&(at, id));
                        Some(id)
                    }
                    _ => None,
                }
            };
            let Some(id) = due else { break };
            self.call_forge("fireTimer", (id,));
            self.drain_jobs();
        }
    }

    /// Runs promise continuations (React and `await` rely on them).
    fn drain_jobs(&self) {
        loop {
            match self.rt.execute_pending_job() {
                Ok(true) => {}
                Ok(false) => break,
                Err(e) => self.log("error", format!("unhandled error in promise job: {e:?}")),
            }
        }
    }

    fn call_forge<A>(&self, name: &str, args: A)
    where
        A: for<'js> IntoArgs<'js>,
    {
        let result = self.ctx.with(|ctx| -> Result<(), String> {
            let forge: Object = ctx.globals().get("__forge").map_err(|e| e.to_string())?;
            let f: Function = forge.get(name).map_err(|e| e.to_string())?;
            f.call::<_, ()>(args).catch(&ctx).map_err(|e| e.to_string())
        });
        if let Err(e) = result {
            self.log("error", format!("__forge.{name} threw: {e}"));
        }
    }

    fn log(&self, level: &str, message: String) {
        let _ = self.out.unbounded_send(FromJs::Log { level: level.into(), message });
    }
}

/// An extension's bundle, run with timer functions of its own (so unloading it stops its
/// timers) and still defining the global `__forgeExtension` the runtime activates.
fn scoped(id: &str, code: &str) -> String {
    let id = serde_json::to_string(id).unwrap_or_default();
    format!(
        "(function (setTimeout, clearTimeout, setInterval, clearInterval) {{\n{code}\n;if (typeof __forgeExtension !== 'undefined') globalThis.__forgeExtension = __forgeExtension;\n}}).apply(globalThis, (t => [t.setTimeout, t.clearTimeout, t.setInterval, t.clearInterval])(__forge.timersFor({id})));"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::{NodeKind, Tree};
    use futures::channel::mpsc::unbounded;

    const FIXTURE: &str = include_str!("../../../packages/forge-api/test/fixture/dist/extension.js");

    fn next_commit(rx: &mut futures::channel::mpsc::UnboundedReceiver<FromJs>, tree: &mut Tree) -> Vec<FromJs> {
        let mut other = vec![];
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            match rx.try_recv() {
                Ok(FromJs::Commit { ops, .. }) => {
                    tree.apply(ops);
                    return other;
                }
                Ok(ev) => other.push(ev),
                _ => thread::sleep(Duration::from_millis(5)),
            }
        }
        panic!("no commit; saw {other:?}");
    }

    #[test]
    fn react_extension_renders_and_reacts_in_quickjs() {
        let (out, mut rx) = unbounded();
        let host = JsHost::spawn(crate::RUNTIME_JS.to_string(), out).unwrap();
        host.send(ToJs::Load { id: "counter".into(), path: "/x".into(), code: FIXTURE.into() });

        let mut tree = Tree::default();
        let before = next_commit(&mut rx, &mut tree);
        assert!(before.iter().any(|e| matches!(e, FromJs::Call { method, .. } if method == "panels.register")), "{before:?}");

        let button = (1..100).find(|id| tree.get(*id).is_some_and(|n| matches!(&n.kind, NodeKind::Element { kind, .. } if kind == "button"))).unwrap();
        assert_eq!(tree.get(button).unwrap().str_prop("label"), Some("Clicked 0 times"));

        host.send(ToJs::Dispatch { node: button, event: "onClick".into(), payload: String::new() });
        next_commit(&mut rx, &mut tree);
        assert_eq!(tree.get(button).unwrap().str_prop("label"), Some("Clicked 1 times"));
    }

    /// An extension's timers stop when it unloads; intervals repeat until then.
    #[test]
    fn unloading_stops_an_extensions_timers() {
        let (out, mut rx) = unbounded();
        let host = JsHost::spawn(crate::RUNTIME_JS.to_string(), out).unwrap();
        let code = "\"use strict\";\nvar __forgeExtension = { activate() { setInterval(() => console.log('tick'), 5); } };";
        host.send(ToJs::Load { id: "t".into(), path: "/t".into(), code: code.into() });
        let mut ticks = 0;
        let deadline = Instant::now() + Duration::from_secs(5);
        while ticks < 3 {
            assert!(Instant::now() < deadline, "the interval stopped after {ticks} ticks");
            match rx.try_recv() {
                Ok(FromJs::Log { message, .. }) if message == "tick" => ticks += 1,
                Ok(FromJs::Log { level, message }) if level == "error" => panic!("{message}"),
                _ => thread::sleep(Duration::from_millis(2)),
            }
        }
        host.send(ToJs::Unload { id: "t".into() });
        thread::sleep(Duration::from_millis(50));
        while rx.try_recv().is_ok() {}
        thread::sleep(Duration::from_millis(60));
        let after: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).filter(|m| matches!(m, FromJs::Log { message, .. } if message == "tick")).collect();
        assert!(after.is_empty(), "ticks after unloading: {}", after.len());
    }

    #[test]
    fn timers_fire_on_the_js_thread() {
        let (out, mut rx) = unbounded();
        let host = JsHost::spawn(crate::RUNTIME_JS.to_string(), out).unwrap();
        let code = "setTimeout(() => console.log('tick'), 20); var __forgeExtension = {};";
        host.send(ToJs::Load { id: "t".into(), path: "/t".into(), code: code.into() });
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Ok(FromJs::Log { message, .. }) = rx.try_recv() {
                if message == "tick" {
                    return;
                }
            }
            thread::sleep(Duration::from_millis(5));
        }
        panic!("timer never fired");
    }
}
