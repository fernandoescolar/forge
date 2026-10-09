//! Builds packages/forge-api/dist/runtime.js (embedded into the binary) with Node + esbuild.

use std::{path::Path, process::Command};

fn main() {
    let pkg = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/forge-api");
    for p in ["src", "build.mjs", "bin", "package.json", "test/fixture/src"] {
        println!("cargo:rerun-if-changed={}", pkg.join(p).display());
    }
    let run = |args: &[&str]| {
        let status = Command::new(args[0]).args(&args[1..]).current_dir(&pkg).status();
        match status {
            Ok(s) if s.success() => {}
            Ok(s) => panic!("`{}` failed with {s} in {}", args.join(" "), pkg.display()),
            Err(e) => panic!("cannot run `{}` ({e}); Node.js is required to build Forge's extension runtime", args[0]),
        }
    };
    // esbuild, not just the folder: an empty `node_modules` (a fresh volume, an aborted
    // install) needs installing too.
    if !pkg.join("node_modules/esbuild").exists() {
        run(&["npm", "install", "--no-audit", "--no-fund"]);
    }
    run(&["node", "build.mjs"]);
    // Test fixture used by the QuickJS integration tests.
    run(&["node", "bin/forge-ext.mjs", "build", "test/fixture"]);
}
