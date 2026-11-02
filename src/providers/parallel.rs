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

pub static SPEC: ProviderSpec = ProviderSpec {
    name: "parallel",
    env_keys: &["PARALLEL_API_KEY"],
    modes: &[Mode::General, Mode::News, Mode::Deep],
    filters: FilterSupport {
        freshness: true,
        domains: true,
        country: true,
        lang: false,
    },
    max_count: 20,
    timeout: Duration::from_secs(15),
    billed_on_send: true,
    price: Price {
        per_call: 1_000,
        per_extra_result: 1_000,
        included_results: 10,
    },
};

pub struct Parallel {
    ctx: Arc<AppContext>,
}

impl Parallel {
    pub fn new(ctx: Arc<AppContext>) -> Self {
        Self { ctx }
    }

    fn api_key(&self) -> String {
        super::key_for(&self.ctx.config, &SPEC)
    }
}

#[derive(Deserialize)]
struct ParallelResponse {
    results: Option<Vec<ParallelResult>>,
}

#[derive(Deserialize)]
struct ParallelResult {
    title: Option<String>,
    url: Option<String>,
    excerpts: Option<Vec<String>>,
}

#[async_trait]
impl Adapter for Parallel {
    fn spec(&self) -> &'static ProviderSpec {
        &SPEC
    }

    fn is_configured(&self) -> bool {
        !self.api_key().is_empty()
    }

    // Every mode sends `mode: "fast"` today, so deep costs the base price. A
    // lane that switches deep to "advanced" must set `cost_micros`
    // (5,000 + 1,000/result over 10).
    async fn call(&self, call: &Call<'_>) -> Result<Output, SearchError> {
        if !self.is_configured() {
            return Err(SearchError::AuthMissing {
                provider: "parallel",
            });
        }
        let opts = call.opts;
        // Filters go natively through advanced_settings.source_policy
        // (include_domains / exclude_domains / after_date).
        let mut source_policy = serde_json::Map::new();
        if !opts.include_domains.is_empty() {
            source_policy.insert("include_domains".to_string(), json!(opts.include_domains));
        }
        if !opts.exclude_domains.is_empty() {
            source_policy.insert("exclude_domains".to_string(), json!(opts.exclude_domains));
        }
        if let Some(days) = opts.freshness.as_deref().and_then(super::freshness_days) {
            source_policy.insert("after_date".to_string(), json!(super::date_days_ago(days)));
        }
        let mut body = json!({
            "objective": call.query,
            "search_queries": [call.query],
            "mode": "fast",
            "num_results": call.count,
            "excerpts": { "max_chars_per_result": 3000 },
        });
        if !source_policy.is_empty() {
            body["advanced_settings"] = json!({ "source_policy": source_policy });
        }
        if let Some(country) = &opts.country {
            body["location"] = json!(country.to_uppercase());
        }

        let client = &self.ctx.client;
        let api_key = self.api_key();
        let resp = super::send(self, call.timeout, || {
            client
                .post("https://api.parallel.ai/v1beta/search")
                .header("x-api-key", api_key.as_str())
                .header("Content-Type", "application/json")
                .json(&body)
        })
        .await?;
        let data: ParallelResponse = resp.json().await?;

        let results = data
            .results
            .unwrap_or_default()
            .into_iter()
            .filter_map(|r| {
                Some(SearchResult {
                    title: r.title.unwrap_or_default(),
                    url: r.url?,
                    snippet: r.excerpts.unwrap_or_default().join("\n\n"),
                    source: "parallel".to_string(),
                    published: None,
                    image_url: None,
                    extra: None,
                })
            })
            .collect();
        Ok(Output {
            results,
            ..Output::default()
        })
    }
}
