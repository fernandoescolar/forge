//! Context menus for each kind of node, with what vscode-solution-explorer offers there.

use dotnet_model::explorer::NodeKind;
use dotnet_model::msbuild::edit::Position;
use dotnet_model::solution::SolutionFormat;
use gpui::{Context, Entity, Window};
use ui::ContextMenu;

use super::*;

impl SolutionExplorer {
    pub(super) fn build_context_menu(&mut self, kind: &NodeKind, window: &mut Window, cx: &mut Context<Self>) -> Entity<ContextMenu> {
        let focus = self.focus_handle.clone();
        let entity = cx.entity();
        let model = self.model.read(cx);
        let format = model.solution.as_ref().map(|s| s.format);
        let several_solutions = model.solutions.len() > 1;
        let project = kind.project().and_then(|p| model.project(p)).cloned();
        let templates = project
            .as_ref()
            .map(|p| dotnet_model::templates::templates_for(&model.roots(cx), p.code_extension()))
            .unwrap_or_default();
        let is_fsharp = project.as_ref().is_some_and(|p| p.is_fsharp());
        let has_clipboard = self.clipboard.is_some();
        let usings_file = crate::config::get(cx).global_usings_file;
        let usings_label = format!("Move Usings to {}", std::path::Path::new(&usings_file).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or(usings_file));
        let kind = kind.clone();

        ContextMenu::build(window, cx, move |menu, _, _| {
            let menu = menu.context(focus.clone());
            let template_entries = |mut menu: ContextMenu, anchor: Option<Position>| {
                for template in &templates {
                    let entity = entity.clone();
                    let template = template.clone();
                    let label = match anchor {
                        Some(Position::Before) => format!("New {} Above…", template.name),
                        Some(Position::After) => format!("New {} Below…", template.name),
                        None => format!("New {}…", template.name),
                    };
                    menu = menu.entry(label, None, move |window, cx| {
                        let template = template.clone();
                        entity.update(cx, |this, cx| this.new_file_with(Some(template), anchor, window, cx));
                    });
                }
                menu
            };
            let build_entries = |menu: ContextMenu| {
                menu.action("Build", Box::new(Build))
                    .action("Rebuild", Box::new(Rebuild))
                    .action("Clean", Box::new(Clean))
                    .action("Restore", Box::new(Restore))
            };
            let location_entries = |menu: ContextMenu| {
                menu.separator()
                    .action("Reveal in Finder", Box::new(RevealInFinder))
                    .action("Open in Terminal", Box::new(OpenInTerminal))
                    .action("Copy Path", Box::new(CopyPath))
                    .action("Copy Relative Path", Box::new(CopyRelativePath))
            };
            let clipboard_entries = |menu: ContextMenu, with_cut_copy: bool| {
                let menu = if with_cut_copy {
                    menu.separator().action("Cut", Box::new(Cut)).action("Copy", Box::new(Copy)).action("Duplicate", Box::new(Duplicate))
                } else {
                    menu.separator()
                };
                if has_clipboard { menu.action("Paste", Box::new(Paste)) } else { menu }
            };

            match &kind {
                NodeKind::Solution => {
                    let real_solution = format != Some(SolutionFormat::Folder);
                    let menu = build_entries(menu).action("Test", Box::new(Test)).separator();
                    let menu = if real_solution {
                        menu.action("New Project…", Box::new(NewProject))
                            .action("Add Existing Project…", Box::new(AddExistingProject))
                            .action("New Solution Folder", Box::new(NewSolutionFolder))
                            .action("Add Existing File…", Box::new(AddSolutionItem))
                    } else {
                        menu.action("New Solution…", Box::new(NewSolution)).action("New Project…", Box::new(NewProject))
                    };
                    let menu = menu
                        .separator()
                        .action("Manage NuGet Packages…", Box::new(ManagePackages))
                        .action("Centralize Package Versions", Box::new(CentralizePackageVersions))
                        .action("Customize File Templates", Box::new(CustomizeFileTemplates))
                        .when(several_solutions, |menu| menu.action("Show Another Solution…", Box::new(SelectSolution)));
                    let menu = if real_solution { menu.separator().action("Rename", Box::new(Rename)).action("Open Solution File", Box::new(Open)) } else { menu };
                    location_entries(menu)
                }
                NodeKind::SolutionFolder { .. } => menu
                    .action("New Project…", Box::new(NewProject))
                    .action("Add Existing Project…", Box::new(AddExistingProject))
                    .action("New Solution Folder", Box::new(NewSolutionFolder))
                    .action("Add Existing File…", Box::new(AddSolutionItem))
                    .separator()
                    .action("Rename", Box::new(Rename))
                    .action("Remove", Box::new(Delete)),
                NodeKind::SolutionItem { .. } => {
                    location_entries(menu.action("Open", Box::new(Open)).action("Remove from Solution", Box::new(RemoveFromSolution)))
                }
                NodeKind::Project { .. } => {
                    let runnable = project.as_ref().is_some_and(|p| p.is_runnable());
                    let testable = project.as_ref().is_some_and(|p| p.is_test_project());
                    let uses_ef = project.as_ref().is_some_and(super::tools::uses_ef);
                    let menu = build_entries(menu)
                        .when(runnable, |menu| menu.action("Run", Box::new(Run)).action("Watch", Box::new(Watch)))
                        .when(testable, |menu| menu.action("Test", Box::new(Test)))
                        .action("Pack", Box::new(Pack))
                        .action("Publish", Box::new(Publish))
                        .separator()
                        .action("New File…", Box::new(NewFile));
                    let menu = template_entries(menu, None)
                        .action("New Folder", Box::new(NewFolder))
                        .separator()
                        .action("Add Project Reference…", Box::new(AddProjectReference))
                        .action("Manage NuGet Packages…", Box::new(ManagePackages))
                        .action("Manage User Secrets", Box::new(ManageUserSecrets))
                        .when(!is_fsharp, |menu| menu.action(usings_label.clone(), Box::new(MoveUsingsToGlobalUsings)))
                        .when(uses_ef, |menu| {
                            menu.separator()
                                .action("Add Migration…", Box::new(AddMigration))
                                .action("Update Database", Box::new(UpdateDatabase))
                                .action("Remove Last Migration", Box::new(RemoveMigration))
                                .action("List Migrations", Box::new(ListMigrations))
                        })
                        .separator()
                        .action("Open Project File", Box::new(Open))
                        .action("Rename", Box::new(Rename))
                        .action("Remove from Solution", Box::new(RemoveFromSolution));
                    location_entries(clipboard_entries(menu, false))
                }
                NodeKind::ProjectError { .. } => menu
                    .action("Open Project File", Box::new(Open))
                    .action("Reload", Box::new(Refresh))
                    .action("Remove from Solution", Box::new(RemoveFromSolution)),
                NodeKind::Dependencies { .. } | NodeKind::Packages { .. } => {
                    menu.action("Manage NuGet Packages…", Box::new(ManagePackages)).action("Restore", Box::new(Restore))
                }
                NodeKind::Package { .. } => menu
                    .action("Update…", Box::new(UpdatePackage))
                    .action("Remove", Box::new(RemoveReference))
                    .separator()
                    .action("Manage NuGet Packages…", Box::new(ManagePackages)),
                NodeKind::ProjectReferences { .. } => menu.action("Add Project Reference…", Box::new(AddProjectReference)),
                NodeKind::ProjectReference { .. } => menu.action("Open Project File", Box::new(Open)).action("Remove", Box::new(RemoveReference)),
                NodeKind::Folder { is_link, .. } => {
                    let menu = template_entries(menu.action("New File…", Box::new(NewFile)), None).action("New Folder", Box::new(NewFolder));
                    let menu = clipboard_entries(menu, !is_link)
                        .separator()
                        .action("Rename", Box::new(Rename))
                        .action(if *is_link { "Remove from Project" } else { "Delete" }, Box::new(Delete));
                    location_entries(menu)
                }
                NodeKind::File { is_link, .. } => {
                    let menu = menu.action("Open", Box::new(Open));
                    let menu = if is_fsharp {
                        let menu = menu.separator().action("Move Up", Box::new(MoveUp)).action("Move Down", Box::new(MoveDown)).separator();
                        template_entries(template_entries(menu, Some(Position::Before)), Some(Position::After))
                    } else {
                        menu
                    };
                    let menu = clipboard_entries(menu, !is_link)
                        .separator()
                        .action("Rename", Box::new(Rename))
                        .action(if *is_link { "Remove from Project" } else { "Delete" }, Box::new(Delete));
                    location_entries(menu)
                }
                NodeKind::Frameworks { .. } | NodeKind::Framework { .. } | NodeKind::Assemblies { .. } | NodeKind::Assembly { .. } | NodeKind::PackageDependency { .. } => {
                    menu.action("Manage NuGet Packages…", Box::new(ManagePackages))
                }
            }
        })
    }

    /// The menu for several selected rows: what applies to all of them.
    pub(super) fn build_multi_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Entity<ContextMenu> {
        let focus = self.focus_handle.clone();
        let kinds = self.selected_kinds();
        let files = kinds.iter().filter(|k| matches!(k, NodeKind::File { .. } | NodeKind::Folder { .. })).count();
        let disk_files = kinds.iter().filter(|k| matches!(k, NodeKind::File { is_link: false, .. } | NodeKind::Folder { is_link: false, .. })).count();
        let in_solution = kinds.iter().filter(|k| matches!(k, NodeKind::Project { .. } | NodeKind::ProjectError { .. } | NodeKind::SolutionItem { .. })).count();
        let references = kinds.iter().filter(|k| matches!(k, NodeKind::Package { .. } | NodeKind::ProjectReference { .. })).count();
        let removable = files + in_solution + references + kinds.iter().filter(|k| matches!(k, NodeKind::SolutionFolder { .. })).count();
        let count = kinds.len();
        ContextMenu::build(window, cx, move |menu, _, _| {
            menu.context(focus.clone())
                .header(format!("{count} selected"))
                .when(disk_files > 0, |menu| menu.action("Cut", Box::new(Cut)).action("Copy", Box::new(Copy)).separator())
                .when(in_solution > 0, |menu| menu.action("Remove from Solution", Box::new(RemoveFromSolution)))
                .when(references > 0, |menu| menu.action("Remove References", Box::new(RemoveReference)))
                .when(removable > 0, |menu| menu.action("Delete", Box::new(Delete)))
        })
    }
}
