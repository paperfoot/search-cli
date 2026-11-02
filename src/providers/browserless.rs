use super::spec::{FilterSupport, Price};
use super::{Adapter, Call, Output, ProviderSpec};
use crate::context::AppContext;
use crate::errors::SearchError;
use crate::types::{Mode, SearchResult};
use async_trait::async_trait;
use std::sync::Arc;
use std::time::Duration;

pub static SPEC: ProviderSpec = ProviderSpec {
    name: "browserless",
    env_keys: &["BROWSERLESS_API_KEY"],
    modes: &[Mode::Extract],
    filters: FilterSupport {
        freshness: false,
        domains: false,
        country: false,
        lang: false,
    },
    max_count: 1,
    timeout: Duration::from_secs(60),
    billed_on_send: true,
    price: Price {
        per_call: 4_000,
        per_extra_result: 0,
        included_results: 0,
    },
};

pub struct Browserless {
    ctx: Arc<AppContext>,
}

impl Browserless {
    pub fn new(ctx: Arc<AppContext>) -> Self {
        Self { ctx }
    }

    fn api_key(&self) -> String {
        super::key_for(&self.ctx.config, &SPEC)
    }
}

#[async_trait]
impl Adapter for Browserless {
    fn spec(&self) -> &'static ProviderSpec {
        &SPEC
    }

    fn is_configured(&self) -> bool {
        !self.api_key().is_empty()
    }

    /// Render the page in Browserless's cloud browser (Cloudflare, JS-heavy
    /// pages), then extract the article text locally.
    async fn call(&self, call: &Call<'_>) -> Result<Output, SearchError> {
        if !self.is_configured() {
            return Err(SearchError::AuthMissing {
                provider: "browserless",
            });
        }
        let url = call.query;
        let body = serde_json::json!({
            "url": url,
            "waitForSelector": { "selector": "body", "timeout": 10000 }
        });
        let client = &self.ctx.client;
        let token = self.api_key();

        let resp = super::send(self, call.timeout, || {
            client
                .post("https://production-sfo.browserless.io/content")
                .header("Content-Type", "application/json")
                .bearer_auth(&token)
                .json(&body)
        })
        .await?;
        let html = resp.text().await?;

        let parsed_url = url::Url::parse(url).map_err(|e| SearchError::InvalidInput {
            message: format!("Invalid URL '{url}': {e}"),
        })?;
        let mut cursor = std::io::Cursor::new(html.as_bytes());
        let (title, text) = match readability::extractor::extract(&mut cursor, &parsed_url) {
            Ok(article) if !article.text.trim().is_empty() => {
                let title = if article.title.is_empty() {
                    url.to_string()
                } else {
                    article.title
                };
                (title, article.text)
            }
            _ => (url.to_string(), extract_text_simple(&html)),
        };

        if text.trim().is_empty() {
            return Err(SearchError::Api {
                provider: "browserless",
                code: "extraction_error",
                message: "Page returned no extractable content".to_string(),
                status: None,
            });
        }

        Ok(Output {
            results: vec![SearchResult {
                title,
                url: url.to_string(),
                snippet: text,
                source: "browserless".to_string(),
                published: None,
                image_url: None,
                extra: None,
            }],
            ..Output::default()
        })
    }
}

/// Simple HTML tag stripper
fn extract_text_simple(html: &str) -> String {
    let mut text = String::with_capacity(html.len() / 3);
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => text.push(c),
            _ => {}
        }
    }
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}
