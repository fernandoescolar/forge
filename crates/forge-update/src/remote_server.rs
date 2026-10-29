//! Where Forge gets the server it runs on remote hosts (`forge-remote-server`, see that
//! crate): the asset `forge-remote-server-<version>-<os>-<arch>.gz` (`.zip` for Windows) of
//! the same GitHub release as the app. The remote host downloads it itself when it can;
//! else Forge downloads it here, into `remote_servers/`, and uploads it over SSH.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context as _, Result};
use gpui::{AsyncApp, Task};
use http_client::HttpClient;
use release_channel::ReleaseChannel;
use remote::RemotePlatform;
use semver::Version;

/// Forge's releases of the remote server, for `remote_connection::set_remote_server_releases`.
pub struct ForgeServerReleases {
    pub http: Arc<dyn HttpClient>,
}

/// The release asset holding the server for `platform`.
pub fn asset_name(version: &str, platform: RemotePlatform) -> String {
    let extension = if platform.os.is_windows() { "zip" } else { "gz" };
    format!("forge-remote-server-{version}-{}-{}.{extension}", platform.os.as_str(), platform.arch.as_str())
}

/// Where the asset of `version` is downloaded from.
pub fn asset_url(repository: &str, version: &str, platform: RemotePlatform) -> String {
    format!("https://github.com/{repository}/releases/download/v{version}/{}", asset_name(version, platform))
}

fn wanted(version: Option<Version>) -> Result<String> {
    // Forge has no nightly channel, so there is always the app's own version to match.
    version.map(|version| version.to_string()).context("no version to download the remote server for")
}

impl remote_connection::RemoteServerReleases for ForgeServerReleases {
    fn download_url(&self, platform: RemotePlatform, _: ReleaseChannel, version: Option<Version>, _: &mut AsyncApp) -> Task<Result<Option<String>>> {
        Task::ready(wanted(version).map(|version| Some(asset_url(crate::REPOSITORY, &version, platform))))
    }

    fn download_locally(
        &self,
        platform: RemotePlatform,
        release_channel: ReleaseChannel,
        version: Option<Version>,
        set_status: Box<dyn Fn(&str, &mut AsyncApp) + Send>,
        cx: &mut AsyncApp,
    ) -> Task<Result<PathBuf>> {
        let http = self.http.clone();
        cx.spawn(async move |cx| {
            let version = wanted(version)?;
            let asset = asset_name(&version, platform);
            let path: PathBuf = paths::remote_servers_dir().join(release_channel.dev_name()).join(&asset);
            if smol::fs::metadata(&path).await.is_ok() {
                return Ok(path);
            }
            set_status("Downloading the remote server", cx);
            let url = asset_url(crate::REPOSITORY, &version, platform);
            let bytes = crate::get(&http, &url).await.with_context(|| format!("downloading {asset}"))?;
            let folder = path.parent().context("the remote servers folder has no parent")?;
            smol::fs::create_dir_all(folder).await?;
            // Written aside and renamed, so an interrupted download never looks complete.
            let partial = path.with_extension("partial");
            smol::fs::write(&partial, &bytes).await?;
            smol::fs::rename(&partial, &path).await?;
            Ok(path)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use remote::{RemoteArch, RemoteOs};

    #[test]
    fn names_the_asset_like_the_release_does() {
        let linux = RemotePlatform { os: RemoteOs::Linux, arch: RemoteArch::Aarch64 };
        assert_eq!(asset_name("0.1.0", linux), "forge-remote-server-0.1.0-linux-aarch64.gz");
        let windows = RemotePlatform { os: RemoteOs::Windows, arch: RemoteArch::X86_64 };
        assert_eq!(asset_name("0.1.0", windows), "forge-remote-server-0.1.0-windows-x86_64.zip");
        assert_eq!(
            asset_url("me/forge", "0.1.0", linux),
            "https://github.com/me/forge/releases/download/v0.1.0/forge-remote-server-0.1.0-linux-aarch64.gz"
        );
    }
}
