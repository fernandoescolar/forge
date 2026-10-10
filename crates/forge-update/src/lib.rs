//! Forge updates itself from the GitHub releases of its repository: a release tagged
//! `v<version>` with a `Forge-<version>-macos-<arch>.zip` asset on macOS (what
//! `scripts/bundle-macos.sh` makes) or `Forge-<version>-linux-<arch>.tar.gz` on Linux
//! (`scripts/bundle-linux.sh`). The repository is Forge's own (`fernandoescolar/forge`), or the one set with
//! `FORGE_UPDATE_REPOSITORY` when Forge is built (for forks). Release builds check on their
//! own; debug builds only when asked (Forge › Check for Updates…).
//!
//! An update is downloaded, unpacked and checked next to the running Forge, then swapped
//! in; it runs from the next start. On macOS it must be a signed Forge.app; on Linux its
//! SHA-256 must be the one GitHub lists for the asset, and the folder it replaces must be
//! one the tarball made (`bin/forge` and `share/forge`). Forge then offers, in a
//! notification in every window, to restart now: restarting saves (or asks about) unsaved
//! work and reopens the projects that were open.

use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use futures::AsyncReadExt as _;
use gpui::{App, AppContext as _, Global, WindowHandle, actions};
use http_client::{AsyncBody, HttpClient, HttpRequestExt as _, RedirectPolicy};
use serde_json::Value;
use workspace::{
    MultiWorkspace, Workspace,
    notifications::{NotificationId, simple_message_notification::MessageNotification},
};

actions!(forge, [
    /// Looks for a newer Forge release and installs it.
    CheckForUpdates
]);

/// The GitHub repository releases come from (`owner/name`).
pub const REPOSITORY: &str = match option_env!("FORGE_UPDATE_REPOSITORY") {
    Some(repository) => repository,
    None => "fernandoescolar/forge",
};
const BUNDLE_ID: &str = "dev.forge.ide";
const CHECK_EVERY: Duration = Duration::from_secs(6 * 60 * 60);

pub fn init(cx: &mut App) {
    cx.set_global(UpdateState::default());
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|_, _: &CheckForUpdates, _, cx| check(true, cx));
    })
    .detach();
    // Release builds look on their own, at startup and every few hours.
    if !cfg!(debug_assertions) {
        // Windows: what the last update moved aside can go now.
        if cfg!(windows) {
            if let Some(prefix) = current_install() {
                cx.background_spawn(async move { remove_old_files(&prefix) }).detach();
            }
        }
        cx.spawn(async move |cx| {
            loop {
                cx.update(|cx| check(false, cx));
                cx.background_executor().timer(CHECK_EVERY).await;
            }
        })
        .detach();
    }
}

#[derive(Default)]
struct UpdateState {
    busy: bool,
    /// A version already installed, waiting for a restart.
    installed: Option<String>,
}
impl Global for UpdateState {}

/// A release that is newer than this Forge.
#[derive(Clone, Debug, PartialEq)]
pub struct Release {
    pub version: String,
    pub download: String,
    pub notes_url: String,
    /// The asset's SHA-256 as GitHub lists it (hex), when it does.
    pub sha256: Option<String>,
}

/// The release asset for this kind of machine.
pub fn asset_name(version: &str, os: &str, arch: &str) -> String {
    match os {
        "macos" => format!("Forge-{version}-macos-{arch}.zip"),
        "windows" => format!("Forge-{version}-windows-{arch}.zip"),
        os => format!("Forge-{version}-{os}-{arch}.tar.gz"),
    }
}

/// `1.10.0` > `1.9.3`; a missing part counts as 0; a pre-release suffix (`-rc.1`) sorts
/// before the release, and suffixes compare as semver says: part by part (split at dots),
/// numbers as numbers (`rc.10` > `rc.9`), a number before a word, more parts after fewer.
/// Build metadata (`+…`) doesn't count.
pub fn compare_versions(a: &str, b: &str) -> Ordering {
    let parse = |v: &str| {
        let v = v.trim().trim_start_matches('v');
        let v = v.split_once('+').map_or(v, |(v, _)| v);
        let (core, pre) = v.split_once('-').map(|(c, p)| (c, Some(p))).unwrap_or((v, None));
        let numbers: Vec<u64> = core.split('.').map(|n| n.parse().unwrap_or(0)).collect();
        (numbers, pre.map(str::to_string))
    };
    let ((mut a_numbers, a_pre), (mut b_numbers, b_pre)) = (parse(a), parse(b));
    let len = a_numbers.len().max(b_numbers.len());
    a_numbers.resize(len, 0);
    b_numbers.resize(len, 0);
    a_numbers.cmp(&b_numbers).then_with(|| match (a_pre, b_pre) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(a), Some(b)) => compare_pre_release(&a, &b),
    })
}

/// Semver's precedence for pre-release suffixes (`beta.2` < `beta.10` < `rc.1`).
fn compare_pre_release(a: &str, b: &str) -> Ordering {
    let (mut a_parts, mut b_parts) = (a.split('.'), b.split('.'));
    loop {
        match (a_parts.next(), b_parts.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) => {
                let order = match (x.parse::<u64>(), y.parse::<u64>()) {
                    (Ok(x), Ok(y)) => x.cmp(&y),
                    (Ok(_), Err(_)) => Ordering::Less,
                    (Err(_), Ok(_)) => Ordering::Greater,
                    (Err(_), Err(_)) => x.cmp(y),
                };
                if order != Ordering::Equal {
                    return order;
                }
            }
        }
    }
}

/// The newer release in GitHub's "latest release" JSON, with its asset for `os` and `arch`.
pub fn newer_release(json: &str, current: &str, os: &str, arch: &str) -> Result<Option<Release>> {
    let release: Value = serde_json::from_str(json).context("unexpected answer from GitHub")?;
    let tag = release.get("tag_name").and_then(Value::as_str).context("the release has no tag")?;
    let version = tag.trim_start_matches('v').to_string();
    if compare_versions(&version, current) != Ordering::Greater {
        return Ok(None);
    }
    let wanted = asset_name(&version, os, arch);
    // Releases up to 0.0.1-rc.4 named the macOS zip without the system: `Forge-<v>-<arch>.zip`.
    let legacy = (os == "macos").then(|| format!("Forge-{version}-{arch}.zip"));
    let assets: Vec<&Value> = release.get("assets").and_then(Value::as_array).into_iter().flatten().collect();
    let named = |name: &str| assets.iter().copied().find(|asset| asset.get("name").and_then(Value::as_str) == Some(name));
    let asset = named(&wanted).or_else(|| legacy.as_deref().and_then(named)).with_context(|| format!("release {tag} has no {wanted}"))?;
    let download = asset.get("browser_download_url").and_then(Value::as_str).with_context(|| format!("release {tag} has no {wanted}"))?;
    let sha256 = asset.get("digest").and_then(Value::as_str).and_then(|d| d.strip_prefix("sha256:")).map(str::to_lowercase);
    Ok(Some(Release { version, download: download.to_string(), notes_url: release.get("html_url").and_then(Value::as_str).unwrap_or_default().to_string(), sha256 }))
}

async fn get(client: &Arc<dyn HttpClient>, url: &str) -> Result<Vec<u8>> {
    let request = http_client::Request::builder()
        .uri(url)
        .header("Accept", "application/vnd.github+json")
        .follow_redirects(RedirectPolicy::FollowAll)
        .body(AsyncBody::empty())?;
    let mut response = client.send(request).await?;
    let mut bytes = Vec::new();
    response.body_mut().read_to_end(&mut bytes).await?;
    if !response.status().is_success() {
        bail!("{url}: {} {}", response.status(), String::from_utf8_lossy(&bytes).chars().take(200).collect::<String>());
    }
    Ok(bytes)
}

/// Looks for a release newer than `current` in `repository`.
pub async fn find_update(client: &Arc<dyn HttpClient>, repository: &str, current: &str) -> Result<Option<Release>> {
    let json = match get(client, &format!("https://api.github.com/repos/{repository}/releases/latest")).await {
        Ok(json) => json,
        // No release published yet.
        Err(e) if e.to_string().contains("404") => return Ok(None),
        Err(e) => return Err(e),
    };
    newer_release(&String::from_utf8_lossy(&json), current, std::env::consts::OS, std::env::consts::ARCH)
}

fn run(program: &str, args: &[&std::ffi::OsStr]) -> Result<()> {
    let output = ide_api::std_command(program).args(args).output().with_context(|| format!("cannot run {program}"))?;
    if !output.status.success() {
        bail!("{program} failed: {}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(())
}

/// Unpacks `zip` and replaces the app at `app` with the Forge.app in it, once that one is a
/// validly signed Forge. The old app is kept until the new one is in place.
pub fn install(zip: &Path, app: &Path) -> Result<()> {
    let parent = app.parent().context("the app has no folder")?;
    let staging = tempfile::Builder::new().prefix(".forge-update-").tempdir_in(parent).context("cannot write next to the app")?;
    run("ditto", &["-x".as_ref(), "-k".as_ref(), zip.as_os_str(), staging.path().as_os_str()])?;
    let new_app = std::fs::read_dir(staging.path())?.flatten().map(|e| e.path()).find(|p| p.extension().is_some_and(|e| e == "app")).context("the download has no app in it")?;
    run("codesign", &["--verify".as_ref(), "--deep".as_ref(), "--strict".as_ref(), new_app.as_os_str()]).context("the downloaded app isn't validly signed")?;
    let plist = std::fs::read_to_string(new_app.join("Contents/Info.plist")).context("the downloaded app has no Info.plist")?;
    if !plist.contains(&format!("<string>{BUNDLE_ID}</string>")) {
        bail!("the downloaded app isn't Forge");
    }
    let old = staging.path().join("previous.app");
    std::fs::rename(app, &old).context("cannot move the current app aside")?;
    if let Err(e) = std::fs::rename(&new_app, app) {
        std::fs::rename(&old, app).ok();
        return Err(e).context("cannot put the new app in place");
    }
    Ok(())
}

/// Unpacks `archive` (the Linux tarball or the Windows zip: a `forge/` folder) and puts it in
/// place of the installation at `prefix`, once its SHA-256 is `sha256`. On Linux the old
/// folder is kept until the new one is in place. Windows doesn't let a folder with a running
/// program in it move, nor that program be overwritten, but it does let it be renamed: there
/// each file is swapped in on its own ([`swap_in`]).
pub fn install_tree(archive: &Path, sha256: Option<&str>, prefix: &Path) -> Result<()> {
    let tarball = archive;
    use sha2::{Digest as _, Sha256};
    let expected = sha256.context("the release doesn't list the download's SHA-256")?;
    let actual: String = Sha256::digest(std::fs::read(tarball)?).iter().map(|b| format!("{b:02x}")).collect();
    if actual != expected {
        bail!("the download's SHA-256 isn't the one the release lists");
    }
    if !is_installation(prefix) {
        bail!("{} isn't a folder the Forge tarball made", prefix.display());
    }
    let parent = prefix.parent().context("the installation has no folder")?;
    let staging = tempfile::Builder::new().prefix(".forge-update-").tempdir_in(parent).context("cannot write next to the installation")?;
    // GNU tar reads the gzip of the tarball; Windows' tar (bsdtar) reads the zip.
    run("tar", &["-xf".as_ref(), tarball.as_os_str(), "-C".as_ref(), staging.path().as_os_str()])?;
    let new_tree = staging.path().join("forge");
    if !is_installation(&new_tree) {
        bail!("the download isn't a Forge tarball");
    }
    if cfg!(windows) {
        return swap_in(&new_tree, prefix);
    }
    let old = staging.path().join("previous");
    std::fs::rename(prefix, &old).context("cannot move the current installation aside")?;
    if let Err(e) = std::fs::rename(&new_tree, prefix) {
        std::fs::rename(&old, prefix).ok();
        return Err(e).context("cannot put the new installation in place");
    }
    Ok(())
}

/// What files the update replaces are renamed to, until the next start removes them.
const OLD: &str = ".forge-old";

/// Puts each file of `new_tree` in its place under `prefix`: the file there is renamed to
/// `<name>.forge-old` first (a running program can be renamed, not overwritten), and so are
/// the files the new version no longer has. [`remove_old_files`] deletes them later.
pub(crate) fn swap_in(new_tree: &Path, prefix: &Path) -> Result<()> {
    fn files(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path();
            if path.is_dir() {
                files(root, &path, out)?;
            } else if !path.to_string_lossy().ends_with(OLD) {
                out.push(path.strip_prefix(root).unwrap_or(&path).to_path_buf());
            }
        }
        Ok(())
    }
    let aside = |target: &Path| -> Result<()> {
        if target.exists() {
            let old = PathBuf::from(format!("{}{OLD}", target.display()));
            std::fs::remove_file(&old).ok();
            std::fs::rename(target, &old).with_context(|| format!("cannot move {} aside", target.display()))?;
        }
        Ok(())
    };
    let (mut new_files, mut current) = (Vec::new(), Vec::new());
    files(new_tree, new_tree, &mut new_files)?;
    files(prefix, prefix, &mut current)?;
    for relative in &new_files {
        let target = prefix.join(relative);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        aside(&target)?;
        std::fs::rename(new_tree.join(relative), &target).with_context(|| format!("cannot put {} in place", target.display()))?;
    }
    for gone in current.iter().filter(|f| !new_files.contains(f)) {
        aside(&prefix.join(gone))?;
    }
    Ok(())
}

/// Deletes what an update on Windows left aside ([`swap_in`]); files still in use stay
/// for the next time.
pub fn remove_old_files(prefix: &Path) {
    fn walk(dir: &Path) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for path in entries.flatten().map(|e| e.path()) {
            if path.is_dir() {
                walk(&path);
            } else if path.to_string_lossy().ends_with(OLD) {
                std::fs::remove_file(&path).ok();
            }
        }
    }
    walk(prefix);
}

/// The program in an installation's `bin` folder.
const EXE: &str = if cfg!(windows) { "forge.exe" } else { "forge" };

/// A folder laid out like the Linux tarball or the Windows zip: `bin/forge` and `share/forge`.
fn is_installation(prefix: &Path) -> bool {
    prefix.join("bin").join(EXE).is_file() && prefix.join("share/forge").is_dir()
}

/// What an update replaces: the running app bundle (`…/Forge.app`) on macOS, the folder
/// the tarball or zip made (`…/forge`, with `bin/forge`) elsewhere; `None` when Forge doesn't run
/// from one (a development build).
fn current_install() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    if cfg!(target_os = "macos") {
        let app = exe.parent()?.parent()?.parent()?;
        return app.extension().is_some_and(|e| e == "app").then(|| app.to_path_buf());
    }
    let prefix = exe.parent()?.parent()?;
    is_installation(prefix).then(|| prefix.to_path_buf())
}

/// The Forge windows, the active one first. Updates are found in the background, often
/// while another app is in front, so any window will do.
fn windows(cx: &App) -> Vec<WindowHandle<MultiWorkspace>> {
    let mut windows: Vec<_> = cx.windows().into_iter().filter_map(|w| w.downcast::<MultiWorkspace>()).collect();
    let active = cx.active_window().map(|w| w.window_id());
    windows.sort_by_key(|w| Some(w.window_id()) != active);
    windows
}

/// Answers Check for Updates in a toast.
fn notify(message: impl Into<String>, cx: &mut App) {
    let message = message.into();
    let Some(workspace) = windows(cx).first().and_then(|w| w.read(cx).ok().map(|mw| mw.workspace().clone())) else {
        log::info!("{message}");
        return;
    };
    let toast = workspace::Toast::new(NotificationId::named("forge-update".into()), message);
    workspace.update(cx, |ws, cx| ws.show_toast(toast, cx));
}

/// Offers to restart into the installed `version` in a notification in every window
/// (and in windows opened later, when none is open yet). Closing it is "later": Check for
/// Updates offers again.
fn offer_restart(version: &str, cx: &mut App) {
    let message = format!("Forge {version} is installed. Restart now to use it: Forge reopens your projects, and asks first about unsaved changes.");
    workspace::notifications::show_app_notification(restart_notification_id(), cx, move |cx| {
        let message = message.clone();
        cx.new(move |cx| {
            MessageNotification::new(message, cx)
                .with_title("Update ready")
                .show_suppress_button(false)
                .primary_message("Restart Now")
                .primary_on_click(|_, cx| cx.spawn(async move |_, cx| restart_forge(cx).await).detach())
                .secondary_message("Later")
                .secondary_on_click(|_, _| {})
        })
    });
}

fn restart_notification_id() -> NotificationId {
    NotificationId::named("forge-update-restart".into())
}

/// Restarts into the installed update once every window is ready to close (unsaved work
/// saved or discarded, layout remembered), so the new Forge reopens the same projects.
async fn restart_forge(cx: &mut gpui::AsyncApp) {
    let windows = cx.update(|cx| windows(cx));
    if workspace::prepare_windows_to_quit(&windows, cx).await {
        cx.update(|cx| cx.restart());
    }
}

/// Looks for an update and installs it. `asked`: the user chose Check for Updates, so say
/// what happened even when there is nothing to do.
pub fn check(asked: bool, cx: &mut App) {
    let repository = REPOSITORY;
    let state = cx.global_mut::<UpdateState>();
    if let Some(version) = state.installed.clone() {
        if asked {
            offer_restart(&version, cx);
        }
        return;
    }
    if state.busy {
        return;
    }
    state.busy = true;
    let client = cx.http_client();
    let current = env!("CARGO_PKG_VERSION");
    cx.spawn(async move |cx| {
        let result: Result<Option<String>> = async {
            let Some(release) = find_update(&client, repository, current).await? else { return Ok(None) };
            let target = current_install().context("Forge isn't running from an installed copy (an app bundle, or the folder of its tarball)")?;
            let bytes = get(&client, &release.download).await?;
            let version = release.version.clone();
            cx.background_spawn(async move {
                let download = tempfile::Builder::new().suffix(if cfg!(any(target_os = "macos", windows)) { ".zip" } else { ".tar.gz" }).tempfile()?;
                std::fs::write(download.path(), &bytes)?;
                if cfg!(target_os = "macos") { install(download.path(), &target) } else { install_tree(download.path(), release.sha256.as_deref(), &target) }
            })
            .await?;
            Ok(Some(version))
        }
        .await;
        cx.update(|cx| {
            let state = cx.global_mut::<UpdateState>();
            state.busy = false;
            match result {
                Ok(Some(version)) => {
                    state.installed = Some(version.clone());
                    offer_restart(&version, cx);
                }
                Ok(None) if asked => notify(format!("Forge {current} is the latest version."), cx),
                Ok(None) => {}
                Err(e) if asked => notify(format!("Couldn't update Forge: {e:#}"), cx),
                Err(e) => log::warn!("update check failed: {e:#}"),
            }
        });
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_versions() {
        assert_eq!(compare_versions("1.10.0", "1.9.3"), Ordering::Greater);
        assert_eq!(compare_versions("v0.2", "0.2.0"), Ordering::Equal);
        assert_eq!(compare_versions("0.2.0-beta.1", "0.2.0"), Ordering::Less);
        // Pre-releases, as semver orders them.
        assert_eq!(compare_versions("0.0.1-rc.10", "0.0.1-rc.9"), Ordering::Greater, "numbers as numbers");
        assert_eq!(compare_versions("0.0.1-rc.1", "0.0.1-kappa"), Ordering::Greater, "from the Greek letters to rc");
        assert_eq!(compare_versions("0.0.1", "0.0.1-rc.3"), Ordering::Greater, "the release after its candidates");
        assert_eq!(compare_versions("0.0.1-beta.2", "0.0.1-beta"), Ordering::Greater, "more parts after fewer");
        assert_eq!(compare_versions("0.0.1-1", "0.0.1-alpha"), Ordering::Less, "a number before a word");
        assert_eq!(compare_versions("0.0.1-rc.1+build.5", "0.0.1-rc.1"), Ordering::Equal, "build metadata doesn't count");
        assert_eq!(compare_versions("0.1.0", "0.2.0"), Ordering::Less);
    }

    #[test]
    fn picks_the_asset_for_this_machine() {
        let json = r#"{"tag_name": "v0.3.0", "html_url": "https://github.com/o/forge/releases/tag/v0.3.0", "assets": [
            {"name": "Forge-0.3.0-macos-x86_64.zip", "browser_download_url": "https://x/intel.zip"},
            {"name": "Forge-0.3.0-macos-aarch64.zip", "browser_download_url": "https://x/arm.zip"}]}"#;
        let release = newer_release(json, "0.2.0", "macos", "aarch64").unwrap().unwrap();
        assert_eq!((release.version.as_str(), release.download.as_str()), ("0.3.0", "https://x/arm.zip"));
        assert_eq!(newer_release(json, "0.3.0", "macos", "aarch64").unwrap(), None, "already up to date");
        assert!(newer_release(json, "0.2.0", "macos", "riscv64").unwrap_err().to_string().contains("no Forge-0.3.0-macos-riscv64.zip"));
        // Releases from before the macOS zip said so in its name.
        let legacy = r#"{"tag_name": "v0.3.0", "assets": [{"name": "Forge-0.3.0-aarch64.zip", "browser_download_url": "https://x/old-arm.zip"}]}"#;
        assert_eq!(newer_release(legacy, "0.2.0", "macos", "aarch64").unwrap().unwrap().download, "https://x/old-arm.zip");

        let linux = r#"{"tag_name": "v0.3.0", "assets": [
            {"name": "Forge-0.3.0-linux-x86_64.tar.gz", "browser_download_url": "https://x/linux.tar.gz", "digest": "sha256:ABC123"}]}"#;
        let release = newer_release(linux, "0.2.0", "linux", "x86_64").unwrap().unwrap();
        assert_eq!((release.download.as_str(), release.sha256.as_deref()), ("https://x/linux.tar.gz", Some("abc123")));
    }

    /// Windows' way: file by file, the replaced ones moved aside until the next start.
    #[test]
    fn swaps_files_in_one_by_one() {
        let dir = tempfile::tempdir().unwrap();
        let (prefix, new) = (dir.path().join("installed"), dir.path().join("new"));
        for (root, files) in [(&prefix, &[("bin/forge.exe", "0.2.0"), ("share/forge/extensions/old/package.json", "gone")][..]), (&new, &[("bin/forge.exe", "0.3.0"), ("share/forge/extensions/db/package.json", "db")][..])] {
            for (file, text) in files {
                std::fs::create_dir_all(root.join(file).parent().unwrap()).unwrap();
                std::fs::write(root.join(file), text).unwrap();
            }
        }
        swap_in(&new, &prefix).unwrap();
        let read = |file: &str| std::fs::read_to_string(prefix.join(file)).ok();
        assert_eq!(read("bin/forge.exe").as_deref(), Some("0.3.0"));
        assert_eq!(read("bin/forge.exe.forge-old").as_deref(), Some("0.2.0"), "the running one is renamed, not overwritten");
        assert_eq!(read("share/forge/extensions/db/package.json").as_deref(), Some("db"));
        assert_eq!(read("share/forge/extensions/old/package.json"), None, "what the new version doesn't have is moved aside too");

        // An update over one that left files aside replaces those too.
        std::fs::write(new.join("bin/forge.exe"), "0.4.0").unwrap();
        swap_in(&new, &prefix).unwrap();
        assert_eq!(read("bin/forge.exe.forge-old").as_deref(), Some("0.3.0"));

        remove_old_files(&prefix);
        assert_eq!(read("bin/forge.exe").as_deref(), Some("0.4.0"));
        assert_eq!(read("bin/forge.exe.forge-old"), None, "the next start removes them");
        assert_eq!(read("share/forge/extensions/old/package.json.forge-old"), None);
    }

    /// The Linux tarball replaces the folder it made before, when its SHA-256 matches.
    #[test]
    fn installs_a_tarball_over_the_installed_one() {
        let dir = tempfile::tempdir().unwrap();
        let tree = |root: &Path, marker: &str| {
            std::fs::create_dir_all(root.join("forge/bin")).unwrap();
            std::fs::create_dir_all(root.join("forge/share/forge/extensions")).unwrap();
            std::fs::write(root.join("forge/bin/forge"), marker).unwrap();
        };
        tree(&dir.path().join("installed"), "0.2.0");
        let prefix = dir.path().join("installed/forge");
        tree(&dir.path().join("release"), "0.3.0");
        let tarball = dir.path().join("Forge-0.3.0-linux-x86_64.tar.gz");
        run("tar", &["-czf".as_ref(), tarball.as_os_str(), "-C".as_ref(), dir.path().join("release").as_os_str(), "forge".as_ref()]).unwrap();
        let digest: String = {
            use sha2::{Digest as _, Sha256};
            Sha256::digest(std::fs::read(&tarball).unwrap()).iter().map(|b| format!("{b:02x}")).collect()
        };

        assert!(install_tree(&tarball, Some("00"), &prefix).unwrap_err().to_string().contains("SHA-256"));
        assert!(install_tree(&tarball, None, &prefix).is_err(), "no digest, no update");
        assert!(install_tree(&tarball, Some(&digest), dir.path()).unwrap_err().to_string().contains("isn't a folder the Forge tarball made"));
        assert_eq!(std::fs::read_to_string(prefix.join("bin/forge")).unwrap(), "0.2.0", "a rejected update changes nothing");

        install_tree(&tarball, Some(&digest), &prefix).unwrap();
        assert_eq!(std::fs::read_to_string(prefix.join("bin/forge")).unwrap(), "0.3.0");
        let leftovers: Vec<String> = std::fs::read_dir(dir.path().join("installed")).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(leftovers, ["forge"], "the old installation and the staging folder are gone");
    }

    /// A tiny signed app bundle, zipped like a release.
    #[cfg(target_os = "macos")]
    fn release_zip(dir: &Path, bundle_id: &str, marker: &str) -> PathBuf {
        let app = dir.join(format!("build-{marker}/Forge.app"));
        std::fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
        std::fs::copy("/bin/echo", app.join("Contents/MacOS/forge")).unwrap();
        std::fs::write(app.join("Contents/Resources.txt"), marker).ok();
        let plist = format!(r#"<?xml version="1.0"?><plist version="1.0"><dict><key>CFBundleExecutable</key><string>forge</string><key>CFBundleIdentifier</key><string>{bundle_id}</string><key>CFBundleShortVersionString</key><string>{marker}</string></dict></plist>"#);
        std::fs::write(app.join("Contents/Info.plist"), plist).unwrap();
        run("codesign", &["--force".as_ref(), "--deep".as_ref(), "--sign".as_ref(), "-".as_ref(), app.as_os_str()]).unwrap();
        let zip = dir.join(format!("{marker}.zip"));
        run("ditto", &["-c".as_ref(), "-k".as_ref(), "--keepParent".as_ref(), app.as_os_str(), zip.as_os_str()]).unwrap();
        zip
    }

    /// A real release zip over a copy of an installed app:
    /// `FORGE_UPDATE_ZIP=dist/Forge-<v>-macos-<arch>.zip FORGE_UPDATE_APP=/tmp/x/Forge.app cargo test -p forge-update -- --ignored`
    #[test]
    #[ignore]
    #[cfg(target_os = "macos")]
    fn installs_a_real_release() {
        let (Ok(zip), Ok(app)) = (std::env::var("FORGE_UPDATE_ZIP"), std::env::var("FORGE_UPDATE_APP")) else { return };
        assert!(Path::new(&zip).file_name().unwrap().to_string_lossy().ends_with(&format!("-{}.zip", std::env::consts::ARCH)), "named for this machine");
        install(Path::new(&zip), Path::new(&app)).unwrap();
        assert!(!Path::new(&app).join("Contents/marker").exists(), "replaced");
        assert!(Path::new(&app).join("Contents/MacOS/forge").is_file());
    }

    /// An update found before any window is open is offered in the first one, in the
    /// window rather than in a dialog; Later keeps Forge running, and Check for Updates
    /// offers again.
    #[gpui::test]
    async fn offers_to_restart_into_an_installed_update(cx: &mut gpui::TestAppContext) {
        let params = cx.update(workspace::AppState::test);
        cx.update(|cx| {
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            init(cx);
            cx.global_mut::<UpdateState>().installed = Some("9.9.9".into());
            offer_restart("9.9.9", cx);
        });
        cx.run_until_parked();

        params.fs.as_fake().insert_tree("/root", serde_json::json!({ "a.txt": "" })).await;
        let project = project::Project::test(params.fs.clone(), ["/root".as_ref()], cx).await;
        let window = cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let cx = &mut gpui::VisualTestContext::from_window(window.into(), cx);
        cx.run_until_parked();
        let workspace = window.read_with(cx, |mw, _| mw.workspace().clone()).unwrap();
        let shown = |cx: &mut gpui::VisualTestContext| workspace.read_with(cx, |ws, _| ws.has_notification(&restart_notification_id()));
        assert!(shown(cx), "offered as soon as a window opens");
        assert!(!cx.has_pending_prompt(), "not in a dialog");

        workspace.update(cx, |ws, cx| ws.dismiss_notification(&restart_notification_id(), cx));
        cx.run_until_parked();
        assert!(!shown(cx));

        cx.update(|_, cx| check(true, cx));
        cx.run_until_parked();
        assert!(shown(cx), "Check for Updates offers again");
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn installs_a_signed_forge_over_the_running_one() {
        let dir = tempfile::tempdir().unwrap();
        let installed = dir.path().join("Applications/Forge.app");
        std::fs::create_dir_all(installed.join("Contents")).unwrap();
        std::fs::write(installed.join("Contents/Info.plist"), "old").unwrap();

        install(&release_zip(dir.path(), BUNDLE_ID, "0.3.0"), &installed).unwrap();
        assert!(std::fs::read_to_string(installed.join("Contents/Info.plist")).unwrap().contains("0.3.0"));
        let leftovers: Vec<_> = std::fs::read_dir(dir.path().join("Applications")).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(leftovers, ["Forge.app"], "the old app and the staging folder are gone");

        let other = release_zip(dir.path(), "com.example.other", "9.9.9");
        assert!(install(&other, &installed).unwrap_err().to_string().contains("isn't Forge"));
        assert!(std::fs::read_to_string(installed.join("Contents/Info.plist")).unwrap().contains("0.3.0"), "a rejected update changes nothing");
    }
}
