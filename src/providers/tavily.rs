use super::spec::{FilterSupport, Price};
use super::{Adapter, Call, Output, ProviderSpec};
use crate::context::AppContext;
use crate::errors::SearchError;
use crate::types::{Answer, Mode, SearchResult};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

pub static SPEC: ProviderSpec = ProviderSpec {
    name: "tavily",
    env_keys: &["TAVILY_API_KEY"],
    modes: &[Mode::General, Mode::News, Mode::Academic, Mode::Deep],
    filters: FilterSupport {
        freshness: true,
        domains: true,
        country: false,
        lang: false,
    },
    max_count: 20,
    timeout: Duration::from_secs(20),
    billed_on_send: true,
    price: Price {
        per_call: 8_000,
        per_extra_result: 0,
        included_results: 0,
    },
};

/// `search_depth: "advanced"` bills two credits (2 × 8,000 µ$).
const ADVANCED_MICROS: u64 = 16_000;

pub struct Tavily {
    ctx: Arc<AppContext>,
}

impl Tavily {
    pub fn new(ctx: Arc<AppContext>) -> Self {
        Self { ctx }
    }

    fn api_key(&self) -> String {
        super::key_for(&self.ctx.config, &SPEC)
    }
}

#[derive(Deserialize)]
struct TavilyResponse {
    results: Vec<TavilyResult>,
    answer: Option<String>,
}

#[derive(Deserialize)]
struct TavilyResult {
    title: Option<String>,
    url: Option<String>,
    content: Option<String>,
    score: Option<f64>,
    published_time: Option<String>,
}

#[async_trait]
impl Adapter for Tavily {
    fn spec(&self) -> &'static ProviderSpec {
        &SPEC
    }

    fn is_configured(&self) -> bool {
        !self.api_key().is_empty()
    }

    async fn call(&self, call: &Call<'_>) -> Result<Output, SearchError> {
        if !self.is_configured() {
            return Err(SearchError::AuthMissing { provider: "tavily" });
        }
        let opts = call.opts;
        let topic = if call.mode == Mode::News {
            "news"
        } else {
            "general"
        };
        let mut body = json!({
            "api_key": self.api_key(),
            "query": call.query,
            "search_depth": "advanced",
            "topic": topic,
            "max_results": call.count,
            "include_answer": "basic",
            "include_raw_content": false,
        });
        if !opts.include_domains.is_empty() {
            body["include_domains"] = json!(opts.include_domains);
        }
        if !opts.exclude_domains.is_empty() {
            body["exclude_domains"] = json!(opts.exclude_domains);
        }
        // Tavily time_range: day, week, month, year
        if let Some(f) = &opts.freshness {
            body["time_range"] = json!(f);
        }

        let client = &self.ctx.client;
        let resp = super::send(self, call.timeout, || {
            client.post("https://api.tavily.com/search").json(&body)
        })
        .await?;
        let resp: TavilyResponse = resp.json().await?;

        let source = if topic == "news" {
            "tavily_news"
        } else {
            "tavily"
        };
        let answers = resp
            .answer
            .filter(|a| !a.trim().is_empty())
            .map(|text| Answer {
                provider: "tavily".to_string(),
                text,
            })
            .into_iter()
            .collect();
        let results = resp
            .results
            .into_iter()
            .map(|r| SearchResult {
                title: r.title.unwrap_or_default(),
                url: r.url.unwrap_or_default(),
                snippet: r.content.unwrap_or_default(),
                source: source.to_string(),
                published: r.published_time,
                image_url: None,
                extra: r.score.map(|s| json!({"score": s})),
            })
            .collect();
        Ok(Output {
            results,
            answers,
            // Every mode sends the advanced depth today.
            cost_micros: Some(ADVANCED_MICROS),
        })
    }
}
