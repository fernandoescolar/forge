//! What the Tests panel runs on a remote project's host (see `forge_host`): discovery and
//! runs, with the same code as for a local project, where the files and the tools are.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use forge_host::Log;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::discovery::{self, TestProject};
use crate::runner::{self, Env, LiveLog, RunOptions, Scope};

pub const DISCOVER: &str = "tests.discover";
pub const RUN: &str = "tests.run";

/// The worktrees' files, by root, as the client's worktrees list them.
#[derive(Serialize, Deserialize)]
pub struct DiscoverParams {
    pub listed: Vec<(PathBuf, Vec<PathBuf>)>,
}

/// Answers [`DISCOVER`]: the test projects (`Vec<TestProject>`).
pub fn discover(params: Value, _: &Log) -> Result<Value> {
    let DiscoverParams { listed } = forge_host::params(params)?;
    let projects: Vec<TestProject> = listed.into_iter().flat_map(|(root, files)| discovery::discover_in(&root, files)).collect();
    forge_host::result(&projects)
}

#[derive(Serialize, Deserialize)]
pub struct RunParams {
    pub project: TestProject,
    pub scope: Scope,
    pub env: Env,
    pub options: RunOptions,
}

/// Answers [`RUN`]: runs the tests and sends their output as it comes; the result is the
/// `RunOutput`.
pub fn run(params: Value, log: &Log) -> Result<Value> {
    let RunParams { project, scope, env, options } = forge_host::params(params)?;
    let live = LiveLog::default();
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    // The runner appends to `live`; what it added goes back every so often.
    let forward = std::thread::spawn({
        let (live, log, done) = (live.clone(), log.clone(), done.clone());
        move || {
            let mut sent = 0;
            loop {
                let finished = done.load(std::sync::atomic::Ordering::Acquire);
                let text = live.lock().map(|text| text.get(sent..).unwrap_or_default().to_string()).unwrap_or_default();
                if !text.is_empty() {
                    sent += text.len();
                    log.write(&text);
                }
                if finished {
                    break;
                }
                std::thread::sleep(Duration::from_millis(150));
            }
        }
    });
    let output = smol::block_on(runner::run(&project, &scope, live, &env, &options));
    done.store(true, std::sync::atomic::Ordering::Release);
    forward.join().ok();
    forge_host::result(&output?)
}
