//! The user's skills and prompts, for every agent: Markdown files in the project's `.forge/`
//! folder (and in Forge's config folder, for every project; the project's win by name).
//!
//! - Skills (`.forge/skills/*.md`, or `.forge/skills/<name>/SKILL.md`) are tools of Forge's
//!   MCP server: agents see each one's description and call it to get its instructions
//!   when the task matches.
//! - Prompts (`.forge/prompts/*.md`) are slash commands: `/name what to do` sends the file,
//!   with `$ARGUMENTS` replaced by what follows the command (or that text added after it).
//!
//! Both may start with front matter: `name:` (else the file's name) and `description:`
//! (else the first line of the text).

use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq)]
pub struct Doc {
    pub name: String,
    pub description: String,
    /// The text after the front matter.
    pub body: String,
    pub path: PathBuf,
}

/// Where skills and prompts are looked for: Forge's config folder, then the project's.
fn folders(root: &Path, kind: &str) -> [PathBuf; 2] {
    [paths::config_dir().join(kind), root.join(".forge").join(kind)]
}

pub fn skills(root: &Path) -> Vec<Doc> {
    collect(&folders(root, "skills"), true)
}

pub fn prompts(root: &Path) -> Vec<Doc> {
    collect(&folders(root, "prompts"), false)
}

/// The docs in `folders`, by name (a later folder's replace an earlier one's), sorted.
fn collect(folders: &[PathBuf], skill_folders: bool) -> Vec<Doc> {
    let mut docs: Vec<Doc> = Vec::new();
    for folder in folders {
        let Ok(entries) = std::fs::read_dir(folder) else { continue };
        let mut files: Vec<(PathBuf, String)> = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if skill_folders && path.join("SKILL.md").is_file() {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    files.push((path.join("SKILL.md"), name));
                }
            } else if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("md")) {
                let name = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                files.push((path, name));
            }
        }
        for (path, file_name) in files {
            let Ok(text) = std::fs::read_to_string(&path) else { continue };
            let doc = parse(&text, &file_name, path);
            if doc.name.is_empty() {
                continue;
            }
            docs.retain(|d| d.name != doc.name);
            docs.push(doc);
        }
    }
    docs.sort_by(|a, b| a.name.cmp(&b.name));
    docs
}

/// A doc from its text: front matter (`name`, `description`), then the body.
pub fn parse(text: &str, file_name: &str, path: PathBuf) -> Doc {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let (front, body) = match text.strip_prefix("---").filter(|rest| rest.starts_with('\n') || rest.starts_with("\r\n")) {
        Some(rest) => match rest.find("\n---") {
            Some(end) => {
                let after = &rest[end + 4..];
                (&rest[..end], after.split_once('\n').map(|(_, body)| body).unwrap_or(""))
            }
            None => ("", text),
        },
        None => ("", text),
    };
    let field = |key: &str| {
        front.lines().find_map(|line| {
            let (k, v) = line.split_once(':')?;
            (k.trim() == key).then(|| v.trim().trim_matches(|c| c == '"' || c == '\'').to_string()).filter(|v| !v.is_empty())
        })
    };
    let body = body.trim().to_string();
    let name = field("name").unwrap_or_else(|| file_name.to_string());
    let description = field("description").unwrap_or_else(|| {
        let first = body.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or_default();
        first.trim_start_matches('#').trim().to_string()
    });
    Doc { name: name.trim().to_string(), description, body, path }
}

/// The MCP tool a skill is: `skill_<name>` (letters, digits, `_` and `-`; at most 64).
pub fn tool_name(skill: &str) -> String {
    let clean: String = skill.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
    format!("skill_{clean}").chars().take(64).collect()
}

/// The skill a tool name stands for.
pub fn skill_for_tool(root: &Path, tool: &str) -> Option<Doc> {
    skills(root).into_iter().find(|s| tool_name(&s.name) == tool)
}

/// What a skill's tool answers: its instructions, and where its files are.
pub fn skill_reply(skill: &Doc) -> String {
    let folder = skill.path.parent().map(|p| p.display().to_string()).unwrap_or_default();
    format!("Follow the skill \"{}\" for this task. (It is in {}; relative paths in it are relative to that folder.)\n\n{}", skill.name, folder, skill.body)
}

/// `/name rest` as the prompt `name`, when there is one: the prompt and the text to send.
pub fn expand_prompt(text: &str, root: &Path) -> Option<(Doc, String)> {
    let command = text.strip_prefix('/')?;
    let (name, rest) = command.split_once(char::is_whitespace).unwrap_or((command, ""));
    let prompt = prompts(root).into_iter().find(|p| p.name == name)?;
    let rest = rest.trim();
    let expanded = if prompt.body.contains("$ARGUMENTS") {
        prompt.body.replace("$ARGUMENTS", rest)
    } else if rest.is_empty() {
        prompt.body.clone()
    } else {
        format!("{}\n\n{rest}", prompt.body)
    };
    Some((prompt, expanded))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_front_matter_or_falls_back() {
        let doc = parse("---\nname: review\ndescription: \"Review a change\"\n---\n\nLook at $ARGUMENTS.\n", "x", "x.md".into());
        assert_eq!((doc.name.as_str(), doc.description.as_str(), doc.body.as_str()), ("review", "Review a change", "Look at $ARGUMENTS."));
        let plain = parse("# Release notes\n\nWrite them.", "notes", "notes.md".into());
        assert_eq!((plain.name.as_str(), plain.description.as_str()), ("notes", "Release notes"));
        assert_eq!(tool_name("db migrations"), "skill_db_migrations");
    }

    #[test]
    fn finds_skills_and_expands_prompts() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".forge/skills/deploy")).unwrap();
        std::fs::create_dir_all(root.join(".forge/prompts")).unwrap();
        std::fs::write(root.join(".forge/skills/migrations.md"), "---\ndescription: Add a database migration\n---\nRun `make migration`.").unwrap();
        std::fs::write(root.join(".forge/skills/deploy/SKILL.md"), "---\nname: deploy\ndescription: Deploy to staging\n---\nSee ./steps.md").unwrap();
        std::fs::write(root.join(".forge/skills/notes.txt"), "not a skill").unwrap();
        std::fs::write(root.join(".forge/prompts/review.md"), "Review $ARGUMENTS for bugs.").unwrap();
        std::fs::write(root.join(".forge/prompts/explain.md"), "Explain this code.").unwrap();

        let names: Vec<String> = skills(root).into_iter().filter(|s| s.path.starts_with(root)).map(|s| s.name).collect();
        assert_eq!(names, ["deploy", "migrations"]);
        let deploy = skill_for_tool(root, "skill_deploy").unwrap();
        assert!(skill_reply(&deploy).contains(&format!("It is in {}", root.join(".forge/skills/deploy").display())));

        assert_eq!(expand_prompt("/review src/a.rs", root).unwrap().1, "Review src/a.rs for bugs.");
        assert_eq!(expand_prompt("/explain", root).unwrap().1, "Explain this code.");
        assert_eq!(expand_prompt("/explain the parser\nplease", root).unwrap().1, "Explain this code.\n\nthe parser\nplease");
        assert_eq!(expand_prompt("/usage", root), None);
        assert_eq!(expand_prompt("review this", root), None);
    }
}
