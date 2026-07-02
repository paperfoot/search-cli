use crate::context::AppContext;
use crate::errors::SearchError;
use crate::types::{SearchOpts, SearchResult};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

pub struct Linkup {
    ctx: Arc<AppContext>,
}

impl Linkup {
    pub fn new(ctx: Arc<AppContext>) -> Self {
        Self { ctx }
    }

    fn api_key(&self) -> String {
        super::resolve_key(&self.ctx.config.keys.linkup, "LINKUP_API_KEY")
    }

    async fn do_search(
        &self,
        query: &str,
        count: usize,
        opts: &SearchOpts,
        default_days_back: Option<u64>,
    ) -> Result<Vec<SearchResult>, SearchError> {
        if self.api_key().is_empty() {
            return Err(SearchError::AuthMissing { provider: "linkup" });
        }

        let mut body = json!({
            "q": query,
            "depth": "standard",
            "outputType": "searchResults",
            "maxResults": count,
        });
        if !opts.include_domains.is_empty() {
            body["includeDomains"] = json!(opts.include_domains);
        }
        if !opts.exclude_domains.is_empty() {
            body["excludeDomains"] = json!(opts.exclude_domains);
        }
        // Freshness maps to fromDate (YYYY-MM-DD). News search without an
        // explicit -f defaults to the last week, since Linkup has no news
        // vertical of its own.
        let days_back = opts
            .freshness
            .as_deref()
            .and_then(super::freshness_days)
            .or(default_days_back);
        if let Some(days) = days_back {
            body["fromDate"] = json!(super::date_days_ago(days));
        }

        let client = &self.ctx.client;
        let key = self.api_key();
        let resp = super::retry_request(|| async {
            let r = client
                .post("https://api.linkup.so/v1/search")
                .bearer_auth(&key)
                .json(&body)
                .send()
                .await?;
            let r = super::ok_or_api_error(r, "linkup").await?;
            Ok(r.json::<LinkupResponse>().await?)
        })
        .await?;

        Ok(resp
            .results
            .unwrap_or_default()
            .into_iter()
            .filter(|r| r.result_type.as_deref() != Some("image"))
            .map(|r| SearchResult {
                title: r.name.unwrap_or_default(),
                url: r.url.unwrap_or_default(),
                snippet: r.content.unwrap_or_default(),
                source: "linkup".to_string(),
                published: None,
                image_url: None,
                extra: None,
            })
            .filter(|r| !r.url.is_empty())
            .collect())
    }
}

#[derive(Deserialize)]
struct LinkupResponse {
    results: Option<Vec<LinkupResult>>,
}

#[derive(Deserialize)]
struct LinkupResult {
    #[serde(rename = "type")]
    result_type: Option<String>,
    name: Option<String>,
    url: Option<String>,
    content: Option<String>,
}

#[async_trait]
impl super::Provider for Linkup {
    fn name(&self) -> &'static str {
        "linkup"
    }

    fn capabilities(&self) -> &[&'static str] {
        &["general", "news", "deep"]
    }

    fn env_keys(&self) -> &[&'static str] {
        &["LINKUP_API_KEY", "SEARCH_KEYS_LINKUP"]
    }

    fn is_configured(&self) -> bool {
        !self.api_key().is_empty()
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(15)
    }

    async fn search(
        &self,
        query: &str,
        count: usize,
        opts: &SearchOpts,
    ) -> Result<Vec<SearchResult>, SearchError> {
        self.do_search(query, count, opts, None).await
    }

    async fn search_news(
        &self,
        query: &str,
        count: usize,
        opts: &SearchOpts,
    ) -> Result<Vec<SearchResult>, SearchError> {
        self.do_search(query, count, opts, Some(7)).await
    }
}
