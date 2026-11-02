use super::spec::{FilterSupport, Price};
use super::{Adapter, Call, Output, ProviderSpec};
use crate::context::AppContext;
use crate::errors::SearchError;
use crate::types::{Mode, SearchOpts, SearchResult};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

pub static SPEC: ProviderSpec = ProviderSpec {
    name: "exa",
    env_keys: &["EXA_API_KEY"],
    modes: &[
        Mode::General,
        Mode::Academic,
        Mode::People,
        Mode::Deep,
        Mode::Similar,
    ],
    filters: FilterSupport {
        freshness: true,
        domains: true,
        country: false,
        lang: false,
    },
    max_count: 20,
    timeout: Duration::from_secs(15),
    billed_on_send: true,
    price: Price {
        per_call: 7_000,
        per_extra_result: 1_000,
        included_results: 10,
    },
};

pub struct Exa {
    ctx: Arc<AppContext>,
}

impl Exa {
    pub fn new(ctx: Arc<AppContext>) -> Self {
        Self { ctx }
    }

    fn api_key(&self) -> String {
        super::key_for(&self.ctx.config, &SPEC)
    }

    async fn post_api(
        &self,
        path: &str,
        body: serde_json::Value,
        timeout: Duration,
    ) -> Result<ExaResponse, SearchError> {
        let url = format!("https://api.exa.ai/{path}");
        let client = &self.ctx.client;
        let api_key = self.api_key();

        let resp = super::send(self, timeout, || {
            client
                .post(&url)
                .header("x-api-key", api_key.as_str())
                .header("Content-Type", "application/json")
                .json(&body)
        })
        .await?;
        Ok(resp.json().await?)
    }
}

#[derive(Deserialize)]
struct ExaResponse {
    results: Option<Vec<ExaResult>>,
}

#[derive(Deserialize)]
struct ExaResult {
    title: Option<String>,
    url: Option<String>,
    text: Option<String>,
    #[serde(rename = "publishedDate")]
    published_date: Option<String>,
    highlights: Option<Vec<String>>,
}

fn to_results(exa: ExaResponse, source: &str) -> Vec<SearchResult> {
    exa.results
        .unwrap_or_default()
        .into_iter()
        .map(|r| {
            // Prefer highlights over full text for snippet (more relevant, shorter)
            let snippet = match r.highlights {
                Some(hl) if !hl.is_empty() => hl.join(" ... "),
                _ => r.text.unwrap_or_default(),
            };
            SearchResult {
                title: r.title.unwrap_or_default(),
                url: r.url.unwrap_or_default(),
                snippet,
                source: source.to_string(),
                published: r.published_date,
                image_url: None,
                extra: None,
            }
        })
        .collect()
}

fn build_search_body(query: &str, count: u16, opts: &SearchOpts) -> serde_json::Value {
    let mut body = json!({
        "query": query,
        "numResults": count,
        "type": "auto",
        "contents": {
            "text": true,
            "highlights": true
        }
    });
    // Exa supports includeDomains / excludeDomains natively
    if !opts.include_domains.is_empty() {
        body["includeDomains"] = json!(opts.include_domains);
    }
    if !opts.exclude_domains.is_empty() {
        body["excludeDomains"] = json!(opts.exclude_domains);
    }
    // Freshness → startPublishedDate (ISO 8601)
    if let Some(days) = opts.freshness.as_deref().and_then(super::freshness_days) {
        body["startPublishedDate"] = json!(format!("{}T00:00:00Z", super::date_days_ago(days)));
    }
    body
}

#[async_trait]
impl Adapter for Exa {
    fn spec(&self) -> &'static ProviderSpec {
        &SPEC
    }

    fn is_configured(&self) -> bool {
        !self.api_key().is_empty()
    }

    // Deep and academic still send the general request (type "auto"), so
    // their cost is the list estimate. A lane that switches deep to
    // `type: "deep"` must set `cost_micros` (12,000 + 1,000/result over 10).
    async fn call(&self, call: &Call<'_>) -> Result<Output, SearchError> {
        if !self.is_configured() {
            return Err(SearchError::AuthMissing { provider: "exa" });
        }
        let (path, body, source) = match call.mode {
            Mode::People => (
                "search",
                json!({
                    "query": call.query, "numResults": call.count, "type": "auto",
                    "category": "people", "contents": { "text": true }
                }),
                "exa_people",
            ),
            Mode::Similar => (
                "findSimilar",
                json!({ "url": call.query, "numResults": call.count, "contents": { "text": true } }),
                "exa_similar",
            ),
            _ => (
                "search",
                build_search_body(call.query, call.count, call.opts),
                "exa",
            ),
        };
        let resp = self.post_api(path, body, call.timeout).await?;
        Ok(Output {
            results: to_results(resp, source),
            ..Output::default()
        })
    }
}
