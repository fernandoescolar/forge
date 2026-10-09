//! The parts of `@forge-ide/api` that reach into the workspace beyond files: events, the active
//! editor (state, edits, selections, decorations), processes, terminals and storage.
//!
//! Positions are `{ line, column }`, zero-based, with columns counted in characters
//! (Unicode scalar values), so they match what JS sees for ASCII and most text.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash as _, Hasher as _};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context as _, Result, anyhow};
use editor::{Editor, EditorEvent, HighlightKey};
use gpui::{App, AppContext as _, Context, Entity, Window};
use language::{Buffer, BufferEvent, Point};
use serde_json::{Map, Value, json};
use task::{TaskContext, TaskTemplate};
use text::ToPoint as _;
use util::ResultExt as _;
use workspace::Workspace;

use crate::host::ExtensionHost;
use crate::js::ToJs;

/// Events extensions can listen to (`forge.*.onDid…`).
pub const ACTIVE_FILE_CHANGED: &str = "workspace.activeFileChanged";
pub const FILE_SAVED: &str = "workspace.fileSaved";
pub const SELECTION_CHANGED: &str = "editor.selectionChanged";

/// Sends every buffer save to the extensions listening for it.
pub fn observe_saves(cx: &mut App) {
    cx.observe_new(|_: &mut Buffer, _, cx| {
        cx.subscribe_self(|buffer, event: &BufferEvent, cx| {
            if !matches!(event, BufferEvent::Saved) {
                return;
            }
            let Some(path) = buffer.file().and_then(|f| f.as_local()).map(|f| f.abs_path(cx)) else { return };
            if let Some(host) = ExtensionHost::global(cx) {
                host.read(cx).emit_event(FILE_SAVED, json!(path.to_string_lossy()));
            }
        })
        .detach();
    })
    .detach();
}

impl ExtensionHost {
    /// Sends `name` to the extensions, if one listens to it.
    pub(crate) fn emit_event(&self, name: &str, value: Value) {
        if self.listened.contains(name) {
            self.js.send(ToJs::Event { name: name.into(), json: value.to_string() });
        }
    }

    /// Follows the workspace's active item: tells extensions when the active file changes,
    /// and listens to the active editor's selections.
    pub(crate) fn watch_workspace(&mut self, workspace: &Entity<Workspace>, cx: &mut Context<Self>) {
        self.workspace_subscription = Some(cx.subscribe(workspace, |this, _, event: &workspace::Event, cx| {
            if matches!(event, workspace::Event::ActiveItemChanged) {
                this.active_item_changed(cx);
            }
        }));
    }

    pub(crate) fn active_item_changed(&mut self, cx: &mut Context<Self>) {
        let editor = self.active_editor(cx);
        let path = editor.as_ref().and_then(|e| editor_path(e, cx));
        if path != self.active_file {
            self.active_file = path.clone();
            self.emit_event(ACTIVE_FILE_CHANGED, json!(path.map(|p| p.to_string_lossy().into_owned())));
        }
        if let Some(editor) = &editor {
            self.restore_decorations(editor, cx);
        }
        self.editor_subscription = editor.map(|editor| {
            cx.subscribe(&editor, |this, editor, event: &EditorEvent, cx| {
                if matches!(event, EditorEvent::SelectionsChanged { .. }) && this.listened.contains(SELECTION_CHANGED) {
                    this.emit_event(SELECTION_CHANGED, editor_state(&editor, cx));
                }
            })
        });
        self.emit_event(SELECTION_CHANGED, self.active_editor(cx).map(|e| editor_state(&e, cx)).unwrap_or(Value::Null));
    }

    /// The active item, if it is an editor of one file.
    fn active_editor(&self, cx: &App) -> Option<Entity<Editor>> {
        let editor = self.workspace()?.read(cx).active_item(cx)?.downcast::<Editor>()?;
        editor.read(cx).buffer().read(cx).as_singleton().is_some().then_some(editor)
    }

    /// Serves `editor.*`, `process.*`, `terminal.*`, `storage.*` and `events.*` calls; false
    /// for other methods.
    pub(crate) fn call_api(&mut self, method: &str, args: &Value, id: u64, cx: &mut Context<Self>) -> bool {
        match method {
            "events.listen" => {
                if let Some(name) = args.get("name").and_then(Value::as_str) {
                    self.listened.insert(name.to_string());
                }
                self.reply(id, Ok(Value::Null));
            }
            "editor.state" => {
                let state = self.active_editor(cx).map(|e| editor_state(&e, cx)).unwrap_or(Value::Null);
                self.reply(id, Ok(state));
            }
            "editor.getText" => {
                let text = self.active_editor(cx).and_then(|e| Some(singleton(&e, cx)?.read(cx).text()));
                self.reply(id, Ok(json!(text)));
            }
            "editor.edit" => {
                let result = self.with_active_editor(cx, |editor, _, cx| {
                    let buffer = editor.buffer().read(cx).as_singleton().context("not a file editor")?.read(cx).snapshot();
                    let edits = args.get("edits").and_then(Value::as_array).context("missing edits")?;
                    let edits: Vec<(std::ops::Range<Point>, String)> = edits
                        .iter()
                        .map(|edit| {
                            let range = to_range(&buffer, edit.get("range").context("an edit without a range")?)?;
                            Ok((range, edit.get("text").and_then(Value::as_str).unwrap_or_default().to_string()))
                        })
                        .collect::<Result<_>>()?;
                    editor.edit(edits, cx);
                    Ok(Value::Bool(true))
                });
                self.reply(id, result);
            }
            "editor.replaceSelections" => {
                let text = args.get("text").and_then(Value::as_str).unwrap_or_default().to_string();
                let result = self.with_active_editor(cx, |editor, _, cx| {
                    let display = editor.display_snapshot(cx);
                    let ranges: Vec<std::ops::Range<Point>> = editor.selections.all::<Point>(&display).into_iter().map(|s| s.start..s.end).collect();
                    editor.edit(ranges.into_iter().map(|r| (r, text.clone())), cx);
                    Ok(Value::Null)
                });
                self.reply(id, result);
            }
            "editor.select" => {
                let result = self.with_active_editor(cx, |editor, window, cx| {
                    let buffer = editor.buffer().read(cx).as_singleton().context("not a file editor")?.read(cx).snapshot();
                    let ranges = args.get("ranges").and_then(Value::as_array).context("missing ranges")?;
                    let ranges: Vec<std::ops::Range<Point>> = ranges.iter().map(|r| to_range(&buffer, r)).collect::<Result<_>>()?;
                    anyhow::ensure!(!ranges.is_empty(), "no ranges to select");
                    let effects = editor::SelectionEffects::scroll(editor::scroll::Autoscroll::center());
                    editor.change_selections(effects, window, cx, |s| s.select_ranges(ranges));
                    Ok(Value::Null)
                });
                self.reply(id, result);
            }
            "editor.setDecorations" => {
                let result = self.set_decorations(args, cx);
                self.reply(id, result);
            }
            "process.exec" => self.exec(args, id, cx),
            "terminal.run" => {
                let result = self.run_in_terminal(args, cx);
                self.reply(id, result);
            }
            "storage.get" | "storage.set" | "storage.keys" => {
                let result = self.storage(method, args, cx);
                self.reply(id, result);
            }
            _ => return false,
        }
        true
    }

    /// `editor.setDecorations`: remembers them for the file (the active one unless `path`
    /// says) and shows them in each of its editors, now and when one opens later.
    fn set_decorations(&mut self, args: &Value, cx: &mut Context<Self>) -> Result<Value> {
        let key = args.get("key").and_then(Value::as_str).unwrap_or_default().to_string();
        let path = match args.get("path").and_then(Value::as_str) {
            Some(path) => self.resolve_path(path, cx),
            None => self.active_editor(cx).and_then(|e| editor_path(&e, cx)).context("no file is open in an editor")?,
        };
        let mut decoration = Decoration {
            ranges: args.get("ranges").and_then(Value::as_array).cloned().unwrap_or_default(),
            color: args.get("color").and_then(Value::as_str).unwrap_or("accent").to_string(),
            anchored: None,
        };
        let editors: Vec<Entity<Editor>> = self.workspace().map(|ws| ws.read(cx).items_of_type::<Editor>(cx).filter(|e| editor_path(e, cx).as_ref() == Some(&path)).collect()).unwrap_or_default();
        if let Some(buffer) = editors.first().and_then(|e| singleton(e, cx)) {
            decoration.anchor(&buffer, cx)?;
        }
        for editor in &editors {
            decorate(editor, &key, &decoration, cx)?;
        }
        let file = self.decorations.entry(path).or_default();
        if decoration.ranges.is_empty() {
            file.remove(&key);
        } else {
            file.insert(key, decoration);
        }
        Ok(Value::Null)
    }

    /// Shows the file's remembered decorations in `editor`, unless it already has them,
    /// where their text is now: anchors follow the edits made since, while the file stays
    /// open.
    fn restore_decorations(&mut self, editor: &Entity<Editor>, cx: &mut App) {
        let Some(path) = editor_path(editor, cx) else { return };
        let Some(buffer) = singleton(editor, cx) else { return };
        let Some(file) = self.decorations.get_mut(&path) else { return };
        for (key, decoration) in file.iter_mut() {
            decoration.refresh(&buffer, cx);
            if !editor.read(cx).has_background_highlights(decoration_key(key)) {
                decorate(editor, key, decoration, cx).log_err();
            }
        }
    }

    /// Runs `f` on the active file editor, in its window (selections need one).
    fn with_active_editor(&self, cx: &mut Context<Self>, f: impl FnOnce(&mut Editor, &mut Window, &mut Context<Editor>) -> Result<Value>) -> Result<Value> {
        let editor = self.active_editor(cx).context("no file is open in an editor")?;
        let (_, window) = self.workspace.clone().context("no workspace is open")?;
        window.update(cx, |_, window, cx| editor.update(cx, |editor, cx| f(editor, window, cx)))?
    }

    /// `process.exec`: runs a program to completion with the project's shell environment
    /// and replies with its exit code and output.
    fn exec(&mut self, args: &Value, id: u64, cx: &mut Context<Self>) {
        let Some(command) = args.get("command").and_then(Value::as_str).map(str::to_string) else {
            return self.reply(id, Err(anyhow!("missing command")));
        };
        let arguments: Vec<String> = args.get("args").and_then(Value::as_array).into_iter().flatten().filter_map(|a| a.as_str().map(str::to_string)).collect();
        let cwd = self.resolve_path(args.get("cwd").and_then(Value::as_str).unwrap_or("."), cx);
        let extra_env: Vec<(String, String)> = args.get("env").and_then(Value::as_object).into_iter().flatten().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string()))).collect();
        let stdin = args.get("stdin").and_then(Value::as_str).map(str::to_string);
        let env_task = self.workspace().map(|ws| {
            let dir: Arc<std::path::Path> = cwd.clone().into();
            let project = ws.read(cx).project().clone();
            project.update(cx, |p, cx| p.environment().update(cx, |env, cx| env.directory_environment(dir, cx)))
        });
        let js = self.js.clone();
        cx.spawn(async move |_, cx| {
            let env: Vec<(String, String)> = match env_task {
                Some(task) => task.await.map(|env| env.into_iter().collect()).unwrap_or_default(),
                None => Vec::new(),
            };
            let result = cx
                .background_spawn(async move {
                    use smol::io::AsyncWriteExt as _;
                    let path = env.iter().find(|(k, _)| k == "PATH").map(|(_, v)| v.clone());
                    let program = ide_api::program_path(&command, path.as_deref().map(std::ffi::OsStr::new));
                    let mut process = util::command::new_command(&program);
                    process
                        .args(&arguments)
                        .envs(env)
                        .envs(extra_env)
                        .current_dir(&cwd)
                        .stdin(util::command::Stdio::piped())
                        .stdout(util::command::Stdio::piped())
                        .stderr(util::command::Stdio::piped())
                        .kill_on_drop(true);
                    let mut child = process.spawn().with_context(|| format!("cannot start {command}"))?;
                    if let Some(mut input) = child.stdin.take() {
                        if let Some(stdin) = stdin {
                            input.write_all(stdin.as_bytes()).await.ok();
                        }
                    }
                    let output = child.output().await?;
                    anyhow::Ok(json!({
                        "code": output.status.code(),
                        "stdout": String::from_utf8_lossy(&output.stdout),
                        "stderr": String::from_utf8_lossy(&output.stderr),
                    }))
                })
                .await;
            let (ok, json) = match result {
                Ok(value) => (true, value.to_string()),
                Err(e) => (false, json!(format!("{e:#}")).to_string()),
            };
            js.send(ToJs::Resolve { call: id, ok, json });
        })
        .detach();
    }

    /// `terminal.run`: runs a command in a terminal tab, like a task.
    fn run_in_terminal(&self, args: &Value, cx: &mut Context<Self>) -> Result<Value> {
        let command = args.get("command").and_then(Value::as_str).context("missing command")?.to_string();
        let arguments: Vec<String> = args.get("args").and_then(Value::as_array).into_iter().flatten().filter_map(|a| a.as_str().map(str::to_string)).collect();
        let cwd = self.resolve_path(args.get("cwd").and_then(Value::as_str).unwrap_or("."), cx);
        let label = args.get("title").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| std::iter::once(command.clone()).chain(arguments.iter().cloned()).collect::<Vec<_>>().join(" "));
        let template = TaskTemplate { label, command, args: arguments, cwd: Some(cwd.to_string_lossy().into_owned()), ..TaskTemplate::default() };
        let resolved = template.resolve_task("forge-extension", &TaskContext { cwd: Some(cwd), ..TaskContext::default() }).context("cannot run that command")?;
        let workspace = self.workspace().context("no workspace is open")?;
        let (_, window) = self.workspace.clone().context("no workspace is open")?;
        // Calls can arrive while the workspace is being updated: schedule on the next tick.
        cx.defer(move |cx| {
            window
                .update(cx, |_, window, cx| {
                    workspace.update(cx, |ws, cx| ws.schedule_resolved_task(project::TaskSourceKind::UserInput, resolved, false, window, cx));
                })
                .ok();
        });
        Ok(Value::Null)
    }

    /// `storage.*`: one JSON object per extension, global or per project, kept in Forge's
    /// key-value store.
    fn storage(&mut self, method: &str, args: &Value, cx: &mut Context<Self>) -> Result<Value> {
        let extension = args.get("extension").and_then(Value::as_str).context("missing extension")?;
        let key = match args.get("scope").and_then(Value::as_str) {
            Some("workspace") => {
                let root = self.workspace().and_then(|ws| ws.read(cx).visible_worktrees(cx).next().map(|wt| wt.read(cx).abs_path().to_string_lossy().into_owned()));
                format!("forge-ext-storage:{extension}:{}", root.context("no project is open")?)
            }
            _ => format!("forge-ext-storage:{extension}"),
        };
        let kvp = db::kvp::KeyValueStore::global(cx);
        let store = self
            .storage
            .entry(key.clone())
            .or_insert_with(|| kvp.read_kvp(&key).ok().flatten().and_then(|json| serde_json::from_str::<Map<String, Value>>(&json).ok()).unwrap_or_default());
        let item = args.get("key").and_then(Value::as_str).unwrap_or_default();
        match method {
            "storage.get" => Ok(store.get(item).cloned().unwrap_or(Value::Null)),
            "storage.keys" => Ok(json!(store.keys().collect::<Vec<_>>())),
            _ => {
                match args.get("value") {
                    None | Some(Value::Null) => store.remove(item),
                    Some(value) => store.insert(item.to_string(), value.clone()),
                };
                let json = Value::Object(store.clone()).to_string();
                cx.background_spawn(async move {
                    if let Err(e) = kvp.write_kvp(key, json).await {
                        log::error!("failed to save extension storage: {e:#}");
                    }
                })
                .detach();
                Ok(Value::Null)
            }
        }
    }
}

/// Highlights an extension asked for in a file (`editor.setDecorations`).
#[derive(Clone, Debug)]
pub(crate) struct Decoration {
    /// `{ start, end }` positions, as of the last time they were read from `anchored`.
    ranges: Vec<Value>,
    color: String,
    /// The ranges anchored in the file's buffer while it is open, so they move with edits.
    anchored: Option<(gpui::WeakEntity<Buffer>, Vec<std::ops::Range<text::Anchor>>)>,
}

impl Decoration {
    /// Anchors the ranges in `buffer`.
    fn anchor(&mut self, buffer: &Entity<Buffer>, cx: &App) -> Result<()> {
        let snapshot = buffer.read(cx).snapshot();
        // Text typed at either edge stays outside the decoration.
        let anchors = self.ranges.iter().map(|r| to_range(&snapshot, r).map(|r| snapshot.anchor_after(r.start)..snapshot.anchor_before(r.end))).collect::<Result<_>>()?;
        self.anchored = Some((buffer.downgrade(), anchors));
        Ok(())
    }

    /// Reads the positions back from the anchors when they are in `buffer`; anchors in a
    /// buffer that was closed are dropped, and the ranges are anchored in `buffer` instead.
    fn refresh(&mut self, buffer: &Entity<Buffer>, cx: &App) {
        let live = self.anchored.as_ref().and_then(|(weak, anchors)| Some((weak.upgrade()?, anchors.clone())));
        match live {
            Some((anchored, anchors)) if &anchored == buffer => {
                let snapshot = buffer.read(cx).snapshot();
                self.ranges = anchors
                    .iter()
                    .map(|r| {
                        let (start, end) = (r.start.to_point(&snapshot), r.end.to_point(&snapshot));
                        json!({ "start": to_position(&snapshot, start.min(end)), "end": to_position(&snapshot, start.max(end)) })
                    })
                    .collect();
            }
            _ => {
                self.anchor(buffer, cx).log_err();
            }
        }
    }
}

fn decorate(editor: &Entity<Editor>, key: &str, decoration: &Decoration, cx: &mut App) -> Result<()> {
    editor.update(cx, |editor, cx| {
        let buffer = editor.buffer().read(cx).as_singleton().context("not a file editor")?.read(cx).snapshot();
        let ranges: Vec<std::ops::Range<Point>> = decoration.ranges.iter().map(|r| to_range(&buffer, r)).collect::<Result<_>>()?;
        let key = decoration_key(key);
        if ranges.is_empty() {
            editor.clear_background_highlights(key, cx);
            return Ok(());
        }
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let anchors: Vec<_> = ranges.into_iter().map(|r| snapshot.anchor_after(r.start)..snapshot.anchor_before(r.end)).collect();
        let color = decoration.color.clone();
        editor.highlight_background(key, &anchors, move |_, theme| decoration_color(&color, theme), cx);
        Ok(())
    })
}

fn singleton(editor: &Entity<Editor>, cx: &App) -> Option<Entity<Buffer>> {
    editor.read(cx).buffer().read(cx).as_singleton()
}

fn editor_path(editor: &Entity<Editor>, cx: &App) -> Option<PathBuf> {
    singleton(editor, cx)?.read(cx).file()?.as_local().map(|f| f.abs_path(cx))
}

/// What extensions see of an editor: its file, language, line count, selections and the
/// text of the newest selection.
fn editor_state(editor: &Entity<Editor>, cx: &mut App) -> Value {
    let Some(buffer) = singleton(editor, cx) else { return Value::Null };
    let snapshot = buffer.read(cx).snapshot();
    let selections = editor.update(cx, |editor, cx| {
        let display = editor.display_snapshot(cx);
        editor.selections.all::<Point>(&display)
    });
    let newest = selections.iter().max_by_key(|s| s.id);
    let selected_text = newest.map(|s| snapshot.text_for_range(s.start..s.end).collect::<String>()).unwrap_or_default();
    json!({
        "path": editor_path(editor, cx).map(|p| p.to_string_lossy().into_owned()),
        "language": snapshot.language().map(|l| l.name().to_string()),
        "lineCount": snapshot.max_point().row + 1,
        "selections": selections.iter().map(|s| json!({ "start": to_position(&snapshot, s.start), "end": to_position(&snapshot, s.end), "reversed": s.reversed })).collect::<Vec<_>>(),
        "selectedText": selected_text,
    })
}

fn line_text(snapshot: &text::BufferSnapshot, row: u32) -> String {
    snapshot.text_for_range(Point::new(row, 0)..Point::new(row, snapshot.line_len(row))).collect()
}

fn to_position(snapshot: &text::BufferSnapshot, point: Point) -> Value {
    let line = line_text(snapshot, point.row);
    let column = line.get(..point.column as usize).map(|prefix| prefix.chars().count()).unwrap_or(line.chars().count());
    json!({ "line": point.row, "column": column })
}

/// A `{ line, column }` as a buffer point, clamped to the text.
fn to_point(snapshot: &text::BufferSnapshot, position: &Value) -> Result<Point> {
    let number = |key: &str| position.get(key).and_then(Value::as_u64).with_context(|| format!("a position needs a numeric `{key}`"));
    let row = (number("line")? as u32).min(snapshot.max_point().row);
    let column = number("column")? as usize;
    let line = line_text(snapshot, row);
    let byte = line.char_indices().nth(column).map(|(i, _)| i).unwrap_or(line.len());
    Ok(Point::new(row, byte as u32))
}

fn to_range(snapshot: &text::BufferSnapshot, range: &Value) -> Result<std::ops::Range<Point>> {
    let start = to_point(snapshot, range.get("start").context("a range needs a start")?)?;
    let end = to_point(snapshot, range.get("end").context("a range needs an end")?)?;
    Ok(start.min(end)..start.max(end))
}

/// Each extension decoration key gets its own highlight layer. Zed has no slot for
/// third-party highlights, so this borrows the keyed variant of its highlights tree view
/// (keyed there by entity id, a small number), with the top bit set to stay clear of it.
fn decoration_key(key: &str) -> HighlightKey {
    let mut hasher = DefaultHasher::new();
    key.hash(&mut hasher);
    HighlightKey::HighlightsTreeView((hasher.finish() as usize) | (1 << (usize::BITS - 1)))
}

fn decoration_color(color: &str, theme: &theme::Theme) -> gpui::Hsla {
    let status = theme.status();
    match color {
        "error" => status.error_background,
        "warning" => status.warning_background,
        "success" => status.success_background,
        "muted" => theme.colors().editor_active_line_background,
        _ => status.info_background,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positions_count_characters() {
        let buffer = text::Buffer::new(text::ReplicaId::LOCAL, text::BufferId::new(1).unwrap(), "héllo\nwörld");
        let snapshot = buffer.snapshot();
        let point = to_point(snapshot, &json!({ "line": 1, "column": 2 })).unwrap();
        assert_eq!(point, Point::new(1, 3), "ö is two bytes");
        assert_eq!(to_position(snapshot, point), json!({ "line": 1, "column": 2 }));
        assert_eq!(to_point(snapshot, &json!({ "line": 9, "column": 99 })).unwrap(), Point::new(1, 6), "clamped to the text");
        let range = to_range(snapshot, &json!({ "start": { "line": 0, "column": 5 }, "end": { "line": 0, "column": 1 } })).unwrap();
        assert_eq!(range, Point::new(0, 1)..Point::new(0, 6), "ordered");
    }

    /// An extension drives the active editor, keeps storage, runs a process and hears
    /// about selections, through `@forge-ide/api` in QuickJS.
    #[gpui::test]
    async fn extensions_drive_the_active_editor(cx: &mut gpui::TestAppContext) {
        use std::time::{Duration, Instant};
        cx.executor().allow_parking();
        let params = cx.update(workspace::AppState::test);
        cx.update(|cx| {
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            crate::init_for_tests(cx);
        });
        let tmp = tempfile::tempdir().unwrap();
        let ext = tmp.path().join("ext-editor");
        std::fs::create_dir_all(ext.join("dist")).unwrap();
        std::fs::write(ext.join("package.json"), r#"{"name":"ext-editor","forge":{}}"#).unwrap();
        let cwd = tmp.path().to_string_lossy().replace('\\', "/");
        let code = format!(
            r#"var __forgeExtension = {{ activate(ctx) {{
                const f = __forge.modules['@forge-ide/api'].forge;
                const mark = (id) => f.commands.register(id, id, () => {{}});
                f.editor.onDidChangeSelection((s) => s && mark('sel-' + s.selections[0].start.line + ':' + s.selections[0].start.column));
                f.workspace.onDidChangeActiveFile((p) => mark('active-' + p));
                f.commands.register('go', 'Go', async () => {{
                    try {{
                        const state = await f.editor.active();
                        await f.editor.edit([{{ range: {{ start: {{ line: 0, column: 0 }}, end: {{ line: 0, column: 5 }} }}, text: 'HÉLLO' }}]);
                        await f.editor.select({{ start: {{ line: 1, column: 2 }}, end: {{ line: 1, column: 4 }} }});
                        const selected = (await f.editor.active()).selectedText;
                        await f.editor.setDecorations('k', [{{ start: {{ line: 0, column: 0 }}, end: {{ line: 0, column: 5 }} }}], 'warning');
                        await ctx.storage.set('n', {{ count: 1 }});
                        const stored = await ctx.storage.get('n');
                        const run = await f.process.exec('sh', {{ args: ['-c', 'cat; echo " $X"'], cwd: '{cwd}', env: {{ X: 'there' }}, stdin: 'hi' }});
                        mark('done ' + JSON.stringify({{ lines: state.lineCount, path: state.path, text: await f.editor.getText(), selected, stored, out: run.stdout.trim(), code: run.code }}));
                    }} catch (e) {{ mark('failed ' + e); }}
                }});
            }} }};"#
        );
        std::fs::write(ext.join("dist/extension.js"), code).unwrap();
        let host = cx.update(|cx| ExtensionHost::init(vec![tmp.path().to_path_buf()], cx)).unwrap();

        params.fs.as_fake().insert_tree("/root", serde_json::json!({ "a.txt": "hello world\nsecond line\n" })).await;
        let project = project::Project::test(params.fs.clone(), ["/root".as_ref()], cx).await;
        let window = cx.add_window(|window, cx| workspace::MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window.read_with(cx, |mw, _| mw.workspace().clone()).unwrap();
        let cx = &mut gpui::VisualTestContext::from_window(window.into(), cx);
        let weak = workspace.downgrade();
        cx.update(|window, cx| host.update(cx, |h, cx| h.set_workspace(weak, window.window_handle(), cx)));

        let commands = |cx: &mut gpui::VisualTestContext| host.read_with(cx, |h, _| h.commands.iter().map(|c| c.id.clone()).collect::<Vec<_>>());
        let wait_for = |prefix: &str, cx: &mut gpui::VisualTestContext| {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                if let Some(id) = commands(cx).into_iter().find(|id| id.starts_with(prefix)) {
                    return id;
                }
                assert!(Instant::now() < deadline, "no command {prefix}…; have {:?}, errors {:?}", commands(cx), host.read_with(cx, |h, _| h.errors.clone()));
                cx.run_until_parked();
                std::thread::sleep(Duration::from_millis(20));
            }
        };
        wait_for("go", cx);
        workspace
            .update_in(cx, |ws, window, cx| ws.open_abs_path(PathBuf::from("/root/a.txt"), workspace::OpenOptions::default(), window, cx))
            .await
            .unwrap();
        assert_eq!(wait_for("active-", cx), "active-/root/a.txt");
        wait_for("sel-0:0", cx);

        host.read_with(cx, |h, _| h.run_command("go"));
        let done = wait_for("done ", cx);
        let result: Value = serde_json::from_str(done.strip_prefix("done ").unwrap()).unwrap_or_else(|_| panic!("{done}"));
        assert_eq!(
            result,
            json!({ "lines": 3, "path": "/root/a.txt", "text": "HÉLLO world\nsecond line\n", "selected": "co", "stored": { "count": 1 }, "out": "hi there", "code": 0 })
        );
        wait_for("sel-1:2", cx);
        let editor = workspace.read_with(cx, |ws, cx| ws.active_item(cx).unwrap().downcast::<Editor>().unwrap());
        let highlighted = editor.update(cx, |e, _| e.has_background_highlights(decoration_key("k")));
        assert!(highlighted, "the decoration is shown");

        // Edits move decorations: another editor of the file shows them where the text went.
        editor.update(cx, |e, cx| e.edit([(language::Point::new(0, 0)..language::Point::new(0, 0), "new line\n")], cx));
        let buffer = editor.read_with(cx, |e, cx| e.buffer().read(cx).as_singleton().unwrap());
        workspace.update_in(cx, |ws, window, cx| {
            let second = cx.new(|cx| Editor::for_buffer(buffer, Some(project.clone()), window, cx));
            ws.add_item_to_active_pane(Box::new(second), None, true, window, cx);
        });
        cx.run_until_parked();
        let start = host.read_with(cx, |h, _| h.decorations[&PathBuf::from("/root/a.txt")]["k"].ranges[0]["start"].clone());
        assert_eq!(start, json!({ "line": 1, "column": 0 }), "the decoration moved down with its text");

        // Closed and opened again, the file shows its decorations.
        workspace.update_in(cx, |ws, window, cx| {
            ws.active_pane().update(cx, |pane, cx| pane.close_all_items(&workspace::CloseAllItems { save_intent: Some(workspace::SaveIntent::Skip), close_pinned: true }, window, cx).detach());
        });
        cx.run_until_parked();
        let reopened = workspace
            .update_in(cx, |ws, window, cx| ws.open_abs_path(PathBuf::from("/root/a.txt"), workspace::OpenOptions::default(), window, cx))
            .await
            .unwrap()
            .downcast::<Editor>()
            .unwrap();
        cx.run_until_parked();
        assert_ne!(reopened, editor, "a new editor");
        assert!(reopened.update(cx, |e, _| e.has_background_highlights(decoration_key("k"))), "decorations follow the file");
    }

    #[test]
    fn decoration_keys_are_stable_and_distinct() {
        assert_eq!(decoration_key("a"), decoration_key("a"));
        assert_ne!(decoration_key("a"), decoration_key("b"));
    }
}
