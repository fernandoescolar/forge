//! Package sources from `NuGet.Config` files, merged the way NuGet does: the user's config
//! first, then every config from the file system root down to the project, the closest
//! one winning. Honors `<clear />`, `disabledPackageSources` and clear-text credentials,
//! with `%VAR%` expanded from the environment.

use std::path::{Path, PathBuf};

pub const NUGET_ORG: &str = "https://api.nuget.org/v3/index.json";

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Credentials {
    pub username: String,
    pub password: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackageSource {
    pub name: String,
    pub url: String,
    pub credentials: Option<Credentials>,
    pub enabled: bool,
}

impl PackageSource {
    pub fn is_http(&self) -> bool {
        self.url.starts_with("http://") || self.url.starts_with("https://")
    }
}

fn config_in(dir: &Path) -> Option<PathBuf> {
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .find(|e| e.file_name().to_string_lossy().eq_ignore_ascii_case("nuget.config") && e.path().is_file())
        .map(|e| e.path())
}

/// The user-level config: `~/.nuget/NuGet/NuGet.Config` (or `%AppData%\NuGet` on Windows).
pub fn user_config() -> Option<PathBuf> {
    let base = if cfg!(windows) { std::env::var_os("APPDATA").map(PathBuf::from) } else { std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".nuget")) };
    let dir = base?.join("NuGet");
    config_in(&dir)
}

/// Config files that apply to `dir`, from least to most specific.
pub fn config_files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = dir.ancestors().filter_map(config_in).collect();
    files.reverse();
    if let Some(user) = user_config()
        && !files.contains(&user) {
            files.insert(0, user);
        }
    files
}

fn expand_env(value: &str) -> String {
    let mut result = String::new();
    let mut rest = value;
    while let Some(start) = rest.find('%') {
        let Some(len) = rest[start + 1..].find('%') else { break };
        let name = &rest[start + 1..start + 1 + len];
        result.push_str(&rest[..start]);
        match std::env::var(name) {
            Ok(v) if !name.is_empty() => result.push_str(&v),
            _ => result.push_str(&rest[start..start + len + 2]),
        }
        rest = &rest[start + len + 2..];
    }
    result.push_str(rest);
    result
}

/// Applies one config file's text on top of the sources so far.
pub fn apply_config(sources: &mut Vec<PackageSource>, text: &str, config_dir: &Path) {
    let Ok(doc) = roxmltree::Document::parse(text) else { return };
    let root = doc.root_element();
    let section = |name: &str| root.children().find(|n| n.is_element() && n.tag_name().name().eq_ignore_ascii_case(name));

    if let Some(package_sources) = section("packageSources") {
        for node in package_sources.children().filter(|n| n.is_element()) {
            match node.tag_name().name() {
                "clear" => sources.clear(),
                "add" => {
                    let (Some(key), Some(value)) = (node.attribute("key"), node.attribute("value")) else { continue };
                    let mut url = expand_env(value);
                    if !url.contains("://") {
                        // Local folder feeds are relative to the config that declares them.
                        url = crate::paths::resolve(config_dir, &url).to_string_lossy().into_owned();
                    }
                    sources.retain(|s| !s.name.eq_ignore_ascii_case(key));
                    sources.push(PackageSource { name: key.to_string(), url, credentials: None, enabled: true });
                }
                "remove" => {
                    if let Some(key) = node.attribute("key") {
                        sources.retain(|s| !s.name.eq_ignore_ascii_case(key));
                    }
                }
                _ => {}
            }
        }
    }
    if let Some(disabled) = section("disabledPackageSources") {
        for node in disabled.children().filter(|n| n.is_element() && n.tag_name().name() == "add") {
            if let (Some(key), Some(value)) = (node.attribute("key"), node.attribute("value"))
                && let Some(source) = sources.iter_mut().find(|s| s.name.eq_ignore_ascii_case(key)) {
                    source.enabled = !value.eq_ignore_ascii_case("true");
                }
        }
    }
    if let Some(credentials) = section("packageSourceCredentials") {
        for source_node in credentials.children().filter(|n| n.is_element()) {
            // Element names encode spaces as `_x0020_`.
            let name = source_node.tag_name().name().replace("_x0020_", " ");
            let value = |key: &str| {
                source_node
                    .children()
                    .find(|n| n.is_element() && n.attribute("key").is_some_and(|k| k.eq_ignore_ascii_case(key)))
                    .and_then(|n| n.attribute("value"))
                    .map(expand_env)
            };
            let (Some(username), Some(password)) = (value("Username"), value("ClearTextPassword")) else { continue };
            if let Some(source) = sources.iter_mut().find(|s| s.name.eq_ignore_ascii_case(&name)) {
                source.credentials = Some(Credentials { username, password });
            }
        }
    }
}

/// The enabled sources for a project or solution folder.
pub fn sources_for(dir: &Path) -> Vec<PackageSource> {
    let mut sources = vec![PackageSource { name: "nuget.org".into(), url: NUGET_ORG.into(), credentials: None, enabled: true }];
    for file in config_files(dir) {
        if let Ok(text) = std::fs::read_to_string(&file) {
            apply_config(&mut sources, &text, file.parent().unwrap_or(Path::new(".")));
        }
    }
    sources.retain(|s| s.enabled);
    sources
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_configs() {
        let mut sources = vec![PackageSource { name: "nuget.org".into(), url: NUGET_ORG.into(), credentials: None, enabled: true }];
        unsafe { std::env::set_var("FORGE_TEST_FEED_TOKEN", "s3cret") };
        apply_config(
            &mut sources,
            r#"<configuration>
                <packageSources>
                  <add key="Company Feed" value="https://pkgs.example.com/v3/index.json" />
                  <add key="local" value="./packages" />
                </packageSources>
                <disabledPackageSources><add key="local" value="true" /></disabledPackageSources>
                <packageSourceCredentials>
                  <Company_x0020_Feed>
                    <add key="Username" value="me" />
                    <add key="ClearTextPassword" value="%FORGE_TEST_FEED_TOKEN%" />
                  </Company_x0020_Feed>
                </packageSourceCredentials>
              </configuration>"#,
            Path::new("/repo"),
        );
        assert_eq!(sources.len(), 3);
        assert_eq!(sources[1].credentials, Some(Credentials { username: "me".into(), password: "s3cret".into() }));
        assert_eq!(sources[2].url, "/repo/packages");
        assert!(!sources[2].enabled);

        apply_config(&mut sources, r#"<configuration><packageSources><clear /><add key="only" value="https://only/v3/index.json" /></packageSources></configuration>"#, Path::new("/"));
        assert_eq!(sources.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), vec!["only"]);
    }
}
