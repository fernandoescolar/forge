//! Edits to project files that keep their formatting: items for files and folders,
//! package and project references, and versions wherever they are defined (the project,
//! `Directory.Packages.props`, `Directory.Build.props` or `packages.config`).

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};

use super::{Project, VersionSource};
use crate::paths;
use crate::xml_edit::{Element, NewElement, XmlText};

/// Reads, edits and writes back an XML file, leaving it untouched when nothing changed.
pub fn edit_xml_file(path: &Path, edit: impl FnOnce(&mut XmlText) -> Result<()>) -> Result<()> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut xml = XmlText::new(text.clone());
    edit(&mut xml)?;
    if xml.as_str() != text {
        std::fs::write(path, xml.as_str()).with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(())
}

fn same_include(a: &str, b: &str) -> bool {
    a.trim().replace('/', "\\").trim_end_matches('\\').eq_ignore_ascii_case(b.trim().replace('/', "\\").trim_end_matches('\\'))
}

fn starts_with_include(value: &str, prefix: &str) -> bool {
    let value = value.trim().replace('/', "\\").to_lowercase();
    let prefix = prefix.trim().replace('/', "\\").trim_end_matches('\\').to_lowercase();
    value == prefix || value.starts_with(&format!("{prefix}\\"))
}

fn root_children(xml: &XmlText) -> Result<Vec<Element>> {
    let root = xml.root()?;
    xml.children(root.start())
}

/// The `ItemGroup` to add an element to: the first unconditioned one already holding
/// `element_name` items, else a new one after the last `ItemGroup`.
fn item_group_for(xml: &mut XmlText, element_name: &str) -> Result<usize> {
    let elements = xml.elements()?;
    let root = elements.first().context("empty project")?.start();
    let groups: Vec<&Element> = elements.iter().filter(|e| e.name == "ItemGroup" && e.parent() == Some(root)).collect();
    if let Some(group) = groups
        .iter()
        .find(|g| g.attr("Condition").is_none() && elements.iter().any(|e| e.parent() == Some(g.start()) && e.name == element_name))
    {
        return Ok(group.start());
    }
    let new = NewElement::new("ItemGroup");
    if let Some(last) = groups.last() {
        xml.insert_after(last.start(), &new)
    } else if let Some(last) = root_children(xml)?.last() {
        xml.insert_after(last.start(), &new)
    } else {
        xml.append_child(root, &new)
    }
}

/// Adds `new` among a group's `name` children, keeping them sorted when they already are.
fn insert_sorted(xml: &mut XmlText, group: usize, name: &str, new: &NewElement, key: &str) -> Result<()> {
    let siblings: Vec<Element> = xml.children(group)?.into_iter().filter(|e| e.name == name).collect();
    let keys: Vec<String> = siblings.iter().map(|e| e.attr("Include").unwrap_or("").to_lowercase()).collect();
    let sorted = keys.windows(2).all(|w| w[0] <= w[1]);
    if sorted
        && let Some(next) = siblings.iter().zip(&keys).find(|(_, k)| k.as_str() > key.to_lowercase().as_str()).map(|(e, _)| e) {
            xml.insert_before(next.start(), new)?;
            return Ok(());
        }
    xml.append_child(group, new)?;
    Ok(())
}

fn find_item<'a>(elements: &'a [Element], name: &str, include: &str) -> Option<&'a Element> {
    elements.iter().find(|e| e.name == name && e.attr("Include").is_some_and(|i| same_include(i, include)))
}

/// Where a new file goes relative to an existing one, for F# compile order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Position {
    Before,
    After,
}

/// Adds an item for a new file unless the project already includes it (SDK globs).
pub fn add_file(project: &Project, file: &Path, item_type: &str, anchor: Option<(&Path, Position)>) -> Result<()> {
    let relative = paths::relative(project.dir(), file).context("file outside the project")?;
    let include = paths::to_msbuild(&relative);
    let covered = project.is_sdk() && !project.is_fsharp() && {
        let entries = super::items::entries(project, &Default::default());
        entries.iter().any(|e| e.full == file && e.item_types.iter().any(|t| t.eq_ignore_ascii_case(item_type)))
    };
    let parent_folder = relative.parent().filter(|p| !p.as_os_str().is_empty()).map(paths::to_msbuild);
    let items_file = project.items_file.clone().unwrap_or_else(|| project.path.clone());
    edit_xml_file(&items_file, |xml| {
        if let Some(folder) = &parent_folder {
            remove_folder_item(xml, folder)?;
        }
        if covered {
            return Ok(());
        }
        let elements = xml.elements()?;
        if find_item(&elements, item_type, &include).is_some() {
            return Ok(());
        }
        let include = if project.is_shared() { format!("$(MSBuildThisFileDirectory){include}") } else { include.clone() };
        let new = NewElement::new(item_type).attr("Include", &include);
        if let Some((anchor, position)) = anchor {
            let anchor = paths::to_msbuild(&paths::relative(project.dir(), anchor).context("anchor outside the project")?);
            if let Some(anchor) = elements.iter().find(|e| e.attr("Include").is_some_and(|i| same_include(i, &anchor))) {
                match position {
                    Position::Before => xml.insert_before(anchor.start(), &new)?,
                    Position::After => xml.insert_after(anchor.start(), &new)?,
                };
                return Ok(());
            }
        }
        let group = item_group_for(xml, item_type)?;
        xml.append_child(group, &new)?;
        Ok(())
    })
}

fn remove_folder_item(xml: &mut XmlText, folder: &str) -> Result<()> {
    let elements = xml.elements()?;
    if let Some(item) = find_item(&elements, "Folder", folder) {
        let group = item.parent();
        xml.remove_element(item.start())?;
        if let Some(group) = group {
            let group = xml.elements()?.into_iter().find(|e| e.name == "ItemGroup" && e.start() == group);
            if let Some(group) = group {
                xml.remove_if_empty(group.start())?;
            }
        }
    }
    Ok(())
}

/// Records an empty folder in projects that do not show folders on their own.
pub fn add_folder(project: &Project, folder: &Path) -> Result<()> {
    if project.is_sdk() && !project.is_fsharp() {
        return Ok(());
    }
    let relative = paths::to_msbuild(&paths::relative(project.dir(), folder).context("folder outside the project")?);
    edit_xml_file(&project.path, |xml| {
        let elements = xml.elements()?;
        if find_item(&elements, "Folder", &relative).is_some() {
            return Ok(());
        }
        let group = item_group_for(xml, "Folder")?;
        xml.append_child(group, &NewElement::new("Folder").attr("Include", format!("{relative}\\")))?;
        Ok(())
    })
}

/// Removes the items for a deleted file or folder (and everything under it).
pub fn remove_path(project: &Project, path: &Path) -> Result<()> {
    let relative = paths::to_msbuild(&paths::relative(project.dir(), path).context("path outside the project")?);
    let items_file = project.items_file.clone().unwrap_or_else(|| project.path.clone());
    edit_xml_file(&items_file, |xml| {
        loop {
            let elements = xml.elements()?;
            let item = elements.iter().find(|e| {
                e.depth == 2 && ["Include", "Update", "Remove"].iter().any(|a| e.attr(a).is_some_and(|v| starts_with_include(&strip_this_dir(v), &relative)))
            });
            let Some(item) = item else { break };
            let group = item.parent();
            xml.remove_element(item.start())?;
            if let Some(group) = group {
                xml.remove_if_empty(group)?;
            }
        }
        Ok(())
    })?;
    // Legacy projects only show folders that have items.
    if !project.is_sdk()
        && let Some(parent) = path.parent().filter(|p| *p != project.dir() && p.exists()) {
            let reloaded = super::evaluate(&project.path, &Default::default())?;
            let still_shown = super::items::entries(&reloaded, &Default::default()).iter().any(|e| e.full.starts_with(parent) && e.full != parent);
            if !still_shown {
                add_folder(&reloaded, parent)?;
            }
        }
    Ok(())
}

fn strip_this_dir(value: &str) -> String {
    value.trim_start_matches("$(MSBuildThisFileDirectory)").to_string()
}

/// Rewrites items after a file or folder was renamed or moved on disk.
pub fn rename_path(project: &Project, from: &Path, to: &Path) -> Result<()> {
    let from_rel = paths::to_msbuild(&paths::relative(project.dir(), from).context("path outside the project")?);
    let to_rel = paths::to_msbuild(&paths::relative(project.dir(), to).context("path outside the project")?);
    let from_name = paths::file_name(from);
    let to_name = paths::file_name(to);
    let items_file = project.items_file.clone().unwrap_or_else(|| project.path.clone());
    edit_xml_file(&items_file, |xml| {
        let elements = xml.elements()?;
        for element in elements.iter().rev() {
            if element.depth == 2 {
                for attr in ["Include", "Update", "Remove"] {
                    let Some(value) = element.attr(attr) else { continue };
                    let prefix = if value.starts_with("$(MSBuildThisFileDirectory)") { "$(MSBuildThisFileDirectory)" } else { "" };
                    let bare = strip_this_dir(value);
                    if starts_with_include(&bare, &from_rel) {
                        let normalized = bare.replace('/', "\\");
                        let rest = &normalized[from_rel.len().min(normalized.len())..];
                        xml.set_attribute(element.start(), attr, &format!("{prefix}{to_rel}{rest}"))?;
                    }
                }
            }
            if element.name == "DependentUpon" && element.text.as_deref().is_some_and(|t| t.eq_ignore_ascii_case(&from_name)) {
                xml.set_text(element.start(), &to_name)?;
            }
        }
        Ok(())
    })
}

/// Moves a compile item one place up or down among its siblings (F# compile order).
pub fn move_item(project: &Project, file: &Path, position: Position) -> Result<()> {
    let relative = paths::to_msbuild(&paths::relative(project.dir(), file).context("file outside the project")?);
    edit_xml_file(&project.path, |xml| {
        let elements = xml.elements()?;
        let item = elements.iter().find(|e| e.depth == 2 && e.attr("Include").is_some_and(|i| same_include(i, &relative))).context("the file has no item")?;
        let siblings: Vec<&Element> = elements.iter().filter(|e| e.parent() == item.parent() && e.attr("Include").is_some()).collect();
        let index = siblings.iter().position(|e| e.start() == item.start()).unwrap();
        let other = match position {
            Position::Before if index > 0 => siblings[index - 1],
            Position::After if index + 1 < siblings.len() => siblings[index + 1],
            _ => return Ok(()),
        };
        let item_text = xml.as_str()[item.range.clone()].to_string();
        let other_text = xml.as_str()[other.range.clone()].to_string();
        let (first, second) = if item.start() < other.start() { (item, other) } else { (other, item) };
        let mut text = xml.as_str().to_string();
        let (first_new, second_new) = if first.start() == item.start() { (&other_text, &item_text) } else { (&item_text, &other_text) };
        text.replace_range(second.range.clone(), second_new);
        text.replace_range(first.range.clone(), first_new);
        *xml = XmlText::new(text);
        Ok(())
    })
}

/// Sets a package's version where it is defined.
pub fn set_package_version(project: &Project, name: &str, version: &str) -> Result<()> {
    let reference = project.package(name).with_context(|| format!("{name} is not referenced by {}", project.name()))?;
    if paths::file_name(&reference.defined_in).eq_ignore_ascii_case("packages.config") {
        return edit_xml_file(&reference.defined_in, |xml| {
            let elements = xml.elements()?;
            let package = elements.iter().find(|e| e.name == "package" && e.attr("id").is_some_and(|i| i.eq_ignore_ascii_case(name))).context("package not found")?;
            xml.set_attribute(package.start(), "version", version)
        });
    }
    match reference.version_source {
        VersionSource::Central => {
            let file = reference.version_defined_in.clone().context("central version file unknown")?;
            set_central_version(&file, name, version)
        }
        VersionSource::Override => set_reference_metadata(&reference.defined_in, "PackageReference", name, "VersionOverride", version),
        VersionSource::Inline => {
            let file = reference.version_defined_in.clone().unwrap_or_else(|| reference.defined_in.clone());
            set_reference_metadata(&file, "PackageReference", name, "Version", version)
        }
        VersionSource::Restored | VersionSource::Unknown => {
            if project.uses_central_packages() {
                let file = project.central_packages_file.clone().context("no Directory.Packages.props")?;
                set_central_version(&file, name, version)
            } else {
                set_reference_metadata(&reference.defined_in, "PackageReference", name, "Version", version)
            }
        }
    }
}

/// Sets `Version` (or another metadata) on an item, as an attribute or a child element,
/// whichever the item already uses.
fn set_reference_metadata(file: &Path, item: &str, name: &str, metadata: &str, value: &str) -> Result<()> {
    edit_xml_file(file, |xml| {
        let elements = xml.elements()?;
        let element = elements
            .iter()
            .find(|e| e.name == item && (e.attr("Include").or(e.attr("Update"))).is_some_and(|i| i.eq_ignore_ascii_case(name)))
            .with_context(|| format!("{name} not found in {}", file.display()))?;
        if let Some(child) = elements.iter().find(|e| e.parent() == Some(element.start()) && e.name.eq_ignore_ascii_case(metadata)) {
            xml.set_text(child.start(), value)
        } else {
            xml.set_attribute(element.start(), metadata, value)
        }
    })
}

/// Adds or updates a `PackageVersion` in a `Directory.Packages.props`.
pub fn set_central_version(file: &Path, name: &str, version: &str) -> Result<()> {
    edit_xml_file(file, |xml| {
        let elements = xml.elements()?;
        if let Some(existing) = elements.iter().find(|e| e.name == "PackageVersion" && e.attr("Include").is_some_and(|i| i.eq_ignore_ascii_case(name))) {
            return xml.set_attribute(existing.start(), "Version", version);
        }
        let group = item_group_for(xml, "PackageVersion")?;
        insert_sorted(xml, group, "PackageVersion", &NewElement::new("PackageVersion").attr("Include", name).attr("Version", version), name)
    })
}

/// References a package from a project; with central package management the version
/// goes to `Directory.Packages.props`.
pub fn add_package(project: &Project, name: &str, version: &str) -> Result<()> {
    if project.package(name).is_some() {
        return set_package_version(project, name, version);
    }
    if !project.is_sdk() && project.dir().join("packages.config").exists() {
        bail!("{} uses packages.config; migrate it to PackageReference to manage packages here", project.name());
    }
    let central = project.uses_central_packages().then(|| project.central_packages_file.clone()).flatten();
    if let Some(central) = &central {
        let defined = project.package_versions.iter().any(|v| v.name.eq_ignore_ascii_case(name));
        if !defined || project.package_versions.iter().any(|v| v.name.eq_ignore_ascii_case(name) && v.version != version) {
            set_central_version(central, name, version)?;
        }
    }
    edit_xml_file(&project.path, |xml| {
        let new = if central.is_some() {
            NewElement::new("PackageReference").attr("Include", name)
        } else {
            NewElement::new("PackageReference").attr("Include", name).attr("Version", version)
        };
        let group = item_group_for(xml, "PackageReference")?;
        insert_sorted(xml, group, "PackageReference", &new, name)
    })
}

/// Removes a package reference from the file that declares it.
pub fn remove_package(project: &Project, name: &str) -> Result<()> {
    let reference = project.package(name).with_context(|| format!("{name} is not referenced by {}", project.name()))?;
    let file = reference.defined_in.clone();
    let is_config = paths::file_name(&file).eq_ignore_ascii_case("packages.config");
    edit_xml_file(&file, |xml| {
        let elements = xml.elements()?;
        let element = elements
            .iter()
            .find(|e| {
                if is_config {
                    e.name == "package" && e.attr("id").is_some_and(|i| i.eq_ignore_ascii_case(name))
                } else {
                    e.name == "PackageReference" && e.attr("Include").is_some_and(|i| i.eq_ignore_ascii_case(name))
                }
            })
            .context("package reference not found")?;
        let group = element.parent();
        xml.remove_element(element.start())?;
        if let (Some(group), false) = (group, is_config) {
            xml.remove_if_empty(group)?;
        }
        Ok(())
    })
}

pub fn add_project_reference(project: &Project, target: &Path) -> Result<()> {
    if paths::normalize(target) == project.path {
        bail!("a project cannot reference itself");
    }
    if project.project_references.iter().any(|r| *r == paths::normalize(target)) {
        return Ok(());
    }
    let relative = paths::to_msbuild(&paths::relative(project.dir(), target).context("the project is on another drive")?);
    edit_xml_file(&project.path, |xml| {
        let group = item_group_for(xml, "ProjectReference")?;
        insert_sorted(xml, group, "ProjectReference", &NewElement::new("ProjectReference").attr("Include", &relative), &relative)
    })
}

pub fn remove_project_reference(project: &Project, target: &Path) -> Result<()> {
    let target = paths::normalize(target);
    edit_xml_file(&project.path, |xml| {
        let elements = xml.elements()?;
        let element = elements
            .iter()
            .find(|e| e.name == "ProjectReference" && e.attr("Include").is_some_and(|i| paths::resolve(project.dir(), i) == target))
            .context("project reference not found")?;
        let group = element.parent();
        xml.remove_element(element.start())?;
        if let Some(group) = group {
            xml.remove_if_empty(group)?;
        }
        Ok(())
    })
}

/// Item type for a new file, from its extension and a user mapping (`*` = default).
pub fn item_type_for(file: &Path, mapping: &[(String, String)]) -> String {
    let ext = paths::extension(file);
    mapping
        .iter()
        .find(|(e, _)| e.eq_ignore_ascii_case(&ext))
        .or_else(|| mapping.iter().find(|(e, _)| e == "*"))
        .map(|(_, t)| t.clone())
        .unwrap_or_else(|| "None".into())
}

pub fn default_item_types() -> Vec<(String, String)> {
    [("*", "Content"), ("cs", "Compile"), ("vb", "Compile"), ("fs", "Compile"), ("cpp", "ClCompile"), ("cc", "ClCompile"), ("c", "ClCompile"), ("h", "ClInclude"), ("hpp", "ClInclude"), ("ts", "TypeScriptCompile"), ("resx", "EmbeddedResource")]
        .into_iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect()
}

/// All projects whose evaluation reads `file` (to know which to reload).
pub fn affected_projects<'a>(projects: impl IntoIterator<Item = &'a Project>, file: &Path) -> Vec<PathBuf> {
    projects.into_iter().filter(|p| p.watched_files().iter().any(|f| f == file)).map(|p| p.path.clone()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::msbuild::{EvalOptions, evaluate};

    fn setup(files: &[(&str, &str)]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        for (name, text) in files {
            let path = root.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        (dir, root)
    }

    fn eval(path: &Path) -> Project {
        evaluate(path, &EvalOptions::default()).unwrap()
    }

    const SDK: &str = "<Project Sdk=\"Microsoft.NET.Sdk\">\n\n  <PropertyGroup>\n    <TargetFramework>net8.0</TargetFramework>\n  </PropertyGroup>\n\n  <ItemGroup>\n    <PackageReference Include=\"A\" Version=\"1.0.0\" />\n    <PackageReference Include=\"C\">\n      <Version>1.0.0</Version>\n    </PackageReference>\n  </ItemGroup>\n\n</Project>\n";

    #[test]
    fn packages_without_central_management() {
        let (_dir, root) = setup(&[("App/App.csproj", SDK)]);
        let path = root.join("App/App.csproj");
        add_package(&eval(&path), "B", "2.0.0").unwrap();
        set_package_version(&eval(&path), "A", "1.5.0").unwrap();
        set_package_version(&eval(&path), "C", "3.0.0").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains(
            "    <PackageReference Include=\"A\" Version=\"1.5.0\" />\n    <PackageReference Include=\"B\" Version=\"2.0.0\" />\n    <PackageReference Include=\"C\">\n      <Version>3.0.0</Version>"
        ));
        remove_package(&eval(&path), "A").unwrap();
        remove_package(&eval(&path), "B").unwrap();
        remove_package(&eval(&path), "C").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("ItemGroup"), "{text}");
        assert!(text.contains("<TargetFramework>net8.0</TargetFramework>"));
    }

    #[test]
    fn packages_with_central_management() {
        let (_dir, root) = setup(&[
            ("Directory.Packages.props", "<Project>\n  <PropertyGroup>\n    <ManagePackageVersionsCentrally>true</ManagePackageVersionsCentrally>\n  </PropertyGroup>\n  <ItemGroup>\n    <PackageVersion Include=\"A\" Version=\"1.0.0\" />\n    <PackageVersion Include=\"Z\" Version=\"1.0.0\" />\n  </ItemGroup>\n</Project>\n"),
            ("App/App.csproj", "<Project Sdk=\"Microsoft.NET.Sdk\">\n  <ItemGroup>\n    <PackageReference Include=\"A\" />\n  </ItemGroup>\n</Project>\n"),
        ]);
        let path = root.join("App/App.csproj");
        add_package(&eval(&path), "M", "2.0.0").unwrap();
        set_package_version(&eval(&path), "A", "1.1.0").unwrap();
        let props = std::fs::read_to_string(root.join("Directory.Packages.props")).unwrap();
        assert!(props.contains("<PackageVersion Include=\"A\" Version=\"1.1.0\" />\n    <PackageVersion Include=\"M\" Version=\"2.0.0\" />\n    <PackageVersion Include=\"Z\""), "{props}");
        let project = std::fs::read_to_string(&path).unwrap();
        assert!(project.contains("<PackageReference Include=\"A\" />\n    <PackageReference Include=\"M\" />"), "{project}");
        assert_eq!(eval(&path).package("M").unwrap().version.as_deref(), Some("2.0.0"));
    }

    #[test]
    fn files_in_legacy_projects() {
        let legacy = "<Project ToolsVersion=\"15.0\">\n  <ItemGroup>\n    <Compile Include=\"Program.cs\" />\n    <Compile Include=\"Forms\\Main.cs\" />\n    <Compile Include=\"Forms\\Main.Designer.cs\">\n      <DependentUpon>Main.cs</DependentUpon>\n    </Compile>\n  </ItemGroup>\n</Project>\n";
        let (_dir, root) = setup(&[("L.csproj", legacy), ("Program.cs", ""), ("Forms/Main.cs", ""), ("Forms/Main.Designer.cs", "")]);
        let path = root.join("L.csproj");
        std::fs::create_dir_all(root.join("Models")).unwrap();
        add_folder(&eval(&path), &root.join("Models")).unwrap();
        assert!(std::fs::read_to_string(&path).unwrap().contains("<Folder Include=\"Models\\\" />"));
        std::fs::write(root.join("Models/User.cs"), "").unwrap();
        add_file(&eval(&path), &root.join("Models/User.cs"), "Compile", None).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("Folder Include"), "{text}");
        assert!(text.contains("<Compile Include=\"Models\\User.cs\" />"));

        std::fs::rename(root.join("Forms"), root.join("Views")).unwrap();
        rename_path(&eval(&path), &root.join("Forms"), &root.join("Views")).unwrap();
        std::fs::rename(root.join("Views/Main.cs"), root.join("Views/Shell.cs")).unwrap();
        rename_path(&eval(&path), &root.join("Views/Main.cs"), &root.join("Views/Shell.cs")).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("<Compile Include=\"Views\\Shell.cs\" />"));
        assert!(text.contains("<Compile Include=\"Views\\Main.Designer.cs\">\n      <DependentUpon>Shell.cs</DependentUpon>"), "{text}");

        std::fs::remove_file(root.join("Models/User.cs")).unwrap();
        remove_path(&eval(&path), &root.join("Models/User.cs")).unwrap();
        assert!(std::fs::read_to_string(&path).unwrap().contains("<Folder Include=\"Models\\\" />"));
    }

    #[test]
    fn sdk_projects_only_get_items_they_need() {
        let (_dir, root) = setup(&[("App.csproj", "<Project Sdk=\"Microsoft.NET.Sdk\">\n</Project>\n"), ("Lib/Lib.csproj", "<Project Sdk=\"Microsoft.NET.Sdk\" />")]);
        let path = root.join("App.csproj");
        std::fs::write(root.join("New.cs"), "").unwrap();
        add_file(&eval(&path), &root.join("New.cs"), "Compile", None).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "<Project Sdk=\"Microsoft.NET.Sdk\">\n</Project>\n");
        add_project_reference(&eval(&path), &root.join("Lib/Lib.csproj")).unwrap();
        assert!(std::fs::read_to_string(&path).unwrap().contains("<ProjectReference Include=\"Lib\\Lib.csproj\" />"));
        assert_eq!(eval(&path).project_references, vec![root.join("Lib/Lib.csproj")]);
        remove_project_reference(&eval(&path), &root.join("Lib/Lib.csproj")).unwrap();
        assert!(!std::fs::read_to_string(&path).unwrap().contains("ProjectReference"));
    }

    #[test]
    fn fsharp_order() {
        let fs = "<Project Sdk=\"Microsoft.NET.Sdk\">\n  <ItemGroup>\n    <Compile Include=\"A.fs\" />\n    <Compile Include=\"B.fs\" />\n  </ItemGroup>\n</Project>\n";
        let (_dir, root) = setup(&[("App.fsproj", fs)]);
        let path = root.join("App.fsproj");
        move_item(&eval(&path), &root.join("B.fs"), Position::Before).unwrap();
        add_file(&eval(&path), &root.join("C.fs"), "Compile", Some((&root.join("B.fs"), Position::After))).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("<Compile Include=\"B.fs\" />\n    <Compile Include=\"C.fs\" />\n    <Compile Include=\"A.fs\" />"), "{text}");
    }
}
