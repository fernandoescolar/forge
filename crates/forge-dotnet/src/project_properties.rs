//! A project's properties in a tab, like Visual Studio's project properties: target
//! frameworks, output type, nullable reference types, language version, implicit usings,
//! warnings as errors, names, package metadata…
//!
//! Each shows the value MSBuild evaluates (with `Directory.Build.props` and the SDK's
//! defaults), and whether the project file itself sets it. Changing one writes it to the
//! project file, keeping its formatting (`dotnet_model::msbuild::edit::set_property`);
//! *Reset* removes it there, so the inherited value applies again.

use std::path::PathBuf;

use dotnet_model::msbuild::edit;
use dotnet_model::{Project as MsBuildProject, paths};
use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, AppContext as _, ClickEvent, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, IntoElement, ParentElement as _, Render,
    ScrollHandle, SharedString, Styled as _, Task, WeakEntity, Window, div, px,
};
use theme::ActiveTheme as _;
use ui::{Button, ButtonCommon as _, ButtonSize, ButtonStyle, Clickable as _, Color, Icon, IconName, Label, LabelCommon as _, LabelSize, Tooltip, h_flex, v_flex};
use ui::{InteractiveElement as _, StatefulInteractiveElement as _};
use workspace::{Item, Workspace, item::ItemEvent};

use crate::config;
use crate::model::DotnetModel;
use forge_ui::pick::{self, Choice};

/// How a property is edited.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Kind {
    /// One of these values.
    Choice(&'static [&'static str]),
    /// `true` or `false`.
    Bool,
    /// Free text.
    Text,
    /// Target frameworks, `;`-separated: written as `TargetFramework` or `TargetFrameworks`.
    Frameworks,
}

struct Field {
    section: &'static str,
    name: &'static str,
    label: &'static str,
    help: &'static str,
    kind: Kind,
}

const FIELDS: &[Field] = &[
    Field { section: "Application", name: "TargetFramework", label: "Target frameworks", help: "net8.0, or several: net8.0;net9.0", kind: Kind::Frameworks },
    Field { section: "Application", name: "OutputType", label: "Output type", help: "What the build makes", kind: Kind::Choice(&["Exe", "Library", "WinExe"]) },
    Field { section: "Application", name: "RootNamespace", label: "Root namespace", help: "The namespace of new files", kind: Kind::Text },
    Field { section: "Application", name: "AssemblyName", label: "Assembly name", help: "The name of the built assembly", kind: Kind::Text },
    Field { section: "Build", name: "Nullable", label: "Nullable reference types", help: "Warnings about null", kind: Kind::Choice(&["enable", "disable", "warnings", "annotations"]) },
    Field { section: "Build", name: "LangVersion", label: "Language version", help: "The C# version to compile with", kind: Kind::Choice(&["default", "latest", "latestMajor", "preview", "14.0", "13.0", "12.0", "11.0", "10.0"]) },
    Field { section: "Build", name: "ImplicitUsings", label: "Implicit usings", help: "Common namespaces without `using`", kind: Kind::Choice(&["enable", "disable"]) },
    Field { section: "Build", name: "TreatWarningsAsErrors", label: "Treat warnings as errors", help: "Fail the build on warnings", kind: Kind::Bool },
    Field { section: "Build", name: "WarningLevel", label: "Warning level", help: "Which warnings the compiler reports", kind: Kind::Choice(&["0", "1", "2", "3", "4", "5", "6", "7", "8", "9"]) },
    Field { section: "Build", name: "NoWarn", label: "Suppressed warnings", help: "Warning codes, `;`-separated", kind: Kind::Text },
    Field { section: "Build", name: "GenerateDocumentationFile", label: "Documentation file", help: "Write the XML documentation", kind: Kind::Bool },
    Field { section: "Build", name: "AllowUnsafeBlocks", label: "Unsafe code", help: "Allow `unsafe`", kind: Kind::Bool },
    Field { section: "Publish", name: "InvariantGlobalization", label: "Invariant globalization", help: "No culture data", kind: Kind::Bool },
    Field { section: "Publish", name: "PublishAot", label: "Native AOT", help: "Publish ahead-of-time compiled", kind: Kind::Bool },
    Field { section: "Publish", name: "SelfContained", label: "Self-contained", help: "Publish with the .NET runtime", kind: Kind::Bool },
    Field { section: "Package", name: "PackageId", label: "Package id", help: "The NuGet package's id", kind: Kind::Text },
    Field { section: "Package", name: "Version", label: "Version", help: "1.2.3", kind: Kind::Text },
    Field { section: "Package", name: "Authors", label: "Authors", help: "", kind: Kind::Text },
    Field { section: "Package", name: "Description", label: "Description", help: "", kind: Kind::Text },
    Field { section: "Package", name: "PackageLicenseExpression", label: "License", help: "An SPDX expression, e.g. MIT", kind: Kind::Text },
];

/// Opens the properties of `project`, reusing a tab already open for it.
pub fn open(workspace: &mut Workspace, model: Entity<DotnetModel>, project: PathBuf, window: &mut Window, cx: &mut gpui::Context<Workspace>) {
    let existing = workspace.items_of_type::<ProjectProperties>(cx).find(|p| p.read(cx).path == project);
    if let Some(existing) = existing {
        workspace.activate_item(&existing, true, true, window, cx);
        return;
    }
    let weak = workspace.weak_handle();
    let view = cx.new(|cx| ProjectProperties::new(weak, model, project, cx));
    workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
}

pub struct ProjectProperties {
    workspace: WeakEntity<Workspace>,
    model: Entity<DotnetModel>,
    path: PathBuf,
    project: Option<MsBuildProject>,
    /// The project file's own text, to tell what it sets from what it inherits.
    file_text: String,
    error: Option<String>,
    focus_handle: FocusHandle,
    scroll: ScrollHandle,
    load_task: Option<Task<()>>,
}

impl ProjectProperties {
    fn new(workspace: WeakEntity<Workspace>, model: Entity<DotnetModel>, path: PathBuf, cx: &mut Context<Self>) -> Self {
        let mut view = Self { workspace, model, path, project: None, file_text: String::new(), error: None, focus_handle: cx.focus_handle(), scroll: ScrollHandle::new(), load_task: None };
        view.load(cx);
        view
    }

    /// Evaluates the project again (in the configuration chosen in the title bar).
    fn load(&mut self, cx: &mut Context<Self>) {
        let path = self.path.clone();
        let mut options = config::get(cx).eval_options();
        let build = forge_languages::active_build::ActiveBuild::get(cx);
        if let Some(configuration) = build.configuration {
            options.configuration = configuration;
        }
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let result = cx.background_spawn(async move { (dotnet_model::evaluate(&path, &options), std::fs::read_to_string(&path).unwrap_or_default()) }).await;
            this.update(cx, |this, cx| {
                match result {
                    (Ok(project), text) => {
                        this.project = Some(project);
                        this.file_text = text;
                    }
                    (Err(error), _) => this.error = Some(format!("{error:#}")),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn value(&self, field: &Field) -> Option<String> {
        let project = self.project.as_ref()?;
        match field.kind {
            Kind::Frameworks => Some(project.target_frameworks().join(";")).filter(|v| !v.is_empty()),
            _ => project.properties.get(field.name).map(str::to_string).filter(|v| !v.is_empty()),
        }
    }

    /// Whether the project file sets the property itself.
    fn set_here(&self, field: &Field) -> bool {
        let names: &[&str] = match field.kind {
            Kind::Frameworks => &["TargetFramework", "TargetFrameworks"],
            _ => &[field.name],
        };
        names.iter().any(|name| self.file_text.contains(&format!("<{name}>")))
    }

    /// Writes `value` (or, with `None`, removes the project's own value) and reloads.
    fn write(&mut self, field_index: usize, value: Option<String>, cx: &mut Context<Self>) {
        let field = &FIELDS[field_index];
        let path = self.path.clone();
        let result = match field.kind {
            Kind::Frameworks => {
                let frameworks: Vec<String> = value.iter().flat_map(|v| v.split([';', ',', ' ']).map(str::trim).filter(|f| !f.is_empty()).map(String::from).collect::<Vec<_>>()).collect();
                let (single, several) = match frameworks.len() {
                    0 => (None, None),
                    1 => (Some(frameworks[0].clone()), None),
                    _ => (None, Some(frameworks.join(";"))),
                };
                edit::set_property(&path, "TargetFramework", single.as_deref()).and_then(|()| edit::set_property(&path, "TargetFrameworks", several.as_deref()))
            }
            _ => edit::set_property(&path, field.name, value.as_deref()),
        };
        match result {
            Ok(()) => {
                self.error = None;
                self.model.update(cx, |model, cx| model.reload_paths(std::slice::from_ref(&path), cx));
                self.load(cx);
            }
            Err(error) => self.error = Some(format!("{error:#}")),
        }
        cx.notify();
    }

    fn edit(&mut self, field_index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let field = &FIELDS[field_index];
        let current = self.value(field).unwrap_or_default();
        let view = cx.entity().downgrade();
        let placeholder = format!("{} ({})", field.label, field.name);
        match field.kind {
            Kind::Choice(_) | Kind::Bool => {
                let values: Vec<&'static str> = match field.kind {
                    Kind::Choice(values) => values.to_vec(),
                    _ => vec!["true", "false"],
                };
                let choices = values.iter().map(|v| if *v == current { Choice::new(*v).detail("current") } else { Choice::new(*v) }).collect();
                pick::defer_workspace(self.workspace.clone(), window, cx, move |workspace, window, cx| {
                    pick::pick(workspace, &placeholder, choices, window, cx, move |index, _, cx| {
                        view.update(cx, |view, cx| view.write(field_index, Some(values[index].to_string()), cx)).ok();
                    });
                });
            }
            Kind::Text | Kind::Frameworks => {
                pick::defer_workspace(self.workspace.clone(), window, cx, move |workspace, window, cx| {
                    pick::ask(workspace, &placeholder, &current, window, cx, move |text, _, cx| {
                        let text = text.trim().to_string();
                        view.update(cx, |view, cx| view.write(field_index, (!text.is_empty()).then_some(text), cx)).ok();
                    });
                });
            }
        }
    }

    fn render_field(&self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let field = &FIELDS[index];
        let colors = cx.theme().colors().clone();
        let value = self.value(field);
        let set_here = self.set_here(field);
        h_flex()
            .id(SharedString::from(format!("property-{}", field.name)))
            .w_full()
            .px_3()
            .py_1()
            .gap_3()
            .cursor_pointer()
            .hover(|row| row.bg(colors.element_hover))
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| this.edit(index, window, cx)))
            .child(
                v_flex()
                    .w(px(240.))
                    .child(Label::new(field.label).size(LabelSize::Small))
                    .child(Label::new(field.name).size(LabelSize::XSmall).color(Color::Muted).buffer_font(cx)),
            )
            .child(
                div().flex_1().min_w_0().child(match &value {
                    Some(value) => Label::new(value.clone()).size(LabelSize::Small).buffer_font(cx).single_line(),
                    None => Label::new(if field.help.is_empty() { "—".to_string() } else { format!("— {}", field.help) }).size(LabelSize::Small).color(Color::Muted),
                }),
            )
            .child(
                Label::new(if set_here { "set here" } else if value.is_some() { "inherited" } else { "" })
                    .size(LabelSize::XSmall)
                    .color(if set_here { Color::Accent } else { Color::Muted }),
            )
            .when(set_here && field.kind != Kind::Frameworks, |row| {
                row.child(
                    Button::new(SharedString::from(format!("reset-{}", field.name)), "Reset")
                        .style(ButtonStyle::Subtle)
                        .size(ButtonSize::Compact)
                        .label_size(LabelSize::XSmall)
                        .tooltip(Tooltip::text("Remove it from the project file, so the inherited value applies"))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.write(index, None, cx);
                        })),
                )
            })
            .into_any_element()
    }
}

impl Render for ProjectProperties {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors().clone();
        let mut sections: Vec<AnyElement> = Vec::new();
        let mut current_section = "";
        for (index, field) in FIELDS.iter().enumerate() {
            if field.section != current_section {
                current_section = field.section;
                sections.push(div().px_3().pt_3().pb_1().child(Label::new(field.section).size(LabelSize::Small).weight(FontWeight::BOLD).color(Color::Muted)).into_any_element());
            }
            sections.push(self.render_field(index, cx));
        }
        v_flex()
            .size_full()
            .track_focus(&self.focus_handle)
            .bg(colors.editor_background)
            .child(
                h_flex()
                    .px_3()
                    .py_2()
                    .gap_2()
                    .border_b_1()
                    .border_color(colors.border)
                    .child(Label::new(paths::file_name(&self.path)).weight(FontWeight::BOLD))
                    .child(Label::new("Click a property to change it; the project file keeps its formatting.").size(LabelSize::Small).color(Color::Muted)),
            )
            .when_some(self.error.clone(), |col, error| col.child(div().px_3().py_1().child(Label::new(error).size(LabelSize::Small).color(Color::Error))))
            .child(div().id("project-properties").flex_1().overflow_y_scroll().track_scroll(&self.scroll).children(sections))
    }
}

impl Focusable for ProjectProperties {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<ItemEvent> for ProjectProperties {}

impl Item for ProjectProperties {
    type Event = ItemEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        format!("Properties: {}", paths::file_stem(&self.path)).into()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Settings))
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}
