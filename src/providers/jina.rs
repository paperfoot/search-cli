use super::spec::{FilterSupport, Price};
use super::{Adapter, Call, Output, ProviderSpec};
use crate::context::AppContext;
use crate::errors::SearchError;
use crate::types::{Mode, SearchResult};
use async_trait::async_trait;
use serde::Deserialize;
use std::sync::Arc;
use std::time::Duration;

pub static SPEC: ProviderSpec = ProviderSpec {
    name: "jina",
    env_keys: &["JINA_API_KEY"],
    modes: &[Mode::General, Mode::Extract],
    filters: FilterSupport {
        freshness: false,
        domains: true,
        country: false,
        lang: false,
    },
    max_count: 20,
    timeout: Duration::from_secs(20),
    billed_on_send: true,
    price: Price {
        per_call: 500,
        per_extra_result: 0,
        included_results: 0,
    },
};

pub struct Jina {
    ctx: Arc<AppContext>,
}

impl Jina {
    pub fn new(ctx: Arc<AppContext>) -> Self {
        Self { ctx }
    }

    fn api_key(&self) -> String {
        super::key_for(&self.ctx.config, &SPEC)
    }
}

#[derive(Deserialize)]
struct JinaSearchResponse {
    data: Option<Vec<JinaResult>>,
}

#[derive(Deserialize)]
struct JinaResult {
    title: Option<String>,
    url: Option<String>,
    description: Option<String>,
    content: Option<String>,
}

#[async_trait]
impl Adapter for Jina {
    fn spec(&self) -> &'static ProviderSpec {
        &SPEC
    }

    fn is_configured(&self) -> bool {
        !self.api_key().is_empty()
    }

    async fn call(&self, call: &Call<'_>) -> Result<Output, SearchError> {
        if !self.is_configured() {
            return Err(SearchError::AuthMissing { provider: "jina" });
        }
        let results = match call.mode {
            Mode::Extract | Mode::Scrape => self.read_url(call).await?,
            _ => self.search(call).await?,
        };
        Ok(Output {
            results,
            ..Output::default()
        })
    }
}

impl Jina {
    async fn search(&self, call: &Call<'_>) -> Result<Vec<SearchResult>, SearchError> {
        let client = &self.ctx.client;
        let auth = format!("Bearer {}", self.api_key());
        let count = call.count.to_string();
        // No native domain filters: rewrite the query with site: operators.
        let q = super::augment_query(call.query, call.opts);

        let resp = super::send(self, call.timeout, || {
            client
                .get("https://s.jina.ai/")
                .header("Authorization", &auth)
                .header("Accept", "application/json")
                .header("X-Retain-Images", "none")
                .query(&[("q", q.as_str()), ("count", count.as_str())])
        })
        .await?;

        let body: JinaSearchResponse = resp.json().await?;
        Ok(body
            .data
            .unwrap_or_default()
            .into_iter()
            .map(|r| SearchResult {
                title: r.title.unwrap_or_default(),
                url: r.url.unwrap_or_default(),
                snippet: r.description.or(r.content).unwrap_or_default(),
                source: "jina".to_string(),
                published: None,
                image_url: None,
                extra: None,
            })
            .collect())
    }

    async fn read_url(&self, call: &Call<'_>) -> Result<Vec<SearchResult>, SearchError> {
        let url = call.query;
        let reader_url = format!("https://r.jina.ai/{url}");
        let client = &self.ctx.client;
        let auth = format!("Bearer {}", self.api_key());

        let resp = super::send(self, call.timeout, || {
            client
                .get(&reader_url)
                .header("Authorization", &auth)
                .header("Accept", "application/json")
                .header("X-Retain-Images", "none")
        })
        .await?;

        let body: serde_json::Value = resp.json().await?;
        let field = |k: &str| {
            body.pointer(&format!("/data/{k}"))
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string()
        };
        Ok(vec![SearchResult {
            title: field("title"),
            url: url.to_string(),
            snippet: field("content"),
            source: "jina_reader".to_string(),
            published: None,
            image_url: None,
            extra: None,
        }])
    }
}
