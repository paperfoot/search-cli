use crate::context::AppContext;
use crate::errors::SearchError;
use crate::types::{SearchOpts, SearchResult};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

pub struct YouCom {
    ctx: Arc<AppContext>,
}

impl YouCom {
    pub fn new(ctx: Arc<AppContext>) -> Self {
        Self { ctx }
    }

    fn api_key(&self) -> String {
        super::resolve_key(&self.ctx.config.keys.youcom, "YDC_API_KEY")
    }
}

#[derive(Debug, Deserialize)]
struct YouResponse {
    results: Option<YouResults>,
}

#[derive(Debug, Deserialize)]
struct YouResults {
    web: Option<Vec<YouResult>>,
    news: Option<Vec<YouResult>>,
}

#[derive(Debug, Deserialize)]
struct YouResult {
    title: Option<String>,
    url: Option<String>,
    description: Option<String>,
    snippets: Option<Vec<String>>,
    page_age: Option<String>,
    thumbnail_url: Option<String>,
}

fn build_body(query: &str, count: usize, opts: &SearchOpts) -> serde_json::Value {
    let mut body = json!({
        "query": query,
        "count": count.min(100).max(1),
    });

    if !opts.include_domains.is_empty() {
        body["include_domains"] = json!(opts.include_domains);
    }
    if !opts.exclude_domains.is_empty() {
        body["exclude_domains"] = json!(opts.exclude_domains);
    }
    if let Some(freshness) = &opts.freshness {
        body["freshness"] = json!(freshness);
    }
    if let Some(country) = &opts.country {
        body["country"] = json!(country.to_uppercase());
    }
    if let Some(lang) = &opts.lang {
        body["language"] = json!(lang.to_uppercase());
    }

    body
}

fn join_snippets(description: Option<String>, snippets: Option<Vec<String>>) -> String {
    let mut out = description.unwrap_or_default();
    let joined = snippets
        .unwrap_or_default()
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    if !joined.is_empty() {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&joined);
    }
    out
}

fn map_result(result: YouResult, source: &str) -> Option<SearchResult> {
    let url = result.url.unwrap_or_default();
    if url.is_empty() {
        return None;
    }

    Some(SearchResult {
        title: result.title.unwrap_or_default(),
        url,
        snippet: join_snippets(result.description, result.snippets),
        source: source.to_string(),
        published: result.page_age,
        image_url: result.thumbnail_url,
        extra: None,
    })
}

fn collect_results(resp: YouResponse, news_only: bool) -> Vec<SearchResult> {
    let sections = resp.results.unwrap_or(YouResults {
        web: None,
        news: None,
    });

    let mut out = Vec::new();
    let push_section = |results: Option<Vec<YouResult>>, source: &str, out: &mut Vec<SearchResult>| {
        if let Some(results) = results {
            out.extend(results.into_iter().filter_map(|r| map_result(r, source)));
        }
    };

    if news_only {
        push_section(sections.news, "youcom_news", &mut out);
        if out.is_empty() {
            push_section(sections.web, "youcom", &mut out);
        }
    } else {
        push_section(sections.web, "youcom", &mut out);
        push_section(sections.news, "youcom_news", &mut out);
    }

    out
}

async fn search_impl(
    ctx: &AppContext,
    key: String,
    query: &str,
    count: usize,
    opts: &SearchOpts,
    news_only: bool,
) -> Result<Vec<SearchResult>, SearchError> {
    if key.is_empty() {
        return Err(SearchError::AuthMissing { provider: "youcom" });
    }

    let body = build_body(query, count, opts);
    super::retry_request(|| async {
        let resp = ctx
            .client
            .post("https://ydc-index.io/v1/search")
            .header("X-API-Key", key.as_str())
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await?;

        let resp = super::ok_or_api_error(resp, "youcom").await?;

        let body_bytes = resp.bytes().await?;
        let mut body_vec = body_bytes.to_vec();
        let parsed: YouResponse =
            simd_json::from_slice(&mut body_vec).map_err(|e| SearchError::Api {
                provider: "youcom",
                code: "json_error",
                status: None,
                message: e.to_string(),
            })?;
        Ok(collect_results(parsed, news_only))
    })
    .await
}

#[async_trait]
impl super::Provider for YouCom {
    fn name(&self) -> &'static str {
        "youcom"
    }

    fn capabilities(&self) -> &[&'static str] {
        &["general", "news", "deep"]
    }

    fn env_keys(&self) -> &[&'static str] {
        &["YDC_API_KEY", "SEARCH_KEYS_YOUCOM"]
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
        search_impl(&self.ctx, self.api_key(), query, count, opts, false).await
    }

    async fn search_news(
        &self,
        query: &str,
        count: usize,
        opts: &SearchOpts,
    ) -> Result<Vec<SearchResult>, SearchError> {
        search_impl(&self.ctx, self.api_key(), query, count, opts, true).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_body_applies_limits_and_filters() {
        let opts = SearchOpts {
            include_domains: vec!["example.com".into()],
            exclude_domains: vec!["spam.example".into()],
            freshness: Some("week".into()),
            country: Some("us".into()),
            lang: Some("en".into()),
        };

        let body = build_body("rust search", 250, &opts);
        assert_eq!(body["query"], "rust search");
        assert_eq!(body["count"], 100);
        assert_eq!(body["include_domains"][0], "example.com");
        assert_eq!(body["exclude_domains"][0], "spam.example");
        assert_eq!(body["freshness"], "week");
        assert_eq!(body["country"], "US");
        assert_eq!(body["language"], "EN");
    }

    #[test]
    fn maps_web_and_news_sections() {
        let resp = YouResponse {
            results: Some(YouResults {
                web: Some(vec![YouResult {
                    title: Some("Web".into()),
                    url: Some("https://example.com/web".into()),
                    description: Some("web desc".into()),
                    snippets: Some(vec!["one".into(), "two".into()]),
                    page_age: Some("2026-08-31T00:00:00Z".into()),
                    thumbnail_url: Some("https://example.com/web.png".into()),
                }]),
                news: Some(vec![YouResult {
                    title: Some("News".into()),
                    url: Some("https://example.com/news".into()),
                    description: Some("news desc".into()),
                    snippets: Some(vec!["fresh".into()]),
                    page_age: Some("2026-08-31T01:00:00Z".into()),
                    thumbnail_url: None,
                }]),
            }),
        };

        let results = collect_results(resp, false);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].source, "youcom");
        assert_eq!(results[0].snippet, "web desc\none\ntwo");
        assert_eq!(results[0].image_url.as_deref(), Some("https://example.com/web.png"));
        assert_eq!(results[1].source, "youcom_news");
    }

    #[test]
    fn news_mode_falls_back_to_web_when_news_missing() {
        let resp = YouResponse {
            results: Some(YouResults {
                web: Some(vec![YouResult {
                    title: Some("Fallback".into()),
                    url: Some("https://example.com/fallback".into()),
                    description: None,
                    snippets: Some(vec!["fallback".into()]),
                    page_age: None,
                    thumbnail_url: None,
                }]),
                news: None,
            }),
        };

        let results = collect_results(resp, true);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].source, "youcom");
        assert_eq!(results[0].snippet, "fallback");
    }
}
