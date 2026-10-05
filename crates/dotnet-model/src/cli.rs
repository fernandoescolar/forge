//! `dotnet` command lines: the defaults for build, test, new project and the rest, which
//! users can replace per command; and reading `dotnet new list`.

use std::collections::HashMap;

/// The commands Forge runs, as argument templates with `$parameters`.
pub fn default_commands() -> HashMap<&'static str, Vec<&'static str>> {
    HashMap::from([
        ("build", vec!["dotnet", "build", "$projectPath"]),
        ("clean", vec!["dotnet", "clean", "$projectPath"]),
        ("pack", vec!["dotnet", "pack", "$projectPath"]),
        ("publish", vec!["dotnet", "publish", "$projectPath"]),
        ("restore", vec!["dotnet", "restore", "$projectPath"]),
        ("rebuild", vec!["dotnet", "build", "$projectPath", "--no-incremental"]),
        ("run", vec!["dotnet", "run", "--project", "$projectPath"]),
        ("watch", vec!["dotnet", "watch", "run", "--project", "$projectPath"]),
        ("test", vec!["dotnet", "test", "$projectPath"]),
        ("createProject", vec!["dotnet", "new", "$projectType", "-lang", "$language", "-n", "$projectName", "-o", "$folderName", "-f", "$framework"]),
        ("createSolution", vec!["dotnet", "new", "sln", "-n", "$solutionName", "--format", "$format"]),
        ("installTool", vec!["dotnet", "tool", "install", "$tool"]),
    ])
}

/// The arguments for a command: the user's version when there is one, with parameters
/// replaced. An empty optional parameter drops the `-flag` before it as well.
pub fn command_line(name: &str, params: &[(&str, &str)], custom: &HashMap<String, Vec<String>>) -> Vec<String> {
    let template: Vec<String> = custom
        .get(name)
        .cloned()
        .or_else(|| default_commands().get(name).map(|args| args.iter().map(|s| s.to_string()).collect()))
        .unwrap_or_default();
    let mut out: Vec<String> = Vec::new();
    for arg in template {
        let bare = arg.trim_matches('"');
        if let Some(param) = bare.strip_prefix('$') {
            let value = params.iter().find(|(k, _)| *k == param).map(|(_, v)| *v).unwrap_or("");
            if value.is_empty() {
                if out.last().is_some_and(|prev| prev.starts_with('-')) {
                    out.pop();
                }
                continue;
            }
            out.push(value.to_string());
        } else {
            let mut arg = arg.clone();
            for (key, value) in params {
                arg = arg.replace(&format!("${key}"), value);
            }
            out.push(arg);
        }
    }
    out
}

/// Quotes arguments for a POSIX shell or `cmd`/PowerShell, for showing and running a
/// command in a terminal.
pub fn shell_line(args: &[String]) -> String {
    args.iter().map(|a| quote(a)).collect::<Vec<_>>().join(" ")
}

/// Quotes one argument for the platform's shell, only when it needs it.
pub fn quote(arg: &str) -> String {
    let safe = !arg.is_empty() && arg.chars().all(|c| c.is_alphanumeric() || "-_./:=@+,%".contains(c) || (!cfg!(windows) && c == '\\'));
    if safe {
        arg.to_string()
    } else if cfg!(windows) {
        format!("\"{}\"", arg.replace('"', "\\\""))
    } else {
        format!("'{}'", arg.replace('\'', "'\\''"))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProjectTemplate {
    pub name: String,
    pub short_names: Vec<String>,
    /// Languages, the default first (`[C#]` marks it).
    pub languages: Vec<String>,
    pub tags: Vec<String>,
}

/// Parses the table `dotnet new list --type project` prints.
pub fn parse_template_list(output: &str) -> Vec<ProjectTemplate> {
    let lines: Vec<&str> = output.lines().collect();
    let Some(dashes) = lines.iter().position(|l| l.trim_start().starts_with("---")) else { return Vec::new() };
    // Column starts are where each run of dashes starts.
    let rule = lines[dashes];
    let mut starts = Vec::new();
    let bytes: Vec<char> = rule.chars().collect();
    for (i, c) in bytes.iter().enumerate() {
        if *c == '-' && (i == 0 || bytes[i - 1] == ' ') {
            starts.push(i);
        }
    }
    let column = |line: &str, index: usize| -> String {
        let chars: Vec<char> = line.chars().collect();
        let start = starts[index].min(chars.len());
        let end = starts.get(index + 1).copied().unwrap_or(chars.len()).min(chars.len());
        chars[start..end].iter().collect::<String>().trim().to_string()
    };
    let list = |text: String| -> Vec<String> { text.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect() };
    lines[dashes + 1..]
        .iter()
        .filter(|l| !l.trim().is_empty() && starts.len() >= 2)
        .map(|line| {
            let mut languages = list(column(line, 2.min(starts.len() - 1)));
            if let Some(default) = languages.iter().position(|l| l.starts_with('[')) {
                let lang = languages.remove(default);
                languages.insert(0, lang.trim_matches(['[', ']']).to_string());
            }
            ProjectTemplate {
                name: column(line, 0),
                short_names: list(column(line, 1)),
                languages: if starts.len() > 3 { languages } else { Vec::new() },
                tags: if starts.len() > 3 { list(column(line, 3)).into_iter().flat_map(|t| t.split('/').map(str::to_string).collect::<Vec<_>>()).collect() } else { Vec::new() },
            }
        })
        .filter(|t| !t.short_names.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_lines_drop_empty_flags() {
        let custom = HashMap::new();
        let args = command_line("createProject", &[("projectType", "console"), ("language", "C#"), ("projectName", "App"), ("folderName", "src/App"), ("framework", "")], &custom);
        assert_eq!(args, vec!["dotnet", "new", "console", "-lang", "C#", "-n", "App", "-o", "src/App"]);
        let custom = HashMap::from([("build".to_string(), vec!["dotnet".to_string(), "build".to_string(), "\"$projectPath\"".to_string(), "-c".to_string(), "Release".to_string()])]);
        assert_eq!(command_line("build", &[("projectPath", "/a b/x.csproj")], &custom), vec!["dotnet", "build", "/a b/x.csproj", "-c", "Release"]);
        if !cfg!(windows) {
            assert_eq!(shell_line(&["dotnet".into(), "build".into(), "/a b/it's.csproj".into()]), "dotnet build '/a b/it'\\''s.csproj'");
        }
    }

    #[test]
    fn reads_the_template_table() {
        let output = "These templates matched your input: \n\nTemplate Name                 Short Name      Language    Tags\n----------------------------  --------------  ----------  --------------------\nASP.NET Core Empty            web             [C#],F#     Web/Empty\nConsole App                   console,cons    [C#],F#,VB  Common/Console\nClass Library                 classlib        [C#],F#,VB  Common/Library\n";
        let templates = parse_template_list(output);
        assert_eq!(templates.len(), 3);
        assert_eq!(templates[1].short_names, vec!["console", "cons"]);
        assert_eq!(templates[1].languages, vec!["C#", "F#", "VB"]);
        assert_eq!(templates[0].tags, vec!["Web", "Empty"]);
    }
}
