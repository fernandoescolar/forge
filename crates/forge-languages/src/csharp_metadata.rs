//! Go-to-definition into compiled assemblies for C#.
//!
//! For a symbol that lives in a referenced assembly (the BCL, NuGet packages), OmniSharp
//! answers with a virtual location such as
//! `file:///$metadata$/Project/CacheService/Tests/Assembly/xunit/core/Symbol/Xunit/FactAttribute.cs`.
//! Nothing exists at that path, so Zed opens an empty buffer. Here we notice those buffers,
//! ask OmniSharp for the generated source (`o#/metadata`), and show it read-only.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use editor::Editor;
use gpui::{App, Entity};
use language::{Buffer, Capability};
use lsp::LanguageServer;
use project::Project;
use serde::{Deserialize, Serialize};

const METADATA_ROOT: &str = "/$metadata$/";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const SERVER_START_TIMEOUT: Duration = Duration::from_secs(120);
const SERVER_POLL_INTERVAL: Duration = Duration::from_millis(500);

pub fn is_metadata_path(path: &str) -> bool {
    path.contains("$metadata$/") || path.contains("%24metadata%24/")
}

pub fn init(cx: &mut App) {
    cx.observe_new(|editor: &mut Editor, _, cx| {
        let Some(project) = editor.project().cloned() else {
            return;
        };
        let Some(buffer) = editor.buffer().read(cx).as_singleton() else {
            return;
        };
        let Some(request) = metadata_request(&buffer, cx) else {
            return;
        };
        load_metadata_source(project, buffer, request, cx);
    })
    .detach();
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct MetadataParams {
    project_name: String,
    assembly_name: String,
    type_name: String,
    language: String,
    timeout: u64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct MetadataResponse {
    source: Option<String>,
}

enum MetadataRequest {}

impl lsp::request::Request for MetadataRequest {
    type Params = MetadataParams;
    type Result = Option<MetadataResponse>;
    const METHOD: &'static str = "o#/metadata";
}

fn metadata_request(buffer: &Entity<Buffer>, cx: &App) -> Option<MetadataParams> {
    let file = buffer.read(cx).file()?.as_local()?;
    let path = file.abs_path(cx);
    parse_metadata_path(&path)
}

/// `/$metadata$/Project/<project>/Assembly/<assembly>/Symbol/<type>.cs`, where OmniSharp
/// turned the dots of each name into path separators.
fn parse_metadata_path(path: &Path) -> Option<MetadataParams> {
    let rest = path.to_str()?.strip_prefix(METADATA_ROOT)?;
    let rest = rest.strip_prefix("Project/")?;
    let (project, rest) = rest.split_once("/Assembly/")?;
    let (assembly, symbol) = rest.split_once("/Symbol/")?;
    let symbol = symbol.strip_suffix(".cs")?;
    let dotted = |s: &str| s.replace('/', ".");
    Some(MetadataParams {
        project_name: dotted(project),
        assembly_name: dotted(assembly),
        type_name: dotted(symbol),
        language: "C#".into(),
        timeout: REQUEST_TIMEOUT.as_millis() as u64,
    })
}

fn load_metadata_source(
    project: Entity<Project>,
    buffer: Entity<Buffer>,
    params: MetadataParams,
    cx: &mut App,
) {
    buffer.update(cx, |buffer, cx| buffer.set_capability(Capability::ReadOnly, cx));
    cx.spawn(async move |cx| {
        // A restored editor can come back before OmniSharp has started.
        let mut waited = Duration::ZERO;
        let servers = loop {
            let servers = cx.update(|cx| omnisharp_servers(&project, cx));
            if !servers.is_empty() {
                break servers;
            }
            if waited >= SERVER_START_TIMEOUT {
                log::warn!("no OmniSharp running to load {}", params.type_name);
                return;
            }
            cx.background_executor().timer(SERVER_POLL_INTERVAL).await;
            waited += SERVER_POLL_INTERVAL;
        };

        for server in servers {
            let response = server
                .request::<MetadataRequest>(params.clone(), REQUEST_TIMEOUT)
                .await
                .into_response();
            match response {
                Ok(Some(MetadataResponse { source: Some(source) })) => {
                    log::info!("loaded generated source for {} from OmniSharp", params.type_name);
                    buffer.update(cx, |buffer, cx| {
                        buffer.set_capability(Capability::ReadWrite, cx);
                        buffer.set_text(source, cx);
                        buffer.set_capability(Capability::ReadOnly, cx);
                    });
                    return;
                }
                Ok(_) => {}
                Err(err) => log::warn!("o#/metadata for {} failed: {err:#}", params.type_name),
            }
        }
        log::warn!("OmniSharp had no source for {}", params.type_name);
    })
    .detach();
}

pub(crate) fn omnisharp_servers(project: &Entity<Project>, cx: &App) -> Vec<Arc<LanguageServer>> {
    let project = project.read(cx);
    let lsp_store = project.lsp_store().read(cx);
    project
        .language_server_statuses(cx)
        .filter(|(_, status)| status.name == crate::omnisharp::SERVER_NAME)
        .filter_map(|(id, _)| lsp_store.language_server_for_id(id))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_omnisharp_metadata_paths() {
        let params = parse_metadata_path(Path::new(
            "/$metadata$/Project/CacheService/Tests/Assembly/xunit/core/Symbol/Xunit/FactAttribute.cs",
        ))
        .unwrap();
        assert_eq!(params.project_name, "CacheService.Tests");
        assert_eq!(params.assembly_name, "xunit.core");
        assert_eq!(params.type_name, "Xunit.FactAttribute");

        assert!(parse_metadata_path(Path::new("/Users/me/src/Program.cs")).is_none());
        assert!(is_metadata_path("file:///%24metadata%24/Project/A/Assembly/B/Symbol/C.cs"));
    }
}
