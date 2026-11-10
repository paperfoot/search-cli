//! Keenable Search: `POST /v1/search` with `X-API-Key`. Keenable's own web
//! index, built for agents; each result carries extracted page text in
//! `snippet`. `mode: "pro"` (deeper retrieval, the default) for every mode
//! this adapter serves. One credit per search, whatever the result count:
//! 100,000 free a month on an account, then $4 per 1,000.
//! https://docs.keenable.ai/products/search.md

use super::spec::{FilterSupport, Price};
use super::{Adapter, Call, Output, ProviderSpec};
use crate::context::AppContext;
use crate::errors::SearchError;
use crate::types::{Mode, SearchResult};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

const ENDPOINT: &str = "https://api.keenable.ai/v1/search";
const SNIPPET_CHARS: u32 = 1_500;

pub static SPEC: ProviderSpec = ProviderSpec {
    name: "keenable",
    env_keys: &["KEENABLE_API_KEY"],
    modes: &[Mode::General, Mode::Deep],
    filters: FilterSupport {
        freshness: true,
        // `site` takes one site only; the include/exclude lists are applied
        // to the results in `parse`.
        domains: true,
        country: false,
        lang: false,
    },
    max_count: 50,
    timeout: Duration::from_secs(30),
    billed_on_send: true,
    price: PRO,
};

/// One credit per call; no per-result fee.
const PRO: Price = Price {
    per_call: 4_000,
    per_extra_result: 0,
    included_results: 50,
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

fn body(call: &Call<'_>) -> Value {
    let opts = call.opts;
    let mut b = json!({
        "query": call.query,
        "max_results": call.count.clamp(1, SPEC.max_count),
        "mode": "pro",
        "snippet_max_length": SNIPPET_CHARS,
        "disable_cards": true,
    });
    if opts.include_domains.len() == 1 {
        b["site"] = json!(opts.include_domains[0]);
    }
    if let Some(days) = opts.freshness.as_deref().and_then(super::freshness_days) {
        b["published_after"] = json!(format!("{days}d"));
    }
    b
}

#[derive(Deserialize)]
struct KeenableResponse {
    #[serde(default)]
    results: Vec<KeenableResult>,
}

#[derive(Deserialize)]
struct KeenableResult {
    url: Option<String>,
    title: Option<String>,
    snippet: Option<String>,
    published_at: Option<String>,
}

fn host_matches(url: &str, domains: &[String]) -> bool {
    super::perplexity::host_in(url, domains)
}

fn parse(data: KeenableResponse, call: &Call<'_>) -> Output {
    let opts = call.opts;
    let results: Vec<SearchResult> = data
        .results
        .into_iter()
        .filter_map(|r| {
            let url = r.url.filter(|u| u.starts_with("http"))?;
            if !opts.include_domains.is_empty() && !host_matches(&url, &opts.include_domains) {
                return None;
            }
            if host_matches(&url, &opts.exclude_domains) {
                return None;
            }
            Some(SearchResult {
                title: r.title.unwrap_or_default(),
                url,
                snippet: r.snippet.unwrap_or_default().trim().to_string(),
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
        cost_micros: Some(PRO.estimate(call.count)),
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

    fn sent_cost(&self, _mode: Mode) -> u64 {
        PRO.estimate(0)
    }

    async fn call(&self, call: &Call<'_>) -> Result<Output, SearchError> {
        if !self.is_configured() {
            return Err(SearchError::AuthMissing {
                provider: "keenable",
            });
        }
        let body = body(call);
        let client = &self.ctx.client;
        let api_key = self.api_key();
        let resp = super::send(self, call.timeout, || {
            client
                .post(ENDPOINT)
                .header("X-API-Key", api_key.as_str())
                .json(&body)
        })
        .await?;
        let data: KeenableResponse = resp.json().await?;
        Ok(parse(data, call))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::SearchOpts;

    fn call<'a>(mode: Mode, count: u16, opts: &'a SearchOpts) -> Call<'a> {
        Call {
            mode,
            query: "kailh choc v2 datasheet",
            count,
            opts,
            timeout: Duration::from_secs(5),
        }
    }

    #[test]
    fn body_carries_query_count_mode_and_filters() {
        let opts = SearchOpts {
            include_domains: vec!["kailh.net".into()],
            freshness: Some("week".into()),
            ..Default::default()
        };
        let b = body(&call(Mode::General, 70, &opts));
        assert_eq!(b["query"], "kailh choc v2 datasheet");
        assert_eq!(b["max_results"], 50);
        assert_eq!(b["mode"], "pro");
        assert_eq!(b["site"], "kailh.net");
        assert_eq!(b["published_after"], "7d");
    }

    #[test]
    fn parses_results_and_applies_domain_lists() {
        let data: KeenableResponse = serde_json::from_value(json!({
            "query": "q", "mode": "pro",
            "results": [
                {"title": "A", "url": "https://kailh.net/a", "snippet": " text ", "published_at": "2026-10-30T00:00:00Z"},
                {"title": "B", "url": "https://spam.example/b", "snippet": "x"},
                {"title": "C", "url": "ftp://kailh.net/c"}
            ],
            "usage": {"sku": "search.pro", "credits": 1}
        }))
        .unwrap();
        let opts = SearchOpts {
            exclude_domains: vec!["spam.example".into()],
            ..Default::default()
        };
        let out = parse(data, &call(Mode::General, 10, &opts));
        assert_eq!(out.results.len(), 1);
        assert_eq!(out.results[0].snippet, "text");
        assert_eq!(out.results[0].source, "keenable");
        assert_eq!(
            out.results[0].published.as_deref(),
            Some("2026-10-30T00:00:00Z")
        );
    }
}
