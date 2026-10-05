//! NuGet over Zed's HTTP client, shared by the package manager and the project file
//! editor features.

use std::sync::Arc;

use anyhow::{Context as _, Result, bail};
use base64::Engine as _;
use dotnet_model::nuget::{Credentials, Fetch, NuGetClient};
use futures::AsyncReadExt as _;
use futures::future::BoxFuture;
use gpui::{App, Global};
use http_client::{AsyncBody, HttpClient, HttpRequestExt as _, RedirectPolicy, Request};

struct HttpFetch {
    http: Arc<dyn HttpClient>,
}

impl Fetch for HttpFetch {
    fn get(&self, url: String, credentials: Option<Credentials>) -> BoxFuture<'static, Result<Vec<u8>>> {
        let http = self.http.clone();
        Box::pin(async move {
            let mut request = Request::get(&url).follow_redirects(RedirectPolicy::FollowAll).header("Accept", "application/json");
            if let Some(credentials) = credentials {
                let token = base64::engine::general_purpose::STANDARD.encode(format!("{}:{}", credentials.username, credentials.password));
                request = request.header("Authorization", format!("Basic {token}"));
            }
            let mut response = http.send(request.body(AsyncBody::default())?).await.with_context(|| format!("requesting {url}"))?;
            let mut body = Vec::new();
            response.body_mut().read_to_end(&mut body).await.with_context(|| format!("reading {url}"))?;
            if !response.status().is_success() {
                bail!("{} answered {}", url, response.status());
            }
            Ok(body)
        })
    }
}

/// The app-wide NuGet client: one cache for every window.
pub struct NuGet(pub NuGetClient);

impl Global for NuGet {}

pub fn init(cx: &mut App) {
    let client = NuGetClient::new(Arc::new(HttpFetch { http: cx.http_client() }));
    cx.set_global(NuGet(client));
}

pub fn client(cx: &App) -> Option<NuGetClient> {
    cx.try_global::<NuGet>().map(|n| n.0.clone())
}
