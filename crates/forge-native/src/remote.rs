//! Folders on other machines, over SSH (File › Open Remote…): Zed's remote development with
//! Forge's server. Forge connects with the system's `ssh`, puts `forge-remote-server` in
//! `~/.forge_server/` on the host the first time (see `forge_update::remote_server` for
//! where it comes from) and runs it there; the editor, language servers, terminals, search,
//! Git and the debugger then work on the host's files. Debug builds compile the server from
//! this checkout instead, or upload the one `ZED_COPY_REMOTE_SERVER` names.
//!
//! `forge ssh://[user@]host[:port]/path` opens a remote folder from the command line.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result};
use gpui::{App, AsyncApp};
use recent_projects::disconnected_overlay::DisconnectedOverlay;
use recent_projects::{RemoteServerProjects, RemoteSettings};
use remote::RemoteConnectionOptions;
use settings::Settings as _;
use workspace::{AppState, OpenOptions, with_active_or_new_workspace};

pub fn init(cx: &mut App) {
    remote_connection::set_remote_server_releases(Arc::new(forge_update::remote_server::ForgeServerReleases { http: cx.http_client() }), cx);
    // What a window shows when its host goes away, with Reconnect.
    cx.observe_new(DisconnectedOverlay::register).detach();
    cx.on_action(|action: &zed_actions::OpenRemote, cx| {
        if action.from_existing_connection {
            return;
        }
        let create_new_window = action.create_new_window.unwrap_or(false);
        with_active_or_new_workspace(cx, move |workspace, window, cx| {
            let handle = cx.entity().downgrade();
            let fs = workspace.project().read(cx).fs().clone();
            workspace.toggle_modal(window, cx, |window, cx| RemoteServerProjects::new(create_new_window, fs, window, handle, cx));
        });
    });
}

/// A folder on a host, from `ssh://[user@]host[:port]/path` (`/~/path` for one in the home
/// folder).
#[derive(Debug, PartialEq)]
pub struct SshTarget {
    pub host: String,
    pub username: Option<String>,
    pub port: Option<u16>,
    pub path: PathBuf,
}

/// The `ssh://` address `path` holds, if it holds one rather than a local path.
pub fn ssh_target(path: &Path) -> Option<Result<SshTarget>> {
    let text = path.to_str()?;
    text.starts_with("ssh://").then(|| parse_ssh_url(text))
}

fn parse_ssh_url(text: &str) -> Result<SshTarget> {
    let url = url::Url::parse(text).with_context(|| format!("not an ssh:// address: {text}"))?;
    let host = match url.host().with_context(|| format!("no host in {text}"))? {
        url::Host::Domain(host) => host.to_string(),
        url::Host::Ipv4(host) => host.to_string(),
        url::Host::Ipv6(host) => host.to_string(),
    };
    let decode = |part: &str| percent_encoding::percent_decode_str(part).decode_utf8().map(|part| part.into_owned());
    let username = (!url.username().is_empty()).then(|| decode(url.username())).transpose()?;
    let path = decode(url.path())?;
    // The remote server expands `~` itself.
    let path = match path.strip_prefix("/~") {
        Some(rest) => format!("~{rest}"),
        None if path.is_empty() => "~".to_string(),
        None => path,
    };
    Ok(SshTarget { host, username, port: url.port(), path: PathBuf::from(path) })
}

/// Opens each host's folders in a window of its own, the way File › Open Remote… would.
pub async fn open_ssh_targets(targets: Vec<SshTarget>, app_state: Arc<AppState>, cx: &mut AsyncApp) -> Result<()> {
    let mut by_host: Vec<((String, Option<String>, Option<u16>), Vec<PathBuf>)> = Vec::new();
    for target in targets {
        let key = (target.host, target.username, target.port);
        match by_host.iter_mut().find(|(k, _)| *k == key) {
            Some((_, paths)) => paths.push(target.path),
            None => by_host.push((key, vec![target.path])),
        }
    }
    for ((host, username, port), paths) in by_host {
        // A host saved in settings (`ssh_connections`) keeps its nickname, arguments and ports.
        let options = cx.update(|cx| RemoteSettings::get_global(cx).connection_options_for(host.clone(), port, username));
        let opened = recent_projects::open_remote_project(RemoteConnectionOptions::Ssh(options), paths, app_state.clone(), OpenOptions::default(), cx).await;
        opened.with_context(|| format!("failed to open {host}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_ssh_addresses() {
        let target = |s: &str| ssh_target(Path::new(s)).map(|t| t.unwrap());
        assert_eq!(target("/home/me/code"), None, "a local path");
        assert_eq!(
            target("ssh://me@box.lan:2222/srv/app"),
            Some(SshTarget { host: "box.lan".into(), username: Some("me".into()), port: Some(2222), path: "/srv/app".into() })
        );
        assert_eq!(target("ssh://box/~/my%20code").unwrap().path, PathBuf::from("~/my code"), "in the home folder");
        assert_eq!(target("ssh://box").unwrap().path, PathBuf::from("~"), "no path: the home folder");
        assert!(ssh_target(Path::new("ssh://")).unwrap().is_err());
    }
}
