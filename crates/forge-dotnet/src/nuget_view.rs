//! The NuGet package manager, a tab in the editor area like Visual Studio's: browse the
//! package sources, see what is installed and what has updates, and consolidate versions
//! across a solution. Changes are made to project files (or `Directory.Packages.props`)
//! directly, keeping their formatting, followed by a restore.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use dotnet_model::nuget::solution_packages::{self, InstalledPackage};
use dotnet_model::nuget::{PackageInfo, PackageSource, version};
use dotnet_model::{Project as MsBuildProject, msbuild::edit, paths};
use editor::{Editor, EditorEvent};
use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, AppContext as _, Context, IntoElement, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, ParentElement as _, Render, ScrollHandle,
    SharedString, Styled as _, Subscription, Task, WeakEntity, Window, div, px,
};
use theme::ActiveTheme as _;
use ui::{
    Button, ButtonCommon as _, ButtonSize, ButtonStyle, Checkbox, Clickable as _, Color, Disableable as _, Icon, IconButton, IconName, IconSize, Label,
    LabelCommon as _, LabelSize, ToggleState, Toggleable as _, Tooltip, h_flex, v_flex,
};
use ui::{InteractiveElement as _, StatefulInteractiveElement as _};
use workspace::{Item, Workspace, item::ItemEvent};

use crate::config;
use crate::model::{DotnetModel, ModelEvent};
use forge_ui::pick::{self, Choice};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tab {
    Browse,
    Installed,
    Updates,
    Consolidate,
}

impl Tab {
    fn label(self) -> &'static str {
        match self {
            Tab::Browse => "Browse",
            Tab::Installed => "Installed",
            Tab::Updates => "Updates",
            Tab::Consolidate => "Consolidate",
        }
    }
}

/// A row of the package list.
#[derive(Clone, Debug)]
struct Row {
    id: String,
    /// Installed version(s), newest first.
    installed: Vec<String>,
    latest: Option<String>,
    description: String,
    downloads: Option<u64>,
    verified: bool,
}

pub struct NuGetManager {
    workspace: WeakEntity<Workspace>,
    model: Entity<DotnetModel>,
    /// The project it manages, or the whole solution.
    scope: Option<PathBuf>,
    focus_handle: FocusHandle,
    search: Entity<Editor>,
    tab: Tab,
    prerelease: bool,
    sources: Vec<PackageSource>,
    results: Vec<PackageInfo>,
    search_errors: Vec<String>,
    searching: bool,
    more_available: bool,
    /// Newest version per package id (lowercase), for installed packages.
    latest: HashMap<String, String>,
    checking_updates: bool,
    selected: Option<String>,
    /// Details and versions of the selected package.
    details: Option<PackageInfo>,
    versions: Vec<String>,
    chosen_version: Option<String>,
    /// Projects the next install/update/uninstall applies to (solution scope).
    chosen_projects: HashSet<PathBuf>,
    busy: Option<SharedString>,
    error: Option<String>,
    search_task: Option<Task<()>>,
    details_task: Option<Task<()>>,
    updates_task: Option<Task<()>>,
    list_scroll: ScrollHandle,
    details_scroll: ScrollHandle,
    _subscriptions: Vec<Subscription>,
}

/// Opens the manager for a project (or the solution), reusing an open one.
pub fn open(workspace: &mut Workspace, model: Entity<DotnetModel>, scope: Option<PathBuf>, window: &mut Window, cx: &mut gpui::Context<Workspace>) {
    let existing = workspace.items_of_type::<NuGetManager>(cx).find(|m| m.read(cx).scope == scope);
    if let Some(existing) = existing {
        workspace.activate_item(&existing, true, true, window, cx);
        return;
    }
    let weak = workspace.weak_handle();
    let manager = cx.new(|cx| NuGetManager::new(weak, model, scope, window, cx));
    workspace.add_item_to_active_pane(Box::new(manager), None, true, window, cx);
}

impl NuGetManager {
    fn new(workspace: WeakEntity<Workspace>, model: Entity<DotnetModel>, scope: Option<PathBuf>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Search packages", window, cx);
            editor
        });
        let search_subscription = cx.subscribe_in(&search, window, |this, _, event, window, cx| {
            if let EditorEvent::BufferEdited = event {
                if this.tab != Tab::Browse {
                    this.tab = Tab::Browse;
                }
                this.search(false, window, cx);
            }
        });
        let model_subscription = cx.subscribe_in(&model, window, |this, _, event, window, cx| match event {
            ModelEvent::Changed => {
                this.sources = this.load_sources(cx);
                this.check_updates(window, cx);
                cx.notify();
            }
        });
        let config = config::get(cx);
        let mut manager = Self {
            workspace,
            model,
            scope,
            focus_handle: cx.focus_handle(),
            search,
            tab: Tab::Installed,
            prerelease: config.nuget.include_prerelease,
            sources: Vec::new(),
            results: Vec::new(),
            search_errors: Vec::new(),
            searching: false,
            more_available: false,
            latest: HashMap::new(),
            checking_updates: false,
            selected: None,
            details: None,
            versions: Vec::new(),
            chosen_version: None,
            chosen_projects: HashSet::new(),
            busy: None,
            error: None,
            search_task: None,
            details_task: None,
            updates_task: None,
            list_scroll: ScrollHandle::new(),
            details_scroll: ScrollHandle::new(),
            _subscriptions: vec![search_subscription, model_subscription],
        };
        manager.sources = manager.load_sources(cx);
        manager.check_updates(window, cx);
        manager.search(false, window, cx);
        manager
    }

    fn title(&self, cx: &App) -> String {
        match &self.scope {
            Some(project) => paths::file_stem(project),
            None => self.model.read(cx).solution.as_ref().map(|s| s.name()).unwrap_or_else(|| "Solution".into()),
        }
    }

    fn base_dir(&self, cx: &App) -> PathBuf {
        match &self.scope {
            Some(project) => project.parent().unwrap_or(Path::new(".")).to_path_buf(),
            None => self.model.read(cx).solution.as_ref().map(|s| s.dir().to_path_buf()).unwrap_or_default(),
        }
    }

    fn load_sources(&self, cx: &App) -> Vec<PackageSource> {
        dotnet_model::nuget::sources_for(&self.base_dir(cx))
    }

    /// The projects in scope that can take package references.
    fn projects(&self, cx: &App) -> Vec<MsBuildProject> {
        let model = self.model.read(cx);
        match &self.scope {
            Some(path) => model.project(path).cloned().into_iter().collect(),
            None => model.loaded_projects().into_iter().filter(|p| p.is_sdk() || !p.package_references.is_empty()).collect(),
        }
    }

    fn installed(&self, cx: &App) -> Vec<InstalledPackage> {
        solution_packages::installed(&self.projects(cx))
    }

    /// Looks up the newest version of every installed package.
    fn check_updates(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(client) = crate::fetch::client(cx) else { return };
        let names: Vec<String> = self.installed(cx).into_iter().map(|p| p.name).collect();
        let sources = self.sources.clone();
        let prerelease = self.prerelease;
        self.checking_updates = true;
        self.updates_task = Some(cx.spawn_in(window, async move |this, cx| {
            let lookups = names.iter().map(|name| {
                let client = client.clone();
                let sources = sources.clone();
                async move { (name.to_lowercase(), client.versions(&sources, name, prerelease).await.into_iter().next()) }
            });
            let found = futures::future::join_all(lookups).await;
            this.update(cx, |this, cx| {
                this.latest = found.into_iter().filter_map(|(name, latest)| latest.map(|l| (name, l))).collect();
                this.checking_updates = false;
                cx.notify();
            })
            .ok();
        }));
    }

    fn search(&mut self, more: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(client) = crate::fetch::client(cx) else { return };
        let query = self.search.read(cx).text(cx).trim().to_string();
        let sources = self.sources.clone();
        let prerelease = self.prerelease;
        let skip = if more { self.results.len() } else { 0 };
        self.searching = true;
        cx.notify();
        self.search_task = Some(cx.spawn_in(window, async move |this, cx| {
            if !more {
                cx.background_executor().timer(Duration::from_millis(300)).await;
            }
            let (results, errors) = client.search(&sources, &query, prerelease, skip, 30).await;
            this.update(cx, |this, cx| {
                this.more_available = results.len() >= 30;
                if more {
                    for result in results {
                        if !this.results.iter().any(|r| r.id.eq_ignore_ascii_case(&result.id)) {
                            this.results.push(result);
                        }
                    }
                } else {
                    this.results = results;
                }
                this.search_errors = errors;
                this.searching = false;
                cx.notify();
            })
            .ok();
        }));
    }

    fn rows(&self, cx: &App) -> Vec<Row> {
        let installed = self.installed(cx);
        let installed_row = |package: &InstalledPackage| {
            let info = self.results.iter().find(|r| r.id.eq_ignore_ascii_case(&package.name));
            Row {
                id: package.name.clone(),
                installed: package.versions(),
                latest: self.latest.get(&package.name.to_lowercase()).cloned(),
                description: info.map(|i| i.description.clone()).unwrap_or_default(),
                downloads: info.map(|i| i.total_downloads),
                verified: info.is_some_and(|i| i.verified),
            }
        };
        let filter = self.search.read(cx).text(cx).trim().to_lowercase();
        let matches = |id: &str| filter.is_empty() || id.to_lowercase().contains(&filter);
        match self.tab {
            Tab::Browse => self
                .results
                .iter()
                .map(|info| {
                    let package = installed.iter().find(|p| p.name.eq_ignore_ascii_case(&info.id));
                    Row {
                        id: info.id.clone(),
                        installed: package.map(|p| p.versions()).unwrap_or_default(),
                        latest: Some(info.version.clone()),
                        description: info.description.clone(),
                        downloads: Some(info.total_downloads),
                        verified: info.verified,
                    }
                })
                .collect(),
            Tab::Installed => installed.iter().filter(|p| matches(&p.name)).map(installed_row).collect(),
            Tab::Updates => installed
                .iter()
                .filter(|p| matches(&p.name))
                .filter(|p| self.latest.get(&p.name.to_lowercase()).is_some_and(|latest| solution_packages::has_update(p, latest)))
                .map(installed_row)
                .collect(),
            Tab::Consolidate => installed.iter().filter(|p| matches(&p.name) && p.needs_consolidation()).map(installed_row).collect(),
        }
    }

    fn select(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected.as_ref() == Some(&id) {
            return;
        }
        self.selected = Some(id.clone());
        self.details = self.results.iter().find(|r| r.id.eq_ignore_ascii_case(&id)).cloned();
        self.versions.clear();
        self.chosen_version = None;
        // In a solution, actions apply to the projects that have the package, or all.
        let installed = self.installed(cx).into_iter().find(|p| p.name.eq_ignore_ascii_case(&id));
        self.chosen_projects = match (&installed, self.tab) {
            (Some(package), Tab::Updates | Tab::Consolidate | Tab::Installed) => package.usages.iter().map(|u| u.project.clone()).collect(),
            _ => HashSet::new(),
        };
        let Some(client) = crate::fetch::client(cx) else { return };
        let sources = self.sources.clone();
        let prerelease = self.prerelease;
        let need_details = self.details.is_none();
        self.details_task = Some(cx.spawn_in(window, async move |this, cx| {
            let versions = client.versions(&sources, &id, prerelease).await;
            let details = if need_details { client.package(&sources, &id, true).await } else { None };
            this.update(cx, |this, cx| {
                if this.selected.as_ref() != Some(&id) {
                    return;
                }
                let versions = if versions.is_empty() { details.as_ref().map(|d| version::sort_desc(d.versions.clone(), prerelease)).unwrap_or_default() } else { versions };
                this.chosen_version = versions.first().cloned();
                this.versions = versions;
                if need_details {
                    this.details = details;
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn choose_version(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let versions = self.versions.clone();
        if versions.is_empty() {
            return;
        }
        let installed: Vec<String> = self.selected.as_ref().and_then(|id| self.installed(cx).into_iter().find(|p| p.name.eq_ignore_ascii_case(id))).map(|p| p.versions()).unwrap_or_default();
        let choices = versions
            .iter()
            .enumerate()
            .map(|(i, v)| {
                let mut choice = Choice::new(v.clone());
                if installed.contains(v) {
                    choice = choice.detail("installed");
                } else if i == 0 {
                    choice = choice.detail("latest");
                }
                choice
            })
            .collect();
        let this = cx.entity().downgrade();
        pick::defer_workspace(self.workspace.clone(), window, cx, move |workspace, window, cx| {
            pick::pick(workspace, "Version", choices, window, cx, move |ix, _, cx| {
                let version = versions[ix].clone();
                this.update(cx, |this, cx| {
                    this.chosen_version = Some(version);
                    cx.notify();
                })
                .ok();
            });
        });
    }

    /// Installs, updates or removes the selected package in the chosen projects.
    fn apply(&mut self, uninstall: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.selected.clone() else { return };
        let version = self.chosen_version.clone();
        let projects: Vec<MsBuildProject> = self.projects(cx).into_iter().filter(|p| self.scope.is_some() || self.chosen_projects.contains(&p.path)).collect();
        if projects.is_empty() {
            self.error = Some("Choose the projects first.".into());
            cx.notify();
            return;
        }
        if !uninstall && version.is_none() {
            return;
        }
        let options = config::get(cx).eval_options();
        self.busy = Some(if uninstall { format!("Removing {id}…") } else { format!("Installing {id} {}…", version.clone().unwrap_or_default()) }.into());
        self.error = None;
        cx.notify();
        let job = cx.background_spawn(async move {
            let mut touched: Vec<PathBuf> = Vec::new();
            // One central version file is shared; edit it once per run.
            for project in &projects {
                // Re-read: an earlier edit may have changed Directory.Packages.props.
                let project = dotnet_model::evaluate(&project.path, &options)?;
                if uninstall {
                    if project.package(&id).is_some() {
                        edit::remove_package(&project, &id)?;
                        touched.push(project.path.clone());
                    }
                } else {
                    edit::add_package(&project, &id, version.as_deref().unwrap())?;
                    touched.push(project.path.clone());
                }
            }
            for project in &touched {
                crate::restore::restore(project).await;
            }
            anyhow::Ok(touched)
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = job.await;
            this.update(cx, |this, cx| {
                this.busy = None;
                let touched = match result {
                    Ok(touched) => touched,
                    Err(error) => {
                        this.error = Some(format!("{error:#}"));
                        Vec::new()
                    }
                };
                this.model.update(cx, |model, cx| model.reload_paths(&touched, cx));
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Updates every package with a newer version, in every project that uses it.
    fn update_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let updates: Vec<(String, String)> = self
            .installed(cx)
            .into_iter()
            .filter_map(|p| {
                let latest = self.latest.get(&p.name.to_lowercase())?.clone();
                solution_packages::has_update(&p, &latest).then_some((p.name, latest))
            })
            .collect();
        if updates.is_empty() {
            return;
        }
        let projects: Vec<PathBuf> = self.projects(cx).into_iter().map(|p| p.path).collect();
        let options = config::get(cx).eval_options();
        self.busy = Some(format!("Updating {} packages…", updates.len()).into());
        self.error = None;
        cx.notify();
        let job = cx.background_spawn(async move {
            let mut touched = Vec::new();
            for (name, latest) in &updates {
                for path in &projects {
                    let project = dotnet_model::evaluate(path, &options)?;
                    if project.package(name).and_then(|p| p.version.as_deref()).is_some_and(|v| version::is_newer(latest, v)) {
                        edit::set_package_version(&project, name, latest)?;
                        if !touched.contains(path) {
                            touched.push(path.clone());
                        }
                    }
                }
            }
            for project in &touched {
                crate::restore::restore(project).await;
            }
            anyhow::Ok(touched)
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = job.await;
            this.update(cx, |this, cx| {
                this.busy = None;
                let touched = match result {
                    Ok(touched) => touched,
                    Err(error) => {
                        this.error = Some(format!("{error:#}"));
                        Vec::new()
                    }
                };
                this.model.update(cx, |model, cx| model.reload_paths(&touched, cx));
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn render_tabs(&self, cx: &mut Context<Self>) -> impl gpui::IntoElement {
        let tabs: Vec<Tab> = if self.scope.is_none() { vec![Tab::Browse, Tab::Installed, Tab::Updates, Tab::Consolidate] } else { vec![Tab::Browse, Tab::Installed, Tab::Updates] };
        let installed = self.installed(cx);
        let updates = installed.iter().filter(|p| self.latest.get(&p.name.to_lowercase()).is_some_and(|l| solution_packages::has_update(p, l))).count();
        let consolidate = installed.iter().filter(|p| p.needs_consolidation()).count();
        h_flex().gap_1().children(tabs.into_iter().map(|tab| {
            let count = match tab {
                Tab::Updates if updates > 0 => Some(updates),
                Tab::Consolidate if consolidate > 0 => Some(consolidate),
                Tab::Installed => Some(installed.len()),
                _ => None,
            };
            let label = match count {
                Some(n) => format!("{} {n}", tab.label()),
                None => tab.label().to_string(),
            };
            Button::new(SharedString::from(format!("tab-{}", tab.label())), label)
                .style(ButtonStyle::Subtle)
                .size(ButtonSize::Compact)
                .toggle_state(self.tab == tab)
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.tab = tab;
                    this.selected = None;
                    this.details = None;
                    if tab == Tab::Browse && this.results.is_empty() {
                        this.search(false, window, cx);
                    }
                    cx.notify();
                }))
        }))
    }

    fn render_row(&self, row: &Row, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors();
        let selected = self.selected.as_ref().is_some_and(|s| s.eq_ignore_ascii_case(&row.id));
        let id = row.id.clone();
        let has_update = match (&row.latest, row.installed.first()) {
            (Some(latest), Some(installed)) => version::is_newer(latest, installed),
            _ => false,
        };
        let version_label = match (row.installed.as_slice(), &row.latest) {
            ([], Some(latest)) => latest.clone(),
            ([one], Some(latest)) if has_update => format!("{one} → {latest}"),
            ([one], _) => one.clone(),
            (many, _) if !many.is_empty() => many.join(", "),
            _ => String::new(),
        };
        v_flex()
            .id(("package", ix))
            .px_3()
            .py_1p5()
            .gap_0p5()
            .border_b_1()
            .border_color(colors.border_variant)
            .cursor_pointer()
            .when(selected, |el| el.bg(colors.element_selected))
            .hover(|el| el.bg(colors.element_hover))
            .on_click(cx.listener(move |this, _, window, cx| this.select(id.clone(), window, cx)))
            .child(
                h_flex()
                    .gap_2()
                    .justify_between()
                    .child(
                        h_flex()
                            .gap_1p5()
                            .min_w_0()
                            .child(Icon::from_path("icons/forge_nuget.svg").size(IconSize::Small).color(if row.installed.is_empty() { Color::Muted } else { Color::Accent }))
                            .child(Label::new(row.id.clone()).weight(FontWeight::SEMIBOLD).single_line())
                            .when(row.verified, |el| el.child(Icon::new(IconName::Check).size(IconSize::XSmall).color(Color::Accent)))
                            .children(row.downloads.filter(|d| *d > 0).map(|d| Label::new(format_downloads(d)).size(LabelSize::XSmall).color(Color::Muted))),
                    )
                    .child(Label::new(version_label).size(LabelSize::Small).color(if has_update { Color::Accent } else { Color::Muted })),
            )
            .when(!row.description.is_empty(), |el| {
                el.child(Label::new(row.description.lines().next().unwrap_or("").to_string()).size(LabelSize::Small).color(Color::Muted).single_line())
            })
            .into_any_element()
    }

    fn render_details(&self, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors();
        let Some(id) = self.selected.clone() else {
            return v_flex().p_4().child(Label::new("Select a package to see its details.").color(Color::Muted)).into_any_element();
        };
        let installed = self.installed(cx).into_iter().find(|p| p.name.eq_ignore_ascii_case(&id));
        let details = self.details.clone();
        let chosen = self.chosen_version.clone();
        let busy = self.busy.is_some();
        let solution_scope = self.scope.is_none();

        let in_scope_version = installed.as_ref().and_then(|p| if solution_scope { p.highest() } else { p.usages.first().and_then(|u| u.version.clone()) });
        let primary_label = match (&in_scope_version, &chosen) {
            (None, _) => "Install",
            (Some(current), Some(chosen)) if version::is_newer(chosen, current) => "Update",
            (Some(current), Some(chosen)) if version::compare(chosen, current) == std::cmp::Ordering::Less => "Downgrade",
            (Some(_), _) => if solution_scope { "Install" } else { "Installed" },
        };
        let primary_disabled = busy || chosen.is_none() || (!solution_scope && in_scope_version == chosen);

        let mut column = v_flex()
            .p_4()
            .gap_3()
            .child(
                h_flex()
                    .gap_2()
                    .child(Icon::from_path("icons/forge_nuget.svg").size(IconSize::Medium).color(Color::Accent))
                    .child(Label::new(id.clone()).size(LabelSize::Large).weight(FontWeight::BOLD)),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(Label::new("Version").size(LabelSize::Small).color(Color::Muted))
                    .child(
                        Button::new("choose-version", chosen.clone().unwrap_or_else(|| "…".into()))
                            .style(ButtonStyle::Outlined)
                            .size(ButtonSize::Compact)
                            .end_icon(Icon::new(IconName::ChevronDown).size(IconSize::XSmall))
                            .disabled(self.versions.is_empty())
                            .on_click(cx.listener(|this, _, window, cx| this.choose_version(window, cx))),
                    )
                    .child(
                        Button::new("install", primary_label)
                            .style(ButtonStyle::Filled)
                            .size(ButtonSize::Compact)
                            .disabled(primary_disabled)
                            .on_click(cx.listener(|this, _, window, cx| this.apply(false, window, cx))),
                    )
                    .when(installed.is_some(), |row| {
                        row.child(
                            Button::new("uninstall", "Uninstall")
                                .style(ButtonStyle::Subtle)
                                .size(ButtonSize::Compact)
                                .disabled(busy)
                                .on_click(cx.listener(|this, _, window, cx| this.apply(true, window, cx))),
                        )
                    }),
            );

        if solution_scope {
            let projects = self.projects(cx);
            let all_chosen = !projects.is_empty() && projects.iter().all(|p| self.chosen_projects.contains(&p.path));
            let mut list = v_flex().gap_1().child(
                h_flex().justify_between().child(Label::new("Projects").size(LabelSize::Small).weight(FontWeight::SEMIBOLD)).child(
                    Checkbox::new("all-projects", if all_chosen { ToggleState::Selected } else { ToggleState::Unselected })
                        .label("All")
                        .label_size(LabelSize::Small)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let projects = this.projects(cx);
                            if all_chosen {
                                this.chosen_projects.clear();
                            } else {
                                this.chosen_projects = projects.into_iter().map(|p| p.path).collect();
                            }
                            cx.notify();
                        })),
                ),
            );
            for (ix, project) in projects.iter().enumerate() {
                let path = project.path.clone();
                let usage = installed.as_ref().and_then(|p| p.usage(&project.path)).and_then(|u| u.version.clone());
                let checked = self.chosen_projects.contains(&project.path);
                list = list.child(
                    h_flex()
                        .justify_between()
                        .child(
                            Checkbox::new(("project", ix), if checked { ToggleState::Selected } else { ToggleState::Unselected })
                                .label(project.name())
                                .label_size(LabelSize::Small)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if !this.chosen_projects.remove(&path) {
                                        this.chosen_projects.insert(path.clone());
                                    }
                                    cx.notify();
                                })),
                        )
                        .child(Label::new(usage.unwrap_or_default()).size(LabelSize::Small).color(Color::Muted)),
                );
            }
            column = column.child(list);
        } else if let Some(package) = &installed
            && let Some(usage) = package.usages.first() {
                let source = match usage.source {
                    dotnet_model::msbuild::VersionSource::Central => "Directory.Packages.props",
                    dotnet_model::msbuild::VersionSource::Override => "VersionOverride",
                    dotnet_model::msbuild::VersionSource::Restored => "resolved at restore",
                    _ => "project",
                };
                column = column.child(
                    h_flex()
                        .gap_2()
                        .child(Label::new("Installed").size(LabelSize::Small).color(Color::Muted))
                        .child(Label::new(usage.version.clone().unwrap_or_else(|| "?".into())).size(LabelSize::Small))
                        .child(Label::new(format!("({source})")).size(LabelSize::XSmall).color(Color::Muted)),
                );
            }

        match details {
            Some(info) => {
                if !info.description.is_empty() {
                    column = column.child(Label::new(info.description.clone()).size(LabelSize::Small));
                }
                let mut facts = v_flex().gap_1();
                let fact = |name: &str, value: String| {
                    h_flex().gap_2().child(div().w(px(90.)).child(Label::new(name.to_string()).size(LabelSize::Small).color(Color::Muted))).child(Label::new(value).size(LabelSize::Small))
                };
                if !info.authors.is_empty() {
                    facts = facts.child(fact("Authors", info.authors.join(", ")));
                }
                if info.total_downloads > 0 {
                    facts = facts.child(fact("Downloads", format_downloads(info.total_downloads)));
                }
                facts = facts.child(fact("Latest", info.version.clone()));
                if !info.sources.is_empty() {
                    facts = facts.child(fact("Source", info.sources.join(", ")));
                }
                if !info.tags.is_empty() {
                    facts = facts.child(fact("Tags", info.tags.join(" ")));
                }
                column = column.child(facts);
                let links: Vec<(&str, String)> = [("Project", info.project_url.clone()), ("License", info.license_url.clone())].into_iter().filter_map(|(n, u)| u.map(|u| (n, u))).collect();
                column = column.child(h_flex().gap_2().children(links.into_iter().map(|(name, url)| {
                    Button::new(SharedString::from(format!("link-{name}")), name)
                        .style(ButtonStyle::Subtle)
                        .size(ButtonSize::Compact)
                        .end_icon(Icon::new(IconName::ArrowUpRight).size(IconSize::XSmall))
                        .on_click(move |_, _, cx| cx.open_url(&url))
                })));
                column = column.child(
                    Button::new("nuget-org", "nuget.org")
                        .style(ButtonStyle::Subtle)
                        .size(ButtonSize::Compact)
                        .end_icon(Icon::new(IconName::ArrowUpRight).size(IconSize::XSmall))
                        .on_click(move |_, _, cx| cx.open_url(&format!("https://www.nuget.org/packages/{id}"))),
                );
            }
            None => column = column.child(Label::new("Loading details…").size(LabelSize::Small).color(Color::Muted)),
        }

        div()
            .id("package-details")
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&self.details_scroll)
            .border_l_1()
            .border_color(colors.border)
            .child(column)
            .into_any_element()
    }
}

fn format_downloads(n: u64) -> String {
    match n {
        n if n >= 1_000_000_000 => format!("{:.1}B downloads", n as f64 / 1e9),
        n if n >= 1_000_000 => format!("{:.1}M downloads", n as f64 / 1e6),
        n if n >= 1_000 => format!("{:.1}K downloads", n as f64 / 1e3),
        n => format!("{n} downloads"),
    }
}

impl Render for NuGetManager {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl gpui::IntoElement {
        let colors = cx.theme().colors().clone();
        let rows = self.rows(cx);
        let title = self.title(cx);
        let busy = self.busy.clone();
        let loading = (self.tab == Tab::Browse && self.searching) || (self.tab != Tab::Browse && self.checking_updates);
        let source_names = self.sources.iter().map(|s| s.name.clone()).collect::<Vec<_>>().join(", ");

        let header = v_flex()
            .px_3()
            .py_2()
            .gap_2()
            .border_b_1()
            .border_color(colors.border)
            .child(
                h_flex()
                    .justify_between()
                    .child(
                        h_flex()
                            .gap_2()
                            .child(Icon::from_path("icons/forge_nuget.svg").color(Color::Accent))
                            .child(Label::new(format!("NuGet: {title}")).size(LabelSize::Large).weight(FontWeight::BOLD)),
                    )
                    .child(self.render_tabs(cx)),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .border_1()
                            .border_color(colors.border)
                            .bg(colors.editor_background)
                            .child(self.search.clone()),
                    )
                    .child(
                        Checkbox::new("prerelease", if self.prerelease { ToggleState::Selected } else { ToggleState::Unselected })
                            .label("Include prerelease")
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.prerelease = !this.prerelease;
                                this.check_updates(window, cx);
                                this.search(false, window, cx);
                                if let Some(id) = this.selected.take() {
                                    this.select(id, window, cx);
                                }
                            })),
                    )
                    .child(
                        IconButton::new("refresh", IconName::RotateCw)
                            .tooltip(Tooltip::text("Refresh (clears the cache)"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                if let Some(client) = crate::fetch::client(cx) {
                                    client.invalidate();
                                }
                                this.sources = this.load_sources(cx);
                                this.check_updates(window, cx);
                                this.search(false, window, cx);
                            })),
                    ),
            )
            .child(Label::new(format!("Sources: {source_names}")).size(LabelSize::XSmall).color(Color::Muted))
            .children(busy.map(|b| Label::new(b).size(LabelSize::Small).color(Color::Accent)))
            .children(self.error.clone().map(|e| Label::new(e).size(LabelSize::Small).color(Color::Error)))
            .children((!self.search_errors.is_empty() && self.tab == Tab::Browse).then(|| Label::new(self.search_errors.join("; ")).size(LabelSize::XSmall).color(Color::Warning)));

        let empty_message = match (self.tab, loading) {
            (_, true) => "Loading…",
            (Tab::Browse, false) => "No packages found.",
            (Tab::Installed, false) => "No packages installed.",
            (Tab::Updates, false) => "Everything is up to date.",
            (Tab::Consolidate, false) => "Every project uses the same versions.",
        };
        let show_more = self.tab == Tab::Browse && self.more_available && !self.searching;
        let can_update_all = self.tab == Tab::Updates && !rows.is_empty() && self.busy.is_none();
        let list = div()
            .id("package-list")
            .flex_1()
            .min_w_0()
            .h_full()
            .overflow_y_scroll()
            .track_scroll(&self.list_scroll)
            .when(can_update_all, |el| {
                el.child(
                    h_flex().px_3().py_1p5().border_b_1().border_color(colors.border_variant).child(
                        Button::new("update-all", "Update all")
                            .style(ButtonStyle::Filled)
                            .size(ButtonSize::Compact)
                            .on_click(cx.listener(|this, _, window, cx| this.update_all(window, cx))),
                    ),
                )
            })
            .when(rows.is_empty(), |el| el.child(div().p_4().child(Label::new(empty_message).color(Color::Muted))))
            .children(rows.iter().enumerate().map(|(ix, row)| self.render_row(row, ix, cx)))
            .when(show_more, |el| {
                el.child(
                    div().p_2().child(
                        Button::new("more", "Load more")
                            .style(ButtonStyle::Subtle)
                            .size(ButtonSize::Compact)
                            .on_click(cx.listener(|this, _, window, cx| this.search(true, window, cx))),
                    ),
                )
            });

        v_flex()
            .key_context("NuGetManager")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(colors.editor_background)
            .child(header)
            .child(h_flex().flex_1().min_h_0().items_start().child(list).child(div().w(px(380.)).h_full().flex_shrink_0().child(self.render_details(cx))))
    }
}

impl Focusable for NuGetManager {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.search.focus_handle(cx)
    }
}

impl EventEmitter<ItemEvent> for NuGetManager {}

impl Item for NuGetManager {
    type Event = ItemEvent;

    fn tab_content_text(&self, _detail: usize, cx: &App) -> SharedString {
        format!("NuGet: {}", self.title(cx)).into()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::from_path("icons/forge_nuget.svg"))
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}
