//! Owns the JS runtime and every extension panel's UI tree, and serves `@forge/api` calls.

use crate::{
    RUNTIME_JS,
    layout::ExtensionLayout,
    js::{FromJs, JsHost, ToJs},
    tree::Tree,
};
use anyhow::{Context as _, Result, anyhow};
use futures::{StreamExt as _, channel::mpsc};
use gpui::{AnyWindowHandle, App, AppContext as _, Context, Entity, EventEmitter, Global, Subscription, WeakEntity};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};
use util::ResultExt as _;
use workspace::{Toast, Workspace, notifications::NotificationId};

#[derive(Debug, Clone)]
pub struct PanelInfo {
    pub id: String,
    pub title: String,
    pub icon: String,
    /// Set for webview panels (rendered by the system web view instead of GPUI).
    pub webview: Option<crate::webview::WebviewSource>,
    /// The panel's root fills it (the extension scrolls what needs it) instead of scrolling.
    pub fill: bool,
    /// The extension that registered it.
    pub extension: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CommandInfo {
    pub id: String,
    pub title: String,
    /// The extension that registered it.
    pub extension: Option<String>,
}

#[derive(Debug, Clone)]
pub struct LoadedExtension {
    pub id: String,
    pub name: String,
    pub path: PathBuf,
    pub version: Option<String>,
    pub description: Option<String>,
    /// It declares settings (a page in the Settings tab, `ext:<id>`).
    pub has_settings: bool,
}

/// Where a loaded extension comes from.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Origin {
    /// Shipped inside Forge.app.
    Bundled,
    /// Installed into Forge's extensions folder (it can be uninstalled).
    Installed,
    /// Loaded from a development folder (`FORGE_EXTENSIONS_PATH`, this repo in debug builds).
    Development,
}

impl Origin {
    pub fn label(self) -> &'static str {
        match self {
            Origin::Bundled => "Included with Forge",
            Origin::Installed => "Installed",
            Origin::Development => "Development",
        }
    }
}

pub enum HostEvent {
    /// Panels, commands or a tree changed; panels re-render.
    Changed,
    /// An extension posted `json` to its webview panel `panel`.
    WebviewMessage { panel: String, json: String },
    /// Extension panel `panel` should be the visible tab of its slot.
    Reveal { panel: String },
}

pub struct ExtensionHost {
    pub(crate) js: JsHost,
    pub panels: Vec<PanelInfo>,
    pub trees: HashMap<String, Tree>,
    pub commands: Vec<CommandInfo>,
    pub extensions: Vec<LoadedExtension>,
    pub errors: Vec<String>,
    /// The workspace extension calls act on (the most recently focused one).
    pub(crate) workspace: Option<(WeakEntity<Workspace>, AnyWindowHandle)>,
    /// Workspace calls made before any workspace window exists (e.g. during activation).
    deferred: Vec<(String, Value, u64)>,
    /// Which extension slot (dock panel) shows which extension panel.
    pub layout: ExtensionLayout,
    /// Declared extension settings and their defaults, by key.
    setting_defaults: HashMap<String, Value>,
    /// Events some extension listens to (`api::ACTIVE_FILE_CHANGED`, …); others aren't sent.
    pub(crate) listened: HashSet<String>,
    pub(crate) active_file: Option<PathBuf>,
    pub(crate) workspace_subscription: Option<Subscription>,
    pub(crate) editor_subscription: Option<Subscription>,
    /// Extension storage loaded so far, by key-value store key.
    pub(crate) storage: HashMap<String, serde_json::Map<String, Value>>,
    /// Decorations by file and key, shown again in editors that open the file later.
    pub(crate) decorations: HashMap<PathBuf, HashMap<String, crate::api::Decoration>>,
    /// Programs started with `process.spawn`, by id.
    pub(crate) processes: HashMap<u64, crate::process::Process>,
    pub(crate) next_process: u64,
}

/// Where extension settings are stored: one object, keyed by setting (`notes.sortBy`).
pub const SETTINGS_FILE: &str = "extensions.json";

const LAYOUT_KEY: &str = "forge-extension-layout";

struct GlobalHost(Entity<ExtensionHost>);
impl Global for GlobalHost {}

impl EventEmitter<HostEvent> for ExtensionHost {}

impl ExtensionHost {
    pub fn global(cx: &App) -> Option<Entity<ExtensionHost>> {
        cx.try_global::<GlobalHost>().map(|g| g.0.clone())
    }

    /// Starts the runtime and loads every extension found in `dirs` (each a folder of
    /// extensions, or an extension folder itself).
    pub fn init(dirs: Vec<PathBuf>, cx: &mut App) -> Result<Entity<Self>> {
        let (out_tx, mut out_rx) = mpsc::unbounded();
        let js = JsHost::spawn(RUNTIME_JS.to_string(), out_tx)?;
        let layout = db::kvp::KeyValueStore::global(cx).read_kvp(LAYOUT_KEY).ok().flatten().map(|j| ExtensionLayout::from_json(&j)).unwrap_or_default();
        let host = cx.new(|_| Self {
            js: js.clone(),
            panels: vec![],
            trees: HashMap::new(),
            commands: vec![],
            extensions: vec![],
            errors: vec![],
            workspace: None,
            deferred: vec![],
            layout,
            setting_defaults: HashMap::new(),
            listened: HashSet::new(),
            active_file: None,
            workspace_subscription: None,
            editor_subscription: None,
            storage: HashMap::new(),
            decorations: HashMap::new(),
            processes: HashMap::new(),
            next_process: 1,
        });
        cx.set_global(GlobalHost(host.clone()));

        let weak = host.downgrade();
        cx.spawn(async move |cx| {
            while let Some(msg) = out_rx.next().await {
                if weak.update(cx, |host, cx| host.handle(msg, cx)).is_err() {
                    break;
                }
            }
        })
        .detach();

        let registry = forge_ui::settings_registry::SettingsRegistry::global(cx);
        let js_for_settings = js.clone();
        cx.subscribe(&registry, move |_, event: &forge_ui::settings_registry::SettingChanged, _| {
            if event.file == forge_ui::settings_registry::SettingsFile::Config(SETTINGS_FILE.into()) {
                if let Some(key) = event.path.first() {
                    let json = event.value.clone().unwrap_or(Value::Null).to_string();
                    js_for_settings.send(ToJs::SettingChanged { key: key.clone(), json });
                }
            }
        })
        .detach();

        for ext in discover(&dirs) {
            host.update(cx, |h, cx| h.load(ext, cx));
        }
        Ok(host)
    }

    /// Registers an extension's settings, then evaluates and activates it.
    pub(crate) fn load(&mut self, ext: Discovered, cx: &mut Context<Self>) {
        if let Some(schema) = &ext.settings {
            self.register_settings(&ext.info, schema, cx);
        }
        match std::fs::read_to_string(&ext.main) {
            Ok(code) => {
                log::info!("loading extension {} from {}", ext.info.id, ext.info.path.display());
                self.js.send(ToJs::Load { id: ext.info.id.clone(), path: ext.info.path.to_string_lossy().into_owned(), code });
                self.extensions.push(ext.info);
            }
            Err(e) => self.errors.push(format!("{}: cannot read {}: {e}", ext.info.id, ext.main.display())),
        }
        cx.emit(HostEvent::Changed);
        cx.notify();
    }

    /// The extension's page in the Settings tab.
    fn register_settings(&mut self, info: &LoadedExtension, schema: &Value, cx: &mut Context<Self>) {
        use forge_ui::settings_registry::{SettingsFile, SettingsPage, SettingsRegistry};
        let mut defaults = serde_json::Map::new();
        for (key, property) in schema.get("properties").and_then(Value::as_object).into_iter().flatten() {
            if let Some(default) = property.get("default") {
                defaults.insert(key.clone(), default.clone());
                self.setting_defaults.insert(key.clone(), default.clone());
            }
        }
        let page = SettingsPage {
            id: format!("ext:{}", info.id),
            title: schema.get("title").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| info.name.clone()),
            file: SettingsFile::Config(SETTINGS_FILE.into()),
            schema: schema.clone(),
            keys: None,
            defaults: Value::Object(defaults),
            order: 100,
            actions: vec![],
        };
        let registry = SettingsRegistry::global(cx);
        registry.update(cx, |registry, cx| registry.register(page, cx));
    }

    /// A setting's value: the user's, else the declared default.
    fn setting(&self, key: &str) -> Value {
        let stored = forge_ui::settings_registry::SettingsFile::Config(SETTINGS_FILE.into()).read();
        stored.get(key).filter(|v| !v.is_null()).or_else(|| self.setting_defaults.get(key)).cloned().unwrap_or(Value::Null)
    }

    pub fn set_workspace(&mut self, workspace: WeakEntity<Workspace>, window: AnyWindowHandle, cx: &mut Context<Self>) {
        if let Some(entity) = workspace.upgrade() {
            self.watch_workspace(&entity, cx);
        }
        self.workspace = Some((workspace, window));
        // The caller is usually inside a workspace update; replay on the next tick so the
        // deferred calls can borrow the workspace.
        cx.spawn(async move |this, cx| {
            this.update(cx, |this, cx| {
                for (method, args, id) in std::mem::take(&mut this.deferred) {
                    this.call(method, args, id, cx);
                }
                this.active_item_changed(cx);
            })
            .ok();
        })
        .detach();
    }

    pub(crate) fn workspace(&self) -> Option<Entity<Workspace>> {
        self.workspace.as_ref().and_then(|(ws, _)| ws.upgrade())
    }

    pub fn dispatch(&self, node: u32, event: &str, payload: Value) {
        let payload = if payload.is_null() { String::new() } else { payload.to_string() };
        self.js.send(ToJs::Dispatch { node, event: event.to_string(), payload });
    }

    /// A webview page posted a message to its extension.
    pub fn page_message(&self, panel: String, json: String) {
        log::debug!("webview {panel} → extension: {json}");
        self.js.send(ToJs::WebviewMessage { panel, json });
    }

    fn registered(&self) -> Vec<String> {
        self.panels.iter().map(|p| p.id.clone()).collect()
    }

    /// The panels shown in extension slot `slot`, in tab order.
    pub fn slot_panels(&self, slot: usize) -> Vec<PanelInfo> {
        let registered = self.registered();
        self.layout.tabs(slot, &registered).filter_map(|id| self.panels.iter().find(|p| &p.id == id).cloned()).collect()
    }

    pub fn slot_of(&self, panel: &str) -> Option<usize> {
        self.layout.slot_of(panel)
    }

    /// Moves an extension panel into `slot` (as a tab, before `index` or at the end).
    pub fn move_tab(&mut self, panel: &str, slot: usize, index: Option<usize>, cx: &mut Context<Self>) {
        let registered = self.registered();
        self.layout.move_tab(panel, slot, index, &registered);
        self.layout_changed(cx);
    }

    /// Gives an extension panel a slot of its own; returns it.
    pub fn move_to_free_slot(&mut self, panel: &str, cx: &mut Context<Self>) -> Option<usize> {
        let registered = self.registered();
        let slot = self.layout.move_to_free_slot(panel, &registered);
        self.layout_changed(cx);
        slot
    }

    fn layout_changed(&mut self, cx: &mut Context<Self>) {
        let json = self.layout.to_json();
        let kvp = db::kvp::KeyValueStore::global(cx);
        cx.background_spawn(async move {
            if let Err(e) = kvp.write_kvp(LAYOUT_KEY.into(), json).await {
                log::error!("failed to save the extension layout: {e:#}");
            }
        })
        .detach();
        cx.emit(HostEvent::Changed);
        cx.notify();
    }

    fn add_panel(&mut self, info: PanelInfo, cx: &mut Context<Self>) {
        self.panels.retain(|p| p.id != info.id);
        let id = info.id.clone();
        self.panels.push(info);
        let registered = self.registered();
        if self.layout.place(&id, &registered) {
            self.layout_changed(cx);
        } else {
            cx.emit(HostEvent::Changed);
        }
    }

    /// Stops an extension and undoes what it registered (panels, commands, listeners).
    pub fn unload(&mut self, id: &str, cx: &mut Context<Self>) -> Option<LoadedExtension> {
        let index = self.extensions.iter().position(|e| e.id == id)?;
        let extension = self.extensions.remove(index);
        self.js.send(ToJs::Unload { id: id.to_string() });
        self.kill_processes_of(id);
        cx.emit(HostEvent::Changed);
        cx.notify();
        Some(extension)
    }

    /// Loads an extension again from its folder (after rebuilding or updating it).
    pub fn reload(&mut self, id: &str, cx: &mut Context<Self>) -> Result<()> {
        let extension = self.unload(id, cx).with_context(|| format!("{id} is not loaded"))?;
        self.errors.retain(|e| !mentions(e, id));
        let discovered = read_manifest(&extension.path)?.with_context(|| format!("{} is no longer an extension", extension.path.display()))?;
        self.load(discovered, cx);
        Ok(())
    }

    pub fn origin(&self, id: &str) -> Origin {
        let Some(extension) = self.extensions.iter().find(|e| e.id == id) else { return Origin::Development };
        let parent = extension.path.parent();
        if parent == Some(paths::data_dir().join("extensions").as_path()) {
            Origin::Installed
        } else if parent.is_some_and(|p| p.ends_with("Contents/Resources/extensions")) {
            Origin::Bundled
        } else {
            Origin::Development
        }
    }

    /// Errors that name extension `id` (loading or activating it, its own messages).
    pub fn errors_of(&self, id: &str) -> Vec<String> {
        self.errors.iter().filter(|e| mentions(e, id)).cloned().collect()
    }

    /// Errors no extension is named in.
    pub fn other_errors(&self) -> Vec<String> {
        self.errors.iter().filter(|e| !self.extensions.iter().any(|x| mentions(e, &x.id))).cloned().collect()
    }

    /// Whether `id` was installed into Forge's extensions folder (and can be uninstalled).
    pub fn is_installed(&self, id: &str) -> bool {
        let installed = paths::data_dir().join("extensions");
        self.extensions.iter().any(|e| e.id == id && e.path.parent() == Some(installed.as_path()))
    }

    /// Unloads an installed extension and deletes its folder.
    pub fn uninstall(&mut self, id: &str, cx: &mut Context<Self>) -> Result<()> {
        anyhow::ensure!(self.is_installed(id), "only extensions in {} can be uninstalled", paths::data_dir().join("extensions").display());
        let extension = self.unload(id, cx).context("not loaded")?;
        std::fs::remove_dir_all(&extension.path).with_context(|| format!("cannot delete {}", extension.path.display()))?;
        Ok(())
    }

    /// Makes extension panel `panel` the visible tab of its slot (the caller shows the slot).
    pub fn reveal(&self, panel: &str, cx: &mut Context<Self>) {
        cx.emit(HostEvent::Reveal { panel: panel.to_string() });
    }

    pub fn run_command(&self, id: &str) {
        self.js.send(ToJs::RunCommand { id: id.to_string() });
    }

    fn handle(&mut self, msg: FromJs, cx: &mut Context<Self>) {
        match msg {
            FromJs::Commit { panel, ops } => {
                // Unmounting a closed tab only removes nodes from a tree that is gone.
                if !self.trees.contains_key(&panel) && ops.iter().all(|op| matches!(op, crate::tree::Op::Remove { .. })) {
                    return;
                }
                self.trees.entry(panel).or_default().apply(ops);
                cx.emit(HostEvent::Changed);
                cx.notify();
            }
            FromJs::Log { level, message } => {
                let level = match level.as_str() {
                    "error" => log::Level::Error,
                    "warn" => log::Level::Warn,
                    "debug" => log::Level::Debug,
                    _ => log::Level::Info,
                };
                log::log!(target: "forge::extensions", level, "{message}");
                if level == log::Level::Error {
                    self.errors.push(message);
                    cx.emit(HostEvent::Changed);
                }
            }
            FromJs::Call { method, args, id } => self.call(method, args, id, cx),
        }
    }

    pub(crate) fn reply(&self, id: u64, result: Result<Value>) {
        if id == 0 {
            if let Err(e) = result {
                log::error!(target: "forge::extensions", "{e:#}");
            }
            return;
        }
        let (ok, json) = match result {
            Ok(v) => (true, v.to_string()),
            Err(e) => (false, json!(format!("{e:#}")).to_string()),
        };
        self.js.send(ToJs::Resolve { call: id, ok, json });
    }

    fn call(&mut self, method: String, args: Value, id: u64, cx: &mut Context<Self>) {
        let needs_workspace = ["workspace.", "editor.", "terminal.", "tabs.", "window."].iter().any(|p| method.starts_with(p));
        if needs_workspace && self.workspace().is_none() {
            self.deferred.push((method, args, id));
            return;
        }
        let str_arg = |k: &str| args.get(k).and_then(Value::as_str).map(str::to_string);
        match method.as_str() {
            "panels.register" => {
                let info = PanelInfo {
                    id: str_arg("id").unwrap_or_default(),
                    title: str_arg("title").unwrap_or_default(),
                    icon: str_arg("icon").unwrap_or_else(|| "sparkle".into()),
                    webview: None,
                    fill: str_arg("layout").as_deref() == Some("fill"),
                    extension: str_arg("extension"),
                };
                self.add_panel(info, cx);
                self.reply(id, Ok(Value::Null));
            }
            "webviews.register" => {
                let (Some(root), Some(html)) = (str_arg("root"), str_arg("html")) else {
                    return self.reply(id, Err(anyhow!("webviews.register needs `root` and `html`")));
                };
                let info = PanelInfo {
                    id: str_arg("id").unwrap_or_default(),
                    title: str_arg("title").unwrap_or_default(),
                    icon: str_arg("icon").unwrap_or_else(|| "globe".into()),
                    webview: Some(crate::webview::WebviewSource { root: root.into(), html }),
                    fill: false,
                    extension: str_arg("extension"),
                };
                self.add_panel(info, cx);
                self.reply(id, Ok(Value::Null));
            }
            "webviews.postMessage" => {
                let panel = str_arg("id").unwrap_or_default();
                let json = args.get("message").cloned().unwrap_or(Value::Null).to_string();
                cx.emit(HostEvent::WebviewMessage { panel, json });
                self.reply(id, Ok(Value::Null));
            }
            "panels.unregister" => {
                let pid = str_arg("id").unwrap_or_default();
                self.panels.retain(|p| p.id != pid);
                self.trees.remove(&pid);
                cx.emit(HostEvent::Changed);
                self.reply(id, Ok(Value::Null));
            }
            "commands.register" => {
                let cmd = CommandInfo { id: str_arg("id").unwrap_or_default(), title: str_arg("title").unwrap_or_default(), extension: str_arg("extension") };
                self.commands.retain(|c| c.id != cmd.id);
                self.commands.push(cmd);
                cx.emit(HostEvent::Changed);
                self.reply(id, Ok(Value::Null));
            }
            "commands.unregister" => {
                let cid = str_arg("id").unwrap_or_default();
                self.commands.retain(|c| c.id != cid);
                cx.emit(HostEvent::Changed);
                self.reply(id, Ok(Value::Null));
            }
            "settings.get" => {
                let key = str_arg("key").unwrap_or_default();
                self.reply(id, Ok(self.setting(&key)));
            }
            "window.showMessage" => {
                let message = str_arg("message").unwrap_or_default();
                let result = self.with_workspace(cx, |ws, cx| {
                    ws.show_toast(Toast::new(NotificationId::named("forge-extension".into()), message), cx);
                    Value::Null
                });
                self.reply(id, result);
            }
            "window.confirm" | "window.pickFiles" | "window.saveFile" => self.dialog(&method, &args, id, cx),
            "clipboard.writeText" => {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(str_arg("text").unwrap_or_default()));
                self.reply(id, Ok(Value::Null));
            }
            "clipboard.readText" => {
                let text = cx.read_from_clipboard().and_then(|item| item.text());
                self.reply(id, Ok(json!(text)));
            }
            "secrets.get" | "secrets.set" | "secrets.delete" => self.secret(&method, &args, id, cx),
            "process.spawn" => self.spawn_process(&args, id, cx),
            "process.write" | "process.closeStdin" | "process.kill" => {
                let result = self.process_call(&method, &args);
                self.reply(id, result);
            }
            "tabs.open" | "tabs.close" | "tabs.update" => {
                let result = self.tab_call(&method, &args, cx);
                self.reply(id, result);
            }
            "workspace.roots" => {
                let result = self.with_workspace(cx, |ws, cx| {
                    let roots: Vec<String> = ws.visible_worktrees(cx).map(|wt| wt.read(cx).abs_path().to_string_lossy().into_owned()).collect();
                    json!(roots)
                });
                self.reply(id, result);
            }
            "workspace.activeFile" => {
                let result = self.with_workspace(cx, |ws, cx| {
                    let path = ws.active_item(cx).and_then(|item| item.project_path(cx)).and_then(|pp| ws.project().read(cx).absolute_path(&pp, cx));
                    json!(path.map(|p| p.to_string_lossy().into_owned()))
                });
                self.reply(id, result);
            }
            "workspace.readFile" => {
                let Some(path) = str_arg("path") else { return self.reply(id, Err(anyhow!("missing path"))) };
                let path = self.resolve_path(&path, cx);
                let fs = <dyn fs::Fs>::global(cx);
                let js = self.js.clone();
                cx.background_spawn(async move {
                    let result = fs.load(&path).await.with_context(|| format!("cannot read {}", path.display()));
                    let (ok, json) = match result {
                        Ok(text) => (true, json!(text).to_string()),
                        Err(e) => (false, json!(format!("{e:#}")).to_string()),
                    };
                    js.send(ToJs::Resolve { call: id, ok, json });
                })
                .detach();
            }
            "workspace.openFile" => {
                let Some(path) = str_arg("path") else { return self.reply(id, Err(anyhow!("missing path"))) };
                let path = self.resolve_path(&path, cx);
                let (Some(workspace), Some((_, window))) = (self.workspace(), self.workspace.clone()) else {
                    return self.reply(id, Err(anyhow!("no workspace is open")));
                };
                let js = self.js.clone();
                window
                    .update(cx, |_, window, cx| {
                        let task = workspace.update(cx, |ws, cx| ws.open_abs_path(path, workspace::OpenOptions::default(), window, cx));
                        cx.spawn(async move |_| {
                            let (ok, json) = match task.await {
                                Ok(_) => (true, String::new()),
                                Err(e) => (false, json!(format!("{e:#}")).to_string()),
                            };
                            js.send(ToJs::Resolve { call: id, ok, json });
                        })
                        .detach();
                    })
                    .log_err();
            }
            other if self.call_api(other, &args, id, cx) => {}
            other => self.reply(id, Err(anyhow!("unknown host method {other}"))),
        }
    }

    /// `tabs.open` / `tabs.close` / `tabs.update`.
    fn tab_call(&mut self, method: &str, args: &Value, cx: &mut Context<Self>) -> Result<Value> {
        let str_arg = |k: &str| args.get(k).and_then(Value::as_str).map(str::to_string);
        let tab = str_arg("id").context("missing tab id")?;
        let workspace = self.workspace().context("no workspace is open")?;
        let (_, window) = self.workspace.clone().context("no workspace is open")?;
        let host = cx.entity();
        let method = method.to_string();
        let args = args.clone();
        // Calls can arrive while the workspace is being updated: act on the next tick.
        cx.defer(move |cx| {
            window
                .update(cx, |_, window, cx| {
                    workspace.update(cx, |ws, cx| match method.as_str() {
                        "tabs.open" => {
                            let title = args.get("title").and_then(Value::as_str).unwrap_or(&tab).to_string();
                            let icon = args.get("icon").and_then(Value::as_str).unwrap_or("sparkle").to_string();
                            crate::tab::open(host, ws, tab, title, icon, window, cx);
                        }
                        "tabs.close" => crate::tab::close(ws, &tab, window, cx),
                        _ => {
                            let get = |k: &str| args.get(k).and_then(Value::as_str).map(str::to_string);
                            crate::tab::update(ws, &tab, get("title"), get("icon"), get("tooltip"), cx);
                        }
                    })
                })
                .ok();
        });
        Ok(Value::Null)
    }

    /// `window.confirm` (a native alert; replies with the index of the button chosen),
    /// `window.pickFiles` and `window.saveFile` (native file dialogs; null when cancelled).
    fn dialog(&mut self, method: &str, args: &Value, id: u64, cx: &mut Context<Self>) {
        let js = self.js.clone();
        let resolve = move |result: Result<Value>| {
            let (ok, json) = match result {
                Ok(v) => (true, v.to_string()),
                Err(e) => (false, json!(format!("{e:#}")).to_string()),
            };
            js.send(ToJs::Resolve { call: id, ok, json });
        };
        match method {
            "window.confirm" => {
                let Some((_, window)) = self.workspace.clone() else { return resolve(Err(anyhow!("no workspace is open"))) };
                let message = args.get("message").and_then(Value::as_str).unwrap_or_default().to_string();
                let detail = args.get("detail").and_then(Value::as_str).map(str::to_string);
                let mut buttons: Vec<String> = args.get("buttons").and_then(Value::as_array).into_iter().flatten().filter_map(|b| b.as_str().map(str::to_string)).collect();
                if buttons.is_empty() {
                    buttons = vec!["OK".into(), "Cancel".into()];
                }
                let level = match args.get("level").and_then(Value::as_str) {
                    Some("warning") => gpui::PromptLevel::Warning,
                    Some("error") | Some("danger") => gpui::PromptLevel::Critical,
                    _ => gpui::PromptLevel::Info,
                };
                cx.defer(move |cx| {
                    let answer = window.update(cx, |_, window, cx| {
                        let buttons: Vec<&str> = buttons.iter().map(String::as_str).collect();
                        window.prompt(level, &message, detail.as_deref(), &buttons, cx)
                    });
                    match answer {
                        Ok(answer) => cx.spawn(async move |_| resolve(Ok(answer.await.map(|i| json!(i)).unwrap_or(Value::Null)))).detach(),
                        Err(e) => resolve(Err(e)),
                    }
                });
            }
            "window.pickFiles" => {
                let flag = |k: &str, default: bool| args.get(k).and_then(Value::as_bool).unwrap_or(default);
                let options = gpui::PathPromptOptions {
                    files: flag("files", true),
                    directories: flag("directories", false),
                    multiple: flag("multiple", false),
                    prompt: args.get("prompt").and_then(Value::as_str).map(|p| p.to_string().into()),
                };
                let paths = cx.prompt_for_paths(options);
                cx.spawn(async move |_, _| {
                    let paths = paths.await.ok().and_then(Result::ok).flatten();
                    resolve(Ok(json!(paths.map(|p| p.into_iter().map(|p| p.to_string_lossy().into_owned()).collect::<Vec<_>>()))));
                })
                .detach();
            }
            _ => {
                let directory = args.get("directory").and_then(Value::as_str).map(PathBuf::from).or_else(|| self.workspace().and_then(|_| Some(self.resolve_path(".", cx)))).unwrap_or_else(|| paths::home_dir().clone());
                let name = args.get("name").and_then(Value::as_str).map(str::to_string);
                let path = cx.prompt_for_new_path(&directory, name.as_deref());
                cx.spawn(async move |_, _| {
                    let path = path.await.ok().and_then(Result::ok).flatten();
                    resolve(Ok(json!(path.map(|p| p.to_string_lossy().into_owned()))));
                })
                .detach();
            }
        }
    }

    /// `secrets.*`: strings an extension keeps in the system keychain (passwords, tokens),
    /// one entry per extension and key.
    fn secret(&mut self, method: &str, args: &Value, id: u64, cx: &mut Context<Self>) {
        let extension = args.get("extension").and_then(Value::as_str).unwrap_or_default();
        let key = args.get("key").and_then(Value::as_str).unwrap_or_default();
        let url = format!("forge-extension://{extension}/{key}");
        let js = self.js.clone();
        let task: gpui::Task<Result<Value>> = match method {
            "secrets.get" => {
                let read = cx.read_credentials(&url);
                cx.background_spawn(async move { Ok(json!(read.await?.map(|(_, secret)| String::from_utf8_lossy(&secret).into_owned()))) })
            }
            "secrets.set" => match args.get("value").and_then(Value::as_str) {
                Some(value) => {
                    let write = cx.write_credentials(&url, key, value.as_bytes());
                    cx.background_spawn(async move { write.await.map(|_| Value::Null) })
                }
                None => {
                    let delete = cx.delete_credentials(&url);
                    cx.background_spawn(async move { delete.await.map(|_| Value::Null) })
                }
            },
            _ => {
                let delete = cx.delete_credentials(&url);
                cx.background_spawn(async move { delete.await.map(|_| Value::Null) })
            }
        };
        cx.background_spawn(async move {
            let (ok, json) = match task.await {
                Ok(v) => (true, v.to_string()),
                Err(e) => (false, json!(format!("{e:#}")).to_string()),
            };
            js.send(ToJs::Resolve { call: id, ok, json });
        })
        .detach();
    }

    fn with_workspace(&self, cx: &mut Context<Self>, f: impl FnOnce(&mut Workspace, &mut Context<Workspace>) -> Value) -> Result<Value> {
        let ws = self.workspace().context("no workspace is open")?;
        Ok(ws.update(cx, f))
    }

    /// Relative paths resolve against the first workspace root.
    pub(crate) fn resolve_path(&self, path: &str, cx: &App) -> PathBuf {
        let p = PathBuf::from(path);
        if p.is_absolute() {
            return p;
        }
        let root = self
            .workspace()
            .and_then(|ws| ws.read(cx).visible_worktrees(cx).next().map(|wt| wt.read(cx).abs_path().to_path_buf()));
        root.map(|r| r.join(&p)).unwrap_or(p)
    }
}

/// Whether an error message is about extension `id` (`id: …`, `… extension id: …`, `activate id: …`).
fn mentions(message: &str, id: &str) -> bool {
    message.starts_with(&format!("{id}:")) || message.contains(&format!(" {id}:")) || message.contains(&format!(" {id} ")) || message.ends_with(&format!(" {id}"))
}

pub(crate) struct Discovered {
    pub(crate) info: LoadedExtension,
    main: PathBuf,
    settings: Option<Value>,
}

#[derive(Deserialize)]
struct Manifest {
    name: String,
    #[serde(default)]
    version: Option<String>,
    #[serde(default, rename = "displayName")]
    display_name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    forge: Option<ForgeManifest>,
}

#[derive(Deserialize, Default)]
struct ForgeManifest {
    #[serde(default)]
    main: Option<String>,
    /// Settings the extension declares, as a JSON schema object (`{ "properties": … }`).
    #[serde(default)]
    settings: Option<Value>,
}

/// An extension is a folder with a `package.json` that has a `forge` section.
fn discover(dirs: &[PathBuf]) -> Vec<Discovered> {
    let mut out = Vec::new();
    let mut candidates = Vec::new();
    for dir in dirs {
        if dir.join("package.json").is_file() {
            candidates.push(dir.clone());
        } else if let Ok(entries) = std::fs::read_dir(dir) {
            let mut subdirs: Vec<_> = entries.filter_map(Result::ok).map(|e| e.path()).filter(|p| p.join("package.json").is_file()).collect();
            subdirs.sort();
            candidates.extend(subdirs);
        }
    }
    for path in candidates {
        match read_manifest(&path) {
            Ok(Some(d)) if !out.iter().any(|o: &Discovered| o.info.id == d.info.id) => out.push(d),
            Ok(_) => {}
            Err(e) => log::warn!("skipping extension at {}: {e:#}", path.display()),
        }
    }
    out
}

pub(crate) fn read_manifest(path: &Path) -> Result<Option<Discovered>> {
    let manifest: Manifest = serde_json::from_str(&std::fs::read_to_string(path.join("package.json"))?)?;
    let Some(forge) = manifest.forge else { return Ok(None) };
    let main = path.join(forge.main.as_deref().unwrap_or("dist/extension.js"));
    anyhow::ensure!(main.is_file(), "{} not built (run `forge-ext build`)", main.display());
    Ok(Some(Discovered {
        info: LoadedExtension { id: manifest.name.clone(), name: manifest.display_name.unwrap_or(manifest.name), path: path.to_path_buf(), version: manifest.version, description: manifest.description, has_settings: forge.settings.is_some() },
        main,
        settings: forge.settings,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_built_extensions_only() {
        let dir = tempfile::tempdir().unwrap();
        let ok = dir.path().join("ok");
        std::fs::create_dir_all(ok.join("dist")).unwrap();
        std::fs::write(ok.join("package.json"), r#"{"name":"ok","displayName":"OK","forge":{}}"#).unwrap();
        std::fs::write(ok.join("dist/extension.js"), "").unwrap();
        let unbuilt = dir.path().join("unbuilt");
        std::fs::create_dir_all(&unbuilt).unwrap();
        std::fs::write(unbuilt.join("package.json"), r#"{"name":"unbuilt","forge":{}}"#).unwrap();
        let plain = dir.path().join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        std::fs::write(plain.join("package.json"), r#"{"name":"not-an-extension"}"#).unwrap();

        let found = discover(&[dir.path().to_path_buf()]);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].info.name, "OK");
    }
}
