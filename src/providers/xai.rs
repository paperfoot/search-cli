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
    name: "xai",
    env_keys: &["XAI_API_KEY"],
    modes: &[Mode::Social, Mode::Deep],
    // Domains map to X handles (see registry::filter_support's note).
    filters: FilterSupport {
        freshness: true,
        domains: true,
        country: false,
        lang: false,
    },
    max_count: 20,
    timeout: Duration::from_secs(60),
    billed_on_send: true,
    price: Price {
        per_call: 45_000,
        per_extra_result: 0,
        included_results: 0,
    },
};

pub struct Xai {
    ctx: Arc<AppContext>,
}

impl Xai {
    pub fn new(ctx: Arc<AppContext>) -> Self {
        Self { ctx }
    }

    fn api_key(&self) -> String {
        super::key_for(&self.ctx.config, &SPEC)
    }

    async fn call_responses_api(
        &self,
        prompt: &str,
        x_search_config: Option<serde_json::Value>,
        timeout: Duration,
    ) -> Result<XaiResponse, SearchError> {
        let mut x_search_tool = json!({"type": "x_search"});
        if let (Some(tool_obj), Some(serde_json::Value::Object(config))) =
            (x_search_tool.as_object_mut(), x_search_config)
        {
            tool_obj.extend(config);
        }

        let body = json!({
            "model": "grok-4-1-fast",
            "input": [{"role": "user", "content": prompt}],
            "tools": [x_search_tool],
            "store": false,
        });

        let client = &self.ctx.client;
        let key = self.api_key();
        let resp = super::send(self, timeout, || {
            client
                .post("https://api.x.ai/v1/responses")
                .bearer_auth(&key)
                .header("Content-Type", "application/json")
                .json(&body)
        })
        .await?;
        Ok(resp.json().await?)
    }

    fn build_x_search_config(&self, opts: &SearchOpts) -> Option<serde_json::Value> {
        let mut config = serde_json::Map::new();

        // Map include_domains to allowed_x_handles
        if !opts.include_domains.is_empty() {
            config.insert("allowed_x_handles".to_string(), json!(opts.include_domains));
        }

        // Map exclude_domains to excluded_x_handles
        if !opts.exclude_domains.is_empty() {
            config.insert(
                "excluded_x_handles".to_string(),
                json!(opts.exclude_domains),
            );
        }

        // Convert freshness to date range
        if let Some(days) = opts.freshness.as_deref().and_then(super::freshness_days) {
            config.insert("from_date".to_string(), json!(super::date_days_ago(days)));
            config.insert("to_date".to_string(), json!(super::date_days_ago(0)));
        }

        if config.is_empty() {
            None
        } else {
            Some(serde_json::Value::Object(config))
        }
    }
}

#[derive(Deserialize)]
struct XaiResponse {
    output: Option<Vec<XaiOutputItem>>,
}

#[derive(Deserialize)]
struct XaiOutputItem {
    #[serde(rename = "type")]
    item_type: Option<String>,
    content: Option<Vec<XaiContent>>,
    text: Option<String>,
}

#[derive(Deserialize)]
struct XaiContent {
    #[serde(rename = "type")]
    content_type: Option<String>,
    text: Option<String>,
    url: Option<String>,
    annotations: Option<Vec<XaiAnnotation>>,
}

#[derive(Deserialize)]
struct XaiAnnotation {
    #[serde(rename = "type")]
    annotation_type: Option<String>,
    url: Option<String>,
}

fn extract_text(resp: &XaiResponse) -> String {
    let mut parts = Vec::new();
    if let Some(output) = &resp.output {
        for item in output {
            if item.item_type.as_deref() == Some("message") {
                if let Some(content) = &item.content {
                    for c in content {
                        if c.content_type.as_deref() == Some("output_text") {
                            if let Some(text) = &c.text {
                                parts.push(text.clone());
                            }
                        }
                    }
                }
            } else if let Some(text) = &item.text {
                parts.push(text.clone());
            }
        }
    }
    parts.join("\n")
}

fn extract_citations(resp: &XaiResponse) -> Vec<String> {
    let mut urls = Vec::new();
    let mut seen = std::collections::HashSet::new();
    if let Some(output) = &resp.output {
        for item in output {
            if let Some(content) = &item.content {
                for c in content {
                    // Extract from url_citation annotations (primary method)
                    if let Some(annotations) = &c.annotations {
                        for ann in annotations {
                            if ann.annotation_type.as_deref() == Some("url_citation") {
                                if let Some(url) = &ann.url {
                                    if seen.insert(url.clone()) {
                                        urls.push(url.clone());
                                    }
                                }
                            }
                        }
                    }
                    // Fallback: check for cite/url content types
                    if c.content_type.as_deref() == Some("cite")
                        || c.content_type.as_deref() == Some("url")
                    {
                        if let Some(url) = &c.url {
                            if seen.insert(url.clone()) {
                                urls.push(url.clone());
                            }
                        }
                    }
                }
            }
        }
    }
    urls
}

#[async_trait]
impl Adapter for Xai {
    fn spec(&self) -> &'static ProviderSpec {
        &SPEC
    }

    fn is_configured(&self) -> bool {
        !self.api_key().is_empty()
    }

    async fn call(&self, call: &Call<'_>) -> Result<Output, SearchError> {
        if !self.is_configured() {
            return Err(SearchError::AuthMissing { provider: "xai" });
        }
        let (query, count) = (call.query, call.count);
        let prompt = format!(
            "Search X (Twitter) for posts about: {query}\n\
             Return up to {count} recent and relevant posts.\n\
             For each post include: author @username, post text, date/time, \
             and engagement metrics (likes, reposts, replies) if available.\n\
             Format as a clear summary."
        );

        let x_config = self.build_x_search_config(call.opts);
        let resp = self
            .call_responses_api(&prompt, x_config, call.timeout)
            .await?;

        let mut results = Vec::new();
        let answer = extract_text(&resp);

        if !answer.is_empty() {
            results.push(SearchResult {
                title: "X Summary".to_string(),
                url: format!("https://x.com/search?q={}", query.replace(' ', "+")),
                snippet: answer,
                source: "xai".to_string(),
                published: None,
                image_url: None,
                extra: None,
            });
        }

        // Append citations as separate results
        for cite_url in extract_citations(&resp) {
            let handle = url::Url::parse(&cite_url)
                .ok()
                .and_then(|u| u.path().split('/').nth(1).map(|s| format!("@{s}")))
                .unwrap_or_else(|| cite_url.clone());

            results.push(SearchResult {
                title: handle,
                url: cite_url,
                snippet: "[Citation from X]".to_string(),
                source: "xai_citation".to_string(),
                published: None,
                image_url: None,
                extra: None,
            });
        }

        Ok(Output {
            results,
            ..Output::default()
        })
    }
}
