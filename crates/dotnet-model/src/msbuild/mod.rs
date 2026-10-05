//! A small MSBuild evaluator: enough of a project to show it in a solution explorer and
//! manage its packages. It reads `Directory.Build.props`, `Directory.Packages.props`, the
//! project, `Directory.Build.targets` and explicit imports in that order, applies
//! conditions, and keeps track of which file defines each property, item and package.

pub mod condition;
pub mod edit;
pub mod glob;
pub mod items;
pub mod text;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use anyhow::{Context as _, Result};
use regex::Regex;

use crate::paths;

static PROPERTY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\$\(([A-Za-z_][A-Za-z0-9_\-]*)\)").unwrap());

/// MSBuild properties: case-insensitive names, last write wins.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Properties {
    values: HashMap<String, (String, String)>,
}

impl Properties {
    pub fn get(&self, name: &str) -> Option<&str> {
        self.values.get(&name.to_lowercase()).map(|(_, value)| value.as_str())
    }

    pub fn set(&mut self, name: &str, value: &str) {
        self.values.insert(name.to_lowercase(), (name.to_string(), value.to_string()));
    }

    pub fn is_true(&self, name: &str) -> bool {
        self.get(name).is_some_and(|v| v.trim().eq_ignore_ascii_case("true"))
    }

    pub fn is_false(&self, name: &str) -> bool {
        self.get(name).is_some_and(|v| v.trim().eq_ignore_ascii_case("false"))
    }

    /// Replaces `$(Name)`; undefined properties expand to nothing, as in MSBuild.
    pub fn expand(&self, text: &str) -> String {
        if !text.contains("$(") {
            return text.to_string();
        }
        PROPERTY.replace_all(text, |caps: &regex::Captures| self.get(&caps[1]).unwrap_or("").to_string()).into_owned()
    }

    /// Name/value pairs with their original spelling, sorted by name.
    pub fn iter(&self) -> Vec<(&str, &str)> {
        let mut pairs: Vec<_> = self.values.values().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        pairs.sort_by_key(|(k, _)| k.to_lowercase());
        pairs
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct EvalOptions {
    pub configuration: String,
    pub platform: String,
    pub msbuild_version: String,
    /// Global properties that win over the project's own, like `-p:` on the command line.
    pub overrides: Vec<(String, String)>,
}

impl Default for EvalOptions {
    fn default() -> Self {
        Self { configuration: "Debug".into(), platform: "AnyCPU".into(), msbuild_version: "17.11.9".into(), overrides: Vec::new() }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ItemOpKind {
    Include,
    Remove,
    Update,
}

/// An item element, in evaluation order.
#[derive(Clone, Debug, PartialEq)]
pub struct ItemOp {
    pub kind: ItemOpKind,
    pub item_type: String,
    /// The `Include`/`Remove`/`Update` value with properties expanded.
    pub value: String,
    pub exclude: Option<String>,
    pub link: Option<String>,
    pub link_base: Option<String>,
    pub dependent_upon: Option<String>,
    pub defined_in: PathBuf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VersionSource {
    /// `Version` on the `PackageReference`.
    Inline,
    /// `VersionOverride` on the `PackageReference`, with central package management.
    Override,
    /// A `PackageVersion` in `Directory.Packages.props`.
    Central,
    /// Only known from the last restore (`obj/project.assets.json`).
    Restored,
    Unknown,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PackageReference {
    pub name: String,
    pub version: Option<String>,
    pub version_source: VersionSource,
    /// The file with the `PackageReference` element.
    pub defined_in: PathBuf,
    /// The file with the `PackageVersion` element, for central versions.
    pub version_defined_in: Option<PathBuf>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PackageVersion {
    pub name: String,
    pub version: String,
    pub defined_in: PathBuf,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AssemblyReference {
    pub name: String,
    pub version: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Project {
    pub path: PathBuf,
    pub sdk: Option<String>,
    pub tools_version: Option<String>,
    pub properties: Properties,
    pub items: Vec<ItemOp>,
    pub package_references: Vec<PackageReference>,
    pub package_versions: Vec<PackageVersion>,
    pub project_references: Vec<PathBuf>,
    pub references: Vec<AssemblyReference>,
    /// Every other file read while evaluating: props, targets, packages.config.
    pub imports: Vec<PathBuf>,
    /// The `Directory.Packages.props` in effect, if any.
    pub central_packages_file: Option<PathBuf>,
    /// For shared projects, the `.projitems` holding the items.
    pub items_file: Option<PathBuf>,
}

impl Project {
    pub fn name(&self) -> String {
        paths::file_stem(&self.path)
    }

    pub fn dir(&self) -> &Path {
        self.path.parent().unwrap_or(Path::new("."))
    }

    /// SDK-style projects (`<Project Sdk="…">`).
    pub fn is_sdk(&self) -> bool {
        self.sdk.is_some()
    }

    pub fn is_fsharp(&self) -> bool {
        paths::extension(&self.path) == "fsproj"
    }

    pub fn is_shared(&self) -> bool {
        paths::extension(&self.path) == "shproj"
    }

    /// Whether versions live in `Directory.Packages.props`.
    pub fn uses_central_packages(&self) -> bool {
        self.properties.is_true("ManagePackageVersionsCentrally")
    }

    pub fn target_frameworks(&self) -> Vec<String> {
        let value = self.properties.get("TargetFrameworks").filter(|v| !v.trim().is_empty()).or(self.properties.get("TargetFramework")).unwrap_or("");
        value.split(';').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect()
    }

    pub fn output_type(&self) -> String {
        self.properties.get("OutputType").unwrap_or("Library").to_string()
    }

    pub fn is_test_project(&self) -> bool {
        self.properties.is_true("IsTestProject")
            || self.package_references.iter().any(|p| {
                let name = p.name.to_lowercase();
                name == "microsoft.net.test.sdk" || name.starts_with("xunit") || name.starts_with("nunit") || name.starts_with("mstest")
            })
    }

    /// Whether `dotnet run` makes sense for it.
    pub fn is_runnable(&self) -> bool {
        let sdk = self.sdk.as_deref().unwrap_or("").to_lowercase();
        let output = self.output_type().to_lowercase();
        output == "exe" || output == "winexe" || sdk.contains(".web") || sdk.contains(".worker") || sdk.contains("blazor")
    }

    /// The extension new code files get: `cs`, `fs` or `vb`.
    pub fn code_extension(&self) -> &'static str {
        match paths::extension(&self.path).as_str() {
            "fsproj" => "fs",
            "vbproj" => "vb",
            "vcxproj" => "cpp",
            "njsproj" | "esproj" => "js",
            _ => "cs",
        }
    }

    /// The namespace new files in `dir` get, as the IDE computes it.
    pub fn namespace_for(&self, dir: &Path) -> String {
        let root = self
            .properties
            .get("RootNamespace")
            .filter(|v| !v.trim().is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| self.name());
        let relative = paths::relative(self.dir(), dir).unwrap_or_default();
        let mut namespace = root;
        for part in relative.components() {
            namespace.push('.');
            namespace.push_str(&part.as_os_str().to_string_lossy());
        }
        namespace
            .chars()
            .map(|c| if c.is_alphanumeric() || c == '.' || c == '_' { c } else { '_' })
            .collect::<String>()
            .split('.')
            .map(|part| if part.starts_with(|c: char| c.is_ascii_digit()) { format!("_{part}") } else { part.to_string() })
            .collect::<Vec<_>>()
            .join(".")
    }

    /// Where `dotnet restore` writes `project.assets.json`.
    pub fn assets_file(&self) -> PathBuf {
        let obj = self
            .properties
            .get("MSBuildProjectExtensionsPath")
            .or(self.properties.get("BaseIntermediateOutputPath"))
            .filter(|v| !v.trim().is_empty())
            .unwrap_or("obj");
        paths::resolve(self.dir(), obj).join("project.assets.json")
    }

    /// Files whose change means the project must be evaluated again.
    pub fn watched_files(&self) -> Vec<PathBuf> {
        let mut files = vec![self.path.clone()];
        files.extend(self.imports.iter().cloned());
        files.extend(self.items_file.iter().cloned());
        files
    }

    pub fn package(&self, name: &str) -> Option<&PackageReference> {
        self.package_references.iter().find(|p| p.name.eq_ignore_ascii_case(name))
    }
}

/// The `<ProjectGuid>` of a project, without braces, uppercase.
pub fn project_guid(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let start = text.find("<ProjectGuid>")? + "<ProjectGuid>".len();
    let end = text[start..].find("</ProjectGuid>")? + start;
    Some(text[start..end].trim().trim_matches(['{', '}']).to_uppercase())
}

/// The nearest `name` in `dir` or above it.
pub fn find_above(dir: &Path, name: &str) -> Option<PathBuf> {
    let mut current = Some(dir);
    while let Some(dir) = current {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                if entry.file_name().to_string_lossy().eq_ignore_ascii_case(name) && entry.path().is_file() {
                    return Some(entry.path());
                }
            }
        }
        current = dir.parent();
    }
    None
}

const IGNORED_ITEMS: &[&str] = &[
    "AssemblyMetadata",
    "BaseApplicationManifest",
    "CodeAnalysisImport",
    "COMReference",
    "COMFileReference",
    "Import",
    "InternalsVisibleTo",
    "NativeReference",
    "TrimmerRootAssembly",
    "Using",
    "Protobuf",
    "Service",
    "BootstrapperPackage",
    "WCFMetadata",
];

struct Evaluator {
    project_path: PathBuf,
    project_dir: PathBuf,
    properties: Properties,
    overrides: Vec<(String, String)>,
    items: Vec<ItemOp>,
    package_references: Vec<PackageReference>,
    package_versions: Vec<PackageVersion>,
    project_references: Vec<PathBuf>,
    references: Vec<AssemblyReference>,
    imports: Vec<PathBuf>,
    seen: Vec<PathBuf>,
}

impl Evaluator {
    fn set_property(&mut self, name: &str, value: &str) {
        // Global properties cannot be overwritten by the project.
        if self.overrides.iter().any(|(k, _)| k.eq_ignore_ascii_case(name)) {
            return;
        }
        self.properties.set(name, value);
    }

    fn condition(&self, node: roxmltree::Node) -> bool {
        node.attribute("Condition").is_none_or(|c| condition::evaluate(c, &self.properties, &self.project_dir))
    }

    fn import_file(&mut self, path: &Path, depth: usize) {
        if depth > 10 || self.seen.iter().any(|seen| seen == path) {
            return;
        }
        let Ok(text) = std::fs::read_to_string(path) else { return };
        self.seen.push(path.to_path_buf());
        if path != self.project_path {
            self.imports.push(path.to_path_buf());
        }
        let options = roxmltree::ParsingOptions { allow_dtd: true, ..Default::default() };
        let Ok(doc) = roxmltree::Document::parse_with_options(&text, options) else {
            log::warn!("{} is not valid XML", path.display());
            return;
        };
        let file_dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();
        let previous = self.properties.get("MSBuildThisFileDirectory").map(str::to_string);
        self.properties.set("MSBuildThisFileDirectory", &format!("{}{}", file_dir.display(), std::path::MAIN_SEPARATOR));
        self.properties.set("MSBuildThisFile", &paths::file_name(path));
        self.properties.set("MSBuildThisFileFullPath", &path.to_string_lossy());
        self.elements(doc.root_element(), path, &file_dir, depth);
        if let Some(previous) = previous {
            self.properties.set("MSBuildThisFileDirectory", &previous);
        }
    }

    fn elements(&mut self, parent: roxmltree::Node, file: &Path, file_dir: &Path, depth: usize) {
        for node in parent.children().filter(|n| n.is_element()) {
            if !self.condition(node) {
                continue;
            }
            match node.tag_name().name() {
                "PropertyGroup" => {
                    for property in node.children().filter(|n| n.is_element()) {
                        if self.condition(property) {
                            let value = self.properties.expand(property.text().unwrap_or("").trim());
                            self.set_property(property.tag_name().name(), &value);
                        }
                    }
                    self.default_target_framework();
                }
                "ItemGroup" => {
                    for item in node.children().filter(|n| n.is_element()) {
                        if self.condition(item) {
                            self.item(item, file);
                        }
                    }
                }
                "Import" => {
                    let Some(project) = node.attribute("Project") else { continue };
                    for spec in self.properties.expand(project).split(';').map(str::trim).filter(|s| !s.is_empty()) {
                        if glob::is_glob(spec) || spec.contains("$(") {
                            continue;
                        }
                        let path = paths::resolve(file_dir, spec);
                        self.import_file(&path, depth + 1);
                    }
                }
                "Choose" => {
                    let mut chosen = false;
                    for branch in node.children().filter(|n| n.is_element()) {
                        match branch.tag_name().name() {
                            "When" if !chosen && branch.attribute("Condition").is_some_and(|c| condition::evaluate(c, &self.properties, &self.project_dir)) => {
                                chosen = true;
                                self.elements(branch, file, file_dir, depth);
                            }
                            "Otherwise" if !chosen => {
                                chosen = true;
                                self.elements(branch, file, file_dir, depth);
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn default_target_framework(&mut self) {
        if self.properties.get("TargetFramework").is_none_or(|v| v.trim().is_empty())
            && let Some(first) = self.properties.get("TargetFrameworks").and_then(|v| v.split(';').map(str::trim).find(|s| !s.is_empty())) {
                let first = first.to_string();
                self.properties.set("TargetFramework", &first);
            }
    }

    fn metadata(&self, item: roxmltree::Node, name: &str) -> Option<String> {
        item.attribute(name)
            .map(str::to_string)
            .or_else(|| item.children().find(|c| c.is_element() && c.tag_name().name() == name).and_then(|c| c.text()).map(str::to_string))
            .map(|value| self.properties.expand(value.trim()))
            .filter(|value| !value.is_empty())
    }

    fn item(&mut self, item: roxmltree::Node, file: &Path) {
        let item_type = item.tag_name().name();
        if IGNORED_ITEMS.contains(&item_type) {
            return;
        }
        let include = self.metadata(item, "Include");
        match (item_type, include) {
            ("PackageReference", Some(name)) => {
                let (version, source) = match (self.metadata(item, "VersionOverride"), self.metadata(item, "Version")) {
                    (Some(version), _) => (Some(version), VersionSource::Override),
                    (None, Some(version)) => (Some(version), VersionSource::Inline),
                    _ => (None, VersionSource::Unknown),
                };
                self.package_references.push(PackageReference { name, version, version_source: source, defined_in: file.to_path_buf(), version_defined_in: None });
            }
            ("PackageReference", None) => {
                // `<PackageReference Update="X" Version="…" />` in props files sets versions.
                if let (Some(name), Some(version)) = (self.metadata(item, "Update"), self.metadata(item, "Version"))
                    && let Some(existing) = self.package_references.iter_mut().find(|p| p.name.eq_ignore_ascii_case(&name)) {
                        existing.version = Some(version);
                        existing.version_source = VersionSource::Inline;
                        existing.version_defined_in = Some(file.to_path_buf());
                    }
                if let Some(name) = self.metadata(item, "Remove") {
                    self.package_references.retain(|p| !p.name.eq_ignore_ascii_case(&name));
                }
            }
            ("PackageVersion", Some(name)) => {
                if let Some(version) = self.metadata(item, "Version") {
                    self.package_versions.retain(|p| !p.name.eq_ignore_ascii_case(&name));
                    self.package_versions.push(PackageVersion { name, version, defined_in: file.to_path_buf() });
                }
            }
            ("ProjectReference", Some(path)) => {
                for spec in path.split(';').filter(|s| !s.trim().is_empty()) {
                    let path = paths::resolve(&self.project_dir, spec);
                    if !self.project_references.contains(&path) {
                        self.project_references.push(path);
                    }
                }
            }
            ("ProjectReference", None) => {
                if let Some(path) = self.metadata(item, "Remove") {
                    let path = paths::resolve(&self.project_dir, &path);
                    self.project_references.retain(|p| *p != path);
                }
            }
            ("Reference", Some(include)) => {
                let mut parts = include.split(',');
                let name = parts.next().unwrap_or("").trim().to_string();
                let version = parts.find_map(|p| p.trim().strip_prefix("Version=").map(str::to_string));
                self.references.push(AssemblyReference { name, version });
            }
            (_, include) => {
                let (kind, value) = match (include, self.metadata(item, "Remove"), self.metadata(item, "Update")) {
                    (Some(value), _, _) => (ItemOpKind::Include, value),
                    (None, Some(value), _) => (ItemOpKind::Remove, value),
                    (None, None, Some(value)) => (ItemOpKind::Update, value),
                    _ => return,
                };
                self.items.push(ItemOp {
                    kind,
                    item_type: item_type.to_string(),
                    value,
                    exclude: self.metadata(item, "Exclude"),
                    link: self.metadata(item, "Link"),
                    link_base: self.metadata(item, "LinkBase"),
                    dependent_upon: self.metadata(item, "DependentUpon"),
                    defined_in: file.to_path_buf(),
                });
            }
        }
    }
}

/// Reads the `Sdk` of a project file's root element, or of its `<Sdk Name>` child.
fn read_sdk(doc: &roxmltree::Document) -> (Option<String>, Option<String>) {
    let root = doc.root_element();
    let sdk = root
        .attribute("Sdk")
        .map(str::to_string)
        .or_else(|| root.children().find(|c| c.is_element() && c.tag_name().name() == "Sdk").and_then(|c| c.attribute("Name")).map(str::to_string))
        .or_else(|| {
            root.children()
                .find(|c| c.is_element() && c.tag_name().name() == "Import" && c.attribute("Sdk").is_some())
                .and_then(|c| c.attribute("Sdk"))
                .map(str::to_string)
        });
    (sdk, root.attribute("ToolsVersion").map(str::to_string))
}

/// Evaluates a project file.
pub fn evaluate(path: &Path, options: &EvalOptions) -> Result<Project> {
    let path = paths::normalize(path);
    let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let parse_options = roxmltree::ParsingOptions { allow_dtd: true, ..Default::default() };
    let doc = roxmltree::Document::parse_with_options(&text, parse_options).with_context(|| format!("{} is not valid XML", path.display()))?;
    let (sdk, tools_version) = read_sdk(&doc);
    let dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();

    let mut eval = Evaluator {
        project_path: path.clone(),
        project_dir: dir.clone(),
        properties: Properties::default(),
        overrides: options.overrides.clone(),
        items: Vec::new(),
        package_references: Vec::new(),
        package_versions: Vec::new(),
        project_references: Vec::new(),
        references: Vec::new(),
        imports: Vec::new(),
        seen: Vec::new(),
    };
    let sep = std::path::MAIN_SEPARATOR;
    let props = &mut eval.properties;
    props.set("MSBuildProjectDirectory", &dir.to_string_lossy());
    props.set("MSBuildProjectDirectoryNoRoot", dir.to_string_lossy().trim_start_matches(['/', '\\']));
    props.set("MSBuildProjectFullPath", &path.to_string_lossy());
    props.set("MSBuildProjectFile", &paths::file_name(&path));
    props.set("MSBuildProjectName", &paths::file_stem(&path));
    props.set("MSBuildProjectExtension", &format!(".{}", paths::extension(&path)));
    props.set("MSBuildThisFileDirectory", &format!("{}{sep}", dir.display()));
    props.set("MSBuildVersion", &options.msbuild_version);
    props.set("Configuration", &options.configuration);
    props.set("Platform", &options.platform);
    for (name, value) in &options.overrides {
        props.set(name, value);
    }

    // What Microsoft.Common.props and the SDK import around the project body.
    if let Some(build_props) = find_above(&dir, "Directory.Build.props") {
        eval.import_file(&build_props, 1);
    }
    let central = find_above(&dir, "Directory.Packages.props");
    if let Some(central) = &central {
        eval.import_file(central, 1);
    }
    if sdk.is_some() && eval.properties.get("ManagePackageVersionsCentrally").is_none() && central.is_none() {
        eval.properties.set("ManagePackageVersionsCentrally", "false");
    }
    eval.import_file(&path, 0);
    if let Some(build_targets) = find_above(&dir, "Directory.Build.targets") {
        eval.import_file(&build_targets, 1);
    }

    let items_file = (paths::extension(&path) == "shproj").then(|| path.with_extension("projitems")).filter(|p| p.exists());
    if let Some(items_file) = &items_file {
        // Shared projects keep their items in a .projitems next to the .shproj.
        let previous = eval.properties.get("MSBuildThisFileDirectory").map(str::to_string);
        eval.import_file(items_file, 1);
        if let Some(previous) = previous {
            eval.properties.set("MSBuildThisFileDirectory", &previous);
        }
    }

    // Legacy projects list packages in packages.config.
    if sdk.is_none() {
        let packages_config = dir.join("packages.config");
        if let Ok(text) = std::fs::read_to_string(&packages_config)
            && let Ok(doc) = roxmltree::Document::parse(&text) {
                for package in doc.descendants().filter(|n| n.has_tag_name("package")) {
                    if let Some(id) = package.attribute("id") {
                        eval.package_references.push(PackageReference {
                            name: id.to_string(),
                            version: package.attribute("version").map(str::to_string),
                            version_source: VersionSource::Inline,
                            defined_in: packages_config.clone(),
                            version_defined_in: None,
                        });
                    }
                }
                eval.imports.push(packages_config);
            }
    }

    let mut project = Project {
        path,
        sdk,
        tools_version,
        properties: eval.properties,
        items: eval.items,
        package_references: eval.package_references,
        package_versions: eval.package_versions,
        project_references: eval.project_references,
        references: eval.references,
        imports: eval.imports,
        central_packages_file: None,
        items_file,
    };

    if project.uses_central_packages() {
        project.central_packages_file = central;
        for reference in project.package_references.iter_mut().filter(|r| r.version.is_none()) {
            if let Some(central) = project.package_versions.iter().find(|v| v.name.eq_ignore_ascii_case(&reference.name)) {
                reference.version = Some(central.version.clone());
                reference.version_source = VersionSource::Central;
                reference.version_defined_in = Some(central.defined_in.clone());
            }
        }
    }

    if project.package_references.iter().any(|r| r.version.is_none())
        && let Some(assets) = crate::assets::Assets::load(&project.assets_file()) {
            let framework = project.target_frameworks().into_iter().next();
            for reference in project.package_references.iter_mut().filter(|r| r.version.is_none()) {
                if let Some(version) = assets.resolved_version(framework.as_deref(), &reference.name) {
                    reference.version = Some(version);
                    reference.version_source = VersionSource::Restored;
                }
            }
        }
    Ok(project)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, text: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, text).unwrap();
        path
    }

    #[test]
    fn evaluates_props_conditions_and_central_versions() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        write(&root, "Directory.Build.props", "<Project><PropertyGroup><Company>Acme</Company><LangVersion>latest</LangVersion></PropertyGroup><ItemGroup><PackageReference Include=\"Analyzers\" /></ItemGroup></Project>");
        write(
            &root,
            "Directory.Packages.props",
            "<Project><PropertyGroup><ManagePackageVersionsCentrally>true</ManagePackageVersionsCentrally></PropertyGroup><ItemGroup><PackageVersion Include=\"Serilog\" Version=\"3.1.1\" /><PackageVersion Include=\"Analyzers\" Version=\"1.0.0\" /></ItemGroup></Project>",
        );
        write(&root, "src/App/common.props", "<Project><PropertyGroup><Imported>yes</Imported></PropertyGroup></Project>");
        let project = write(
            &root,
            "src/App/App.csproj",
            r#"<Project Sdk="Microsoft.NET.Sdk.Web">
  <Import Project="common.props" />
  <PropertyGroup>
    <TargetFrameworks>net8.0;net9.0</TargetFrameworks>
    <AssemblyName>$(Company).App</AssemblyName>
  </PropertyGroup>
  <PropertyGroup Condition="'$(Configuration)' == 'Release'">
    <Optimize>true</Optimize>
  </PropertyGroup>
  <ItemGroup>
    <PackageReference Include="Serilog" />
    <PackageReference Include="Newtonsoft.Json" VersionOverride="13.0.3" />
    <ProjectReference Include="..\Lib\Lib.csproj" />
    <Compile Remove="Old\**" />
  </ItemGroup>
  <ItemGroup Condition="'$(TargetFramework)' == 'net8.0'">
    <PackageReference Include="Polly" Version="8.0.0" />
  </ItemGroup>
</Project>"#,
        );
        let p = evaluate(&project, &EvalOptions::default()).unwrap();
        assert_eq!(p.sdk.as_deref(), Some("Microsoft.NET.Sdk.Web"));
        assert_eq!(p.properties.get("AssemblyName"), Some("Acme.App"));
        assert_eq!(p.properties.get("Imported"), Some("yes"));
        assert_eq!(p.properties.get("Optimize"), None);
        assert_eq!(p.target_frameworks(), vec!["net8.0", "net9.0"]);
        assert!(p.uses_central_packages());
        assert!(p.is_runnable());
        assert_eq!(p.central_packages_file, Some(root.join("Directory.Packages.props")));
        let serilog = p.package("serilog").unwrap();
        assert_eq!((serilog.version.as_deref(), serilog.version_source), (Some("3.1.1"), VersionSource::Central));
        assert_eq!(p.package("Analyzers").unwrap().defined_in, root.join("Directory.Build.props"));
        assert_eq!(p.package("Newtonsoft.Json").unwrap().version_source, VersionSource::Override);
        assert_eq!(p.package("Polly").unwrap().version.as_deref(), Some("8.0.0"));
        assert_eq!(p.project_references, vec![root.join("src/Lib/Lib.csproj")]);
        assert_eq!(p.items.len(), 1);
        assert_eq!(p.items[0].kind, ItemOpKind::Remove);
        assert_eq!(p.imports.len(), 3);

        let release = EvalOptions { configuration: "Release".into(), ..Default::default() };
        assert_eq!(evaluate(&project, &release).unwrap().properties.get("Optimize"), Some("true"));
        assert_eq!(p.namespace_for(&root.join("src/App/Controllers/V1")), "App.Controllers.V1");
    }

    #[test]
    fn legacy_projects_and_packages_config() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        write(&root, "packages.config", r#"<?xml version="1.0"?><packages><package id="NUnit" version="3.13.0" targetFramework="net48" /></packages>"#);
        let project = write(
            &root,
            "Legacy.csproj",
            r#"<?xml version="1.0" encoding="utf-8"?>
<Project ToolsVersion="15.0" xmlns="http://schemas.microsoft.com/developer/msbuild/2003">
  <PropertyGroup><RootNamespace>My.Legacy</RootNamespace><ProjectGuid>{ABCDEF00-0000-0000-0000-000000000000}</ProjectGuid></PropertyGroup>
  <ItemGroup>
    <Reference Include="System.Xml, Version=4.0.0.0, Culture=neutral" />
    <Compile Include="Program.cs" />
    <Compile Include="Form1.Designer.cs"><DependentUpon>Form1.cs</DependentUpon></Compile>
  </ItemGroup>
</Project>"#,
        );
        let p = evaluate(&project, &EvalOptions::default()).unwrap();
        assert!(!p.is_sdk());
        assert_eq!(p.tools_version.as_deref(), Some("15.0"));
        assert_eq!(p.references[0].name, "System.Xml");
        assert_eq!(p.references[0].version.as_deref(), Some("4.0.0.0"));
        assert_eq!(p.package("NUnit").unwrap().version.as_deref(), Some("3.13.0"));
        assert_eq!(p.items[1].dependent_upon.as_deref(), Some("Form1.cs"));
        assert_eq!(p.namespace_for(&root), "My.Legacy");
        assert_eq!(project_guid(&project).as_deref(), Some("ABCDEF00-0000-0000-0000-000000000000"));
    }
}
