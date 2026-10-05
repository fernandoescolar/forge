//! Installing and exporting extensions: Install from Folder copies a built extension into
//! Forge's extensions folder, Install from Package unpacks a `.forgeext` file there (see
//! [`crate::package`]), and Export packs an extension into one. Installed extensions load
//! right away.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use gpui::{App, PathPromptOptions, actions};
use workspace::{Toast, Workspace, notifications::NotificationId};

use crate::host::{ExtensionHost, read_manifest};

actions!(forge_extensions, [
    /// Installs a built extension from a folder.
    InstallFromFolder,
    /// Installs an extension from a package (`.forgeext`).
    InstallFromPackage
]);

/// Folders that are never copied: dependencies and VCS data are not needed at runtime.
const SKIPPED: &[&str] = &["node_modules", ".git"];

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|_, _: &InstallFromFolder, _, cx| install_from_folder(cx));
        workspace.register_action(|_, _: &InstallFromPackage, _, cx| install_from_package(cx));
    })
    .detach();
}

/// Asks for a folder, installs the extension in it and tells the user how it went.
pub fn install_from_folder(cx: &mut App) {
    let paths = cx.prompt_for_paths(PathPromptOptions { files: false, directories: true, multiple: false, prompt: Some("Install".into()) });
    cx.spawn(async move |cx| {
        let Ok(Ok(Some(paths))) = paths.await else { return };
        let Some(source) = paths.into_iter().next() else { return };
        let target = paths::data_dir().join("extensions");
        let message = cx.update(|cx| match install(&source, &target, cx) {
            Ok(Installed::Loaded(name)) => format!("Installed {name}."),
            Ok(Installed::Replaced(name)) => format!("Updated {name} and reloaded it."),
            Err(e) => format!("Could not install the extension: {e:#}"),
        });
        cx.update(|cx| show(message, cx));
    })
    .detach();
}

/// Asks for a `.forgeext` file and installs the extension in it.
pub fn install_from_package(cx: &mut App) {
    let paths = cx.prompt_for_paths(PathPromptOptions { files: true, directories: false, multiple: false, prompt: Some("Install".into()) });
    cx.spawn(async move |cx| {
        let Ok(Ok(Some(paths))) = paths.await else { return };
        let Some(package) = paths.into_iter().next() else { return };
        let target = paths::data_dir().join("extensions");
        let message = match install_package(&package, &target, cx).await {
            Ok(Installed::Loaded(name)) => format!("Installed {name}."),
            Ok(Installed::Replaced(name)) => format!("Updated {name} and reloaded it."),
            Err(e) => format!("Could not install the extension: {e:#}"),
        };
        cx.update(|cx| show(message, cx));
    })
    .detach();
}

/// Installs the extension packed in `package` into `extensions_dir` and loads it (reloads
/// it, when one with its id is running).
pub async fn install_package(package: &Path, extensions_dir: &Path, cx: &mut gpui::AsyncApp) -> Result<Installed> {
    let unpacked = crate::package::unpack(package, extensions_dir).await?;
    read_manifest(&unpacked.root)?.context("the package's extension is not built (no dist/extension.js)")?;
    let host = cx.update(|cx| ExtensionHost::global(cx)).context("the extension host is not running")?;
    let destination = cx.update(|cx| {
        // Stop the running copy before its folder is replaced.
        let id = unpacked.info.name.clone();
        let was_loaded = host.update(cx, |host, cx| host.unload(&id, cx).is_some());
        (was_loaded, crate::package::place(unpacked, extensions_dir))
    });
    let (replaced, destination) = (destination.0, destination.1?);
    let installed = read_manifest(&destination)?.context("the installed extension has no manifest")?;
    let name = installed.info.name.clone();
    cx.update(|cx| host.update(cx, |host, cx| host.load(installed, cx)));
    Ok(if replaced { Installed::Replaced(name) } else { Installed::Loaded(name) })
}

/// Asks where to save, then packs the loaded extension `id` into a `.forgeext` file.
pub fn export(id: &str, cx: &mut App) {
    let Some(host) = ExtensionHost::global(cx) else { return };
    let Some(extension) = host.read(cx).extensions.iter().find(|e| e.id == id).cloned() else { return };
    let info = match crate::package::inspect(&extension.path) {
        Ok(info) => info,
        Err(e) => return show(format!("Could not export {}: {e:#}", extension.name), cx),
    };
    let directory = paths::home_dir().join("Downloads");
    let path = cx.prompt_for_new_path(&directory, Some(&crate::package::file_name(&info)));
    cx.spawn(async move |cx| {
        let Ok(Ok(Some(path))) = path.await else { return };
        let message = match crate::package::pack(&extension.path, &path).await {
            Ok(info) => {
                let platforms: Vec<String> = info.platforms.iter().map(|(sidecar, platforms)| format!("{sidecar} for {}", platforms.join(", "))).collect();
                let sidecars = if platforms.is_empty() { String::new() } else { format!(" (with {})", platforms.join("; ")) };
                format!("Exported {} to {}{sidecars}.", extension.name, path.display())
            }
            Err(e) => format!("Could not export {}: {e:#}", extension.name),
        };
        cx.update(|cx| show(message, cx));
    })
    .detach();
}

fn show(message: String, cx: &mut App) {
    let workspace = ExtensionHost::global(cx).and_then(|host| host.read(cx).workspace());
    match workspace {
        Some(workspace) => workspace.update(cx, |ws, cx| ws.show_toast(Toast::new(NotificationId::named("forge-extension-install".into()), message), cx)),
        None => log::info!("{message}"),
    }
}

pub enum Installed {
    /// New, and running.
    Loaded(String),
    /// Replaced an installed copy, and reloaded it.
    Replaced(String),
}

/// Copies the extension in `source` into `extensions_dir` and loads it (reloads it, when
/// one with its id is running).
pub fn install(source: &Path, extensions_dir: &Path, cx: &mut App) -> Result<Installed> {
    let discovered = read_manifest(source)?.context("this folder's package.json has no `forge` section, so it is not a Forge extension")?;
    let name = discovered.info.name.clone();
    let folder = source.file_name().context("not a folder")?;
    let destination = extensions_dir.join(folder);
    if destination != source {
        if destination.exists() {
            std::fs::remove_dir_all(&destination).with_context(|| format!("cannot replace {}", destination.display()))?;
        }
        copy_dir(source, &destination)?;
    }
    let host = ExtensionHost::global(cx).context("the extension host is not running")?;
    let installed = read_manifest(&destination)?.context("the copied extension has no manifest")?;
    let id = installed.info.id.clone();
    let replaced = host.update(cx, |host, cx| {
        let replaced = host.unload(&id, cx).is_some();
        host.load(installed, cx);
        replaced
    });
    Ok(if replaced { Installed::Replaced(name) } else { Installed::Loaded(name) })
}

fn copy_dir(source: &Path, destination: &Path) -> Result<()> {
    std::fs::create_dir_all(destination).with_context(|| format!("cannot create {}", destination.display()))?;
    for entry in std::fs::read_dir(source)?.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let target: PathBuf = destination.join(&name);
        if path.is_dir() {
            if !SKIPPED.iter().any(|s| name == *s) {
                copy_dir(&path, &target)?;
            }
        } else {
            std::fs::copy(&path, &target).with_context(|| format!("cannot copy {}", path.display()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    async fn installs_and_loads_a_built_extension(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        cx.update(|cx| {
            let store = settings::SettingsStore::test(cx);
            cx.set_global(store);
        });
        let tmp = tempfile::tempdir().unwrap();
        let (installed, source) = (tmp.path().join("installed"), tmp.path().join("src/hello"));
        std::fs::create_dir_all(source.join("dist")).unwrap();
        std::fs::create_dir_all(source.join("node_modules/react")).unwrap();
        std::fs::write(source.join("package.json"), r#"{"name":"hello","displayName":"Hello","forge":{}}"#).unwrap();
        std::fs::write(source.join("dist/extension.js"), "var __forgeExtension = {};").unwrap();
        std::fs::write(source.join("node_modules/react/index.js"), "").unwrap();
        let host = cx.update(|cx| ExtensionHost::init(vec![installed.clone()], cx)).unwrap();

        let result = cx.update(|cx| install(&source, &installed, cx)).unwrap();
        assert!(matches!(result, Installed::Loaded(ref name) if name == "Hello"));
        assert!(installed.join("hello/dist/extension.js").is_file());
        assert!(!installed.join("hello/node_modules").exists(), "dependencies are bundled, not copied");
        assert_eq!(host.read_with(cx, |h, _| h.extensions.iter().map(|e| e.id.clone()).collect::<Vec<_>>()), ["hello"]);

        let again = cx.update(|cx| install(&source, &installed, cx)).unwrap();
        assert!(matches!(again, Installed::Replaced(_)), "installing again replaces and reloads it");
        assert_eq!(host.read_with(cx, |h, _| h.extensions.len()), 1);

        // Reloading picks up a rebuilt bundle and drops what the old one registered;
        // uninstalling takes it all away.
        let commands = |cx: &mut gpui::TestAppContext| host.read_with(cx, |h, _| h.commands.iter().map(|c| c.id.clone()).collect::<Vec<_>>());
        let wait_for = |want: &[&str], cx: &mut gpui::TestAppContext| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while commands(cx) != want {
                assert!(std::time::Instant::now() < deadline, "commands {:?}, wanted {want:?}", commands(cx));
                cx.run_until_parked();
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        };
        let bundle = |version: &str| format!("var __forgeExtension = {{ activate() {{ __forge.modules['@forge/api'].forge.commands.register('hello.{version}', '{version}', () => {{}}); }} }};");
        std::fs::write(installed.join("hello/dist/extension.js"), bundle("v1")).unwrap();
        cx.update(|cx| host.update(cx, |h, cx| h.reload("hello", cx))).unwrap();
        wait_for(&["hello.v1"], cx);
        std::fs::write(installed.join("hello/dist/extension.js"), bundle("v2")).unwrap();
        cx.update(|cx| host.update(cx, |h, cx| h.reload("hello", cx))).unwrap();
        wait_for(&["hello.v2"], cx);
        // Only extensions in Forge's own folder can be uninstalled; this test's isn't it.
        assert!(cx.update(|cx| host.update(cx, |h, cx| h.uninstall("hello", cx))).is_err());
        cx.update(|cx| host.update(cx, |h, cx| h.unload("hello", cx)));
        wait_for(&[], cx);

        let plain = tmp.path().join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        std::fs::write(plain.join("package.json"), r#"{"name":"plain"}"#).unwrap();
        assert!(cx.update(|cx| install(&plain, &installed, cx)).is_err());
    }
}
