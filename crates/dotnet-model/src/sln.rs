//! Classic `.sln` files: a line-oriented format. Reading follows MSBuild's `SolutionFile`;
//! edits splice lines so the rest of the file (and its CRLF line endings) stays as it was.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context as _, Result, bail};
use regex::Regex;
use std::sync::LazyLock;

pub const SOLUTION_FOLDER_TYPE: &str = "2150E333-8FDC-42A3-9474-1A3956D46DE8";
pub const CSHARP_TYPE: &str = "FAE04EC0-301F-11D3-BF4B-00C04F79EFBC";
pub const CSHARP_SDK_TYPE: &str = "9A19103F-16F7-4668-BE54-9A1E7A4F7556";
pub const FSHARP_TYPE: &str = "F2A71F9B-5D33-465A-A702-920D77279786";
pub const FSHARP_SDK_TYPE: &str = "6EC3EE1D-3C4E-46DD-8F32-0CC8E7565705";
pub const VB_TYPE: &str = "F184B08F-C81C-45F6-A57F-5ABD9991F28F";
pub const VB_SDK_TYPE: &str = "778DAE3C-4631-46EA-AA77-85C1314464D9";
pub const SHARED_PROJECT_TYPE: &str = "D954291E-2A0B-460D-934E-DC6B0785DB48";

static PROJECT_LINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"^Project\("\{([^}]+)\}"\)\s*=\s*"([^"]*)"\s*,\s*"([^"]*)"\s*,\s*"\{([^}]+)\}""#).unwrap()
});

#[derive(Clone, Debug, PartialEq)]
pub struct SlnProject {
    pub type_guid: String,
    pub name: String,
    /// As written in the file, with backslashes, relative to the solution.
    pub path: String,
    pub guid: String,
    /// `ProjectSection(SolutionItems)`: files shown in a solution folder.
    pub solution_items: Vec<String>,
    pub dependencies: Vec<String>,
}

impl SlnProject {
    pub fn is_folder(&self) -> bool {
        self.type_guid.eq_ignore_ascii_case(SOLUTION_FOLDER_TYPE)
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SlnFile {
    pub format_version: Option<String>,
    pub visual_studio_version: Option<String>,
    pub projects: Vec<SlnProject>,
    /// Child guid → parent guid, from `GlobalSection(NestedProjects)`.
    pub nested: HashMap<String, String>,
    /// `Debug|Any CPU`, ….
    pub configurations: Vec<String>,
    /// `{guid}.Debug|Any CPU.ActiveCfg` → `Debug|Any CPU`, keys uppercased.
    pub project_configurations: HashMap<String, String>,
}

impl SlnFile {
    pub fn parse(text: &str) -> Result<Self> {
        let mut sln = SlnFile::default();
        let lines: Vec<&str> = text.lines().map(str::trim).collect();
        if !lines.iter().take(3).any(|l| l.starts_with("Microsoft Visual Studio Solution File")) {
            bail!("not a Visual Studio solution file");
        }
        let mut i = 0;
        while i < lines.len() {
            let line = lines[i];
            if let Some(version) = line.strip_prefix("Microsoft Visual Studio Solution File, Format Version") {
                sln.format_version = Some(version.trim().to_string());
            } else if let Some(version) = line.strip_prefix("VisualStudioVersion") {
                sln.visual_studio_version = Some(version.trim_start_matches([' ', '=']).trim().to_string());
            } else if let Some(caps) = PROJECT_LINE.captures(line) {
                let mut project = SlnProject {
                    type_guid: caps[1].to_uppercase(),
                    name: caps[2].to_string(),
                    path: caps[3].to_string(),
                    guid: caps[4].to_uppercase(),
                    solution_items: Vec::new(),
                    dependencies: Vec::new(),
                };
                i += 1;
                let mut section = "";
                while i < lines.len() && lines[i] != "EndProject" {
                    let inner = lines[i];
                    if inner.starts_with("ProjectSection(SolutionItems)") {
                        section = "items";
                    } else if inner.starts_with("ProjectSection(ProjectDependencies)") {
                        section = "deps";
                    } else if inner.starts_with("ProjectSection(") {
                        section = "other";
                    } else if inner == "EndProjectSection" {
                        section = "";
                    } else if let Some((key, _)) = inner.split_once('=') {
                        match section {
                            "items" => project.solution_items.push(key.trim().to_string()),
                            "deps" => project.dependencies.push(key.trim().trim_matches(['{', '}']).to_uppercase()),
                            _ => {}
                        }
                    }
                    i += 1;
                }
                sln.projects.push(project);
            } else if line.starts_with("GlobalSection(") {
                let name = line.trim_start_matches("GlobalSection(").split(')').next().unwrap_or("").to_string();
                i += 1;
                while i < lines.len() && lines[i] != "EndGlobalSection" {
                    let entry = lines[i];
                    if let Some((key, value)) = entry.split_once('=') {
                        let (key, value) = (key.trim(), value.trim());
                        match name.as_str() {
                            "NestedProjects" => {
                                sln.nested.insert(key.trim_matches(['{', '}']).to_uppercase(), value.trim_matches(['{', '}']).to_uppercase());
                            }
                            "SolutionConfigurationPlatforms" if key != "DESCRIPTION" => sln.configurations.push(key.to_string()),
                            "ProjectConfigurationPlatforms" => {
                                sln.project_configurations.insert(key.to_uppercase(), value.to_string());
                            }
                            _ => {}
                        }
                    }
                    i += 1;
                }
            }
            i += 1;
        }
        Ok(sln)
    }

    pub fn project(&self, guid: &str) -> Option<&SlnProject> {
        self.projects.iter().find(|p| p.guid.eq_ignore_ascii_case(guid))
    }

    /// The project configuration a solution configuration builds, looked up by building the
    /// key the way the IDE does (configuration names may contain dots).
    pub fn project_configuration(&self, guid: &str, solution_configuration: &str) -> Option<&str> {
        let key = format!("{{{}}}.{}.ActiveCfg", guid.to_uppercase(), solution_configuration).to_uppercase();
        self.project_configurations.get(&key).map(String::as_str)
    }

    pub fn builds(&self, guid: &str, solution_configuration: &str) -> bool {
        let key = format!("{{{}}}.{}.Build.0", guid.to_uppercase(), solution_configuration).to_uppercase();
        self.project_configurations.contains_key(&key)
    }

    /// Guids of a folder and everything nested in it, folder first.
    pub fn descendants(&self, guid: &str) -> Vec<String> {
        let mut result = vec![guid.to_uppercase()];
        let mut i = 0;
        while i < result.len() {
            let parent = result[i].clone();
            for (child, p) in &self.nested {
                if *p == parent && !result.contains(child) {
                    result.push(child.clone());
                }
            }
            i += 1;
        }
        result
    }
}

/// The project type guid `.sln` files use for a project file's extension.
pub fn project_type_for(path: &Path) -> &'static str {
    match crate::paths::extension(path).as_str() {
        "fsproj" => FSHARP_TYPE,
        "vbproj" => VB_TYPE,
        "shproj" => SHARED_PROJECT_TYPE,
        _ => CSHARP_TYPE,
    }
}

pub fn new_guid() -> String {
    uuid::Uuid::new_v4().to_string().to_uppercase()
}

/// Line-based editor over a `.sln` text.
pub struct SlnEditor {
    lines: Vec<String>,
    newline: &'static str,
    trailing_newline: bool,
}

impl SlnEditor {
    pub fn new(text: &str) -> Self {
        let newline = if text.contains("\r\n") { "\r\n" } else { "\n" };
        Self {
            lines: text.lines().map(|l| l.trim_end_matches('\r').to_string()).collect(),
            newline,
            trailing_newline: text.ends_with('\n'),
        }
    }

    pub fn into_string(self) -> String {
        let mut text = self.lines.join(self.newline);
        if self.trailing_newline {
            text.push_str(self.newline);
        }
        text
    }

    fn find(&self, from: usize, pred: impl Fn(&str) -> bool) -> Option<usize> {
        (from..self.lines.len()).find(|&i| pred(self.lines[i].trim()))
    }

    fn project_block(&self, guid: &str) -> Option<(usize, usize)> {
        let needle = format!("\"{{{}}}\"", guid.to_uppercase());
        let start = self.find(0, |l| l.starts_with("Project(") && l.to_uppercase().ends_with(&needle))?;
        let end = self.find(start, |l| l == "EndProject")?;
        Some((start, end))
    }

    fn global_start(&self) -> Result<usize> {
        self.find(0, |l| l == "Global").context("the solution has no Global section")
    }

    /// The range of lines inside `GlobalSection(name)`, creating the section if asked.
    fn global_section(&mut self, name: &str, create: Option<&str>) -> Result<Option<(usize, usize)>> {
        let header = format!("GlobalSection({name})");
        if let Some(start) = self.find(0, |l| l.starts_with(&header)) {
            let end = self.find(start, |l| l == "EndGlobalSection").context("unterminated GlobalSection")?;
            return Ok(Some((start, end)));
        }
        let Some(when) = create else { return Ok(None) };
        let end_global = self.find(0, |l| l == "EndGlobal").context("the solution has no EndGlobal")?;
        self.lines.insert(end_global, format!("\t{header} = {when}"));
        self.lines.insert(end_global + 1, "\tEndGlobalSection".into());
        Ok(Some((end_global, end_global + 1)))
    }

    fn set_parent(&mut self, guid: &str, parent: Option<&str>) -> Result<()> {
        let child_key = format!("{{{}}}", guid.to_uppercase());
        if let Some((start, end)) = self.global_section("NestedProjects", None)?
            && let Some(i) = (start + 1..end).find(|&i| self.lines[i].trim().to_uppercase().starts_with(&child_key)) {
                self.lines.remove(i);
            }
        if let Some(parent) = parent {
            let (_, end) = self.global_section("NestedProjects", Some("preSolution"))?.unwrap();
            self.lines.insert(end, format!("\t\t{child_key} = {{{}}}", parent.to_uppercase()));
        }
        Ok(())
    }

    fn insert_project(&mut self, type_guid: &str, name: &str, path: &str, guid: &str) -> Result<()> {
        let at = self.global_start()?;
        self.lines.insert(at, format!("Project(\"{{{type_guid}}}\") = \"{name}\", \"{path}\", \"{{{guid}}}\""));
        self.lines.insert(at + 1, "EndProject".into());
        Ok(())
    }

    /// Adds a solution folder and returns its guid.
    pub fn create_folder(&mut self, name: &str, parent: Option<&str>) -> Result<String> {
        let sln = SlnFile::parse(&self.lines.join("\n"))?;
        let siblings_clash = sln.projects.iter().any(|p| {
            p.is_folder() && p.name.eq_ignore_ascii_case(name) && sln.nested.get(&p.guid).map(String::as_str) == parent.map(|g| g.to_uppercase()).as_deref()
        });
        if siblings_clash {
            bail!("a solution folder named \"{name}\" already exists there");
        }
        let guid = new_guid();
        self.insert_project(SOLUTION_FOLDER_TYPE, name, name, &guid)?;
        if parent.is_some() {
            self.set_parent(&guid, parent)?;
        }
        Ok(guid)
    }

    /// Removes a project or folder from the solution, with everything nested in it.
    pub fn remove(&mut self, guid: &str) -> Result<()> {
        let sln = SlnFile::parse(&self.lines.join("\n"))?;
        for guid in sln.descendants(guid) {
            if let Some((start, end)) = self.project_block(&guid) {
                self.lines.drain(start..=end);
            }
            let key = format!("{{{guid}}}");
            self.lines.retain(|line| !line.to_uppercase().contains(&key));
        }
        Ok(())
    }

    /// Renames a solution folder or a project's display name (and its path, for projects).
    pub fn rename(&mut self, guid: &str, name: &str, path: Option<&str>) -> Result<()> {
        let (start, _) = self.project_block(guid).context("not in the solution")?;
        let caps = PROJECT_LINE.captures(self.lines[start].trim()).context("malformed Project line")?;
        let path = path.map(str::to_string).unwrap_or_else(|| {
            if caps[1].eq_ignore_ascii_case(SOLUTION_FOLDER_TYPE) { name.to_string() } else { caps[3].to_string() }
        });
        self.lines[start] = format!("Project(\"{{{}}}\") = \"{name}\", \"{path}\", \"{{{}}}\"", &caps[1], &caps[4]);
        Ok(())
    }

    /// Moves a project or folder into `parent`, or to the root with `None`.
    pub fn move_to(&mut self, guid: &str, parent: Option<&str>) -> Result<()> {
        if let Some(parent) = parent {
            let sln = SlnFile::parse(&self.lines.join("\n"))?;
            if sln.descendants(guid).iter().any(|g| g.eq_ignore_ascii_case(parent)) {
                bail!("cannot move a folder into itself");
            }
        }
        self.set_parent(guid, parent)
    }

    /// Adds a project file, with entries for every solution configuration.
    pub fn add_project(&mut self, name: &str, relative_path: &str, type_guid: &str, guid: &str, parent: Option<&str>) -> Result<()> {
        let sln = SlnFile::parse(&self.lines.join("\n"))?;
        if sln.projects.iter().any(|p| p.path.eq_ignore_ascii_case(relative_path)) {
            bail!("{relative_path} is already in the solution");
        }
        self.insert_project(type_guid, name, relative_path, guid)?;
        if !sln.configurations.is_empty() {
            let (_, end) = self.global_section("ProjectConfigurationPlatforms", Some("postSolution"))?.unwrap();
            let mut at = end;
            for configuration in &sln.configurations {
                // SDK projects only know "Any CPU" unless they say otherwise.
                let config = configuration.split_once('|').map_or(configuration.as_str(), |(config, _)| config);
                let target = format!("{config}|Any CPU");
                for suffix in ["ActiveCfg", "Build.0"] {
                    self.lines.insert(at, format!("\t\t{{{guid}}}.{configuration}.{suffix} = {target}"));
                    at += 1;
                }
            }
        }
        if parent.is_some() {
            self.set_parent(guid, parent)?;
        }
        Ok(())
    }

    /// Adds a file to a solution folder's `SolutionItems`.
    pub fn add_solution_item(&mut self, folder: &str, relative_path: &str) -> Result<()> {
        let (start, end) = self.project_block(folder).context("solution folder not found")?;
        let entry = format!("\t\t{relative_path} = {relative_path}");
        if let Some(section) = (start..end).find(|&i| self.lines[i].trim().starts_with("ProjectSection(SolutionItems)")) {
            let section_end = self.find(section, |l| l == "EndProjectSection").context("unterminated ProjectSection")?;
            if (section..section_end).any(|i| self.lines[i].trim().split('=').next().map(str::trim) == Some(relative_path)) {
                return Ok(());
            }
            self.lines.insert(section_end, entry);
        } else {
            self.lines.insert(end, "\tProjectSection(SolutionItems) = preProject".into());
            self.lines.insert(end + 1, entry);
            self.lines.insert(end + 2, "\tEndProjectSection".into());
        }
        Ok(())
    }

    pub fn remove_solution_item(&mut self, folder: &str, relative_path: &str) -> Result<()> {
        let (start, end) = self.project_block(folder).context("solution folder not found")?;
        if let Some(i) = (start..end).find(|&i| {
            self.lines[i].split('=').next().map(str::trim).is_some_and(|key| key.eq_ignore_ascii_case(relative_path))
        }) {
            self.lines.remove(i);
        }
        // Drop the section when it is left empty.
        if let Some((start, end)) = self.project_block(folder)
            && let Some(section) = (start..end).find(|&i| self.lines[i].trim().starts_with("ProjectSection(SolutionItems)"))
                && self.lines.get(section + 1).map(|l| l.trim()) == Some("EndProjectSection") {
                    self.lines.drain(section..=section + 1);
                }
        Ok(())
    }
}

/// An empty solution, as `dotnet new sln` writes it.
pub fn empty_solution() -> String {
    [
        "",
        "Microsoft Visual Studio Solution File, Format Version 12.00",
        "# Visual Studio Version 17",
        "VisualStudioVersion = 17.0.31903.59",
        "MinimumVisualStudioVersion = 10.0.40219.1",
        "Global",
        "\tGlobalSection(SolutionConfigurationPlatforms) = preSolution",
        "\t\tDebug|Any CPU = Debug|Any CPU",
        "\t\tRelease|Any CPU = Release|Any CPU",
        "\tEndGlobalSection",
        "\tGlobalSection(SolutionProperties) = preSolution",
        "\t\tHideSolutionNode = FALSE",
        "\tEndGlobalSection",
        "EndGlobal",
        "",
    ]
    .join("\r\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    pub const SAMPLE: &str = "\r
Microsoft Visual Studio Solution File, Format Version 12.00\r
# Visual Studio Version 17\r
VisualStudioVersion = 17.0.31903.59\r
Project(\"{2150E333-8FDC-42A3-9474-1A3956D46DE8}\") = \"src\", \"src\", \"{11111111-1111-1111-1111-111111111111}\"\r
\tProjectSection(SolutionItems) = preProject\r
\t\tREADME.md = README.md\r
\tEndProjectSection\r
EndProject\r
Project(\"{FAE04EC0-301F-11D3-BF4B-00C04F79EFBC}\") = \"App\", \"src\\App\\App.csproj\", \"{22222222-2222-2222-2222-222222222222}\"\r
EndProject\r
Global\r
\tGlobalSection(SolutionConfigurationPlatforms) = preSolution\r
\t\tDebug|Any CPU = Debug|Any CPU\r
\tEndGlobalSection\r
\tGlobalSection(ProjectConfigurationPlatforms) = postSolution\r
\t\t{22222222-2222-2222-2222-222222222222}.Debug|Any CPU.ActiveCfg = Debug|Any CPU\r
\t\t{22222222-2222-2222-2222-222222222222}.Debug|Any CPU.Build.0 = Debug|Any CPU\r
\tEndGlobalSection\r
\tGlobalSection(NestedProjects) = preSolution\r
\t\t{22222222-2222-2222-2222-222222222222} = {11111111-1111-1111-1111-111111111111}\r
\tEndGlobalSection\r
EndGlobal\r
";

    #[test]
    fn parses_projects_folders_and_configurations() {
        let sln = SlnFile::parse(SAMPLE).unwrap();
        assert_eq!(sln.format_version.as_deref(), Some("12.00"));
        assert_eq!(sln.projects.len(), 2);
        assert!(sln.projects[0].is_folder());
        assert_eq!(sln.projects[0].solution_items, vec!["README.md"]);
        assert_eq!(sln.projects[1].path, r"src\App\App.csproj");
        assert_eq!(sln.nested["22222222-2222-2222-2222-222222222222"], "11111111-1111-1111-1111-111111111111");
        assert_eq!(sln.configurations, vec!["Debug|Any CPU"]);
        assert_eq!(sln.project_configuration("22222222-2222-2222-2222-222222222222", "Debug|Any CPU"), Some("Debug|Any CPU"));
        assert!(sln.builds("22222222-2222-2222-2222-222222222222", "Debug|Any CPU"));
    }

    #[test]
    fn folder_edits_keep_crlf() {
        let mut editor = SlnEditor::new(SAMPLE);
        let guid = editor.create_folder("tests", Some("11111111-1111-1111-1111-111111111111")).unwrap();
        editor.rename(&guid, "specs", None).unwrap();
        let text = editor.into_string();
        assert!(!text.replace("\r\n", "").contains('\n'));
        let sln = SlnFile::parse(&text).unwrap();
        let folder = sln.project(&guid).unwrap();
        assert_eq!((folder.name.as_str(), folder.path.as_str()), ("specs", "specs"));
        assert_eq!(sln.nested[&guid], "11111111-1111-1111-1111-111111111111");

        let mut editor = SlnEditor::new(&text);
        assert!(editor.create_folder("specs", Some("11111111-1111-1111-1111-111111111111")).is_err());
        editor.move_to(&guid, None).unwrap();
        assert!(editor.move_to("11111111-1111-1111-1111-111111111111", Some("22222222-2222-2222-2222-222222222222")).is_err());
        let sln = SlnFile::parse(&editor.into_string()).unwrap();
        assert!(!sln.nested.contains_key(&guid));
    }

    #[test]
    fn removing_a_folder_removes_its_projects_and_their_configurations() {
        let mut editor = SlnEditor::new(SAMPLE);
        editor.remove("11111111-1111-1111-1111-111111111111").unwrap();
        let text = editor.into_string();
        let sln = SlnFile::parse(&text).unwrap();
        assert!(sln.projects.is_empty());
        assert!(!text.contains("2222"));
        assert!(text.contains("GlobalSection(NestedProjects)"));
    }

    #[test]
    fn adding_projects_and_solution_items() {
        let mut editor = SlnEditor::new(SAMPLE);
        editor.add_project("Lib", r"src\Lib\Lib.csproj", CSHARP_TYPE, "33333333-3333-3333-3333-333333333333", Some("11111111-1111-1111-1111-111111111111")).unwrap();
        editor.add_solution_item("11111111-1111-1111-1111-111111111111", ".editorconfig").unwrap();
        editor.remove_solution_item("11111111-1111-1111-1111-111111111111", "README.md").unwrap();
        let sln = SlnFile::parse(&editor.into_string()).unwrap();
        assert_eq!(sln.projects[0].solution_items, vec![".editorconfig"]);
        assert_eq!(sln.projects[2].name, "Lib");
        assert!(sln.builds("33333333-3333-3333-3333-333333333333", "Debug|Any CPU"));
        assert_eq!(sln.nested["33333333-3333-3333-3333-333333333333"], "11111111-1111-1111-1111-111111111111");

        let mut editor = SlnEditor::new(SAMPLE);
        editor.remove_solution_item("11111111-1111-1111-1111-111111111111", "README.md").unwrap();
        assert!(!editor.into_string().contains("ProjectSection(SolutionItems)"));
    }

    #[test]
    fn new_solution_parses() {
        let sln = SlnFile::parse(&empty_solution()).unwrap();
        assert_eq!(sln.configurations.len(), 2);
    }
}
