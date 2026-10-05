//! OmniSharp, the C# language server.
//!
//! An `omnisharp` (or `OmniSharp`) binary on the PATH wins. Otherwise the latest
//! release of OmniSharp/omnisharp-roslyn is downloaded into Forge's languages
//! directory. Releases since v2 are framework-dependent .NET apps, so a .NET
//! runtime (`dotnet`) has to be installed either way.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::SeqCst};

use anyhow::{Context as _, Result};
use async_trait::async_trait;
use gpui::AsyncApp;
use http_client::github::{AssetKind, GitHubLspBinaryVersion, latest_github_release};
use http_client::github_download::{GithubBinaryMetadata, download_server_binary};
use language::{
    LanguageServerId, LanguageServerName, LspAdapter, LspAdapterDelegate, LspInstaller, Toolchain,
};
use lsp::LanguageServerBinary;
use util::fs::{make_file_executable, remove_matching};

pub(crate) const SERVER_NAME: LanguageServerName = LanguageServerName::new_static("omnisharp");
const REPO: &str = "OmniSharp/omnisharp-roslyn";

#[cfg(not(windows))]
const ASSET_KIND: AssetKind = AssetKind::TarGz;
#[cfg(windows)]
const ASSET_KIND: AssetKind = AssetKind::Zip;

#[cfg(not(windows))]
const BINARY: &str = "OmniSharp";
#[cfg(windows)]
const BINARY: &str = "OmniSharp.exe";

pub struct OmniSharpLspAdapter;

impl OmniSharpLspAdapter {
    fn asset_name() -> Option<String> {
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
        let extension = match ASSET_KIND {
            AssetKind::Zip => "zip",
            _ => "tar.gz",
        };
        Some(format!("omnisharp-{os}-{arch}.{extension}"))
    }

    /// OmniSharp leaves most Roslyn features off unless asked; these are the ones VS Code's
    /// C# extension exposes, passed as `Section:Key=value` configuration arguments.
    fn arguments() -> Vec<std::ffi::OsString> {
        [
            "-lsp",
            // Inspections: the project's Roslyn analyzers (NuGet ones included) and .editorconfig rules.
            "RoslynExtensionsOptions:EnableAnalyzersSupport=true",
            "FormattingOptions:EnableEditorConfigSupport=true",
            // Complete types from namespaces that are not imported yet, adding the `using`.
            "RoslynExtensionsOptions:EnableImportCompletion=true",
            // Go to definition into assemblies shows decompiled code, not just signatures.
            "RoslynExtensionsOptions:EnableDecompilationSupport=true",
            "FormattingOptions:OrganizeImports=true",
            "RoslynExtensionsOptions:InlayHintsOptions:EnableForParameters=true",
            "RoslynExtensionsOptions:InlayHintsOptions:ForLiteralParameters=true",
            "RoslynExtensionsOptions:InlayHintsOptions:ForObjectCreationParameters=true",
            "RoslynExtensionsOptions:InlayHintsOptions:SuppressForParametersThatDifferOnlyBySuffix=true",
            "RoslynExtensionsOptions:InlayHintsOptions:SuppressForParametersThatMatchMethodIntent=true",
            "RoslynExtensionsOptions:InlayHintsOptions:SuppressForParametersThatMatchArgumentName=true",
            "RoslynExtensionsOptions:InlayHintsOptions:EnableForTypes=true",
            "RoslynExtensionsOptions:InlayHintsOptions:ForImplicitVariableTypes=true",
            "RoslynExtensionsOptions:InlayHintsOptions:ForLambdaParameterTypes=true",
        ]
        .into_iter()
        .map(Into::into)
        .collect()
    }

    async fn binary(path: PathBuf, delegate: &dyn LspAdapterDelegate) -> LanguageServerBinary {
        LanguageServerBinary {
            path,
            arguments: Self::arguments(),
            // OmniSharp shells out to `dotnet` and MSBuild, which need the user's
            // PATH and DOTNET_ROOT rather than the GUI app's minimal environment.
            env: Some(delegate.shell_env().await),
        }
    }

    async fn ensure_dotnet(delegate: &dyn LspAdapterDelegate, cx: &mut AsyncApp) -> Result<()> {
        static DID_SHOW_NOTIFICATION: AtomicBool = AtomicBool::new(false);
        const MESSAGE: &str =
            "Could not start the C# language server `OmniSharp`, because `dotnet` was not found.";

        if delegate.which("dotnet".as_ref()).await.is_some() {
            return Ok(());
        }
        if DID_SHOW_NOTIFICATION
            .compare_exchange(false, true, SeqCst, SeqCst)
            .is_ok()
        {
            cx.update(|cx| delegate.show_notification(MESSAGE, cx));
        }
        anyhow::bail!(MESSAGE)
    }
}

#[async_trait(?Send)]
impl LspAdapter for OmniSharpLspAdapter {
    fn name(&self) -> LanguageServerName {
        SERVER_NAME
    }

    /// Sources OmniSharp generates for compiled assemblies are analyzed outside any
    /// project, so every type in them is reported as missing (CS0518 and friends).
    fn process_diagnostics(&self, params: &mut lsp::PublishDiagnosticsParams, _: LanguageServerId) {
        if crate::csharp_metadata::is_metadata_path(params.uri.as_str()) {
            params.diagnostics.clear();
        }
    }
}

impl LspInstaller for OmniSharpLspAdapter {
    type BinaryVersion = GitHubLspBinaryVersion;

    async fn check_if_user_installed(
        &self,
        delegate: &Arc<dyn LspAdapterDelegate>,
        _: Option<Toolchain>,
        _: &AsyncApp,
    ) -> Option<LanguageServerBinary> {
        for name in ["omnisharp", "OmniSharp"] {
            if let Some(path) = delegate.which(name.as_ref()).await {
                return Some(Self::binary(path, delegate.as_ref()).await);
            }
        }
        None
    }

    async fn fetch_latest_server_version(
        &self,
        delegate: &Arc<dyn LspAdapterDelegate>,
        pre_release: bool,
        cx: &mut AsyncApp,
    ) -> Result<GitHubLspBinaryVersion> {
        Self::ensure_dotnet(delegate.as_ref(), cx).await?;
        let asset_name = Self::asset_name().context("OmniSharp has no build for this platform")?;
        let release = latest_github_release(REPO, true, pre_release, delegate.http_client()).await?;
        let asset = release
            .assets
            .into_iter()
            .find(|asset| asset.name == asset_name)
            .with_context(|| format!("no OmniSharp asset named `{asset_name}`"))?;
        Ok(GitHubLspBinaryVersion {
            name: release.tag_name,
            url: asset.browser_download_url,
            digest: asset.digest,
        })
    }

    fn fetch_server_binary(
        &self,
        version: GitHubLspBinaryVersion,
        container_dir: PathBuf,
        delegate: &Arc<dyn LspAdapterDelegate>,
    ) -> impl Send + Future<Output = Result<LanguageServerBinary>> + use<> {
        let delegate = delegate.clone();
        async move {
            let GitHubLspBinaryVersion {
                name,
                url,
                digest: expected_digest,
            } = version;
            let destination_path = container_dir.join(format!("omnisharp-{name}"));
            let server_path = destination_path.join(BINARY);
            let metadata_path = container_dir.join(format!("omnisharp-{name}.metadata"));

            let installed_digest = GithubBinaryMetadata::read_from_file(&metadata_path)
                .await
                .ok()
                .map(|metadata| metadata.digest);
            let up_to_date = match (&installed_digest, &expected_digest) {
                (Some(Some(actual)), Some(expected)) => actual == expected,
                (Some(_), _) => true,
                (None, _) => false,
            };
            if !(up_to_date && server_path.is_file()) {
                download_server_binary(
                    &*delegate.http_client(),
                    &url,
                    expected_digest.as_deref(),
                    &destination_path,
                    ASSET_KIND,
                )
                .await?;
                make_file_executable(&server_path).await?;
                remove_matching(&container_dir, |path| {
                    path != destination_path && path != metadata_path
                })
                .await;
                GithubBinaryMetadata::write_to_file(
                    &GithubBinaryMetadata {
                        metadata_version: 1,
                        digest: expected_digest,
                    },
                    &metadata_path,
                )
                .await?;
            }

            Ok(Self::binary(server_path, delegate.as_ref()).await)
        }
    }

    async fn cached_server_binary(
        &self,
        container_dir: PathBuf,
        delegate: &dyn LspAdapterDelegate,
    ) -> Option<LanguageServerBinary> {
        let mut entries = smol::fs::read_dir(&container_dir).await.ok()?;
        let mut latest = None;
        while let Some(entry) = smol::stream::StreamExt::next(&mut entries).await {
            let path = entry.ok()?.path();
            if path.is_dir() && path.join(BINARY).is_file() {
                latest = Some(path);
            }
        }
        Some(Self::binary(latest?.join(BINARY), delegate).await)
    }
}
