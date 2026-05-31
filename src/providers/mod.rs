pub mod brave;
pub mod browserless;
pub mod exa;
pub mod firecrawl;
pub mod jina;
pub mod parallel;
pub mod perplexity;
pub mod serpapi;
pub mod serper;
pub mod stealth;
pub mod tavily;
pub mod xai;

use crate::context::AppContext;
use crate::errors::SearchError;
use crate::types::{SearchOpts, SearchResult};
use async_trait::async_trait;
use backon::{ExponentialBuilder, Retryable};
use std::sync::Arc;
use std::time::Duration;

pub async fn retry_request<F, Fut, T>(f: F) -> Result<T, SearchError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, SearchError>>,
{
    f.retry(
        ExponentialBuilder::default()
            .with_min_delay(Duration::from_secs(1))
            .with_max_delay(Duration::from_secs(4))
            .with_max_times(3),
    )
    .when(|e| matches!(e, SearchError::Http(_)))
    .await
}

/// Resolve a provider API key. **Environment variable wins over the config
/// file** — CI/session credentials and a freshly-exported key must be able to
/// override stale local config. (Previously config won, so an out-of-date
/// `keys.brave` silently shadowed a working `BRAVE_API_KEY` and every brave
/// request failed.) Because figment already merges `SEARCH_KEYS_*` env over
/// TOML into `config_value`, the full precedence is:
/// bare env var (e.g. `BRAVE_API_KEY`) > `SEARCH_KEYS_*` env > config file.
pub fn resolve_key(config_value: &str, env_var: &str) -> String {
    if let Ok(v) = std::env::var(env_var) {
        if !v.is_empty() {
            return v;
        }
    }
    config_value.to_string()
}

/// Turn a non-2xx reqwest response into a categorized [`SearchError`], preserving
/// a short, sanitized body excerpt so "HTTP 422" becomes
/// "HTTP 422: The provided subscription token is invalid." Returns the response
/// unchanged on 2xx. Centralizing this here keeps status/body/429 handling
/// identical across every provider (previously copy-pasted and drift-prone).
pub async fn ok_or_api_error(
    resp: reqwest::Response,
    provider: &'static str,
) -> Result<reqwest::Response, SearchError> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let code = status.as_u16();
    if code == 429 {
        return Err(SearchError::RateLimited { provider });
    }
    // Body is only read on the error path; the success path stays zero-copy.
    let body = resp.text().await.unwrap_or_default();
    Err(SearchError::Api {
        provider,
        code: "api_error",
        message: http_error_message(code, &body),
        status: Some(code),
    })
}

/// Format `HTTP <status>` with a sanitized one-line body excerpt when present.
pub fn http_error_message(status: u16, body: &str) -> String {
    let excerpt = sanitize_excerpt(body, 200);
    if excerpt.is_empty() {
        format!("HTTP {status}")
    } else {
        format!("HTTP {status}: {excerpt}")
    }
}

/// Collapse whitespace and clip to `max` chars (char-safe, never panics on UTF-8).
fn sanitize_excerpt(body: &str, max: usize) -> String {
    let collapsed = body.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out: String = collapsed.chars().take(max).collect();
    if collapsed.chars().count() > max {
        out.push('…');
    }
    out
}

#[async_trait]
pub trait Provider: Send + Sync {
    fn name(&self) -> &'static str;
    fn capabilities(&self) -> &[&'static str];
    fn is_configured(&self) -> bool;
    /// Standard env var names accepted by this provider (e.g. BRAVE_API_KEY).
    fn env_keys(&self) -> &[&'static str];
    fn timeout(&self) -> Duration {
        Duration::from_secs(10)
    }

    async fn search(
        &self,
        query: &str,
        count: usize,
        opts: &SearchOpts,
    ) -> Result<Vec<SearchResult>, SearchError>;
    async fn search_news(
        &self,
        query: &str,
        count: usize,
        opts: &SearchOpts,
    ) -> Result<Vec<SearchResult>, SearchError>;
}

pub fn build_providers(ctx: &Arc<AppContext>) -> Vec<Box<dyn Provider>> {
    vec![
        Box::new(parallel::Parallel::new(ctx.clone())),
        Box::new(brave::Brave::new(ctx.clone())),
        Box::new(serper::Serper::new(ctx.clone())),
        Box::new(exa::Exa::new(ctx.clone())),
        Box::new(jina::Jina::new(ctx.clone())),
        Box::new(stealth::Stealth::new(ctx.clone())),
        Box::new(firecrawl::Firecrawl::new(ctx.clone())),
        Box::new(tavily::Tavily::new(ctx.clone())),
        Box::new(browserless::Browserless::new(ctx.clone())),
        Box::new(perplexity::Perplexity::new(ctx.clone())),
        Box::new(serpapi::SerpApi::new(ctx.clone())),
        Box::new(xai::Xai::new(ctx.clone())),
    ]
}
