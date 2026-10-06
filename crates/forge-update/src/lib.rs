//! Forge updates itself from the GitHub releases of its repository: a release tagged
//! `v<version>` with a `Forge-<version>-<arch>.zip` asset (what `scripts/bundle-macos.sh`
//! makes). The repository is Forge's own (`fernandoescolar/forge`), or the one set with
//! `FORGE_UPDATE_REPOSITORY` when Forge is built (for forks). Release builds check on their
//! own; debug builds only when asked (Forge › Check for Updates…).
//!
//! An update is downloaded, unpacked and checked (it must be a signed Forge.app) next to
//! the running app, then swapped in; it runs from the next start. Forge then asks, in a
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
}

/// `1.10.0` > `1.9.3`; a missing part counts as 0; a pre-release suffix (`-beta.1`) sorts
/// before the release.
pub fn compare_versions(a: &str, b: &str) -> Ordering {
    let parse = |v: &str| {
        let v = v.trim().trim_start_matches('v');
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
        (Some(a), Some(b)) => a.cmp(&b),
    })
}

/// The newer release in GitHub's "latest release" JSON, with its asset for `arch`.
pub fn newer_release(json: &str, current: &str, arch: &str) -> Result<Option<Release>> {
    let release: Value = serde_json::from_str(json).context("unexpected answer from GitHub")?;
    let tag = release.get("tag_name").and_then(Value::as_str).context("the release has no tag")?;
    let version = tag.trim_start_matches('v').to_string();
    if compare_versions(&version, current) != Ordering::Greater {
        return Ok(None);
    }
    let wanted = format!("Forge-{version}-{arch}.zip");
    let download = release
        .get("assets")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|asset| asset.get("name").and_then(Value::as_str) == Some(wanted.as_str()))
        .and_then(|asset| asset.get("browser_download_url").and_then(Value::as_str))
        .with_context(|| format!("release {tag} has no {wanted}"))?;
    Ok(Some(Release { version, download: download.to_string(), notes_url: release.get("html_url").and_then(Value::as_str).unwrap_or_default().to_string() }))
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
    newer_release(&String::from_utf8_lossy(&json), current, std::env::consts::ARCH)
}

fn run(program: &str, args: &[&std::ffi::OsStr]) -> Result<()> {
    let output = std::process::Command::new(program).args(args).output().with_context(|| format!("cannot run {program}"))?;
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

/// The running app bundle (`…/Forge.app`), if Forge runs from one.
fn current_app() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let app = exe.parent()?.parent()?.parent()?;
    (app.extension().is_some_and(|e| e == "app")).then(|| app.to_path_buf())
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
            let app = current_app().context("Forge isn't running from an app bundle")?;
            let bytes = get(&client, &release.download).await?;
            let version = release.version.clone();
            cx.background_spawn(async move {
                let zip = tempfile::Builder::new().suffix(".zip").tempfile()?;
                std::fs::write(zip.path(), &bytes)?;
                install(zip.path(), &app)
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
        assert_eq!(compare_versions("0.1.0", "0.2.0"), Ordering::Less);
    }

    #[test]
    fn picks_the_asset_for_this_machine() {
        let json = r#"{"tag_name": "v0.3.0", "html_url": "https://github.com/o/forge/releases/tag/v0.3.0", "assets": [
            {"name": "Forge-0.3.0-x86_64.zip", "browser_download_url": "https://x/intel.zip"},
            {"name": "Forge-0.3.0-aarch64.zip", "browser_download_url": "https://x/arm.zip"}]}"#;
        let release = newer_release(json, "0.2.0", "aarch64").unwrap().unwrap();
        assert_eq!((release.version.as_str(), release.download.as_str()), ("0.3.0", "https://x/arm.zip"));
        assert_eq!(newer_release(json, "0.3.0", "aarch64").unwrap(), None, "already up to date");
        assert!(newer_release(json, "0.2.0", "riscv64").unwrap_err().to_string().contains("no Forge-0.3.0-riscv64.zip"));
    }

    /// A tiny signed app bundle, zipped like a release.
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
        assert!(path.is_none() && args.is_empty(), "the new Forge starts plain, restoring the last session");
    }

    #[test]
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
