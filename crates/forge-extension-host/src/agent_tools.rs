//! Extensions' tools for agents (`forge.agents.registerTool`): registered in the shared
//! registry (`forge_ui::agent_tools`), which Forge's MCP server lists to agents, and run on
//! the JS thread when an agent calls one. The extension answers with `agents.toolResult`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use forge_ui::agent_tools::{AgentTool, AgentToolReply, AgentToolRunner, agent_tools, mcp_name};
use futures::channel::oneshot;
use serde_json::{Value, json};

use crate::js::{JsHost, ToJs};

/// Calls waiting for their extension's answer, by call id.
#[derive(Clone, Default)]
pub(crate) struct PendingCalls(Arc<Mutex<HashMap<u64, oneshot::Sender<AgentToolReply>>>>);

impl PendingCalls {
    /// The answer to call `call` (`agents.toolResult`).
    pub fn answer(&self, call: u64, reply: AgentToolReply) {
        if let Some(waiting) = self.0.lock().unwrap().remove(&call) {
            waiting.send(reply).ok();
        }
    }
}

/// Runs tools on the JS thread.
pub(crate) struct JsToolRunner {
    pub js: JsHost,
    pub pending: PendingCalls,
    pub next: AtomicU64,
}

impl AgentToolRunner for JsToolRunner {
    fn run(&self, tool: &AgentTool, args: Value, cwd: PathBuf) -> oneshot::Receiver<AgentToolReply> {
        let (tx, rx) = oneshot::channel();
        let call = self.next.fetch_add(1, Ordering::Relaxed);
        self.pending.0.lock().unwrap().insert(call, tx);
        let context = json!({ "cwd": cwd.to_string_lossy() }).to_string();
        self.js.send(ToJs::RunTool { call, extension: tool.extension.clone(), tool: tool.tool.clone(), args: args.to_string(), context });
        rx
    }
}

/// `agents.registerTool`: the tool as the registry keeps it.
pub(crate) fn tool_from(args: &Value) -> Option<AgentTool> {
    let text = |key: &str| args.get(key).and_then(Value::as_str).map(str::to_string);
    let extension = text("extension")?;
    let tool = text("name")?;
    Some(AgentTool {
        name: mcp_name(&extension, &tool),
        title: text("title").unwrap_or_else(|| tool.clone()),
        description: text("description").unwrap_or_default(),
        input_schema: args.get("inputSchema").cloned().filter(Value::is_object).unwrap_or_else(|| json!({ "type": "object", "properties": {} })),
        read_only: args.get("readOnly").and_then(Value::as_bool).unwrap_or(false),
        tool,
        extension,
    })
}

pub(crate) fn register(args: &Value) -> anyhow::Result<()> {
    let tool = tool_from(args).ok_or_else(|| anyhow::anyhow!("agents.registerTool needs `extension` and `name`"))?;
    agent_tools().register(tool);
    Ok(())
}

pub(crate) fn unregister(args: &Value) {
    let text = |key: &str| args.get(key).and_then(Value::as_str).unwrap_or_default();
    agent_tools().unregister(&mcp_name(text("extension"), text("name")));
}

/// `agents.toolResult`: an extension's answer to a call.
pub(crate) fn answer(pending: &PendingCalls, args: &Value) {
    let Some(call) = args.get("call").and_then(Value::as_u64) else { return };
    let text = args.get("text").and_then(Value::as_str).unwrap_or_default().to_string();
    let reply = if args.get("isError").and_then(Value::as_bool).unwrap_or(false) { Err(text) } else { Ok(text) };
    pending.answer(call, reply);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::ExtensionHost;
    use gpui::TestAppContext;
    use std::time::{Duration, Instant};

    /// An extension registers tools; an agent's call runs them on the JS thread (sync, async,
    /// failing) and gets their answer; unloading the extension takes them away.
    #[gpui::test]
    async fn extensions_offer_tools_to_agents(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        cx.update(|cx| {
            let store = settings::SettingsStore::test(cx);
            cx.set_global(store);
        });
        let tmp = tempfile::tempdir().unwrap();
        let ext = tmp.path().join("ext-tools");
        std::fs::create_dir_all(ext.join("dist")).unwrap();
        std::fs::write(ext.join("package.json"), r#"{"name":"ext-tools","displayName":"Tools Test","forge":{}}"#).unwrap();
        let code = "var __forgeExtension = { activate() { const f = __forge.modules['@forge-ide/api'].forge; \
            f.agents.registerTool({ name: 'echo', title: 'Echo', description: 'Says it back.', readOnly: true, \
              inputSchema: { type: 'object', properties: { text: { type: 'string' } }, required: ['text'] }, \
              run: (args, call) => `${args.text} from ${call.cwd}` }); \
            f.agents.registerTool({ name: 'later', title: 'Later', description: 'Answers later.', \
              run: async () => { await new Promise(r => setTimeout(r, 10)); return { text: 'done', isError: false }; } }); \
            f.agents.registerTool({ name: 'broken', title: 'Broken', description: 'Fails.', run: () => { throw new Error('it broke'); } }); \
          } };";
        std::fs::write(ext.join("dist/extension.js"), code).unwrap();
        let host = cx.update(|cx| ExtensionHost::init(vec![tmp.path().to_path_buf()], cx)).unwrap();

        // Other tests' hosts share the registry: look only at this extension's tools.
        let mine = || agent_tools().list().into_iter().filter(|t| t.extension == "ext-tools").collect::<Vec<_>>();
        let deadline = Instant::now() + Duration::from_secs(10);
        while mine().len() < 3 {
            assert!(Instant::now() < deadline, "the tools were never registered");
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(20));
        }
        let echo = agent_tools().get("ext_tools__echo").expect("named by extension and tool");
        assert_eq!((echo.title.as_str(), echo.read_only), ("Echo", true));
        assert_eq!(echo.input_schema["required"], json!(["text"]));
        assert!(!agent_tools().get("ext_tools__later").unwrap().read_only, "tools ask unless they say they only read");

        // Run them through this host (other tests' hosts may have set the shared runner).
        let runner = host.read_with(cx, |h, _| JsToolRunner { js: h.js.clone(), pending: h.agent_calls.clone(), next: 1.into() });
        let call = |name: &str, args: Value, cx: &mut TestAppContext| {
            let tool = agent_tools().get(name).unwrap();
            let mut answer = runner.run(&tool, args, "/work".into());
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                if let Ok(Some(reply)) = answer.try_recv() {
                    return reply;
                }
                assert!(Instant::now() < deadline, "{name} never answered");
                cx.run_until_parked();
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        assert_eq!(call("ext_tools__echo", json!({ "text": "hi" }), cx), Ok("hi from /work".into()));
        assert_eq!(call("ext_tools__later", json!({}), cx), Ok("done".into()));
        assert_eq!(call("ext_tools__broken", json!({}), cx), Err("it broke".into()));

        host.update(cx, |h, cx| h.unload("ext-tools", cx));
        cx.run_until_parked();
        assert!(mine().is_empty(), "unloading the extension takes its tools away");
    }
}
