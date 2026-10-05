//! MSBuild item specs (`src/**/*.cs`, `..\Shared\*.cs`) matched against absolute paths.

use std::path::{Path, PathBuf};

use globset::{GlobBuilder, GlobMatcher};

use crate::paths;

pub fn is_glob(spec: &str) -> bool {
    spec.contains(['*', '?', '[', '{'])
}

/// One item spec resolved against a directory.
#[derive(Clone, Debug)]
pub struct Spec {
    /// The absolute pattern, with `/` separators.
    pub pattern: PathBuf,
    /// The deepest directory without wildcards: where to start walking.
    pub base: PathBuf,
    matcher: Option<GlobMatcher>,
}

impl Spec {
    pub fn new(dir: &Path, spec: &str) -> Self {
        let spec = spec.trim().replace('\\', "/");
        let pattern = paths::normalize(&dir.join(&spec));
        let pattern_text = pattern.to_string_lossy().replace('\\', "/");
        let matcher = is_glob(&spec).then(|| {
            GlobBuilder::new(&pattern_text)
                .literal_separator(true)
                .case_insensitive(cfg!(any(target_os = "macos", target_os = "windows")))
                .backslash_escape(false)
                .build()
                .ok()
                .map(|glob| glob.compile_matcher())
        });
        let base = if is_glob(&spec) {
            let mut base = PathBuf::new();
            for component in pattern.components() {
                if is_glob(&component.as_os_str().to_string_lossy()) {
                    break;
                }
                base.push(component.as_os_str());
            }
            base
        } else {
            pattern.parent().map(Path::to_path_buf).unwrap_or_default()
        };
        Spec { pattern, base, matcher: matcher.flatten() }
    }

    pub fn is_glob(&self) -> bool {
        self.matcher.is_some()
    }

    pub fn matches(&self, path: &Path) -> bool {
        match &self.matcher {
            Some(matcher) => matcher.is_match(path) || matcher.is_match(path.to_string_lossy().replace('\\', "/")),
            None => {
                // A literal spec also matches everything under it when it names a directory.
                paths::is_within(path, &self.pattern)
            }
        }
    }

    /// The part of `path` under the spec's wildcard-free base, as `%(RecursiveDir)`.
    pub fn recursive_dir(&self, path: &Path) -> PathBuf {
        let dir = path.parent().unwrap_or(Path::new(""));
        paths::relative(&self.base, dir).filter(|r| !r.starts_with("..")).unwrap_or_default()
    }
}

/// Several `;`-separated specs.
pub fn specs(dir: &Path, value: &str) -> Vec<Spec> {
    value.split(';').map(str::trim).filter(|s| !s.is_empty()).map(|s| Spec::new(dir, s)).collect()
}

pub fn any_match(specs: &[Spec], path: &Path) -> bool {
    specs.iter().any(|spec| spec.matches(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs_match_like_msbuild() {
        let dir = Path::new("/p");
        let all = Spec::new(dir, "**/*.cs");
        assert!(all.matches(Path::new("/p/A.cs")));
        assert!(all.matches(Path::new("/p/x/y/B.cs")));
        assert!(!all.matches(Path::new("/p/x/B.csx")));
        assert_eq!(all.base, PathBuf::from("/p"));

        let shared = Spec::new(dir, r"..\Shared\**\*.cs");
        assert_eq!(shared.base, PathBuf::from("/Shared"));
        assert!(shared.matches(Path::new("/Shared/a/C.cs")));
        assert_eq!(shared.recursive_dir(Path::new("/Shared/a/C.cs")), PathBuf::from("a"));

        let one_level = Spec::new(dir, "x/*.cs");
        assert!(!one_level.matches(Path::new("/p/x/y/B.cs")));
        assert!(Spec::new(dir, "{a,b}/*.txt").matches(Path::new("/p/b/n.txt")));

        let literal = Spec::new(dir, r"Folder\File.cs");
        assert!(literal.matches(Path::new("/p/Folder/File.cs")));
        assert!(Spec::new(dir, "Folder").matches(Path::new("/p/Folder/File.cs")));
    }
}
