//! The HTTP client's tool for agents: `http_request` sends a request of a `.http` file, or
//! one the agent writes, through the client (the user sees it and its response).

use forge_ui::ide_tools::{IdeTool, ToolContext, ToolEffect, ToolReply, closed, ide_tools, line_arg, str_arg};
use gpui::{App, AsyncWindowContext};
use serde_json::{Value, json};

/// Response bodies quoted to the agent, at most (bytes).
const BODY_BYTES: usize = 8_000;

pub(crate) fn init(_: &mut App) {
    ide_tools().register(IdeTool::new(
        "http_request",
        "Send an HTTP request",
        "Send an HTTP request through Forge's .http support, instead of curl: the user sees it and its response in Forge. \
Either the request at `line` of a `.http` file, or an ad-hoc one (`method`, `url`, `headers`, `body`). `{{variables}}` resolve with the project's \
http-client.env.json environment the user selected. Returns status, headers, timing and body.",
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "A .http file with the request (relative to the project root)." },
                "line": { "type": "integer", "minimum": 1, "description": "A line of the request in that file (1-based)." },
                "method": { "type": "string", "description": "For an ad-hoc request; default GET." },
                "url": { "type": "string", "description": "For an ad-hoc request; may use {{variables}}." },
                "headers": { "type": "object", "additionalProperties": { "type": "string" } },
                "body": { "description": "Text, or JSON to send as is." }
            }
        }),
        ToolEffect::Network,
        |ide, args, mut cx| Box::pin(async move { http_request(ide, args, &mut cx).await }),
    ));
}

/// Sends the request and returns the status, headers and body.
async fn http_request(ide: ToolContext, args: Value, cx: &mut AsyncWindowContext) -> ToolReply {
    let receiver = match str_arg(&args, "path") {
        Some(path) => {
            let file = ide.resolve(&path)?;
            let line = line_arg(&args, "line").ok_or("`line` (1-based) of the request in the .http file is missing.")?;
            ide.workspace.update_in(cx, |ws, window, cx| crate::send_request_at(ws, &file, line - 1, window, cx)).map_err(|_| closed())?
        }
        None => {
            let url = str_arg(&args, "url").ok_or("Give `url` (and `method`), or `path` and `line` of a request in a .http file.")?;
            let method = str_arg(&args, "method").unwrap_or_else(|| "GET".into());
            let headers: Vec<(String, String)> = args.get("headers").and_then(Value::as_object).map(|h| h.iter().map(|(k, v)| (k.clone(), v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string()))).collect()).unwrap_or_default();
            let body = args.get("body").map(|b| b.as_str().map(str::to_string).unwrap_or_else(|| b.to_string()));
            // Resolved as if in a .http file at the root: the project's environments apply.
            let file = ide.root.join("agent.http");
            ide.workspace.update_in(cx, |ws, window, cx| crate::send_adhoc(ws, &file, &method, &url, headers, body, window, cx)).map_err(|_| closed())?
        }
    }
    .map_err(|e| format!("Can't send it: {e:#}"))?;
    let outcome = receiver.await.map_err(|_| "The request was cancelled.".to_string())?;
    let request = &outcome.request;
    let environment = outcome.environment.as_deref().map(|e| format!(" (environment {e})")).unwrap_or_default();
    let exchange = outcome.response.map_err(|e| format!("{} {}{environment} failed: {e}", request.method, request.url))?;
    let headers: Vec<String> = exchange.headers.iter().map(|(k, v)| format!("{k}: {v}")).collect();
    let body = if exchange.is_binary() {
        format!("({} bytes of binary content)", exchange.body.len())
    } else {
        let text = String::from_utf8_lossy(&exchange.body);
        if text.len() > BODY_BYTES {
            let mut cut = BODY_BYTES;
            while !text.is_char_boundary(cut) {
                cut -= 1;
            }
            format!("{}\n… ({} more bytes)", &text[..cut], text.len() - cut)
        } else {
            text.into_owned()
        }
    };
    Ok(format!(
        "{} {}{environment} → {} {} in {} ms (the user sees it in the response tab)\n\n{}\n\n{body}",
        request.method,
        request.url,
        exchange.status,
        exchange.reason,
        exchange.elapsed.as_millis(),
        headers.join("\n")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    fn registers_http_request(cx: &mut gpui::TestAppContext) {
        cx.update(init);
        assert_eq!(ide_tools().get("http_request").map(|t| t.effect), Some(ToolEffect::Network));
    }
}
