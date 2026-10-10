//! Extension packages: a `.forgeext` file is a zip of a built extension, with what it needs
//! at run time and nothing else: `package.json`, `dist/`, its assets, and
//! its sidecars (programs it starts with `forge.process.spawn({ sidecar })`), one build per
//! platform under `bin/<platform>/` (`darwin-arm64`, `darwin-x64`, `linux-x64`, `win32-x64`…).
//!
//! `package.json` can list what goes in with `forge.files` (paths relative to the folder);
//! by default it is the folders and files in [`DEFAULT_FILES`]. The theme files it declares
//! (`forge.themes`, `forge.iconThemes`) always go in. Declared sidecars
//! (`forge.sidecars: ["name"]`) must be built for at least one platform to pack, and for this
//! machine's to install.
//!
//! The same format is written by `forge-ext pack` (packages/forge-api/bin/forge-ext.mjs).

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use serde_json::Value;

use crate::process::{platform, sidecar_path};

/// File extension of extension packages.
pub const EXTENSION: &str = "forgeext";

/// What a package holds when `package.json` doesn't say (`forge.files`).
pub const DEFAULT_FILES: &[&str] = &["package.json", "dist", "assets", "media", "bin", "themes", "icon_themes", "icons", "README.md", "CHANGELOG.md", "LICENSE", "LICENSE.md", "icon.png"];

/// Never packed, wherever they are.
const SKIPPED: &[&str] = &["node_modules", ".git", ".DS_Store"];

/// A package's manifest fields that packing and installing check.
#[derive(Debug)]
pub struct PackageInfo {
    pub name: String,
    pub version: Option<String>,
    pub sidecars: Vec<String>,
    /// Platforms each sidecar is built for, by sidecar.
    pub platforms: Vec<(String, Vec<String>)>,
}

/// Reads `package.json` in `dir` and finds the platforms its sidecars are built for.
pub fn inspect(dir: &Path) -> Result<PackageInfo> {
    let manifest: Value = serde_json::from_str(&std::fs::read_to_string(dir.join("package.json")).context("no package.json")?).context("package.json is not valid JSON")?;
    let forge = manifest.get("forge").context("package.json has no `forge` section, so it is not a Forge extension")?;
    let name = manifest.get("name").and_then(Value::as_str).context("package.json has no name")?.to_string();
    let sidecars: Vec<String> = forge.get("sidecars").and_then(Value::as_array).into_iter().flatten().filter_map(|s| s.as_str().map(str::to_string)).collect();
    let platforms = sidecars
        .iter()
        .map(|sidecar| {
            let mut found: Vec<String> = std::fs::read_dir(dir.join("bin"))
                .into_iter()
                .flatten()
                .flatten()
                .filter(|e| e.path().join(sidecar).is_file() || e.path().join(format!("{sidecar}.exe")).is_file())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect();
            found.sort();
            (sidecar.clone(), found)
        })
        .collect();
    Ok(PackageInfo { name, version: manifest.get("version").and_then(Value::as_str).map(str::to_string), sidecars, platforms })
}

/// The files of the extension in `dir` that go into its package: `(path in the package,
/// path on disk, unix mode)`, sorted.
pub fn files(dir: &Path) -> Result<Vec<(String, PathBuf, u32)>> {
    let manifest: Value = serde_json::from_str(&std::fs::read_to_string(dir.join("package.json"))?)?;
    let forge = manifest.get("forge").cloned().unwrap_or_default();
    let main = forge.get("main").and_then(Value::as_str);
    let listed = |key: &str| -> Vec<String> { forge.get(key).and_then(Value::as_array).into_iter().flatten().filter_map(|f| f.as_str().map(|f| f.trim_start_matches("./").trim_end_matches('/').to_string())).collect() };
    let themes: Vec<String> = [listed("themes"), listed("iconThemes")].concat();
    // An extension that only brings themes needs no code.
    let main = main.or((themes.is_empty() || dir.join("dist/extension.js").is_file()).then_some("dist/extension.js"));
    if let Some(main) = main {
        anyhow::ensure!(dir.join(main).is_file(), "{main} is missing: build the extension first (`forge-ext build`)");
    }
    let mut roots: Vec<String> = match forge.get("files").and_then(Value::as_array) {
        Some(_) => listed("files"),
        None => DEFAULT_FILES.iter().map(|f| f.to_string()).collect(),
    };
    roots.push("package.json".into());
    roots.extend(main.map(str::to_string));
    roots.extend(themes);
    roots.sort();
    roots.dedup();

    let mut out = Vec::new();
    for root in roots {
        anyhow::ensure!(Path::new(&root).components().all(|c| matches!(c, std::path::Component::Normal(_))), "`{root}` in forge.files must be a path inside the extension");
        let path = dir.join(&root);
        if path.is_file() {
            out.push((root, path.clone(), mode(&path)));
        } else if path.is_dir() {
            walk(&path, &root, &mut out)?;
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out.dedup_by(|a, b| a.0 == b.0);
    Ok(out)
}

fn walk(dir: &Path, prefix: &str, out: &mut Vec<(String, PathBuf, u32)>) -> Result<()> {
    for entry in std::fs::read_dir(dir).with_context(|| format!("cannot read {}", dir.display()))?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if SKIPPED.contains(&name.as_str()) {
            continue;
        }
        let path = entry.path();
        let relative = format!("{prefix}/{name}");
        if path.is_dir() {
            walk(&path, &relative, out)?;
        } else if path.is_file() {
            out.push((relative, path.clone(), mode(&path)));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path).map(|m| m.permissions().mode() & 0o777).unwrap_or(0o644)
}

#[cfg(not(unix))]
fn mode(_: &Path) -> u32 {
    0o644
}

/// Packs the extension in `dir` into `out` (a `.forgeext` file).
pub async fn pack(dir: &Path, out: &Path) -> Result<PackageInfo> {
    use async_zip::{Compression, ZipEntryBuilder, base::write::ZipFileWriter};
    let info = inspect(dir)?;
    for (sidecar, platforms) in &info.platforms {
        anyhow::ensure!(!platforms.is_empty(), "sidecar `{sidecar}` is not built: put it in bin/<platform>/{sidecar} (e.g. bin/{}/{sidecar})", platform());
    }
    let mut writer = ZipFileWriter::new(futures::io::Cursor::new(Vec::new()));
    for (name, path, mode) in files(dir)? {
        let data = std::fs::read(&path).with_context(|| format!("cannot read {}", path.display()))?;
        // Sidecars must stay executable whatever the file system they were built on says.
        let mode = if name.starts_with("bin/") && info.sidecars.iter().any(|s| name.ends_with(&format!("/{s}")) || name.ends_with(&format!("/{s}.exe"))) { 0o755 } else { mode };
        let entry = ZipEntryBuilder::new(name.clone().into(), Compression::Deflate).unix_permissions(mode as u16);
        writer.write_entry_whole(entry, &data).await.with_context(|| format!("cannot add {name}"))?;
    }
    let bytes = writer.close().await.context("cannot finish the package")?.into_inner();
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(out, bytes).with_context(|| format!("cannot write {}", out.display()))?;
    Ok(info)
}

/// An unpacked package, waiting to be put in place; dropping it removes the files.
pub struct Unpacked {
    /// The folder the package was unpacked into.
    staging: PathBuf,
    /// The extension's folder (the staging folder, or the one folder inside it).
    pub root: PathBuf,
    pub info: PackageInfo,
}

impl Drop for Unpacked {
    fn drop(&mut self) {
        if self.staging.exists() {
            std::fs::remove_dir_all(&self.staging).ok();
        }
    }
}

/// Unpacks `package` into a new folder inside `parent`, checked: it is an extension, and
/// its sidecars run on this machine. A zip of the extension's folder (one folder at the
/// top) works too.
pub async fn unpack(package: &Path, parent: &Path) -> Result<Unpacked> {
    std::fs::create_dir_all(parent).with_context(|| format!("cannot create {}", parent.display()))?;
    let staging = tempfile::Builder::new().prefix(".installing-").tempdir_in(parent).context("cannot create a folder to unpack into")?.keep();
    let mut unpacked = Unpacked { root: staging.clone(), staging, info: PackageInfo { name: String::new(), version: None, sidecars: vec![], platforms: vec![] } };
    let file = smol::fs::File::open(package).await.with_context(|| format!("cannot open {}", package.display()))?;
    util::archive::extract_zip(&unpacked.staging, file).await.context("this file is not a valid extension package (zip)")?;

    if !unpacked.root.join("package.json").is_file() {
        let folders: Vec<PathBuf> = std::fs::read_dir(&unpacked.root)?.flatten().map(|e| e.path()).filter(|p| p.is_dir() && p.file_name().is_some_and(|n| n != "__MACOSX")).collect();
        match folders.as_slice() {
            [only] if only.join("package.json").is_file() => unpacked.root = only.clone(),
            _ => bail!("the package has no package.json"),
        }
    }
    let info = inspect(&unpacked.root)?;
    for sidecar in &info.sidecars {
        if !sidecar_path(&unpacked.root, sidecar).is_file() {
            let built: Vec<String> = info.platforms.iter().find(|(s, _)| s == sidecar).map(|(_, p)| p.clone()).unwrap_or_default();
            let built = if built.is_empty() { "no platform".to_string() } else { built.join(", ") };
            bail!("{} needs its program `{sidecar}`, which this package has for {built} but not for this computer ({})", info.name, platform());
        }
    }
    unpacked.info = info;
    Ok(unpacked)
}

/// Moves an unpacked extension into `extensions_dir/<name>`, replacing an installed copy.
pub fn place(unpacked: Unpacked, extensions_dir: &Path) -> Result<PathBuf> {
    let folder = unpacked.info.name.trim_start_matches('@').replace(['/', '\\'], "-");
    let destination = extensions_dir.join(folder);
    if destination.exists() {
        std::fs::remove_dir_all(&destination).with_context(|| format!("cannot replace {}", destination.display()))?;
    }
    std::fs::rename(&unpacked.root, &destination).with_context(|| format!("cannot move the extension to {}", destination.display()))?;
    Ok(destination)
}

/// The package file name for an extension: `<name>-<version>.forgeext`.
pub fn file_name(info: &PackageInfo) -> String {
    let name = info.name.trim_start_matches('@').replace('/', "-");
    match &info.version {
        Some(v) => format!("{name}-{v}.{EXTENSION}"),
        None => format!("{name}.{EXTENSION}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extension(dir: &Path) {
        std::fs::create_dir_all(dir.join("dist")).unwrap();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("node_modules/react")).unwrap();
        std::fs::create_dir_all(dir.join(format!("bin/{}", platform()))).unwrap();
        std::fs::create_dir_all(dir.join("bin/linux-x64")).unwrap();
        std::fs::write(dir.join("package.json"), r#"{"name":"db","version":"1.2.0","forge":{"sidecars":["tool"]}}"#).unwrap();
        std::fs::write(dir.join("dist/extension.js"), "var __forgeExtension = {};").unwrap();
        std::fs::write(dir.join("src/extension.tsx"), "source").unwrap();
        std::fs::write(dir.join("node_modules/react/index.js"), "").unwrap();
        std::fs::write(dir.join(format!("bin/{}/tool", platform())), "#!/bin/sh\necho hi\n").unwrap();
        std::fs::write(dir.join("bin/linux-x64/tool"), "elf").unwrap();
    }

    #[test]
    fn packs_what_runs_and_nothing_else() {
        let tmp = tempfile::tempdir().unwrap();
        extension(tmp.path());
        let names: Vec<String> = files(tmp.path()).unwrap().into_iter().map(|(n, ..)| n).collect();
        let mut expected = vec![format!("bin/{}/tool", platform()), "bin/linux-x64/tool".into(), "dist/extension.js".into(), "package.json".into()];
        expected.sort();
        assert_eq!(names, expected, "no sources or dependencies");
        let info = inspect(tmp.path()).unwrap();
        assert_eq!(info.platforms[0].1.len(), 2);
        assert_eq!(file_name(&info), "db-1.2.0.forgeext");
    }

    #[test]
    fn packs_an_extension_that_only_brings_themes() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("colours")).unwrap();
        std::fs::create_dir_all(tmp.path().join("icons")).unwrap();
        std::fs::write(tmp.path().join("package.json"), r#"{"name":"ocean","forge":{"themes":["colours/ocean.json"]}}"#).unwrap();
        std::fs::write(tmp.path().join("colours/ocean.json"), "{}").unwrap();
        std::fs::write(tmp.path().join("icons/rust.svg"), "<svg/>").unwrap();
        let names: Vec<String> = files(tmp.path()).unwrap().into_iter().map(|(n, ..)| n).collect();
        assert_eq!(names, ["colours/ocean.json", "icons/rust.svg", "package.json"], "declared themes go in wherever they are");
    }

    #[test]
    fn round_trips_a_package_with_its_sidecars() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        extension(&source);
        let package = tmp.path().join("db.forgeext");
        smol::block_on(pack(&source, &package)).unwrap();

        let extensions = tmp.path().join("extensions");
        let unpacked = smol::block_on(unpack(&package, &extensions)).unwrap();
        let installed = place(unpacked, &extensions).unwrap();
        assert_eq!(installed, extensions.join("db"));
        assert!(installed.join("dist/extension.js").is_file());
        assert!(!installed.join("src").exists() && !installed.join("node_modules").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(sidecar_path(&installed, "tool")).unwrap().permissions().mode();
            assert_eq!(mode & 0o111, 0o111, "sidecars stay executable");
        }
        let leftovers: Vec<_> = std::fs::read_dir(&extensions).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(leftovers, ["db"], "no staging folder is left behind");
    }

    /// A package written by `forge-ext pack` (Node) installs like one Forge exported.
    #[test]
    fn installs_packages_made_by_forge_ext() {
        let Ok(package) = std::env::var("FORGE_TEST_PACKAGE") else { return };
        let tmp = tempfile::tempdir().unwrap();
        let unpacked = smol::block_on(unpack(Path::new(&package), tmp.path())).unwrap();
        let installed = place(unpacked, tmp.path()).unwrap();
        assert!(installed.join("dist/extension.js").is_file());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let sidecar = std::fs::read_dir(installed.join("bin").join(platform())).unwrap().next().unwrap().unwrap().path();
            assert_eq!(std::fs::metadata(sidecar).unwrap().permissions().mode() & 0o111, 0o111);
        }
    }

    #[test]
    fn refuses_packages_without_this_machines_sidecar() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        extension(&source);
        std::fs::remove_file(sidecar_path(&source, "tool")).unwrap();
        let package = tmp.path().join("db.forgeext");
        smol::block_on(pack(&source, &package)).unwrap();
        let extensions = tmp.path().join("extensions");
        let error = smol::block_on(unpack(&package, &extensions)).err().expect("refused");
        assert!(format!("{error:#}").contains("linux-x64"), "{error:#}");
        assert_eq!(std::fs::read_dir(&extensions).unwrap().count(), 0, "the unpacked files are removed");

        std::fs::remove_dir_all(source.join("bin")).unwrap();
        assert!(smol::block_on(pack(&source, &package)).is_err(), "a declared sidecar must be built to pack");
    }
}
