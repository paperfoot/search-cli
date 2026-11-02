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
    name: "firecrawl",
    env_keys: &["FIRECRAWL_API_KEY"],
    modes: &[Mode::Extract],
    filters: FilterSupport {
        freshness: false,
        domains: false,
        country: false,
        lang: false,
    },
    max_count: 1,
    timeout: Duration::from_secs(30),
    billed_on_send: true,
    price: Price {
        per_call: 5_000,
        per_extra_result: 0,
        included_results: 0,
    },
};

pub struct Firecrawl {
    ctx: Arc<AppContext>,
}

impl Firecrawl {
    pub fn new(ctx: Arc<AppContext>) -> Self {
        Self { ctx }
    }

    fn api_key(&self) -> String {
        super::key_for(&self.ctx.config, &SPEC)
    }
}

#[derive(Deserialize)]
struct FirecrawlScrapeResponse {
    data: Option<FirecrawlData>,
}

#[derive(Deserialize)]
struct FirecrawlData {
    markdown: Option<String>,
    metadata: Option<FirecrawlMetadata>,
}

#[derive(Deserialize)]
struct FirecrawlMetadata {
    title: Option<String>,
    #[serde(rename = "sourceURL")]
    source_url: Option<String>,
}

#[async_trait]
impl Adapter for Firecrawl {
    fn spec(&self) -> &'static ProviderSpec {
        &SPEC
    }

    fn is_configured(&self) -> bool {
        !self.api_key().is_empty()
    }

    async fn call(&self, call: &Call<'_>) -> Result<Output, SearchError> {
        if !self.is_configured() {
            return Err(SearchError::AuthMissing {
                provider: "firecrawl",
            });
        }
        let url = call.query;
        let client = &self.ctx.client;
        let auth = format!("Bearer {}", self.api_key());
        let body = json!({
            "url": url,
            "formats": ["markdown"]
        });

        let resp = super::send(self, call.timeout, || {
            client
                .post("https://api.firecrawl.dev/v2/scrape")
                .header("Authorization", &auth)
                .header("Content-Type", "application/json")
                .json(&body)
        })
        .await?;
        let body: FirecrawlScrapeResponse = resp.json().await?;

        let results = match body.data {
            Some(data) => {
                let (title, source_url) = data
                    .metadata
                    .map(|m| (m.title, m.source_url))
                    .unwrap_or_default();
                vec![SearchResult {
                    title: title.unwrap_or_default(),
                    url: source_url.unwrap_or_else(|| url.to_string()),
                    snippet: data.markdown.unwrap_or_default(),
                    source: "firecrawl".to_string(),
                    published: None,
                    image_url: None,
                    extra: None,
                }]
            }
            None => Vec::new(),
        };
        Ok(Output {
            results,
            ..Output::default()
        })
    }
}
