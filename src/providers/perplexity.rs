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
    name: "perplexity",
    env_keys: &["PERPLEXITY_API_KEY"],
    modes: &[Mode::General, Mode::News, Mode::Academic, Mode::Deep],
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
        per_call: 5_000,
        per_extra_result: 0,
        included_results: 0,
    },
};

const MODEL: &str = "sonar-pro";

pub struct Perplexity {
    ctx: Arc<AppContext>,
}

impl Perplexity {
    pub fn new(ctx: Arc<AppContext>) -> Self {
        Self { ctx }
    }

    fn api_key(&self) -> String {
        super::key_for(&self.ctx.config, &SPEC)
    }
}

#[derive(Deserialize)]
struct PerplexityResponse {
    choices: Vec<PerplexityChoice>,
    citations: Option<Vec<String>>,
    search_results: Option<Vec<PerplexitySearchResult>>,
}

#[derive(Deserialize)]
struct PerplexityChoice {
    message: PerplexityMessage,
}

#[derive(Deserialize)]
struct PerplexityMessage {
    content: String,
}

#[derive(Deserialize)]
struct PerplexitySearchResult {
    title: Option<String>,
    url: Option<String>,
    snippet: Option<String>,
    date: Option<String>,
}

#[async_trait]
impl Adapter for Perplexity {
    fn spec(&self) -> &'static ProviderSpec {
        &SPEC
    }

    fn is_configured(&self) -> bool {
        !self.api_key().is_empty()
    }

    async fn call(&self, call: &Call<'_>) -> Result<Output, SearchError> {
        if !self.is_configured() {
            return Err(SearchError::AuthMissing {
                provider: "perplexity",
            });
        }
        let opts = call.opts;
        let mut body = json!({
            "model": MODEL,
            "messages": [{"role": "user", "content": call.query}],
            "return_related_questions": false,
            "return_images": false,
            "web_search_options": {
                "search_context_size": "high"
            },
        });

        // Perplexity's search_domain_filter takes one array; exclusions are
        // entries prefixed with '-'.
        if !opts.include_domains.is_empty() || !opts.exclude_domains.is_empty() {
            let domains: Vec<String> = opts
                .include_domains
                .iter()
                .cloned()
                .chain(opts.exclude_domains.iter().map(|d| format!("-{d}")))
                .collect();
            body["search_domain_filter"] = json!(domains);
        }

        // News without an explicit -f defaults to the last day.
        let recency = opts
            .freshness
            .as_deref()
            .or((call.mode == Mode::News).then_some("day"));
        if let Some(r) = recency {
            body["search_recency_filter"] = json!(r);
        }

        let client = &self.ctx.client;
        let key = self.api_key();
        let resp = super::send(self, call.timeout, || {
            client
                .post("https://api.perplexity.ai/chat/completions")
                .bearer_auth(&key)
                .json(&body)
        })
        .await?;
        let resp: PerplexityResponse = resp.json().await?;

        let source_label = format!("perplexity_{MODEL}");
        let answers = resp
            .choices
            .into_iter()
            .next()
            .map(|c| c.message.content)
            .filter(|text| !text.trim().is_empty())
            .map(|text| Answer {
                provider: "perplexity".to_string(),
                text,
            })
            .into_iter()
            .collect();

        let mut results = Vec::new();
        if let Some(search_results) = resp.search_results {
            for sr in search_results {
                results.push(SearchResult {
                    title: sr.title.unwrap_or_default(),
                    url: sr.url.unwrap_or_default(),
                    snippet: sr.snippet.unwrap_or_else(|| "[Search result]".to_string()),
                    source: format!("{source_label}_result"),
                    published: sr.date,
                    image_url: None,
                    extra: None,
                });
            }
        } else if let Some(citations) = resp.citations {
            // Fallback: one result per citation URL
            for cite_url in citations {
                let hostname = url::Url::parse(&cite_url)
                    .ok()
                    .and_then(|u| u.host_str().map(|h| h.to_string()))
                    .unwrap_or_else(|| cite_url.clone());
                results.push(SearchResult {
                    title: hostname,
                    url: cite_url,
                    snippet: "[Citation]".to_string(),
                    source: "perplexity_citation".to_string(),
                    published: None,
                    image_url: None,
                    extra: None,
                });
            }
        }

        Ok(Output {
            results,
            answers,
            cost_micros: None,
        })
    }
}
