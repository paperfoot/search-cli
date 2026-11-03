use super::spec::{FilterSupport, Price};
use super::{Adapter, Call, Output, ProviderSpec};
use crate::context::AppContext;
use crate::errors::SearchError;
use crate::types::{Mode, SearchResult};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

/// Keenable: an independent web index built for agents (https://keenable.ai).
pub static SPEC: ProviderSpec = ProviderSpec {
    name: "keenable",
    env_keys: &["KEENABLE_API_KEY"],
    modes: &[Mode::General, Mode::Deep],
    filters: FilterSupport {
        freshness: false,
        domains: false,
        country: false,
        lang: false,
    },
    max_count: 50,
    timeout: Duration::from_secs(15),
    billed_on_send: true,
    // Pay as you go: $4 per 1,000 requests (keenable.ai/pricing, 2026-11).
    price: Price {
        per_call: 4_000,
        per_extra_result: 0,
        included_results: 0,
    },
};

pub struct Keenable {
    ctx: Arc<AppContext>,
}

impl Keenable {
    pub fn new(ctx: Arc<AppContext>) -> Self {
        Self { ctx }
    }

    fn api_key(&self) -> String {
        super::key_for(&self.ctx.config, &SPEC)
    }
}

#[derive(Deserialize)]
struct KeenableResponse {
    #[serde(default)]
    results: Vec<KeenableResult>,
}

#[derive(Deserialize)]
struct KeenableResult {
    title: Option<String>,
    url: Option<String>,
    snippet: Option<String>,
    description: Option<String>,
    published_at: Option<String>,
}

fn request(client: &reqwest::Client, key: &str, call: &Call<'_>) -> reqwest::RequestBuilder {
    client
        .post("https://api.keenable.ai/v1/search")
        .header("X-API-Key", key)
        .json(&json!({
            "query": call.query,
            "max_results": call.count.clamp(1, SPEC.max_count),
        }))
}

fn is_http(u: &str) -> bool {
    let l = u.get(..8).unwrap_or(u).to_ascii_lowercase();
    l.starts_with("https://") || l.starts_with("http://")
}

fn parse(resp: KeenableResponse) -> Output {
    let results = resp
        .results
        .into_iter()
        .filter_map(|r| {
            let url = r.url.filter(|u| is_http(u))?;
            let snippet = r
                .snippet
                .filter(|s| !s.trim().is_empty())
                .or(r.description)
                .unwrap_or_default();
            Some(SearchResult {
                title: r.title.unwrap_or_default(),
                url,
                snippet,
                source: "keenable".to_string(),
                published: r.published_at.filter(|d| !d.is_empty()),
                image_url: None,
                extra: None,
            })
        })
        .collect();
    Output {
        results,
        answers: Vec::new(),
        cost_micros: Some(u64::from(SPEC.price.per_call)),
    }
}

#[async_trait]
impl Adapter for Keenable {
    fn spec(&self) -> &'static ProviderSpec {
        &SPEC
    }

    fn is_configured(&self) -> bool {
        !self.api_key().is_empty()
    }

    async fn call(&self, call: &Call<'_>) -> Result<Output, SearchError> {
        if !self.is_configured() {
            return Err(SearchError::AuthMissing {
                provider: "keenable",
            });
        }
        let key = self.api_key();
        let client = &self.ctx.client;
        let resp = super::send(self, call.timeout, || request(client, &key, call)).await?;
        let resp: KeenableResponse = resp.json().await?;
        Ok(parse(resp))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::SearchOpts;

    #[test]
    fn key_rides_in_header_and_count_is_capped() {
        let opts = SearchOpts::default();
        let call = Call {
            mode: Mode::General,
            query: "world cup",
            count: 80,
            opts: &opts,
            timeout: Duration::from_secs(1),
        };
        let req = request(&reqwest::Client::new(), "kn-test", &call)
            .build()
            .unwrap();
        assert_eq!(req.url().as_str(), "https://api.keenable.ai/v1/search");
        assert_eq!(req.headers().get("x-api-key").unwrap(), "kn-test");
        let body: serde_json::Value =
            serde_json::from_slice(req.body().unwrap().as_bytes().unwrap()).unwrap();
        assert_eq!(body["query"], "world cup");
        assert_eq!(body["max_results"], 50);
    }

    #[test]
    fn parses_results_snippet_fallback_and_drops_non_http() {
        let resp: KeenableResponse =
            serde_json::from_str(include_str!("../../tests/fixtures/keenable/search.json"))
                .unwrap();
        let out = parse(resp);
        assert_eq!(out.results.len(), 2);
        assert_eq!(out.results[0].source, "keenable");
        assert_eq!(
            out.results[0].published.as_deref(),
            Some("2026-07-19T22:10:00Z")
        );
        assert_eq!(out.results[1].snippet, "Fallback description");
        assert_eq!(out.cost_micros, Some(4_000));
    }
}
