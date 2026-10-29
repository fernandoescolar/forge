//! What run targets need from a remote project's host (see `forge_host`): discovery, with
//! the same code as for a local project, where the manifests are.

use std::path::PathBuf;

use anyhow::Result;
use forge_host::Log;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::targets::{self, RunTarget};

pub const DISCOVER: &str = "run.discover";

/// The worktrees' files, by root, as the client's worktrees list them.
#[derive(Serialize, Deserialize)]
pub struct DiscoverParams {
    pub listed: Vec<(PathBuf, Vec<PathBuf>)>,
}

/// Answers [`DISCOVER`]: the run targets (`Vec<RunTarget>`).
pub fn discover(params: Value, _: &Log) -> Result<Value> {
    let DiscoverParams { listed } = forge_host::params(params)?;
    let found: Vec<RunTarget> = listed.into_iter().flat_map(|(root, files)| targets::discover_in(&root, files)).collect();
    forge_host::result(&found)
}
