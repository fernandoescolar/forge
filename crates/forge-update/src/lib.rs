//! Forge updates itself from the GitHub releases of its repository: a release tagged
//! `v<version>` with a `Forge-<version>-<arch>.zip` asset on macOS (what
//! `scripts/bundle-macos.sh` makes) or `Forge-<version>-linux-<arch>.tar.gz` on Linux
//! (`scripts/bundle-linux.sh`). The repository is Forge's own (`fernandoescolar/forge`), or the one set with
//! `FORGE_UPDATE_REPOSITORY` when Forge is built (for forks). Release builds check on their
//! own; debug builds only when asked (Forge › Check for Updates…).
//!
//! An update is downloaded, unpacked and checked next to the running Forge, then swapped
//! in; it runs from the next start. On macOS it must be a signed Forge.app; on Linux its
//! SHA-256 must be the one GitHub lists for the asset, and the folder it replaces must be
//! one the tarball made (`bin/forge` and `share/forge`). Forge then asks, in a
//! dialog, whether to restart now: restarting saves (or asks about) unsaved work and
//! reopens the projects that were open.

use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use futures::AsyncReadExt as _;
use gpui::{App, AppContext as _, Global, PromptLevel, WindowHandle, actions};
use http_client::{AsyncBody, HttpClient, HttpRequestExt as _, RedirectPolicy};
use serde_json::Value;
use workspace::{MultiWorkspace, Workspace, notifications::NotificationId};

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
    cx.observe_new(|workspace: &mut Workspace, _, cx| {
        workspace.register_action(|_, _: &CheckForUpdates, _, cx| check(true, cx));
        // An update installed before any window was open asks in the first one.
        if cx.global::<UpdateState>().unanswered {
            if let Some(version) = cx.global::<UpdateState>().installed.clone() {
                offer_restart(&version, cx);
            }
        }
    })
    .detach();
    // Release builds look on their own, at startup and every few hours.
    if !cfg!(debug_assertions) {
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
    /// What to start on restart: the update's app bundle or `bin/forge`. The running
    /// executable's own path no longer works then, as the old copy is gone.
    restart_path: Option<PathBuf>,
    /// The restart dialog for `installed` couldn't be shown yet (no window was open).
    unanswered: bool,
    /// The restart dialog is on screen.
    asking: bool,
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
    /// Where its signature (`<asset>.sig`) is, when the release has one.
    pub signature: Option<String>,
}

/// Forge's release key (Ed25519: base64 of its 32 bytes), built in from
/// `FORGE_UPDATE_PUBLIC_KEY` (scripts/create-update-key.sh makes one). With it, updates
/// install only downloads signed with the matching private key, which only the release
/// workflow holds: a release asset swapped on GitHub, or a stolen token, isn't enough.
const RELEASE_KEY: Option<&str> = option_env!("FORGE_UPDATE_PUBLIC_KEY");

/// Checks that `signature` is `public_key`'s Ed25519 signature of `data`.
pub fn verify_signature(public_key: &str, data: &[u8], signature: &[u8]) -> Result<()> {
    use base64::Engine as _;
    let key = base64::engine::general_purpose::STANDARD.decode(public_key.trim()).context("the release key isn't base64")?;
    anyhow::ensure!(key.len() == 32, "the release key isn't an Ed25519 public key");
    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, key)
        .verify(data, signature)
        .map_err(|_| anyhow::anyhow!("the download isn't signed with Forge's release key"))
}

/// The SHA-256 of `data`, in hex.
fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest as _, Sha256};
    Sha256::digest(data).iter().map(|b| format!("{b:02x}")).collect()
}

/// The release asset for this kind of machine.
pub fn asset_name(version: &str, os: &str, arch: &str) -> String {
    match os {
        "macos" => format!("Forge-{version}-{arch}.zip"),
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
    let asset = release
        .get("assets")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|asset| asset.get("name").and_then(Value::as_str) == Some(wanted.as_str()))
        .with_context(|| format!("release {tag} has no {wanted}"))?;
    let download = asset.get("browser_download_url").and_then(Value::as_str).with_context(|| format!("release {tag} has no {wanted}"))?;
    let sha256 = asset.get("digest").and_then(Value::as_str).and_then(|d| d.strip_prefix("sha256:")).map(str::to_lowercase);
    let signature = release
        .get("assets")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|asset| asset.get("name").and_then(Value::as_str) == Some(format!("{wanted}.sig").as_str()))
        .and_then(|asset| asset.get("browser_download_url").and_then(Value::as_str))
        .map(str::to_string);
    Ok(Some(Release { version, download: download.to_string(), notes_url: release.get("html_url").and_then(Value::as_str).unwrap_or_default().to_string(), sha256, signature }))
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
    let mut command = std::process::Command::new(program);
    command.args(args);
    forge_process::output(command).map(drop)
}

/// Unpacks `zip` and replaces the app at `app` with the Forge.app in it, once that one is a
/// validly signed Forge. The old app is kept until the new one is in place.
pub fn install(zip: &Path, app: &Path) -> Result<()> {
    let parent = app.parent().context("the app has no folder")?;
    let staging = tempfile::Builder::new().prefix(".forge-update-").tempdir_in(parent).context("cannot write next to the app")?;
    run("ditto", &["-x".as_ref(), "-k".as_ref(), zip.as_os_str(), staging.path().as_os_str()])?;
    let new_app = std::fs::read_dir(staging.path())?.flatten().map(|e| e.path()).find(|p| p.extension().is_some_and(|e| e == "app")).context("the download has no app in it")?;
    run("codesign", &["--verify".as_ref(), "--deep".as_ref(), "--strict".as_ref(), new_app.as_os_str()]).context("the downloaded app isn't validly signed")?;
    // Signed like this one: the same certificate (any valid signature, ad-hoc included,
    // would pass the check above). An ad-hoc signed Forge has no certificate to compare.
    if let Some(requirement) = designated_requirement(app).filter(|r| !r.contains("cdhash")) {
        let requirement = format!("-R={requirement}");
        run("codesign", &["--verify".as_ref(), "--deep".as_ref(), "--strict".as_ref(), requirement.as_ref(), new_app.as_os_str()]).context("the downloaded app isn't signed with this Forge's certificate")?;
    }
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

/// The designated requirement of the app at `app` (`codesign -d -r-`): what a signature
/// of the same app must satisfy.
fn designated_requirement(app: &Path) -> Option<String> {
    let output = std::process::Command::new("codesign").args(["-d", "-r-"]).arg(app).output().ok()?;
    let text = format!("{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    text.lines().find_map(|l| l.strip_prefix("designated => ")).map(|r| r.trim().to_string())
}

/// Unpacks the Linux tarball `tarball` (a `forge/` folder) and puts it in place of the
/// installation at `prefix`, once its SHA-256 is `sha256`. The old one is kept until the
/// new one is in place.
pub fn install_tree(tarball: &Path, sha256: Option<&str>, prefix: &Path) -> Result<()> {
    let expected = sha256.context("the release doesn't list the download's SHA-256")?;
    let actual = sha256_hex(&std::fs::read(tarball)?);
    if actual != expected {
        bail!("the download's SHA-256 isn't the one the release lists");
    }
    if !is_installation(prefix) {
        bail!("{} isn't a folder the Forge tarball made", prefix.display());
    }
    let parent = prefix.parent().context("the installation has no folder")?;
    let staging = tempfile::Builder::new().prefix(".forge-update-").tempdir_in(parent).context("cannot write next to the installation")?;
    run("tar", &["-xzf".as_ref(), tarball.as_os_str(), "-C".as_ref(), staging.path().as_os_str()])?;
    let new_tree = staging.path().join("forge");
    if !is_installation(&new_tree) {
        bail!("the download isn't a Forge tarball");
    }
    let old = staging.path().join("previous");
    std::fs::rename(prefix, &old).context("cannot move the current installation aside")?;
    if let Err(e) = std::fs::rename(&new_tree, prefix) {
        std::fs::rename(&old, prefix).ok();
        return Err(e).context("cannot put the new installation in place");
    }
    Ok(())
}

/// A folder laid out like the Linux tarball: `bin/forge` and `share/forge`.
fn is_installation(prefix: &Path) -> bool {
    prefix.join("bin/forge").is_file() && prefix.join("share/forge").is_dir()
}

/// What an update replaces: the running app bundle (`…/Forge.app`) on macOS, the folder
/// the tarball made (`…/forge`, with `bin/forge`) elsewhere; `None` when Forge doesn't run
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

/// Asks whether to restart into the installed `version` now; asks in the first window
/// that opens when none is open yet. Check for Updates runs while its window is being
/// updated, and the dialog needs that window: ask right after.
fn offer_restart(version: &str, cx: &mut App) {
    let version = version.to_string();
    cx.defer(move |cx| ask_to_restart(&version, cx));
}

fn ask_to_restart(version: &str, cx: &mut App) {
    let state = cx.global_mut::<UpdateState>();
    if state.asking {
        return;
    }
    let Some(window) = windows(cx).into_iter().next() else {
        cx.global_mut::<UpdateState>().unanswered = true;
        return;
    };
    let message = format!("Forge {version} is installed");
    let detail = "Restart now to use it. Forge reopens your projects, and asks first about unsaved changes.";
    let Ok(answer) = window.update(cx, |_, window, cx| window.prompt(PromptLevel::Info, &message, Some(detail), &["Restart Now", "Later"], cx)) else {
        return;
    };
    let state = cx.global_mut::<UpdateState>();
    state.asking = true;
    state.unanswered = false;
    cx.spawn(async move |cx| {
        let restart = answer.await == Ok(0);
        cx.update(|cx| cx.global_mut::<UpdateState>().asking = false);
        if restart {
            restart_forge(cx).await;
        }
    })
    .detach();
}

/// Restarts into the installed update once every window is ready to close (unsaved work
/// saved or discarded, layout remembered), so the new Forge reopens the same projects.
async fn restart_forge(cx: &mut gpui::AsyncApp) {
    let windows = cx.update(|cx| windows(cx));
    if workspace::prepare_windows_to_quit(&windows, cx).await {
        cx.update(|cx| {
            if let Some(path) = cx.global::<UpdateState>().restart_path.clone() {
                cx.set_restart_path(path);
            }
            cx.restart();
        });
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
        let result: Result<Option<(String, PathBuf)>> = async {
            let Some(release) = find_update(&client, repository, current).await? else { return Ok(None) };
            let target = current_install().context("Forge isn't running from an installed copy (an app bundle, or the folder of its tarball)")?;
            let bytes = get(&client, &release.download).await?;
            if let Some(expected) = &release.sha256 {
                anyhow::ensure!(sha256_hex(&bytes) == *expected, "the download's SHA-256 isn't the one the release lists");
            }
            if let Some(key) = RELEASE_KEY.filter(|k| !k.trim().is_empty()) {
                let url = release.signature.as_deref().context("the release isn't signed, and this Forge only installs signed updates")?;
                verify_signature(key, &bytes, &get(&client, url).await?)?;
            }
            let version = release.version.clone();
            let restart_path = if cfg!(target_os = "macos") { target.clone() } else { target.join("bin/forge") };
            cx.background_spawn(async move {
                let download = tempfile::Builder::new().suffix(if cfg!(target_os = "macos") { ".zip" } else { ".tar.gz" }).tempfile()?;
                std::fs::write(download.path(), &bytes)?;
                if cfg!(target_os = "macos") { install(download.path(), &target) } else { install_tree(download.path(), release.sha256.as_deref(), &target) }
            })
            .await?;
            Ok(Some((version, restart_path)))
        }
        .await;
        cx.update(|cx| {
            let state = cx.global_mut::<UpdateState>();
            state.busy = false;
            match result {
                Ok(Some((version, restart_path))) => {
                    state.installed = Some(version.clone());
                    state.restart_path = Some(restart_path);
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
            {"name": "Forge-0.3.0-x86_64.zip", "browser_download_url": "https://x/intel.zip"},
            {"name": "Forge-0.3.0-aarch64.zip", "browser_download_url": "https://x/arm.zip"}]}"#;
        let release = newer_release(json, "0.2.0", "macos", "aarch64").unwrap().unwrap();
        assert_eq!((release.version.as_str(), release.download.as_str()), ("0.3.0", "https://x/arm.zip"));
        assert_eq!(newer_release(json, "0.3.0", "macos", "aarch64").unwrap(), None, "already up to date");
        assert!(newer_release(json, "0.2.0", "macos", "riscv64").unwrap_err().to_string().contains("no Forge-0.3.0-riscv64.zip"));

        let linux = r#"{"tag_name": "v0.3.0", "assets": [
            {"name": "Forge-0.3.0-linux-x86_64.tar.gz", "browser_download_url": "https://x/linux.tar.gz", "digest": "sha256:ABC123"}]}"#;
        let release = newer_release(linux, "0.2.0", "linux", "x86_64").unwrap().unwrap();
        assert_eq!((release.download.as_str(), release.sha256.as_deref()), ("https://x/linux.tar.gz", Some("abc123")));
        assert_eq!(release.signature, None);

        let signed = r#"{"tag_name": "v0.3.0", "assets": [
            {"name": "Forge-0.3.0-linux-x86_64.tar.gz", "browser_download_url": "https://x/linux.tar.gz"},
            {"name": "Forge-0.3.0-linux-x86_64.tar.gz.sig", "browser_download_url": "https://x/linux.tar.gz.sig"}]}"#;
        assert_eq!(newer_release(signed, "0.2.0", "linux", "x86_64").unwrap().unwrap().signature.as_deref(), Some("https://x/linux.tar.gz.sig"));
    }

    /// Only the release key's signature of exactly these bytes passes.
    #[test]
    fn verifies_release_signatures() {
        use base64::Engine as _;
        use ring::signature::{Ed25519KeyPair, KeyPair as _};
        let random = ring::rand::SystemRandom::new();
        let keys = |random| Ed25519KeyPair::from_pkcs8(Ed25519KeyPair::generate_pkcs8(random).unwrap().as_ref()).unwrap();
        let (release_key, other_key) = (keys(&random), keys(&random));
        let public = base64::engine::general_purpose::STANDARD.encode(release_key.public_key().as_ref());
        let data = b"the tarball";
        let signature = release_key.sign(data);
        verify_signature(&public, data, signature.as_ref()).unwrap();
        assert!(verify_signature(&public, b"another tarball", signature.as_ref()).is_err());
        assert!(verify_signature(&public, data, other_key.sign(data).as_ref()).is_err());
        assert!(verify_signature("not base64!", data, signature.as_ref()).is_err());
    }

    /// What the release workflow does (`openssl pkeyutl -sign -rawin`) verifies here, with
    /// the public key as scripts/create-update-key.sh prints it.
    #[test]
    fn verifies_signatures_made_with_openssl() {
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("key.pem");
        let openssl = |args: &[&str]| std::process::Command::new("openssl").args(args).current_dir(dir.path()).output();
        let Ok(out) = openssl(&["genpkey", "-algorithm", "ed25519", "-out", key.to_str().unwrap()]) else { return }; // no openssl
        if !out.status.success() {
            return;
        }
        std::fs::write(dir.path().join("asset"), b"the zip").unwrap();
        assert!(openssl(&["pkeyutl", "-sign", "-inkey", "key.pem", "-rawin", "-in", "asset", "-out", "asset.sig"]).unwrap().status.success());
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/create-update-key.sh");
        let public = std::process::Command::new("bash").arg(&script).arg("--public-of").arg(&key).output().unwrap();
        assert!(public.status.success(), "{}", String::from_utf8_lossy(&public.stderr));
        let public = String::from_utf8(public.stdout).unwrap();
        verify_signature(&public, b"the zip", &std::fs::read(dir.path().join("asset.sig")).unwrap()).unwrap();
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
    /// `FORGE_UPDATE_ZIP=dist/Forge-<v>-<arch>.zip FORGE_UPDATE_APP=/tmp/x/Forge.app cargo test -p forge-update -- --ignored`
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

    /// An update found before any window is open asks in the first one; Later keeps
    /// Forge running, and Check for Updates asks again and restarts.
    #[gpui::test]
    async fn asks_to_restart_into_an_installed_update(cx: &mut gpui::TestAppContext) {
        let params = cx.update(workspace::AppState::test);
        cx.update(|cx| {
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            init(cx);
            cx.global_mut::<UpdateState>().installed = Some("9.9.9".into());
            cx.global_mut::<UpdateState>().restart_path = Some(PathBuf::from("/opt/forge/bin/forge"));
            offer_restart("9.9.9", cx);
        });
        cx.run_until_parked();
        assert!(cx.update(|cx| cx.global::<UpdateState>().unanswered), "no window to ask in yet");

        params.fs.as_fake().insert_tree("/root", serde_json::json!({ "a.txt": "" })).await;
        let project = project::Project::test(params.fs.clone(), ["/root".as_ref()], cx).await;
        let window = cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let cx = &mut gpui::VisualTestContext::from_window(window.into(), cx);
        cx.run_until_parked();
        let (message, _) = cx.pending_prompt().expect("asks as soon as a window opens");
        assert!(message.contains("Forge 9.9.9 is installed"), "{message}");

        cx.simulate_prompt_answer("Later");
        cx.run_until_parked();
        assert!(!cx.has_pending_prompt());
        assert!(!cx.update(|_, cx| cx.global::<UpdateState>().asking));

        let restarted = cx.expect_restart();
        cx.update(|_, cx| check(true, cx));
        cx.run_until_parked();
        cx.simulate_prompt_answer("Restart Now");
        cx.run_until_parked();
        let (path, args) = restarted.await.expect("Forge restarts");
        assert_eq!(path, Some(PathBuf::from("/opt/forge/bin/forge")), "the updated copy starts, not the running one (gone by now)");
        assert!(args.is_empty(), "the new Forge starts plain, restoring the last session");
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
