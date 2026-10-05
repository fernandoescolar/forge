//! forge-sql: JSON-lines database sidecar for the Forge DB Explorer extension.

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    if let Err(e) = forge_sql::server::run(tokio::io::stdin(), tokio::io::stdout()).await {
        eprintln!("forge-sql: fatal: {e:#}");
        std::process::exit(1);
    }
}
