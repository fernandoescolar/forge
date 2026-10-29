//! The server Forge starts on a remote host, over SSH, to open folders there: Zed's remote
//! server (`remote_server`) with Forge's languages, language servers and debug adapters
//! registered next to Zed's. Forge uploads it to `~/.forge_server/` on first connection.
//! The command line is Zed's, which the client drives (`run`, `proxy`, `version`…), plus
//! `forge-host <method>` for Forge's own work on the host (`forge_host`).

use std::io::Write as _;
use std::path::PathBuf;

use clap::Parser;

#[derive(Parser)]
#[command(disable_version_flag = true)]
struct Cli {
    #[command(subcommand)]
    command: Option<remote_server::Commands>,
    /// SSH and Git password prompts: the server acts as netcat over a Unix socket.
    #[arg(long, hide = true)]
    askpass: Option<String>,
    /// Records minidumps of crashes from a separate process listening on this socket.
    #[arg(long, hide = true)]
    crash_handler: Option<PathBuf>,
    /// Prints the environment, for loading the login shell's one.
    #[arg(long, hide = true)]
    printenv: bool,
}

/// Forge's own work that runs where a remote project's files are (`forge_host`): each
/// feature's discovery, runs and loads, by method.
const HOST_HANDLERS: &[(&str, forge_host::Handler)] = &[
    (forge_tests::host::DISCOVER, forge_tests::host::discover),
    (forge_tests::host::RUN, forge_tests::host::run),
    (forge_run::host::DISCOVER, forge_run::host::discover),
    (dotnet_model::load::LOAD, load_dotnet),
];

/// The Solution Explorer's load (`dotnet_model::load`).
fn load_dotnet(params: serde_json::Value, _: &forge_host::Log) -> anyhow::Result<serde_json::Value> {
    let params: dotnet_model::load::LoadParams = forge_host::params(params)?;
    forge_host::result(&params.load())
}

fn main() -> anyhow::Result<()> {
    // Before anything reads a path: they all derive from the data folder.
    paths::set_custom_data_dir(&forge_remote_server::data_dir().to_string_lossy());
    forge_remote_server::use_forge_names();

    // `forge-host <method>`: one call from Forge, started through the server's `exec`.
    let mut args = std::env::args().skip(1);
    if args.next().as_deref() == Some(forge_host::SUBCOMMAND) {
        let method = args.next().unwrap_or_default();
        forge_host::serve(&method, HOST_HANDLERS);
    }

    let cli = Cli::parse();

    if let Some(socket_path) = &cli.askpass {
        askpass::main(socket_path);
        return Ok(());
    }
    if let Some(socket) = &cli.crash_handler {
        crashes::crash_server(socket.as_path(), paths::logs_dir().clone());
        return Ok(());
    }
    if cli.printenv {
        util::shell_env::print_env();
        return Ok(());
    }

    let Some(command) = cli.command else {
        std::io::stderr().write_all(b"usage: forge-remote-server <run|proxy|version|exec>\n").ok();
        std::process::exit(1);
    };

    if remote_server::set_package_version(env!("CARGO_PKG_VERSION")).is_err() {
        anyhow::bail!("the server's version was already set");
    }
    if remote_server::set_languages_init(forge_languages::register_languages).is_err() {
        anyhow::bail!("the server's languages were already set");
    }

    let result = remote_server::run(command);
    // The client reads the proxy's exit code to tell whether the server died and has to be
    // started again, so report it instead of the generic 1.
    if let Err(error) = &result
        && let Some(error) = error.downcast_ref::<remote_server::ExecuteProxyError>()
    {
        std::io::stderr().write_fmt(format_args!("{error:#}\n")).ok();
        std::process::exit(error.to_exit_code());
    }
    result
}
