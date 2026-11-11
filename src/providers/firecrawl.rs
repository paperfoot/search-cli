use super::spec::{FilterSupport, Price};
use super::{Adapter, Call, Output, ProviderSpec};
use crate::context::AppContext;
use crate::errors::SearchError;
use crate::page::Page;
use crate::types::Mode;
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
}

/// Firecrawl bills one credit per page and one per PDF page; this caps a
/// PDF at 20 credits.
const PDF_MAX_PAGES: u32 = 20;
/// One Firecrawl credit at the rate-card price (PLAN.md §5), in micro-USD.
const MICROS_PER_CREDIT: u64 = 5_000;

#[derive(Deserialize)]
struct FirecrawlScrapeResponse {
    data: Option<FirecrawlData>,
    #[serde(rename = "creditsUsed")]
    credits_used: Option<u64>,
}

#[derive(Deserialize)]
struct FirecrawlData {
    markdown: Option<String>,
    metadata: Option<FirecrawlMetadata>,
}

#[derive(Deserialize)]
struct FirecrawlMetadata {
    title: Option<String>,
    url: Option<String>,
    #[serde(rename = "sourceURL")]
    source_url: Option<String>,
    #[serde(rename = "statusCode")]
    status_code: Option<u16>,
    #[serde(rename = "creditsUsed")]
    credits_used: Option<u64>,
}

fn scrape_body(url: &str) -> serde_json::Value {
    json!({
        "url": url,
        "formats": ["markdown"],
        "parsers": [{ "type": "pdf", "maxPages": PDF_MAX_PAGES }],
    })
}

#[async_trait]
impl Adapter for Firecrawl {
    fn spec(&self) -> &'static ProviderSpec {
        &SPEC
    }

    fn keys(&self) -> Vec<String> {
        super::keys_for(&self.ctx.config, &SPEC)
    }

    fn is_configured(&self) -> bool {
        !self.keys().is_empty()
    }

    async fn call(&self, call: &Call<'_>) -> Result<Output, SearchError> {
        super::direct::unsupported_unless_extract("firecrawl", call.mode)?;
        if !self.is_configured() {
            return Err(SearchError::AuthMissing {
                provider: "firecrawl",
            });
        }
        crate::registry::rung_gate("firecrawl", call.query)?;
        let url = call.query;
        let client = &self.ctx.client;
        let auth = format!("Bearer {}", call.key);
        let body = scrape_body(url);

        let resp = super::send(self, call.timeout, || {
            client
                .post("https://api.firecrawl.dev/v2/scrape")
                .header("Authorization", &auth)
                .header("Content-Type", "application/json")
                .json(&body)
        })
        .await?;
        let body: FirecrawlScrapeResponse = resp.json().await?;
        Ok(to_output(body, url).await)
    }
}

/// The scrape as one result, judged on its markdown and the target's own
/// status (a 404 page is not content), priced from `creditsUsed`.
async fn to_output(body: FirecrawlScrapeResponse, url: &str) -> Output {
    let Some(data) = body.data else {
        return Output::default();
    };
    let meta = data.metadata;
    let credits = body
        .credits_used
        .or_else(|| meta.as_ref().and_then(|m| m.credits_used));
    let (title, final_url, status) = match meta {
        Some(m) => (
            m.title.filter(|t| !t.is_empty()),
            m.url.or(m.source_url),
            m.status_code,
        ),
        None => (None, None, None),
    };
    let page = Page {
        final_url: final_url.unwrap_or_else(|| url.to_string()),
        status: status.unwrap_or(200),
        content_type: Some("text/markdown".to_string()),
        body: data.markdown.unwrap_or_default(),
        headers: reqwest::header::HeaderMap::new(),
    };
    let mut out = super::direct::marked_output("firecrawl", page).await;
    if let (Some(r), Some(t)) = (out.results.first_mut(), title) {
        r.title = t;
    }
    out.cost_micros = credits.map(|c| c * MICROS_PER_CREDIT);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pdfs_are_capped_at_twenty_pages() {
        let body = scrape_body("https://example.com/paper.pdf");
        assert_eq!(body["parsers"][0]["type"], "pdf");
        assert_eq!(body["parsers"][0]["maxPages"], 20);
        assert!(body.get("scrapeOptions").is_none());
    }

    #[tokio::test]
    async fn credits_price_the_call_and_a_404_page_is_rejected() {
        let article = "A real paragraph of page text. ".repeat(20);
        let ok: FirecrawlScrapeResponse = serde_json::from_value(json!({
            "success": true,
            "data": {
                "markdown": article,
                "metadata": { "title": "Paper", "url": "https://example.com/final", "statusCode": 200, "creditsUsed": 7 }
            }
        }))
        .unwrap();
        let out = to_output(ok, "https://example.com/paper.pdf").await;
        assert_eq!(out.cost_micros, Some(35_000));
        assert_eq!(out.results[0].url, "https://example.com/final");
        assert_eq!(out.results[0].title, "Paper");
        assert!(crate::registry::accept(Mode::Extract, &out).is_ok());

        let missing: FirecrawlScrapeResponse = serde_json::from_value(json!({
            "data": { "markdown": article, "metadata": { "statusCode": 404 } }
        }))
        .unwrap();
        let out = to_output(missing, "https://example.com/gone").await;
        assert!(crate::registry::accept(Mode::Extract, &out).is_err());
    }
}
