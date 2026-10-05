//! Paths as MSBuild and solution files write them (`src\App\App.csproj`) and as the file
//! system wants them.

use std::path::{Component, Path, PathBuf};

/// A path from a project or solution file, with `\` turned into this platform's separator.
pub fn from_msbuild(path: &str) -> PathBuf {
    let path = path.trim();
    if std::path::MAIN_SEPARATOR == '\\' { PathBuf::from(path) } else { PathBuf::from(path.replace('\\', "/")) }
}

/// A relative path written the way project and `.sln` files store it: with backslashes.
pub fn to_msbuild(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            Component::ParentDir => Some("..".to_string()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\\")
}

/// A relative path with forward slashes, as `.slnx` files store them.
pub fn to_forward(path: &Path) -> String {
    to_msbuild(path).replace('\\', "/")
}

/// Resolves `.` and `..` without touching the file system.
pub fn normalize(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !result.pop() {
                    result.push("..");
                }
            }
            other => result.push(other.as_os_str()),
        }
    }
    result
}

/// `path` relative to the directory `base`, both absolute; `None` on another drive.
pub fn relative(base: &Path, path: &Path) -> Option<PathBuf> {
    let base = normalize(base);
    let path = normalize(path);
    let base_parts: Vec<_> = base.components().collect();
    let path_parts: Vec<_> = path.components().collect();
    if base_parts.first() != path_parts.first() {
        return None;
    }
    let common = base_parts.iter().zip(&path_parts).take_while(|(a, b)| eq_component(a, b)).count();
    let mut result = PathBuf::new();
    for _ in common..base_parts.len() {
        result.push("..");
    }
    for part in &path_parts[common..] {
        result.push(part.as_os_str());
    }
    Some(result)
}

/// Joins a path read from a project or solution file to the directory it is relative to.
pub fn resolve(base: &Path, msbuild_path: &str) -> PathBuf {
    normalize(&base.join(from_msbuild(msbuild_path)))
}

/// Whether `path` is `dir` or inside it.
pub fn is_within(path: &Path, dir: &Path) -> bool {
    let path = normalize(path);
    let dir = normalize(dir);
    let mut path_parts = path.components();
    dir.components().all(|part| path_parts.next().is_some_and(|other| eq_component(&part, &other)))
}

fn eq_component(a: &Component, b: &Component) -> bool {
    // macOS and Windows file systems are case-insensitive by default, and projects written
    // on Windows often disagree with the disk about case.
    if cfg!(any(target_os = "macos", target_os = "windows")) {
        a.as_os_str().to_string_lossy().to_lowercase() == b.as_os_str().to_string_lossy().to_lowercase()
    } else {
        a == b
    }
}

/// The file name without its last extension.
pub fn file_stem(path: &Path) -> String {
    path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
}

/// The file name, or the whole path when it has none.
pub fn file_name(path: &Path) -> String {
    path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| path.to_string_lossy().into_owned())
}

/// The lowercase extension without its dot.
pub fn extension(path: &Path) -> String {
    path.extension().map(|s| s.to_string_lossy().to_lowercase()).unwrap_or_default()
}

/// A name for a copy of `path` that does not exist yet: `Foo copy.cs`, `Foo copy 2.cs`, ….
pub fn copy_name(path: &Path) -> PathBuf {
    let dir = path.parent().unwrap_or(Path::new(""));
    let stem = file_stem(path);
    let ext = path.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    let mut index = 1;
    loop {
        let name = if index == 1 { format!("{stem} copy{ext}") } else { format!("{stem} copy {index}{ext}") };
        let candidate = dir.join(name);
        if !candidate.exists() {
            return candidate;
        }
        index += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn msbuild_paths_round_trip() {
        assert_eq!(to_msbuild(&from_msbuild(r"src\App\App.csproj")), r"src\App\App.csproj");
        assert_eq!(to_forward(Path::new("src/App/App.csproj")), "src/App/App.csproj");
    }

    #[test]
    fn relative_paths() {
        assert_eq!(relative(Path::new("/a/b"), Path::new("/a/b/c/d.cs")).unwrap(), PathBuf::from("c/d.cs"));
        assert_eq!(relative(Path::new("/a/b"), Path::new("/a/x/y.cs")).unwrap(), PathBuf::from("../x/y.cs"));
        assert_eq!(normalize(Path::new("/a/b/../c/./d")), PathBuf::from("/a/c/d"));
        assert!(is_within(Path::new("/a/b/c"), Path::new("/a/b")));
        assert!(!is_within(Path::new("/a/bc"), Path::new("/a/b")));
    }
}
