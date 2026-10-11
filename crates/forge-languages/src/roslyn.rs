//! The Roslyn language server (Microsoft.CodeAnalysis.LanguageServer), the one VS Code's
//! C# extension and C# Dev Kit use, and Forge's C# language server.
//!
//! A `roslyn-language-server` (the .NET tool) or `Microsoft.CodeAnalysis.LanguageServer` on
//! the PATH wins. Otherwise the latest stable `roslyn-language-server.<rid>` package is
//! downloaded from nuget.org into Forge's languages directory and run with `dotnet`. It is
//! a framework-dependent app for .NET 10 (or later), so without that runtime Forge keeps
//! OmniSharp (see [`use_roslyn`]).
//!
//! Unlike OmniSharp, Roslyn loads nothing until the client tells it what to open
//! (`solution/open`, `project/open`); `forge-dotnet` does that with the Solution Explorer's
//! solution. Its options come through `workspace/configuration`, as `csharp|section.option`
//! keys.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::SeqCst};

use anyhow::{Context as _, Result};
use async_trait::async_trait;
use futures::AsyncReadExt as _;
use gpui::{App, AsyncApp};
use http_client::github::AssetKind;
use http_client::github_download::download_server_binary;
use language::{LanguageServerName, LspAdapter, LspAdapterDelegate, LspInstaller, Toolchain};
use lsp::{InitializeParams, LanguageServerBinary, Uri};
use serde_json::{Value, json};
use util::fs::remove_matching;

pub(crate) const SERVER_NAME: LanguageServerName = LanguageServerName::new_static("roslyn");
const PACKAGE: &str = "roslyn-language-server";
const SERVER_DLL: &str = "Microsoft.CodeAnalysis.LanguageServer.dll";
const NUGET_PACKAGES: &str = "https://api.nuget.org/v3-flatcontainer";
/// The oldest .NET runtime the server runs on (its `rollForward` is `Major`).
const RUNTIME_MAJOR: u32 = 10;

/// Which server C# uses: `csharpLanguageServer` in `dotnet.json` (`auto`, `roslyn` or
/// `omnisharp`). `auto`, the default, is Roslyn when a .NET 10 (or later) runtime is
/// installed, and OmniSharp otherwise. Read once, at start.
pub(crate) fn use_roslyn() -> bool {
    let setting = std::fs::read_to_string(paths::config_dir().join("dotnet.json"))
        .ok()
        .and_then(|text| serde_json_lenient::from_str::<Value>(&text).ok())
        .and_then(|config| config.get("csharpLanguageServer")?.as_str().map(str::to_lowercase));
    match setting.as_deref() {
        Some("roslyn") => true,
        Some("omnisharp") => false,
        _ => {
            let found = dotnet_roots().iter().any(|root| has_runtime(root, RUNTIME_MAJOR));
            if !found {
                log::info!("no .NET {RUNTIME_MAJOR} runtime found: C# uses OmniSharp");
            }
            found
        }
    }
}

/// Where .NET is usually installed, and where the `dotnet` on the PATH lives.
fn dotnet_roots() -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = std::env::var_os("DOTNET_ROOT").map(PathBuf::from).into_iter().collect();
    if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        roots.push(Path::new(&home).join(".dotnet"));
    }
    if cfg!(windows) {
        if let Some(program_files) = std::env::var_os("ProgramFiles") {
            roots.push(Path::new(&program_files).join("dotnet"));
        }
    } else {
        roots.extend(["/usr/local/share/dotnet", "/usr/share/dotnet", "/usr/lib/dotnet", "/usr/lib64/dotnet", "/opt/homebrew/opt/dotnet/libexec", "/usr/local/opt/dotnet/libexec"].map(PathBuf::from));
    }
    let executable = if cfg!(windows) { "dotnet.exe" } else { "dotnet" };
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            if let Ok(dotnet) = dir.join(executable).canonicalize()
                && let Some(root) = dotnet.parent()
            {
                roots.push(root.to_path_buf());
            }
        }
    }
    roots
}

/// Whether `root` has a .NET runtime of version `major` or later.
fn has_runtime(root: &Path, major: u32) -> bool {
    let Ok(entries) = std::fs::read_dir(root.join("shared/Microsoft.NETCore.App")) else { return false };
    entries.flatten().any(|entry| {
        let name = entry.file_name();
        let version = name.to_string_lossy();
        version.split('.').next().and_then(|m| m.parse::<u32>().ok()).is_some_and(|m| m >= major)
    })
}

/// The package for this platform (`roslyn-language-server.osx-arm64` and so on).
fn package_id() -> Option<String> {
    let os = match std::env::consts::OS {
        "macos" => "osx",
        "linux" => "linux",
        "windows" => "win",
        _ => return None,
    };
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "x64",
        _ => return None,
    };
    Some(format!("{PACKAGE}.{os}-{arch}"))
}

/// The newest stable version in a flat-container `index.json` (oldest first), or the
/// newest prerelease when there is no stable one.
fn pick_version(index: &Value) -> Option<String> {
    let versions: Vec<&str> = index.get("versions")?.as_array()?.iter().filter_map(Value::as_str).collect();
    versions.iter().rev().find(|v| !v.contains('-')).or(versions.last()).map(|v| v.to_string())
}

/// The server in an extracted package: `tools/<tfm>/<rid>/Microsoft.CodeAnalysis.LanguageServer.dll`.
fn find_server_dll(package_dir: &Path) -> Option<PathBuf> {
    let frameworks = std::fs::read_dir(package_dir.join("tools")).ok()?;
    frameworks.flatten().filter(|e| e.path().is_dir()).find_map(|framework| {
        let runtimes = std::fs::read_dir(framework.path()).ok()?;
        runtimes.flatten().map(|rid| rid.path().join(SERVER_DLL)).find(|dll| dll.is_file())
    })
}

pub struct RoslynVersion {
    version: String,
    url: String,
}

pub struct RoslynLspAdapter;

impl RoslynLspAdapter {
    async fn environment(delegate: &dyn LspAdapterDelegate) -> collections::HashMap<String, String> {
        // `dotnet` and MSBuild need the user's PATH and DOTNET_ROOT rather than the GUI
        // app's minimal environment.
        let mut env = delegate.shell_env().await;
        // Decompiled sources go to $TMPDIR/MetadataAsSource. On macOS $TMPDIR is a symlink
        // (/var → /private/var), and the paths the server answers with must be the ones
        // the editor opens, or navigating from them fails.
        if let Some(tmp) = env.get("TMPDIR").cloned().or_else(|| std::env::var("TMPDIR").ok())
            && let Ok(resolved) = std::fs::canonicalize(&tmp)
        {
            env.insert("TMPDIR".into(), resolved.to_string_lossy().into_owned());
        }
        env
    }

    fn server_arguments(log_dir: Option<&Path>) -> Vec<std::ffi::OsString> {
        let mut args: Vec<std::ffi::OsString> = vec!["--stdio".into(), "--logLevel".into(), "Information".into()];
        if let Some(log_dir) = log_dir {
            args.push("--extensionLogDirectory".into());
            args.push(log_dir.into());
        }
        args
    }

    /// Runs the server in an extracted package with `dotnet`.
    async fn packaged_binary(dll: PathBuf, container_dir: &Path, delegate: &dyn LspAdapterDelegate) -> Option<LanguageServerBinary> {
        let dotnet = delegate.which("dotnet".as_ref()).await?;
        let mut arguments = vec![dll.into_os_string()];
        arguments.extend(Self::server_arguments(Some(&container_dir.join("logs"))));
        Some(LanguageServerBinary { path: dotnet, arguments, env: Some(Self::environment(delegate).await) })
    }

    async fn ensure_dotnet(delegate: &dyn LspAdapterDelegate, cx: &mut AsyncApp) -> Result<()> {
        static DID_SHOW_NOTIFICATION: AtomicBool = AtomicBool::new(false);
        const MESSAGE: &str = "Could not start the C# language server (Roslyn), because `dotnet` was not found.";
        if delegate.which("dotnet".as_ref()).await.is_some() {
            return Ok(());
        }
        if DID_SHOW_NOTIFICATION.compare_exchange(false, true, SeqCst, SeqCst).is_ok() {
            cx.update(|cx| delegate.show_notification(MESSAGE, cx));
        }
        anyhow::bail!(MESSAGE)
    }

    /// Forge's defaults for the server's options, as the flat `csharp|section.option` keys
    /// it asks for (Zed answers `workspace/configuration` by looking each section up).
    fn default_configuration() -> Value {
        json!({
            // Errors and analyzer warnings for every file of the solution, not only open ones,
            // so the Problems panel lists them all as Visual Studio's Error List does.
            "csharp|background_analysis.dotnet_analyzer_diagnostics_scope": "fullSolution",
            "csharp|background_analysis.dotnet_compiler_diagnostics_scope": "fullSolution",
            "csharp|completion.dotnet_show_completion_items_from_unimported_namespaces": true,
            "csharp|completion.dotnet_show_name_completion_suggestions": true,
            "csharp|completion.dotnet_provide_regex_completions": true,
            "csharp|symbol_search.dotnet_search_reference_assemblies": true,
            "csharp|inlay_hints.dotnet_enable_inlay_hints_for_parameters": true,
            "csharp|inlay_hints.dotnet_enable_inlay_hints_for_literal_parameters": true,
            "csharp|inlay_hints.dotnet_enable_inlay_hints_for_indexer_parameters": true,
            "csharp|inlay_hints.dotnet_enable_inlay_hints_for_object_creation_parameters": true,
            "csharp|inlay_hints.dotnet_enable_inlay_hints_for_other_parameters": true,
            "csharp|inlay_hints.dotnet_suppress_inlay_hints_for_parameters_that_differ_only_by_suffix": true,
            "csharp|inlay_hints.dotnet_suppress_inlay_hints_for_parameters_that_match_method_intent": true,
            "csharp|inlay_hints.dotnet_suppress_inlay_hints_for_parameters_that_match_argument_name": true,
            "csharp|inlay_hints.csharp_enable_inlay_hints_for_types": true,
            "csharp|inlay_hints.csharp_enable_inlay_hints_for_implicit_variable_types": true,
            "csharp|inlay_hints.csharp_enable_inlay_hints_for_lambda_parameter_types": true,
            "csharp|inlay_hints.csharp_enable_inlay_hints_for_implicit_object_creation": false,
            "csharp|inlay_hints.csharp_enable_inlay_hints_for_collection_expressions": false,
            // Its lenses run commands of VS Code's ("Unknown Command" elsewhere).
            "csharp|code_lens.dotnet_enable_references_code_lens": false,
            "csharp|code_lens.dotnet_enable_tests_code_lens": false,
            "csharp|formatting.dotnet_organize_imports_on_format": true,
        })
    }
}

#[async_trait(?Send)]
impl LspAdapter for RoslynLspAdapter {
    fn name(&self) -> LanguageServerName {
        SERVER_NAME
    }

    async fn workspace_configuration(self: Arc<Self>, _: &Arc<dyn LspAdapterDelegate>, _: Option<Toolchain>, _: Option<Uri>, _: &mut AsyncApp) -> Result<Value> {
        Ok(Self::default_configuration())
    }

    /// Roslyn only reports diagnostics to clients that let it register its diagnostic
    /// providers (compiler, analyzers, …) dynamically.
    fn prepare_initialize_params(&self, mut params: InitializeParams, _: &App) -> Result<InitializeParams> {
        let text_document = params.capabilities.text_document.get_or_insert_with(Default::default);
        let diagnostic = text_document.diagnostic.get_or_insert_with(Default::default);
        diagnostic.dynamic_registration = Some(true);
        Ok(params)
    }
}

impl LspInstaller for RoslynLspAdapter {
    type BinaryVersion = RoslynVersion;

    async fn check_if_user_installed(&self, delegate: &Arc<dyn LspAdapterDelegate>, _: Option<Toolchain>, _: &AsyncApp) -> Option<LanguageServerBinary> {
        for name in [PACKAGE, "Microsoft.CodeAnalysis.LanguageServer"] {
            if let Some(path) = delegate.which(name.as_ref()).await {
                return Some(LanguageServerBinary { path, arguments: Self::server_arguments(None), env: Some(Self::environment(delegate.as_ref()).await) });
            }
        }
        None
    }

    async fn fetch_latest_server_version(&self, delegate: &Arc<dyn LspAdapterDelegate>, _pre_release: bool, cx: &mut AsyncApp) -> Result<RoslynVersion> {
        Self::ensure_dotnet(delegate.as_ref(), cx).await?;
        let id = package_id().context("the Roslyn language server has no build for this platform")?;
        let index_url = format!("{NUGET_PACKAGES}/{id}/index.json");
        let mut response = delegate.http_client().get(&index_url, Default::default(), true).await.with_context(|| format!("fetching {index_url}"))?;
        let mut body = Vec::new();
        response.body_mut().read_to_end(&mut body).await.with_context(|| format!("reading {index_url}"))?;
        anyhow::ensure!(response.status().is_success(), "{index_url}: status {}", response.status());
        let index: Value = serde_json::from_slice(&body).with_context(|| format!("parsing {index_url}"))?;
        let version = pick_version(&index).with_context(|| format!("no versions in {index_url}"))?;
        let url = format!("{NUGET_PACKAGES}/{id}/{version}/{id}.{version}.nupkg");
        Ok(RoslynVersion { version, url })
    }

    fn check_if_version_installed(
        &self,
        version: &RoslynVersion,
        container_dir: &PathBuf,
        delegate: &Arc<dyn LspAdapterDelegate>,
    ) -> impl Send + Future<Output = Option<LanguageServerBinary>> + use<> {
        let package_dir = container_dir.join(format!("roslyn-{}", version.version));
        let container_dir = container_dir.clone();
        let delegate = delegate.clone();
        async move {
            let dll = find_server_dll(&package_dir)?;
            Self::packaged_binary(dll, &container_dir, delegate.as_ref()).await
        }
    }

    fn fetch_server_binary(
        &self,
        version: RoslynVersion,
        container_dir: PathBuf,
        delegate: &Arc<dyn LspAdapterDelegate>,
    ) -> impl Send + Future<Output = Result<LanguageServerBinary>> + use<> {
        let delegate = delegate.clone();
        async move {
            let destination = container_dir.join(format!("roslyn-{}", version.version));
            if find_server_dll(&destination).is_none() {
                // A .nupkg is a zip.
                download_server_binary(&*delegate.http_client(), &version.url, None, &destination, AssetKind::Zip).await?;
                let logs = container_dir.join("logs");
                remove_matching(&container_dir, |path| path != destination && path != logs).await;
            }
            let dll = find_server_dll(&destination).with_context(|| format!("no {SERVER_DLL} in {}", destination.display()))?;
            Self::packaged_binary(dll, &container_dir, delegate.as_ref()).await.context("`dotnet` was not found")
        }
    }

    async fn cached_server_binary(&self, container_dir: PathBuf, delegate: &dyn LspAdapterDelegate) -> Option<LanguageServerBinary> {
        let mut entries = smol::fs::read_dir(&container_dir).await.ok()?;
        let mut latest = None;
        while let Some(entry) = smol::stream::StreamExt::next(&mut entries).await {
            let path = entry.ok()?.path();
            if let Some(dll) = find_server_dll(&path) {
                latest = Some(dll);
            }
        }
        Self::packaged_binary(latest?, &container_dir, delegate).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_the_newest_stable_version() {
        let index = json!({ "versions": ["5.8.0-1.26266.2", "5.11.0", "5.12.0-1.26475.2"] });
        assert_eq!(pick_version(&index).as_deref(), Some("5.11.0"));
        let only_previews = json!({ "versions": ["5.8.0-1.1", "5.9.0-1.2"] });
        assert_eq!(pick_version(&only_previews).as_deref(), Some("5.9.0-1.2"));
        assert_eq!(pick_version(&json!({ "versions": [] })), None);
    }

    #[test]
    fn finds_runtimes_and_the_server_in_a_package() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("shared/Microsoft.NETCore.App/9.0.16")).unwrap();
        assert!(!has_runtime(root, 10));
        std::fs::create_dir_all(root.join("shared/Microsoft.NETCore.App/11.0.0-preview.1")).unwrap();
        assert!(has_runtime(root, 10), "a later major version runs it too");

        let package = root.join("roslyn-5.11.0");
        let server = package.join("tools/net10.0/osx-arm64").join(SERVER_DLL);
        std::fs::create_dir_all(server.parent().unwrap()).unwrap();
        assert_eq!(find_server_dll(&package), None);
        std::fs::write(&server, b"").unwrap();
        assert_eq!(find_server_dll(&package), Some(server));
    }
}
