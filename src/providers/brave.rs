use super::spec::{FilterSupport, Price};
use super::{Adapter, Call, Output, ProviderSpec};
use crate::context::AppContext;
use crate::errors::SearchError;
use crate::types::{FailureCategory, Mode, SearchOpts, SearchResult};
use async_trait::async_trait;
use serde::Deserialize;
use std::sync::Arc;
use std::time::Duration;

pub static SPEC: ProviderSpec = ProviderSpec {
    name: "brave",
    env_keys: &["BRAVE_API_KEY"],
    modes: &[Mode::General, Mode::News, Mode::Deep],
    filters: FilterSupport {
        freshness: true,
        domains: true,
        country: true,
        lang: true,
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

pub struct Brave {
    ctx: Arc<AppContext>,
}

impl Brave {
    pub fn new(ctx: Arc<AppContext>) -> Self {
        Self { ctx }
    }

    fn api_key(&self) -> String {
        super::key_for(&self.ctx.config, &SPEC)
    }
}

#[derive(Deserialize)]
struct BraveResponse {
    web: Option<BraveWeb>,
    news: Option<BraveNews>,
}

#[derive(Deserialize)]
struct BraveWeb {
    results: Vec<BraveResult>,
}

#[derive(Deserialize)]
struct BraveNews {
    results: Vec<BraveNewsResult>,
}

#[derive(Deserialize)]
struct BraveResult {
    title: Option<String>,
    url: Option<String>,
    description: Option<String>,
    extra_snippets: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct BraveNewsResult {
    title: Option<String>,
    url: Option<String>,
    description: Option<String>,
    age: Option<String>,
}

/// Brave freshness: pd (day), pw (week), pm (month), py (year)
fn map_freshness(f: &str) -> &str {
    match f {
        "day" => "pd",
        "week" => "pw",
        "month" => "pm",
        "year" => "py",
        other => other, // pass through if already in Brave format
    }
}

#[async_trait]
impl Adapter for Brave {
    fn spec(&self) -> &'static ProviderSpec {
        &SPEC
    }

    fn is_configured(&self) -> bool {
        !self.api_key().is_empty()
    }

    /// Brave names the cause in `error.code`. Its per-second 429 also lists
    /// the monthly quota in `meta`, so only the code tells a rate limit
    /// (retry shortly) from an exhausted plan (add credit).
    fn classify(&self, _status: u16, body: &str) -> Option<FailureCategory> {
        let v: serde_json::Value = serde_json::from_str(body).ok()?;
        match v.pointer("/error/code")?.as_str()? {
            "RATE_LIMITED" => Some(FailureCategory::RateLimit),
            "QUOTA_LIMITED" => Some(FailureCategory::BillingQuota),
            "SUBSCRIPTION_TOKEN_INVALID" | "SUBSCRIPTION_NOT_FOUND" => Some(FailureCategory::Auth),
            _ => None,
        }
    }

    async fn call(&self, call: &Call<'_>) -> Result<Output, SearchError> {
        if !self.is_configured() {
            return Err(SearchError::AuthMissing { provider: "brave" });
        }
        let per_call = SPEC.price.per_call as u64;
        match call.mode {
            Mode::News => Ok(Output {
                results: self.search_news(call).await?,
                ..Output::default()
            }),
            // Deep = web search plus LLM Context, concurrently, inside one
            // provider call, so `-p` governs both and the cost is one line.
            Mode::Deep => {
                let (web, grounding) = tokio::join!(self.search(call), self.llm_context(call));
                let calls_ok = web.is_ok() as u64 + grounding.is_ok() as u64;
                let results = match (web, grounding) {
                    (Ok(w), Ok(g)) => merge(w, g),
                    (Ok(w), Err(e)) => {
                        tracing::warn!("brave llm context: {e}");
                        w
                    }
                    (Err(e), Ok(g)) => {
                        tracing::warn!("brave web: {e}");
                        g
                    }
                    (Err(e), Err(_)) => return Err(e),
                };
                Ok(Output {
                    results,
                    answers: Vec::new(),
                    cost_micros: Some(per_call * calls_ok),
                })
            }
            _ => Ok(Output {
                results: self.search(call).await?,
                ..Output::default()
            }),
        }
    }
}

/// Web results first, then grounding results for URLs web search missed.
fn merge(web: Vec<SearchResult>, grounding: Vec<SearchResult>) -> Vec<SearchResult> {
    let seen: std::collections::HashSet<String> = web
        .iter()
        .map(|r| r.url.trim_end_matches('/').to_string())
        .collect();
    let mut out = web;
    out.extend(
        grounding
            .into_iter()
            .filter(|r| !seen.contains(r.url.trim_end_matches('/'))),
    );
    out
}

fn common_params(
    mut req: reqwest::RequestBuilder,
    opts: &SearchOpts,
    with_locale: bool,
) -> reqwest::RequestBuilder {
    if let Some(f) = opts.freshness.as_deref().map(map_freshness) {
        req = req.query(&[("freshness", f)]);
    }
    if with_locale {
        if let Some(country) = &opts.country {
            req = req.query(&[("country", country.to_uppercase())]);
        }
        if let Some(lang) = &opts.lang {
            req = req.query(&[("search_lang", lang.to_lowercase())]);
        }
    }
    req
}

impl Brave {
    async fn search(&self, call: &Call<'_>) -> Result<Vec<SearchResult>, SearchError> {
        let client = &self.ctx.client;
        let api_key = self.api_key();
        let count = call.count.to_string();
        let q = super::augment_query(call.query, call.opts);

        let resp = super::send(self, call.timeout, || {
            let req = client
                .get("https://api.search.brave.com/res/v1/web/search")
                .header("X-Subscription-Token", api_key.as_str())
                .header("Accept", "application/json")
                .query(&[
                    ("q", q.as_str()),
                    ("count", count.as_str()),
                    ("extra_snippets", "true"),
                ]);
            common_params(req, call.opts, true)
        })
        .await?;

        let body: BraveResponse = resp.json().await?;
        Ok(body
            .web
            .map(|w| w.results)
            .unwrap_or_default()
            .into_iter()
            .map(|r| {
                // Description plus extra snippets for richer context.
                let mut snippet = r.description.unwrap_or_default();
                for extra in r.extra_snippets.unwrap_or_default() {
                    snippet.push('\n');
                    snippet.push_str(&extra);
                }
                SearchResult {
                    title: r.title.unwrap_or_default(),
                    url: r.url.unwrap_or_default(),
                    snippet,
                    source: "brave".to_string(),
                    published: None,
                    image_url: None,
                    extra: None,
                }
            })
            .collect())
    }

    async fn search_news(&self, call: &Call<'_>) -> Result<Vec<SearchResult>, SearchError> {
        let client = &self.ctx.client;
        let api_key = self.api_key();
        let count = call.count.to_string();
        let q = super::augment_query(call.query, call.opts);

        let resp = super::send(self, call.timeout, || {
            let req = client
                .get("https://api.search.brave.com/res/v1/news/search")
                .header("X-Subscription-Token", api_key.as_str())
                .header("Accept", "application/json")
                .query(&[("q", q.as_str()), ("count", count.as_str())]);
            common_params(req, call.opts, true)
        })
        .await?;

        let body: BraveResponse = resp.json().await?;
        Ok(body
            .news
            .map(|n| n.results)
            .unwrap_or_default()
            .into_iter()
            .map(|r| SearchResult {
                title: r.title.unwrap_or_default(),
                url: r.url.unwrap_or_default(),
                snippet: r.description.unwrap_or_default(),
                source: "brave_news".to_string(),
                published: r.age,
                image_url: None,
                extra: None,
            })
            .collect())
    }

    /// LLM Context API — pre-extracted, relevance-scored content for grounding.
    async fn llm_context(&self, call: &Call<'_>) -> Result<Vec<SearchResult>, SearchError> {
        let client = &self.ctx.client;
        let api_key = self.api_key();
        let q = super::augment_query(call.query, call.opts);
        let count = call.count.to_string();

        let resp = super::send(self, call.timeout, || {
            let req = client
                .get("https://api.search.brave.com/res/v1/llm/context")
                .header("X-Subscription-Token", api_key.as_str())
                .header("Accept", "application/json")
                .query(&[
                    ("q", q.as_str()),
                    ("count", count.as_str()),
                    ("maximum_number_of_tokens", "16384"),
                    ("context_threshold_mode", "balanced"),
                ]);
            common_params(req, call.opts, false)
        })
        .await?;

        let body: serde_json::Value = resp.json().await?;
        let mut results = Vec::new();
        if let Some(generic) = body
            .pointer("/grounding/generic")
            .and_then(|v| v.as_array())
        {
            for item in generic {
                let str_of = |k: &str| {
                    item.get(k)
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string()
                };
                let snippets = item
                    .get("snippets")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|s| s.as_str())
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .unwrap_or_default();
                results.push(SearchResult {
                    title: str_of("title"),
                    url: str_of("url"),
                    snippet: snippets,
                    source: "brave_llm_context".to_string(),
                    published: None,
                    image_url: None,
                    extra: None,
                });
            }
        }
        Ok(results)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;

    fn brave() -> Brave {
        Brave::new(Arc::new(AppContext::new(AppConfig::default())))
    }

    #[test]
    fn error_code_decides_rate_limit_quota_and_auth() {
        use FailureCategory as C;
        let b = brave();
        // The free plan's 1 req/s answer (as published in openclaw/openclaw#14245).
        let per_second = r#"{"type":"ErrorResponse","error":{"id":"4083cd30","status":429,"detail":"Request rate limit exceeded for plan","meta":{"plan":"Free","rate_limit":1,"rate_current":1,"quota_limit":2000,"quota_current":342},"code":"RATE_LIMITED"}}"#;
        assert_eq!(b.classify(429, per_second), Some(C::RateLimit));
        let monthly = r#"{"type":"ErrorResponse","error":{"status":429,"detail":"Request quota limit exceeded for plan","code":"QUOTA_LIMITED"}}"#;
        assert_eq!(b.classify(429, monthly), Some(C::BillingQuota));
        let dead_key = r#"{"type":"ErrorResponse","error":{"status":422,"detail":"The provided subscription token is invalid.","code":"SUBSCRIPTION_TOKEN_INVALID"}}"#;
        assert_eq!(b.classify(422, dead_key), Some(C::Auth));
        assert_eq!(b.classify(400, "not json"), None);
    }
}
