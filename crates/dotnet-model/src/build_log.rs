//! Errors and warnings in MSBuild's output. MSBuild, the compilers and the tools it runs
//! all write them in its canonical format:
//!
//! ```text
//! /src/App/Program.cs(12,5): error CS0103: The name 'x' does not exist [/src/App/App.csproj]
//! /src/App/App.csproj : error NU1101: Unable to find package Nope. [/src/App/App.csproj]
//! MSBUILD : error MSB1009: Project file does not exist.
//! ```
//!
//! The location after the file is `(line)`, `(line-endLine)`, `(line,col)`,
//! `(line,col-endCol)` or `(line,col,endLine,endCol)`, and may be missing. The project in
//! brackets can carry `::TargetFramework=…` when a project builds for several frameworks,
//! which repeats the same problem once per framework.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;

/// An error or warning a build reported. Lines and columns start at 1; 0 means unknown.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuildProblem {
    pub file: PathBuf,
    pub line: u32,
    pub column: u32,
    pub end_line: u32,
    pub end_column: u32,
    pub error: bool,
    pub code: Option<String>,
    pub message: String,
}

static LINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(?:\d+>)?(?P<origin>.*?)\s*:\s*(?:[^:]*?\s+)?(?P<category>error|warning)\s*(?P<code>[A-Za-z]+\d+)?\s*:\s*(?P<message>.*?)\s*$").unwrap()
});
static PROJECT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+\[(?P<project>[^\[\]]+)\]$").unwrap());
static LOCATION: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(?P<file>.+?)\((?P<location>[\d,\-]+)\)$").unwrap());

/// The problems in a build's output, each once. Relative paths are resolved against the
/// project the problem came from, else `dir`; problems that name no file (`MSBUILD :`,
/// `CSC :`) go on their project's file, else `fallback`.
pub fn parse(output: &str, dir: &Path, fallback: &Path) -> Vec<BuildProblem> {
    let mut problems: Vec<BuildProblem> = Vec::new();
    for line in output.lines() {
        let Some(problem) = parse_line(line, dir, fallback) else { continue };
        if !problems.contains(&problem) {
            problems.push(problem);
        }
    }
    problems
}

fn parse_line(line: &str, dir: &Path, fallback: &Path) -> Option<BuildProblem> {
    let captures = LINE.captures(line)?;
    let origin = captures["origin"].trim();
    let mut message = &captures["message"];
    let mut project = None;
    if let Some(found) = PROJECT.captures(message) {
        let name = found["project"].split("::").next().unwrap_or_default().trim();
        project = Some(PathBuf::from(name));
        message = &message[..found.get(0)?.start()];
    }
    let project_dir = project.as_deref().and_then(Path::parent).filter(|p| p.is_absolute()).unwrap_or(dir);

    let (file, location) = match LOCATION.captures(origin) {
        Some(found) => (found["file"].trim().to_string(), parse_location(&found["location"])),
        None => (origin.to_string(), [0; 4]),
    };
    let file = if looks_like_a_path(&file) {
        project_dir.join(file)
    } else {
        // A tool's name: the problem is about the project as a whole.
        project.filter(|p| p.is_absolute()).unwrap_or_else(|| fallback.to_path_buf())
    };
    let [line, column, end_line, end_column] = location;
    Some(BuildProblem {
        file,
        line,
        column,
        end_line,
        end_column,
        error: &captures["category"] == "error",
        code: captures.name("code").map(|c| c.as_str().to_string()),
        message: message.trim().to_string(),
    })
}

/// `line`, `line-endLine`, `line,col`, `line,col-endCol` or `line,col,endLine,endCol`.
fn parse_location(text: &str) -> [u32; 4] {
    let numbers = |part: &str| -> Vec<u32> { part.split(',').filter_map(|n| n.trim().parse().ok()).collect() };
    let (start, end) = text.split_once('-').unwrap_or((text, ""));
    let (start, end) = (numbers(start), numbers(end));
    let line = start.first().copied().unwrap_or(0);
    let column = start.get(1).copied().unwrap_or(0);
    match (start.len(), end.len()) {
        (4, _) => [line, column, start[2], start[3]],
        (1, 1) => [line, 0, end[0], 0],
        (2, 1) => [line, column, line, end[0]],
        _ => [line, column, line, column],
    }
}

/// Whether an origin is a file rather than a tool such as `MSBUILD` or `CSC`.
fn looks_like_a_path(origin: &str) -> bool {
    origin.contains(['/', '\\']) || Path::new(origin).extension().is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn problem(file: &str, location: [u32; 4], error: bool, code: &str, message: &str) -> BuildProblem {
        let [line, column, end_line, end_column] = location;
        BuildProblem { file: file.into(), line, column, end_line, end_column, error, code: Some(code.into()), message: message.into() }
    }

    #[test]
    fn reads_compiler_and_tool_problems() {
        let output = "\
  Determining projects to restore...
/src/App/Program.cs(12,5): error CS0103: The name 'x' does not exist in the current context [/src/App/App.csproj]
/src/App/Program.cs(3,1,3,9): warning CS8019: Unnecessary using directive. [/src/App/App.csproj::TargetFramework=net8.0]
/src/App/Program.cs(3,1,3,9): warning CS8019: Unnecessary using directive. [/src/App/App.csproj::TargetFramework=net9.0]
/src/App/App.csproj : error NU1101: Unable to find package Nope. No packages exist with this id in source(s): nuget.org [/src/App/App.csproj]
CSC : warning CS2008: No source files specified. [/src/Lib/Lib.csproj]
MSBUILD : error MSB1009: Project file does not exist.
Build FAILED.
    1 Warning(s)
";
        let problems = parse(output, Path::new("/src"), Path::new("/src/App.sln"));
        assert_eq!(
            problems,
            vec![
                problem("/src/App/Program.cs", [12, 5, 12, 5], true, "CS0103", "The name 'x' does not exist in the current context"),
                problem("/src/App/Program.cs", [3, 1, 3, 9], false, "CS8019", "Unnecessary using directive."),
                problem("/src/App/App.csproj", [0; 4], true, "NU1101", "Unable to find package Nope. No packages exist with this id in source(s): nuget.org"),
                problem("/src/Lib/Lib.csproj", [0; 4], false, "CS2008", "No source files specified."),
                problem("/src/App.sln", [0; 4], true, "MSB1009", "Project file does not exist."),
            ]
        );
    }

    #[test]
    fn reads_windows_paths_relative_paths_and_ranges() {
        let output = "\
C:\\src\\App\\Program.cs(7,9-14): error CS1002: ; expected [C:\\src\\App\\App.csproj]
 2>Views/Home.cshtml(4-6): warning RZ1000: Unterminated block. [/src/Web/Web.csproj]
";
        let problems = parse(output, Path::new("/src"), Path::new("/src/App.sln"));
        if cfg!(windows) {
            assert_eq!(problems[0].file, Path::new("C:\\src\\App\\Program.cs"));
        }
        assert_eq!([problems[0].line, problems[0].column, problems[0].end_line, problems[0].end_column], [7, 9, 7, 14]);
        assert_eq!(problems[0].message, "; expected");
        assert_eq!(problems[1], problem("/src/Web/Views/Home.cshtml", [4, 0, 6, 0], false, "RZ1000", "Unterminated block."));
    }
}
