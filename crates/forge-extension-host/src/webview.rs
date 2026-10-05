//! Webview panels: for extensions whose UI needs the DOM. The page is a native system
//! web view (WebKit on macOS, via wry) placed over the panel's bounds; its files are served
//! from the extension folder through the `forge-ext://` protocol, and it talks to its
//! extension with `window.forge.postMessage` / `window.forge.onMessage`.
//!
//! Limitation: the web view is a native view on top of GPUI, so GPUI popovers that overlap
//! the panel draw underneath it.

use futures::channel::mpsc::UnboundedSender;
use gpui::{App, Bounds, Hsla, Pixels, Window};
use std::{
    borrow::Cow,
    path::{Path, PathBuf},
};
use theme::ActiveTheme as _;
use wry::{
    Rect, WebView, WebViewBuilder,
    dpi::{LogicalPosition, LogicalSize},
    http::{Request, Response, StatusCode, header::CONTENT_TYPE},
};

#[derive(Debug, Clone, PartialEq)]
pub struct WebviewSource {
    /// Extension folder; nothing outside it is served.
    pub root: PathBuf,
    /// Entry page relative to `root`.
    pub html: String,
}

const SCHEME: &str = "forge-ext";

/// Creates the web view as a child of `window` (initially hidden, zero-sized; the panel
/// positions it every frame). Messages from the page are sent to `to_extension`.
pub fn create(panel_id: &str, source: &WebviewSource, to_extension: UnboundedSender<(String, String)>, window: &Window, cx: &App) -> wry::Result<WebView> {
    let root = source.root.clone();
    let panel = panel_id.to_string();
    WebViewBuilder::new()
        .with_custom_protocol(SCHEME.into(), move |_, request| serve(&root, &request))
        .with_ipc_handler(move |request: Request<String>| {
            let _ = to_extension.unbounded_send((panel.clone(), request.into_body()));
        })
        .with_initialization_script(bootstrap_script(&theme_css(cx)))
        .with_url(format!("{SCHEME}://localhost/{}", source.html.trim_start_matches('/')))
        .with_bounds(Rect { position: LogicalPosition::new(0.0, 0.0).into(), size: LogicalSize::new(0.0, 0.0).into() })
        .with_visible(false)
        .with_transparent(false)
        .build_as_child(window)
}

pub fn set_bounds(view: &WebView, bounds: Bounds<Pixels>) {
    let rect = Rect {
        position: LogicalPosition::new(f32::from(bounds.origin.x) as f64, f32::from(bounds.origin.y) as f64).into(),
        size: LogicalSize::new(f32::from(bounds.size.width) as f64, f32::from(bounds.size.height) as f64).into(),
    };
    let _ = view.set_bounds(rect);
    let _ = view.set_visible(true);
}

/// Delivers a message from the extension to the page.
pub fn post(view: &WebView, json: &str) {
    let _ = view.evaluate_script(&format!("window.__forgeReceive && window.__forgeReceive({json})"));
}

fn bootstrap_script(css: &str) -> String {
    let css = serde_json::to_string(css).unwrap_or_default();
    format!(
        r#"(() => {{
  const listeners = new Set();
  window.forge = {{
    postMessage: (message) => window.ipc.postMessage(JSON.stringify(message)),
    onMessage: (listener) => {{ listeners.add(listener); return () => listeners.delete(listener); }},
  }};
  window.__forgeReceive = (message) => listeners.forEach((l) => {{ try {{ l(message); }} catch (e) {{ console.error(e); }} }});
  const style = () => {{ const s = document.createElement('style'); s.textContent = {css}; document.head.prepend(s); }};
  if (document.head) style(); else document.addEventListener('DOMContentLoaded', style);
}})();"#
    )
}

/// The active theme as CSS variables, plus defaults so plain HTML matches the editor.
pub fn theme_css(cx: &App) -> String {
    let c = cx.theme().colors();
    let css = |h: Hsla| {
        let rgba = h.to_rgb();
        format!("rgba({}, {}, {}, {:.3})", (rgba.r * 255.0).round(), (rgba.g * 255.0).round(), (rgba.b * 255.0).round(), rgba.a)
    };
    format!(
        ":root {{ --forge-bg: {}; --forge-fg: {}; --forge-muted: {}; --forge-accent: {}; --forge-border: {}; --forge-surface: {}; color-scheme: dark light; }}\n\
         html, body {{ margin: 0; background: var(--forge-bg); color: var(--forge-fg); font: 13px/1.45 -apple-system, system-ui, sans-serif; }}",
        css(c.panel_background),
        css(c.text),
        css(c.text_muted),
        css(c.text_accent),
        css(c.border),
        css(c.surface_background),
    )
}

/// Maps a `forge-ext://localhost/<path>` request to a file under `root`.
pub fn resolve(root: &Path, url_path: &str) -> Option<PathBuf> {
    let rel = percent_decode(url_path.trim_start_matches('/'));
    let rel = if rel.is_empty() { "index.html".to_string() } else { rel };
    let root = root.canonicalize().ok()?;
    let path = root.join(rel).canonicalize().ok()?;
    (path.starts_with(&root) && path.is_file()).then_some(path)
}

fn serve(root: &Path, request: &Request<Vec<u8>>) -> Response<Cow<'static, [u8]>> {
    let respond = |status: StatusCode, mime: &str, body: Vec<u8>| Response::builder().status(status).header(CONTENT_TYPE, mime).body(Cow::Owned(body)).unwrap();
    match resolve(root, request.uri().path()).and_then(|p| std::fs::read(&p).ok().map(|b| (p, b))) {
        Some((path, bytes)) => respond(StatusCode::OK, mime(&path), bytes),
        None => respond(StatusCode::NOT_FOUND, "text/plain", b"not found".to_vec()),
    }
}

pub fn mime(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).unwrap_or_default() {
        "html" | "htm" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "wasm" => "application/wasm",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        _ => "application/octet-stream",
    }
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serves_only_files_inside_the_extension() {
        let dir = tempfile::tempdir().unwrap();
        let ext = dir.path().join("ext");
        std::fs::create_dir_all(ext.join("web")).unwrap();
        std::fs::write(ext.join("web/index.html"), "<p>hi</p>").unwrap();
        std::fs::write(ext.join("web/my page.html"), "x").unwrap();
        std::fs::write(dir.path().join("secret.txt"), "s").unwrap();

        assert!(resolve(&ext, "/web/index.html").is_some());
        assert!(resolve(&ext, "/web/my%20page.html").is_some(), "percent-decoded");
        assert!(resolve(&ext, "/../secret.txt").is_none(), "no traversal");
        assert!(resolve(&ext, "/web/%2e%2e/%2e%2e/secret.txt").is_none(), "no encoded traversal");
        assert!(resolve(&ext, "/missing.html").is_none());
        assert!(resolve(&ext, "/web").is_none(), "directories are not served");
        assert_eq!(mime(Path::new("a.js")), "text/javascript; charset=utf-8");
    }

    #[test]
    fn bootstrap_embeds_css_safely() {
        let script = bootstrap_script("body { content: \"</style>\" }");
        assert!(script.contains("window.forge"));
        assert!(script.contains(r#"\"</style>\""#), "CSS is a JSON string literal");
    }
}
