//! Packages across the projects of a solution: what is installed where, which versions
//! disagree, and moving a solution to central package management.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Result;

use super::version;
use crate::msbuild::{Project, VersionSource, edit};
use crate::paths;
use crate::xml_edit::XmlText;

#[derive(Clone, Debug, PartialEq)]
pub struct PackageUsage {
    pub project: PathBuf,
    pub project_name: String,
    pub version: Option<String>,
    pub source: VersionSource,
}

#[derive(Clone, Debug, PartialEq)]
pub struct InstalledPackage {
    pub name: String,
    pub usages: Vec<PackageUsage>,
}

impl InstalledPackage {
    /// Distinct versions in use, newest first.
    pub fn versions(&self) -> Vec<String> {
        version::sort_desc(self.usages.iter().filter_map(|u| u.version.clone()), true)
    }

    pub fn highest(&self) -> Option<String> {
        self.versions().into_iter().next()
    }

    /// Projects disagree on the version.
    pub fn needs_consolidation(&self) -> bool {
        self.versions().len() > 1
    }

    pub fn usage(&self, project: &Path) -> Option<&PackageUsage> {
        self.usages.iter().find(|u| u.project == project)
    }
}

/// Every package referenced by the projects, by name.
pub fn installed(projects: &[Project]) -> Vec<InstalledPackage> {
    let mut map: BTreeMap<String, InstalledPackage> = BTreeMap::new();
    for project in projects {
        for reference in &project.package_references {
            let entry = map
                .entry(reference.name.to_lowercase())
                .or_insert_with(|| InstalledPackage { name: reference.name.clone(), usages: Vec::new() });
            entry.usages.push(PackageUsage {
                project: project.path.clone(),
                project_name: project.name(),
                version: reference.version.clone(),
                source: reference.version_source,
            });
        }
    }
    map.into_values().collect()
}

/// Whether `latest` is newer than every version installed.
pub fn has_update(package: &InstalledPackage, latest: &str) -> bool {
    package.usages.iter().any(|u| u.version.as_deref().is_some_and(|v| version::is_newer(latest, v)))
}

const CENTRAL_TEMPLATE: &str = "<Project>\n  <PropertyGroup>\n    <ManagePackageVersionsCentrally>true</ManagePackageVersionsCentrally>\n  </PropertyGroup>\n  <ItemGroup>\n  </ItemGroup>\n</Project>\n";

/// Creates (or completes) `Directory.Packages.props` in `dir` from the versions the
/// projects use, taking the highest when they disagree, and removes the versions from
/// the projects. Returns the file.
pub fn centralize(dir: &Path, projects: &[Project]) -> Result<PathBuf> {
    let file = crate::msbuild::find_above(dir, "Directory.Packages.props").filter(|f| f.parent() == Some(dir)).unwrap_or_else(|| dir.join("Directory.Packages.props"));
    if !file.exists() {
        std::fs::write(&file, CENTRAL_TEMPLATE)?;
    } else {
        edit::edit_xml_file(&file, |xml| {
            let elements = xml.elements()?;
            if let Some(flag) = elements.iter().find(|e| e.name == "ManagePackageVersionsCentrally") {
                xml.set_text(flag.start(), "true")?;
            }
            Ok(())
        })?;
    }

    let mut versions: BTreeMap<String, (String, String)> = BTreeMap::new();
    for project in projects.iter().filter(|p| p.is_sdk()) {
        for reference in &project.package_references {
            let Some(v) = &reference.version else { continue };
            let entry = versions.entry(reference.name.to_lowercase()).or_insert_with(|| (reference.name.clone(), v.clone()));
            if version::is_newer(v, &entry.1) {
                entry.1 = v.clone();
            }
        }
    }
    for (name, v) in versions.values() {
        edit::set_central_version(&file, name, v)?;
    }

    // Drop `Version` from every file that declares references, once per file.
    let mut files: Vec<PathBuf> = projects
        .iter()
        .filter(|p| p.is_sdk())
        .flat_map(|p| p.package_references.iter().filter(|r| r.version_source == VersionSource::Inline).map(|r| r.defined_in.clone()))
        .filter(|f| !paths::file_name(f).eq_ignore_ascii_case("packages.config"))
        .collect();
    files.sort();
    files.dedup();
    for path in files {
        edit::edit_xml_file(&path, strip_versions)?;
    }
    Ok(file)
}

fn strip_versions(xml: &mut XmlText) -> Result<()> {
    loop {
        let elements = xml.elements()?;
        if let Some(reference) = elements.iter().find(|e| e.name == "PackageReference" && e.attr("Include").is_some() && e.attr("Version").is_some()) {
            xml.remove_attribute(reference.start(), "Version")?;
            continue;
        }
        let child = elements.iter().find(|e| {
            e.name == "Version" && e.parent().and_then(|p| elements.iter().find(|x| x.start() == p)).is_some_and(|p| p.name == "PackageReference")
        });
        if let Some(child) = child {
            let parent = child.parent().unwrap();
            xml.remove_element(child.start())?;
            // `<PackageReference Include="X">\n</PackageReference>` → `<PackageReference Include="X" />`.
            let parent_el = xml.element_at(parent)?;
            if xml.children(parent)?.is_empty() {
                let attrs: String = parent_el.attributes.iter().map(|(k, v)| format!(" {k}=\"{}\"", crate::xml_edit::escape(v))).collect();
                let mut text = xml.as_str().to_string();
                text.replace_range(parent_el.range.clone(), &format!("<PackageReference{attrs} />"));
                *xml = XmlText::new(text);
            }
            continue;
        }
        break;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::msbuild::{EvalOptions, evaluate};

    #[test]
    fn centralizes_versions_taking_the_highest() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("A")).unwrap();
        std::fs::create_dir_all(root.join("B")).unwrap();
        std::fs::write(root.join("A/A.csproj"), "<Project Sdk=\"Microsoft.NET.Sdk\">\n  <ItemGroup>\n    <PackageReference Include=\"Serilog\" Version=\"3.0.0\" />\n  </ItemGroup>\n</Project>\n").unwrap();
        std::fs::write(
            root.join("B/B.csproj"),
            "<Project Sdk=\"Microsoft.NET.Sdk\">\n  <ItemGroup>\n    <PackageReference Include=\"serilog\">\n      <Version>3.1.0</Version>\n    </PackageReference>\n    <PackageReference Include=\"Polly\" Version=\"8.0.0\" />\n  </ItemGroup>\n</Project>\n",
        )
        .unwrap();
        let load = |name: &str| evaluate(&root.join(name), &EvalOptions::default()).unwrap();
        let projects = vec![load("A/A.csproj"), load("B/B.csproj")];
        let packages = installed(&projects);
        assert_eq!(packages.len(), 2);
        let serilog = packages.iter().find(|p| p.name.eq_ignore_ascii_case("serilog")).unwrap();
        assert!(serilog.needs_consolidation());
        assert_eq!(serilog.highest().as_deref(), Some("3.1.0"));
        assert!(has_update(serilog, "3.1.0"));

        let file = centralize(&root, &projects).unwrap();
        let central = std::fs::read_to_string(&file).unwrap();
        assert!(central.contains("<PackageVersion Include=\"Polly\" Version=\"8.0.0\" />\n    <PackageVersion Include=\"Serilog\" Version=\"3.1.0\" />"), "{central}");
        assert!(std::fs::read_to_string(root.join("A/A.csproj")).unwrap().contains("<PackageReference Include=\"Serilog\" />"));
        assert!(std::fs::read_to_string(root.join("B/B.csproj")).unwrap().contains("<PackageReference Include=\"serilog\" />"));
        let reloaded = load("B/B.csproj");
        assert_eq!(reloaded.package("Serilog").unwrap().version.as_deref(), Some("3.1.0"));
        assert_eq!(reloaded.package("Serilog").unwrap().version_source, VersionSource::Central);
    }
}
