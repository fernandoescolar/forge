//! XML solution files (`.slnx`). Folders are flat elements named by their full path
//! (`/src/libs/`); a folder's parent is implied by its name.

use anyhow::{Context as _, Result, bail};

use crate::xml_edit::{Element, NewElement, XmlText};

#[derive(Clone, Debug, PartialEq, Default)]
pub struct SlnxFolder {
    /// The full name, normalized to `/a/b/`.
    pub name: String,
    /// Project paths as written, relative to the solution.
    pub projects: Vec<String>,
    pub files: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct SlnxFile {
    pub folders: Vec<SlnxFolder>,
    pub projects: Vec<String>,
    pub build_types: Vec<String>,
    pub platforms: Vec<String>,
}

/// `src/libs` or `/src/libs` → `/src/libs/`.
pub fn normalize_folder(name: &str) -> String {
    let trimmed = name.trim().trim_matches(['/', '\\']).replace('\\', "/");
    if trimmed.is_empty() { "/".into() } else { format!("/{trimmed}/") }
}

/// The leaf of a folder name: `/src/libs/` → `libs`.
pub fn folder_leaf(name: &str) -> &str {
    name.trim_matches('/').rsplit('/').next().unwrap_or("")
}

/// The parent of a folder name: `/src/libs/` → `Some("/src/")`.
pub fn folder_parent(name: &str) -> Option<String> {
    let trimmed = name.trim_matches('/');
    trimmed.rsplit_once('/').map(|(parent, _)| format!("/{parent}/"))
}

fn same_path(a: &str, b: &str) -> bool {
    a.replace('\\', "/").eq_ignore_ascii_case(&b.replace('\\', "/"))
}

impl SlnxFile {
    pub fn parse(text: &str) -> Result<Self> {
        let xml = XmlText::new(text);
        let elements = xml.elements()?;
        let root = elements.first().context("empty solution")?;
        if root.name != "Solution" {
            bail!("not a .slnx solution");
        }
        let mut file = SlnxFile::default();
        for element in &elements {
            let parent = element.parent().and_then(|p| elements.iter().find(|e| e.start() == p));
            match (element.name.as_str(), parent.map(|p| p.name.as_str())) {
                ("Folder", Some("Solution")) => {
                    let Some(name) = element.attr("Name") else { continue };
                    let mut folder = SlnxFolder { name: normalize_folder(name), ..Default::default() };
                    for child in elements.iter().filter(|e| e.parent() == Some(element.start())) {
                        match (child.name.as_str(), child.attr("Path")) {
                            ("Project", Some(path)) => folder.projects.push(path.to_string()),
                            ("File", Some(path)) => folder.files.push(path.to_string()),
                            _ => {}
                        }
                    }
                    file.folders.push(folder);
                }
                ("Project", Some("Solution")) => {
                    if let Some(path) = element.attr("Path") {
                        file.projects.push(path.to_string());
                    }
                }
                ("BuildType", Some("Configurations")) => file.build_types.extend(element.attr("Name").map(str::to_string)),
                ("Platform", Some("Configurations")) => file.platforms.extend(element.attr("Name").map(str::to_string)),
                _ => {}
            }
        }
        Ok(file)
    }

    /// Every folder, including the ones only implied by a nested folder's name, sorted.
    pub fn all_folder_names(&self) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        for folder in &self.folders {
            let mut current = Some(folder.name.clone());
            while let Some(name) = current {
                if !names.iter().any(|n| n.eq_ignore_ascii_case(&name)) {
                    names.push(name.clone());
                }
                current = folder_parent(&name);
            }
        }
        names.sort_by_key(|n| n.to_lowercase());
        names
    }

    /// Solution configurations as `Debug|Any CPU` pairs.
    pub fn configurations(&self) -> Vec<String> {
        let build_types = if self.build_types.is_empty() { vec!["Debug".to_string(), "Release".to_string()] } else { self.build_types.clone() };
        let platforms = if self.platforms.is_empty() { vec!["Any CPU".to_string()] } else { self.platforms.clone() };
        build_types.iter().flat_map(|b| platforms.iter().map(move |p| format!("{b}|{p}"))).collect()
    }
}

/// Edits over a `.slnx` text that keep the rest of it as it was.
pub struct SlnxEditor {
    xml: XmlText,
}

impl SlnxEditor {
    pub fn new(text: &str) -> Self {
        Self { xml: XmlText::new(text) }
    }

    pub fn into_string(self) -> String {
        self.xml.into_string()
    }

    fn solution(&self) -> Result<Element> {
        self.xml.root()
    }

    fn folder_elements(&self) -> Result<Vec<Element>> {
        let root = self.solution()?;
        Ok(self.xml.children(root.start())?.into_iter().filter(|e| e.name == "Folder").collect())
    }

    fn folder_element(&self, name: &str) -> Result<Option<Element>> {
        Ok(self.folder_elements()?.into_iter().find(|e| e.attr("Name").is_some_and(|n| normalize_folder(n).eq_ignore_ascii_case(name))))
    }

    /// The folder's element, creating it when the folder only exists implicitly.
    fn ensure_folder(&mut self, name: &str) -> Result<Element> {
        if let Some(element) = self.folder_element(name)? {
            return Ok(element);
        }
        self.insert_folder(name)?;
        self.folder_element(name)?.context("folder not created")
    }

    fn insert_folder(&mut self, name: &str) -> Result<()> {
        let new = NewElement::new("Folder").attr("Name", name);
        let root = self.solution()?;
        let children = self.xml.children(root.start())?;
        if let Some(last_folder) = children.iter().filter(|e| e.name == "Folder").next_back() {
            self.xml.insert_after(last_folder.start(), &new)?;
        } else if let Some(first_project) = children.iter().find(|e| e.name == "Project") {
            self.xml.insert_before(first_project.start(), &new)?;
        } else {
            self.xml.append_child(root.start(), &new)?;
        }
        Ok(())
    }

    pub fn create_folder(&mut self, parent: Option<&str>, name: &str) -> Result<String> {
        if name.contains(['/', '\\']) {
            bail!("folder names cannot contain slashes");
        }
        let full = normalize_folder(&format!("{}{name}", parent.map(normalize_folder).unwrap_or_else(|| "/".into())));
        let file = SlnxFile::parse(self.xml.as_str())?;
        if file.all_folder_names().iter().any(|n| n.eq_ignore_ascii_case(&full)) {
            bail!("a solution folder named \"{name}\" already exists there");
        }
        self.insert_folder(&full)?;
        Ok(full)
    }

    /// Removes a folder, its subfolders and the projects in them.
    pub fn delete_folder(&mut self, name: &str) -> Result<()> {
        let prefix = normalize_folder(name).to_lowercase();
        loop {
            let next = self.folder_elements()?.into_iter().find(|e| e.attr("Name").is_some_and(|n| normalize_folder(n).to_lowercase().starts_with(&prefix)));
            let Some(element) = next else { break };
            self.xml.remove_element(element.start())?;
        }
        Ok(())
    }

    fn rewrite_folder_prefix(&mut self, from: &str, to: &str) -> Result<()> {
        let from = normalize_folder(from);
        let to = normalize_folder(to);
        for element in self.folder_elements()?.into_iter().rev() {
            let Some(name) = element.attr("Name") else { continue };
            let normalized = normalize_folder(name);
            if normalized.to_lowercase().starts_with(&from.to_lowercase()) {
                let renamed = format!("{to}{}", &normalized[from.len()..]);
                self.xml.set_attribute(element.start(), "Name", &renamed)?;
            }
        }
        Ok(())
    }

    pub fn rename_folder(&mut self, name: &str, new_leaf: &str) -> Result<String> {
        let name = normalize_folder(name);
        let parent = folder_parent(&name).unwrap_or_else(|| "/".into());
        let renamed = normalize_folder(&format!("{parent}{new_leaf}"));
        let file = SlnxFile::parse(self.xml.as_str())?;
        if !renamed.eq_ignore_ascii_case(&name) && file.all_folder_names().iter().any(|n| n.eq_ignore_ascii_case(&renamed)) {
            bail!("a solution folder named \"{new_leaf}\" already exists there");
        }
        self.ensure_folder(&name)?;
        self.rewrite_folder_prefix(&name, &renamed)?;
        Ok(renamed)
    }

    pub fn move_folder(&mut self, name: &str, new_parent: Option<&str>) -> Result<String> {
        let name = normalize_folder(name);
        let parent = new_parent.map(normalize_folder).unwrap_or_else(|| "/".into());
        if parent.to_lowercase().starts_with(&name.to_lowercase()) {
            bail!("cannot move a folder into itself");
        }
        let moved = normalize_folder(&format!("{parent}{}", folder_leaf(&name)));
        self.ensure_folder(&name)?;
        self.rewrite_folder_prefix(&name, &moved)?;
        Ok(moved)
    }

    fn item_element(&self, kind: &str, path: &str) -> Result<Option<Element>> {
        Ok(self.xml.elements()?.into_iter().find(|e| e.name == kind && e.attr("Path").is_some_and(|p| same_path(p, path))))
    }

    /// Moves a project into a folder, or to the root with `None`.
    pub fn move_project(&mut self, path: &str, folder: Option<&str>) -> Result<()> {
        let target = match folder {
            Some(folder) => self.ensure_folder(&normalize_folder(folder))?,
            None => self.solution()?,
        };
        let element = self.item_element("Project", path)?.context("project not in the solution")?;
        if element.parent() != Some(target.start()) {
            self.xml.move_element(element.start(), target.start())?;
        }
        Ok(())
    }

    pub fn add_project(&mut self, path: &str, folder: Option<&str>) -> Result<()> {
        if self.item_element("Project", path)?.is_some() {
            bail!("{path} is already in the solution");
        }
        let target = match folder {
            Some(folder) => self.ensure_folder(&normalize_folder(folder))?,
            None => self.solution()?,
        };
        self.xml.append_child(target.start(), &NewElement::new("Project").attr("Path", path.replace('\\', "/")))?;
        Ok(())
    }

    pub fn remove_project(&mut self, path: &str) -> Result<()> {
        let element = self.item_element("Project", path)?.context("project not in the solution")?;
        self.xml.remove_element(element.start())
    }

    pub fn rename_project(&mut self, path: &str, new_path: &str) -> Result<()> {
        let element = self.item_element("Project", path)?.context("project not in the solution")?;
        self.xml.set_attribute(element.start(), "Path", &new_path.replace('\\', "/"))
    }

    pub fn add_file(&mut self, folder: &str, path: &str) -> Result<()> {
        let folder = self.ensure_folder(&normalize_folder(folder))?;
        let exists = self.xml.children(folder.start())?.iter().any(|e| e.name == "File" && e.attr("Path").is_some_and(|p| same_path(p, path)));
        if !exists {
            self.xml.append_child(folder.start(), &NewElement::new("File").attr("Path", path.replace('\\', "/")))?;
        }
        Ok(())
    }

    pub fn remove_file(&mut self, folder: &str, path: &str) -> Result<()> {
        let Some(folder) = self.folder_element(&normalize_folder(folder))? else { return Ok(()) };
        if let Some(file) = self.xml.children(folder.start())?.into_iter().find(|e| e.name == "File" && e.attr("Path").is_some_and(|p| same_path(p, path))) {
            self.xml.remove_element(file.start())?;
        }
        Ok(())
    }
}

pub fn empty_solution() -> String {
    "<Solution>\n</Solution>\n".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<Solution>
  <Configurations>
    <Platform Name="Any CPU" />
    <Platform Name="x64" />
  </Configurations>
  <Folder Name="/Solution Items/">
    <File Path="README.md" />
  </Folder>
  <Folder Name="/src/libs/">
    <Project Path="src/Lib/Lib.csproj" />
  </Folder>
  <Project Path="src/App/App.csproj" />
</Solution>
"#;

    #[test]
    fn parses_folders_projects_and_implied_folders() {
        let file = SlnxFile::parse(SAMPLE).unwrap();
        assert_eq!(file.folders.len(), 2);
        assert_eq!(file.folders[0].files, vec!["README.md"]);
        assert_eq!(file.projects, vec!["src/App/App.csproj"]);
        assert_eq!(file.all_folder_names(), vec!["/Solution Items/", "/src/", "/src/libs/"]);
        assert_eq!(file.configurations(), vec!["Debug|Any CPU", "Debug|x64", "Release|Any CPU", "Release|x64"]);
    }

    #[test]
    fn folder_edits() {
        let mut editor = SlnxEditor::new(SAMPLE);
        assert_eq!(editor.create_folder(Some("/src/"), "tests").unwrap(), "/src/tests/");
        assert!(editor.create_folder(None, "src").is_err());
        assert_eq!(editor.rename_folder("/src/", "source").unwrap(), "/source/");
        let file = SlnxFile::parse(&editor.into_string()).unwrap();
        assert_eq!(file.all_folder_names(), vec!["/Solution Items/", "/source/", "/source/libs/", "/source/tests/"]);
    }

    #[test]
    fn moving_and_deleting() {
        let mut editor = SlnxEditor::new(SAMPLE);
        editor.move_project("src\\App\\App.csproj", Some("/apps/")).unwrap();
        assert!(editor.move_folder("/src/", Some("/src/libs/")).is_err());
        editor.move_folder("/src/libs/", Some("/apps/")).unwrap();
        let text = editor.into_string();
        let file = SlnxFile::parse(&text).unwrap();
        assert!(file.projects.is_empty());
        let apps = file.folders.iter().find(|f| f.name == "/apps/").unwrap();
        assert_eq!(apps.projects, vec!["src/App/App.csproj"]);
        assert!(file.folders.iter().any(|f| f.name == "/apps/libs/"));

        let mut editor = SlnxEditor::new(&text);
        editor.delete_folder("/apps/").unwrap();
        let file = SlnxFile::parse(&editor.into_string()).unwrap();
        assert_eq!(file.all_folder_names(), vec!["/Solution Items/"]);
    }

    #[test]
    fn files_and_projects() {
        let mut editor = SlnxEditor::new(SAMPLE);
        editor.add_file("/Solution Items/", ".editorconfig").unwrap();
        editor.remove_file("/Solution Items/", "README.md").unwrap();
        editor.add_project("tests/T/T.csproj", None).unwrap();
        assert!(editor.add_project("tests/T/T.csproj", None).is_err());
        editor.remove_project("src/Lib/Lib.csproj").unwrap();
        editor.rename_project("src/App/App.csproj", "src/App/Web.csproj").unwrap();
        let file = SlnxFile::parse(&editor.into_string()).unwrap();
        assert_eq!(file.folders[0].files, vec![".editorconfig"]);
        assert_eq!(file.projects, vec!["src/App/Web.csproj", "tests/T/T.csproj"]);
        assert!(file.folders[1].projects.is_empty());
    }
}
