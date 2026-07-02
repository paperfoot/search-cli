use crate::context::AppContext;
use crate::errors::SearchError;
use crate::types::{SearchOpts, SearchResult};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

pub struct Parallel {
    ctx: Arc<AppContext>,
}

impl Parallel {
    pub fn new(ctx: Arc<AppContext>) -> Self {
        Self { ctx }
    }

    fn api_key(&self) -> String {
        super::resolve_key(&self.ctx.config.keys.parallel, "PARALLEL_API_KEY")
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
impl super::Provider for Parallel {
    fn name(&self) -> &'static str {
        "parallel"
    }

    fn capabilities(&self) -> &[&'static str] {
        &["general", "news", "deep"]
    }

    fn env_keys(&self) -> &[&'static str] {
        &["PARALLEL_API_KEY", "SEARCH_KEYS_PARALLEL"]
    }

    fn is_configured(&self) -> bool {
        !self.api_key().is_empty()
    }

    async fn search(
        &self,
        query: &str,
        count: usize,
        opts: &SearchOpts,
    ) -> Result<Vec<SearchResult>, SearchError> {
        if !self.is_configured() {
            return Err(SearchError::AuthMissing {
                provider: "parallel",
            });
        }

        let client = &self.ctx.client;
        let api_key = self.api_key();

        // Parallel supports filters natively via advanced_settings.source_policy
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

        super::retry_request(|| async {
            let mut body = json!({
                "objective": query,
                "search_queries": [query],
                "mode": "fast",
                "num_results": count,
                "excerpts": { "max_chars_per_result": 3000 },
            });
            if !source_policy.is_empty() {
                body["advanced_settings"] = json!({ "source_policy": source_policy.clone() });
            }

            let resp = client
                .post("https://api.parallel.ai/v1beta/search")
                .header("x-api-key", api_key.as_str())
                .header("Content-Type", "application/json")
                .json(&body)
                .send()
                .await?;

            let resp = super::ok_or_api_error(resp, "parallel").await?;

            let data: ParallelResponse = resp.json().await?;
            let results = data
                .results
                .unwrap_or_default()
                .into_iter()
                .filter_map(|r| {
                    let url = r.url?;
                    Some(SearchResult {
                        title: r.title.unwrap_or_default(),
                        url,
                        snippet: r.excerpts.unwrap_or_default().join("\n\n"),
                        source: "parallel".to_string(),
                        published: None,
                        image_url: None,
                        extra: None,
                    })
                })
                .collect();

            Ok(results)
        })
        .await
    }

    async fn search_news(
        &self,
        query: &str,
        count: usize,
        opts: &SearchOpts,
    ) -> Result<Vec<SearchResult>, SearchError> {
        self.search(query, count, opts).await
    }
}
