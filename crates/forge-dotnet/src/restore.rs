//! `dotnet` commands Forge runs on its own, without a terminal: restoring after package
//! changes, and reading `dotnet new` output.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use util::command::{Stdio, new_command};

/// Runs `dotnet <args>` and returns what it printed; fails with that output on errors.
pub async fn dotnet_output(args: Vec<String>, cwd: Option<PathBuf>) -> Result<String> {
    let mut command = new_command("dotnet");
    command.args(&args).stdin(Stdio::null()).kill_on_drop(true);
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let output = forge_ui::process::spawn_unblocked(command)
        .context("failed to start `dotnet`; is the .NET SDK installed?")?
        .output()
        .await
        .context("`dotnet` failed")?;
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    if !output.status.success() {
        bail!("`dotnet {}` failed:\n{}", args.join(" "), text.trim());
    }
    Ok(text)
}

/// Restores a project so `project.assets.json` (resolved versions, dependencies) is
/// current. Failures are only logged: the project file change already happened.
pub async fn restore(project: &Path) {
    let args = vec!["restore".to_string(), project.to_string_lossy().into_owned(), "--nologo".to_string()];
    if let Err(error) = dotnet_output(args, project.parent().map(Path::to_path_buf)).await {
        log::warn!("{error:#}");
    }
}
