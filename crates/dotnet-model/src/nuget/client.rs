//! The NuGet V3 protocol over any HTTP client: service index discovery, search and the
//! version lists of the flat container, across several sources, cached for a while.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow};
use futures::future::{BoxFuture, join_all};
use serde::Deserialize;
use serde_json::Value;

use super::config::{Credentials, PackageSource};
use super::version;

/// The HTTP GET the client needs; Forge implements it on Zed's HTTP client.
pub trait Fetch: Send + Sync {
    fn get(&self, url: String, credentials: Option<Credentials>) -> BoxFuture<'static, Result<Vec<u8>>>;
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct PackageInfo {
    pub id: String,
    /// The newest version the source lists.
    pub version: String,
    pub description: String,
    pub authors: Vec<String>,
    pub total_downloads: u64,
    pub icon_url: Option<String>,
    pub project_url: Option<String>,
    pub license_url: Option<String>,
    pub tags: Vec<String>,
    pub verified: bool,
    pub versions: Vec<String>,
    /// Names of the sources it was found in.
    pub sources: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchResponse {
    data: Vec<SearchItem>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct SearchItem {
    id: String,
    version: String,
    description: Option<String>,
    authors: Option<Value>,
    total_downloads: Option<u64>,
    icon_url: Option<String>,
    project_url: Option<String>,
    license_url: Option<String>,
    tags: Option<Value>,
    verified: Option<bool>,
    versions: Vec<SearchVersion>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct SearchVersion {
    version: String,
}

fn strings(value: Option<Value>) -> Vec<String> {
    match value {
        Some(Value::String(s)) => s.split([',', ' ']).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect(),
        Some(Value::Array(items)) => items.into_iter().filter_map(|v| v.as_str().map(str::to_string)).collect(),
        _ => Vec::new(),
    }
}

impl SearchItem {
    fn into_info(self, source: &str) -> PackageInfo {
        PackageInfo {
            id: self.id,
            version: self.version,
            description: self.description.unwrap_or_default(),
            authors: strings(self.authors),
            total_downloads: self.total_downloads.unwrap_or(0),
            icon_url: self.icon_url.filter(|u| !u.is_empty()),
            project_url: self.project_url.filter(|u| !u.is_empty()),
            license_url: self.license_url.filter(|u| !u.is_empty()),
            tags: strings(self.tags),
            verified: self.verified.unwrap_or(false),
            versions: self.versions.into_iter().map(|v| v.version).collect(),
            sources: vec![source.to_string()],
        }
    }
}

#[derive(Clone, Debug, Default)]
struct Services {
    search: Option<String>,
    flat_container: Option<String>,
}

const TTL: Duration = Duration::from_secs(30 * 60);

#[derive(Default)]
struct Cache {
    responses: HashMap<String, (Instant, Arc<Vec<u8>>)>,
    services: HashMap<String, Services>,
}

/// A NuGet client over several sources.
#[derive(Clone)]
pub struct NuGetClient {
    fetch: Arc<dyn Fetch>,
    cache: Arc<parking_lot::Mutex<Cache>>,
}

impl NuGetClient {
    pub fn new(fetch: Arc<dyn Fetch>) -> Self {
        Self { fetch, cache: Default::default() }
    }

    /// Forgets cached responses, so the next queries see new versions.
    pub fn invalidate(&self) {
        let mut cache = self.cache.lock();
        cache.responses.clear();
        cache.services.clear();
    }

    async fn get(&self, url: &str, credentials: Option<Credentials>) -> Result<Arc<Vec<u8>>> {
        if let Some((at, body)) = self.cache.lock().responses.get(url)
            && at.elapsed() < TTL {
                return Ok(body.clone());
            }
        let body = Arc::new(self.fetch.get(url.to_string(), credentials).await?);
        self.cache.lock().responses.insert(url.to_string(), (Instant::now(), body.clone()));
        Ok(body)
    }

    async fn services(&self, source: &PackageSource) -> Result<Services> {
        if let Some(services) = self.cache.lock().services.get(&source.url) {
            return Ok(services.clone());
        }
        let body = self.get(&source.url, source.credentials.clone()).await?;
        let index: Value = serde_json::from_slice(&body).with_context(|| format!("{} is not a NuGet V3 service index", source.url))?;
        let resources = index.get("resources").and_then(Value::as_array).ok_or_else(|| anyhow!("{} has no resources", source.url))?;
        let find = |prefix: &str| {
            resources
                .iter()
                .filter(|r| r.get("@type").and_then(Value::as_str).is_some_and(|t| t.starts_with(prefix)))
                .filter_map(|r| r.get("@id").and_then(Value::as_str))
                .next()
                .map(|s| s.trim_end_matches('/').to_string())
        };
        let services = Services { search: find("SearchQueryService"), flat_container: find("PackageBaseAddress") };
        self.cache.lock().services.insert(source.url.clone(), services.clone());
        Ok(services)
    }

    async fn search_source(&self, source: &PackageSource, query: &str, prerelease: bool, skip: usize, take: usize) -> Result<Vec<PackageInfo>> {
        let services = self.services(source).await?;
        let search = services.search.ok_or_else(|| anyhow!("{} has no search service", source.name))?;
        let url = format!("{search}?q={}&skip={skip}&take={take}&prerelease={prerelease}&semVerLevel=2.0.0", encode(query));
        let body = self.get(&url, source.credentials.clone()).await?;
        let response: SearchResponse = serde_json::from_slice(&body).with_context(|| format!("unexpected search response from {}", source.name))?;
        Ok(response.data.into_iter().map(|item| item.into_info(&source.name)).collect())
    }

    /// Searches every HTTP source and merges results by id, keeping the order of the
    /// first source that answered and the newest version anywhere.
    pub async fn search(&self, sources: &[PackageSource], query: &str, prerelease: bool, skip: usize, take: usize) -> (Vec<PackageInfo>, Vec<String>) {
        let http: Vec<&PackageSource> = sources.iter().filter(|s| s.is_http()).collect();
        let results = join_all(http.iter().map(|s| self.search_source(s, query, prerelease, skip, take))).await;
        let mut merged: Vec<PackageInfo> = Vec::new();
        let mut errors = Vec::new();
        for (source, result) in http.iter().zip(results) {
            match result {
                Ok(items) => {
                    for item in items {
                        if let Some(existing) = merged.iter_mut().find(|p| p.id.eq_ignore_ascii_case(&item.id)) {
                            merge(existing, item);
                        } else {
                            merged.push(item);
                        }
                    }
                }
                Err(error) => errors.push(format!("{}: {error:#}", source.name)),
            }
        }
        (merged, errors)
    }

    /// Details of one package by id, from the first source that knows it.
    pub async fn package(&self, sources: &[PackageSource], id: &str, prerelease: bool) -> Option<PackageInfo> {
        let (results, _) = self.search(sources, &format!("packageid:{id}"), prerelease, 0, 5).await;
        if let Some(found) = results.into_iter().find(|p| p.id.eq_ignore_ascii_case(id)) {
            return Some(found);
        }
        // Not every feed understands `packageid:`.
        let (results, _) = self.search(sources, id, prerelease, 0, 20).await;
        results.into_iter().find(|p| p.id.eq_ignore_ascii_case(id))
    }

    /// Every version of a package across sources, newest first.
    pub async fn versions(&self, sources: &[PackageSource], id: &str, prerelease: bool) -> Vec<String> {
        let http: Vec<&PackageSource> = sources.iter().filter(|s| s.is_http()).collect();
        let lists = join_all(http.iter().map(|source| async move {
            let services = self.services(source).await.ok()?;
            let base = services.flat_container?;
            let url = format!("{base}/{}/index.json", id.to_lowercase());
            let body = self.get(&url, source.credentials.clone()).await.ok()?;
            let json: Value = serde_json::from_slice(&body).ok()?;
            Some(json.get("versions")?.as_array()?.iter().filter_map(|v| v.as_str().map(str::to_string)).collect::<Vec<_>>())
        }))
        .await;
        version::sort_desc(lists.into_iter().flatten().flatten(), prerelease)
    }
}

fn merge(existing: &mut PackageInfo, other: PackageInfo) {
    if version::is_newer(&other.version, &existing.version) {
        existing.version = other.version.clone();
    }
    for v in other.versions {
        if !existing.versions.iter().any(|e| e.eq_ignore_ascii_case(&v)) {
            existing.versions.push(v);
        }
    }
    existing.sources.extend(other.sources);
    existing.total_downloads = existing.total_downloads.max(other.total_downloads);
    if existing.description.is_empty() {
        existing.description = other.description;
    }
}

/// Percent-encodes a query string value.
pub fn encode(value: &str) -> String {
    value
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            b' ' => "+".to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct FakeFeed {
        requests: Mutex<Vec<String>>,
    }

    impl Fetch for FakeFeed {
        fn get(&self, url: String, _credentials: Option<Credentials>) -> BoxFuture<'static, Result<Vec<u8>>> {
            self.requests.lock().unwrap().push(url.clone());
            let body = if url == "https://a/flat/serilog/index.json" {
                r#"{"versions":["2.0.0","3.1.1","4.0.0-beta"]}"#.to_string()
            } else if url.ends_with("index.json") && url.contains("/a/") {
                r#"{"resources":[{"@id":"https://a/query","@type":"SearchQueryService/3.5.0"},{"@id":"https://a/flat/","@type":"PackageBaseAddress/3.0.0"}]}"#.to_string()
            } else if url.ends_with("index.json") && url.contains("/b/") {
                r#"{"resources":[{"@id":"https://b/query","@type":"SearchQueryService"}]}"#.to_string()
            } else if url.starts_with("https://a/query") {
                r#"{"totalHits":1,"data":[{"id":"Serilog","version":"3.1.1","description":"Logging","authors":["Serilog Contributors"],"totalDownloads":10,"versions":[{"version":"3.0.0"},{"version":"3.1.1"}]}]}"#.to_string()
            } else if url.starts_with("https://b/query") {
                r#"{"data":[{"id":"serilog","version":"4.0.0-dev","authors":"me, you","versions":[{"version":"4.0.0-dev"}]},{"id":"Internal","version":"1.0.0"}]}"#.to_string()
            } else {
                return Box::pin(async move { Err(anyhow!("404 {url}")) });
            };
            Box::pin(async move { Ok(body.into_bytes()) })
        }
    }

    fn sources() -> Vec<PackageSource> {
        vec![
            PackageSource { name: "a".into(), url: "https://a/v3/index.json".into(), credentials: None, enabled: true },
            PackageSource { name: "b".into(), url: "https://b/v3/index.json".into(), credentials: None, enabled: true },
            PackageSource { name: "local".into(), url: "/tmp/feed".into(), credentials: None, enabled: true },
        ]
    }

    #[test]
    fn searches_and_merges_sources() {
        let feed = Arc::new(FakeFeed { requests: Mutex::new(Vec::new()) });
        let client = NuGetClient::new(feed.clone());
        let (results, errors) = futures::executor::block_on(client.search(&sources(), "seri log", true, 0, 20));
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].id, "Serilog");
        assert_eq!(results[0].version, "4.0.0-dev");
        assert_eq!(results[0].sources, vec!["a", "b"]);
        assert_eq!(results[1].authors, Vec::<String>::new());
        assert!(feed.requests.lock().unwrap().iter().any(|u| u.contains("q=seri+log")));

        let versions = futures::executor::block_on(client.versions(&sources(), "Serilog", false));
        assert_eq!(versions, vec!["3.1.1", "2.0.0"]);

        // Cached: the service index is not fetched again.
        let before = feed.requests.lock().unwrap().len();
        futures::executor::block_on(client.versions(&sources(), "Serilog", false));
        assert_eq!(feed.requests.lock().unwrap().len(), before);
    }
}
