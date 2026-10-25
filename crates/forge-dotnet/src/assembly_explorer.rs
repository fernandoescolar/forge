//! The Assembly Explorer: the assemblies a project builds into (its own and those it
//! depends on), their types, and each type as C# or as IL, like ILSpy or Rider's assembly
//! explorer. Decompiling is `ilspycmd`'s (`dotnet tool install -g ilspycmd`), which the
//! view offers to install when it is missing; the code opens read-only.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use dotnet_model::decompiler::{self, TypeEntry};
use dotnet_model::paths;
use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, AppContext as _, ClickEvent, Context, EventEmitter, FocusHandle, Focusable, FontWeight, IntoElement, ParentElement as _, Render,
    ScrollHandle, SharedString, Styled as _, WeakEntity, Window, div, px,
};
use theme::ActiveTheme as _;
use ui::{Button, ButtonCommon as _, ButtonSize, ButtonStyle, Clickable as _, Color, Icon, IconName, IconSize, Label, LabelCommon as _, LabelSize, Tooltip, h_flex, v_flex};
use ui::{InteractiveElement as _, StatefulInteractiveElement as _};
use workspace::{Item, Workspace, item::ItemEvent};

/// How a type is shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Language {
    CSharp,
    Il,
}

/// Opens the explorer for `project`'s build output, reusing a tab already open for it.
pub fn open(workspace: &mut Workspace, project: PathBuf, window: &mut Window, cx: &mut gpui::Context<Workspace>) {
    let existing = workspace.items_of_type::<AssemblyExplorer>(cx).find(|e| e.read(cx).project == project);
    if let Some(existing) = existing {
        workspace.activate_item(&existing, true, true, window, cx);
        return;
    }
    let weak = workspace.weak_handle();
    let view = cx.new(|cx| AssemblyExplorer::new(weak, project, cx));
    workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
}

/// `ilspycmd` where `dotnet tool install -g` puts it, or on the PATH.
fn find_ilspycmd() -> Option<PathBuf> {
    let name = if cfg!(windows) { "ilspycmd.exe" } else { "ilspycmd" };
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from);
    let global = home.map(|home| home.join(".dotnet").join("tools").join(name));
    let on_path = std::env::var_os("PATH").into_iter().flat_map(|path| std::env::split_paths(&path).map(|dir| dir.join(name)).collect::<Vec<_>>());
    global.into_iter().chain(on_path).find(|candidate| candidate.is_file())
}

/// Runs `ilspycmd` and returns what it printed.
async fn run_ilspycmd(tool: &Path, args: Vec<String>) -> Result<String> {
    let mut command = util::command::new_command(tool.to_string_lossy().as_ref());
    command.args(&args).stdin(util::command::Stdio::null()).kill_on_drop(true);
    let output = forge_ui::process::spawn_unblocked(command).context("failed to start ilspycmd")?.output().await.context("ilspycmd failed")?;
    anyhow::ensure!(output.status.success(), "ilspycmd: {}", String::from_utf8_lossy(&output.stderr).trim());
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

pub struct AssemblyExplorer {
    workspace: WeakEntity<Workspace>,
    project: PathBuf,
    focus_handle: FocusHandle,
    tool: Option<PathBuf>,
    folder: Option<PathBuf>,
    assemblies: Vec<PathBuf>,
    expanded: HashSet<PathBuf>,
    types: HashMap<PathBuf, Vec<TypeEntry>>,
    loading: HashSet<PathBuf>,
    message: Option<String>,
    scroll: ScrollHandle,
}

impl AssemblyExplorer {
    fn new(workspace: WeakEntity<Workspace>, project: PathBuf, cx: &mut Context<Self>) -> Self {
        let mut view = Self {
            workspace,
            project,
            focus_handle: cx.focus_handle(),
            tool: None,
            folder: None,
            assemblies: Vec::new(),
            expanded: HashSet::new(),
            types: HashMap::new(),
            loading: HashSet::new(),
            message: None,
            scroll: ScrollHandle::new(),
        };
        view.refresh(cx);
        view
    }

    /// Looks for `ilspycmd` and the build output again.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        let build = forge_languages::active_build::ActiveBuild::get(cx);
        self.tool = find_ilspycmd();
        match decompiler::output_assemblies(&self.project, build.configuration_name(), build.framework.as_deref()) {
            Some((folder, assemblies)) => {
                self.folder = Some(folder);
                self.assemblies = assemblies;
                self.message = None;
            }
            None => {
                self.folder = None;
                self.assemblies.clear();
                self.message = Some(format!("{} hasn't been built in {} yet: build it, then refresh.", paths::file_name(&self.project), build.configuration_name()));
            }
        }
        cx.notify();
    }

    fn toggle(&mut self, assembly: PathBuf, cx: &mut Context<Self>) {
        if !self.expanded.remove(&assembly) {
            self.expanded.insert(assembly.clone());
            if !self.types.contains_key(&assembly) {
                self.load_types(assembly, cx);
            }
        }
        cx.notify();
    }

    fn load_types(&mut self, assembly: PathBuf, cx: &mut Context<Self>) {
        let Some(tool) = self.tool.clone() else { return };
        self.loading.insert(assembly.clone());
        cx.spawn(async move |this, cx| {
            let args = decompiler::list_args(&assembly);
            let result = cx.background_spawn(async move { run_ilspycmd(&tool, args).await }).await;
            this.update(cx, |this, cx| {
                this.loading.remove(&assembly);
                match result {
                    Ok(output) => {
                        this.types.insert(assembly, decompiler::parse_type_list(&output));
                    }
                    Err(error) => this.message = Some(format!("{error:#}")),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Decompiles a type into a file under the temporary folder and opens it read-only.
    fn show_type(&mut self, assembly: PathBuf, type_name: String, language: Language, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tool) = self.tool.clone() else { return };
        let Some(workspace) = self.workspace.upgrade() else { return };
        let stem = paths::file_stem(&assembly);
        let extension = if language == Language::Il { "il" } else { "cs" };
        let file = std::env::temp_dir().join("forge-decompiled").join(&stem).join(format!("{}.{extension}", type_name.replace(['<', '>', '`', '+'], "_")));
        cx.spawn_in(window, async move |this, cx| {
            let target = file.clone();
            let written = cx
                .background_spawn(async move {
                    let text = match language {
                        Language::CSharp => run_ilspycmd(&tool, decompiler::csharp_args(&assembly, &type_name)).await?,
                        Language::Il => {
                            let il = run_ilspycmd(&tool, decompiler::il_args(&assembly)).await?;
                            decompiler::il_of_type(&il, &type_name).with_context(|| format!("no IL for {type_name}"))?
                        }
                    };
                    std::fs::create_dir_all(target.parent().context("no folder")?)?;
                    std::fs::write(&target, text)?;
                    anyhow::Ok(())
                })
                .await;
            if let Err(error) = written {
                this.update(cx, |this, cx| {
                    this.message = Some(format!("{error:#}"));
                    cx.notify();
                })
                .ok();
                return anyhow::Ok(());
            }
            let open = workspace.update_in(cx, |workspace, window, cx| workspace.open_abs_path(file, workspace::OpenOptions::default(), window, cx))?;
            let item = open.await?;
            if let Some(editor) = item.downcast::<editor::Editor>() {
                editor.update(cx, |editor, cx| {
                    if let Some(buffer) = editor.buffer().read(cx).as_singleton() {
                        buffer.update(cx, |buffer, cx| buffer.set_capability(language::Capability::ReadOnly, cx));
                    }
                });
            }
            anyhow::Ok(())
        })
        .detach();
    }

    /// Runs `dotnet tool install -g ilspycmd` in a terminal.
    fn install_tool(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let template = task::TaskTemplate {
            label: "dotnet tool install -g ilspycmd".into(),
            command: "dotnet".into(),
            args: vec!["tool".into(), "install".into(), "-g".into(), "ilspycmd".into()],
            ..task::TaskTemplate::default()
        };
        let Some(resolved) = template.resolve_task("forge-dotnet", &task::TaskContext::default()) else { return };
        if let Some(workspace) = self.workspace.upgrade() {
            workspace.update(cx, |workspace, cx| workspace.schedule_resolved_task(project::TaskSourceKind::UserInput, resolved, false, window, cx));
        }
        self.message = Some("Installing ilspycmd in a terminal; refresh when it's done.".into());
        cx.notify();
    }

    fn render_assembly(&self, assembly: &Path, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors().clone();
        let expanded = self.expanded.contains(assembly);
        let toggle = assembly.to_path_buf();
        let mut column = v_flex().child(
            h_flex()
                .id(SharedString::from(format!("assembly-{}", assembly.display())))
                .w_full()
                .h(px(26.))
                .px_2()
                .gap_2()
                .cursor_pointer()
                .hover(|row| row.bg(colors.element_hover))
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.toggle(toggle.clone(), cx)))
                .child(Icon::new(if expanded { IconName::ChevronDown } else { IconName::ChevronRight }).size(IconSize::XSmall).color(Color::Muted))
                .child(Icon::new(IconName::FileCode).size(IconSize::Small).color(Color::Muted))
                .child(Label::new(paths::file_name(assembly)).size(LabelSize::Small))
                .when(self.loading.contains(assembly), |row| row.child(Label::new("Reading…").size(LabelSize::XSmall).color(Color::Muted))),
        );
        if expanded && let Some(types) = self.types.get(assembly) {
            let mut namespace = None;
            for (index, entry) in types.iter().enumerate() {
                if namespace != Some(entry.namespace()) {
                    namespace = Some(entry.namespace());
                    let label = if entry.namespace().is_empty() { "(global namespace)" } else { entry.namespace() };
                    column = column.child(div().pl(px(32.)).pt_1().child(Label::new(label.to_string()).size(LabelSize::XSmall).color(Color::Muted)));
                }
                let (cs_assembly, il_assembly) = (assembly.to_path_buf(), assembly.to_path_buf());
                let (cs_name, il_name) = (entry.name.clone(), entry.name.clone());
                column = column.child(
                    h_flex()
                        .id(SharedString::from(format!("type-{}-{index}", assembly.display())))
                        .w_full()
                        .pl(px(44.))
                        .pr_2()
                        .h(px(22.))
                        .gap_2()
                        .hover(|row| row.bg(colors.element_hover))
                        .child(Label::new(entry.kind.to_lowercase()).size(LabelSize::XSmall).color(Color::Muted))
                        .child(div().flex_1().min_w_0().child(Label::new(entry.short_name().to_string()).size(LabelSize::Small).single_line()))
                        .child(
                            Button::new(SharedString::from(format!("cs-{index}")), "C#")
                                .style(ButtonStyle::Subtle)
                                .size(ButtonSize::Compact)
                                .label_size(LabelSize::XSmall)
                                .tooltip(Tooltip::text("Decompile to C#"))
                                .on_click(cx.listener(move |this, _, window, cx| this.show_type(cs_assembly.clone(), cs_name.clone(), Language::CSharp, window, cx))),
                        )
                        .child(
                            Button::new(SharedString::from(format!("il-{index}")), "IL")
                                .style(ButtonStyle::Subtle)
                                .size(ButtonSize::Compact)
                                .label_size(LabelSize::XSmall)
                                .tooltip(Tooltip::text("Show its IL"))
                                .on_click(cx.listener(move |this, _, window, cx| this.show_type(il_assembly.clone(), il_name.clone(), Language::Il, window, cx))),
                        ),
                );
            }
        }
        column.into_any_element()
    }
}

impl Render for AssemblyExplorer {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors().clone();
        let assemblies: Vec<AnyElement> = self.assemblies.clone().iter().map(|a| self.render_assembly(a, cx)).collect();
        v_flex()
            .size_full()
            .track_focus(&self.focus_handle)
            .bg(colors.editor_background)
            .child(
                h_flex()
                    .justify_between()
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(colors.border)
                    .child(
                        h_flex()
                            .gap_2()
                            .child(Label::new(format!("Assemblies of {}", paths::file_stem(&self.project))).weight(FontWeight::BOLD))
                            .children(self.folder.as_ref().map(|f| Label::new(f.display().to_string()).size(LabelSize::XSmall).color(Color::Muted))),
                    )
                    .child(
                        Button::new("refresh", "Refresh")
                            .style(ButtonStyle::Subtle)
                            .size(ButtonSize::Compact)
                            .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
                    ),
            )
            .when(self.tool.is_none(), |col| {
                col.child(
                    h_flex()
                        .px_3()
                        .py_2()
                        .gap_2()
                        .child(Label::new("Decompiling needs ilspycmd, the command-line ILSpy.").size(LabelSize::Small))
                        .child(Button::new("install", "Install ilspycmd").style(ButtonStyle::Filled).size(ButtonSize::Compact).on_click(cx.listener(|this, _, window, cx| this.install_tool(window, cx)))),
                )
            })
            .when_some(self.message.clone(), |col, message| col.child(div().px_3().py_1().child(Label::new(message).size(LabelSize::Small).color(Color::Muted))))
            .child(div().id("assemblies").flex_1().overflow_y_scroll().track_scroll(&self.scroll).children(assemblies))
    }
}

impl Focusable for AssemblyExplorer {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<ItemEvent> for AssemblyExplorer {}

impl Item for AssemblyExplorer {
    type Event = ItemEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        format!("Assemblies: {}", paths::file_stem(&self.project)).into()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::FileCode))
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}
