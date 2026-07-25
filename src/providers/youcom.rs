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
        super::resolve_key(&self.ctx.config.keys.youcom, "YOUCOM_API_KEY")
    }

    fn search_url(&self) -> &'static str {
        "https://ydc-index.io/v1/search"
    }

    fn build_query(&self, query: &str, opts: &SearchOpts) -> String {
        let mut q = query.to_string();
        for domain in &opts.include_domains {
            q = format!("{q} site:{domain}");
        }
        for domain in &opts.exclude_domains {
            q = format!("{q} -site:{domain}");
        }
        q
    }

    async fn query(
        &self,
        query: &str,
        count: usize,
        opts: &SearchOpts,
    ) -> Result<YouComResponse, SearchError> {
        if self.api_key().is_empty() {
            return Err(SearchError::AuthMissing { provider: "youcom" });
        }

        let client = &self.ctx.client;
        let api_key = self.api_key();
        let q = self.build_query(query, opts);
        let count_str = count.to_string();

        super::retry_request(|| async {
            let mut req = client
                .get(self.search_url())
                .header("X-API-Key", api_key.as_str())
                .header("Accept", "application/json")
                .query(&[("query", q.as_str()), ("count", count_str.as_str())]);

            if let Some(freshness) = opts.freshness.as_deref() {
                req = req.query(&[("freshness", freshness)]);
            }

            let resp = req.send().await?;
            let status = resp.status();
            if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                return Err(SearchError::RateLimited { provider: "youcom" });
            }
            if !status.is_success() {
                return Err(SearchError::Api {
                    provider: "youcom",
                    code: "api_error",
                    message: format!("HTTP {status}"),
                });
            }

            Ok(resp.json::<YouComResponse>().await?)
        })
        .await
    }

    fn map_results(items: &[YouComResult], source: &str) -> Vec<SearchResult> {
        items
            .iter()
            .map(|item| {
                let snippet = item
                    .snippets
                    .as_ref()
                    .filter(|snippets| !snippets.is_empty())
                    .map(|snippets| snippets.join(" ... "))
                    .or_else(|| item.description.clone())
                    .unwrap_or_default();

                SearchResult {
                    title: item.title.clone().unwrap_or_default(),
                    url: item.url.clone().unwrap_or_default(),
                    snippet,
                    source: source.to_string(),
                    published: item.page_age.clone(),
                    image_url: None,
                    extra: item
                        .favicon_url
                        .as_ref()
                        .map(|favicon_url| json!({ "favicon_url": favicon_url })),
                }
            })
            .collect()
    }

    fn merge_sections(response: YouComResponse, news_only: bool) -> Vec<SearchResult> {
        let mut results = Vec::new();
        if let Some(results_by_type) = response.results {
            if news_only {
                if !results_by_type.news.is_empty() {
                    results.extend(Self::map_results(&results_by_type.news, "youcom_news"));
                } else {
                    results.extend(Self::map_results(&results_by_type.web, "youcom_web"));
                }
            } else {
                results.extend(Self::map_results(&results_by_type.web, "youcom_web"));
                results.extend(Self::map_results(&results_by_type.news, "youcom_news"));
            }
        }
        results
    }
}

#[derive(Debug, Deserialize)]
struct YouComResponse {
    #[serde(default)]
    results: Option<YouComSections>,
}

#[derive(Debug, Deserialize, Default)]
struct YouComSections {
    #[serde(default)]
    web: Vec<YouComResult>,
    #[serde(default)]
    news: Vec<YouComResult>,
}

#[derive(Debug, Deserialize, Clone)]
struct YouComResult {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    snippets: Option<Vec<String>>,
    #[serde(default, rename = "page_age")]
    page_age: Option<String>,
    #[serde(default)]
    favicon_url: Option<String>,
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
        &["YOUCOM_API_KEY", "SEARCH_KEYS_YOUCOM"]
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
        let response = self.query(query, count, opts).await?;
        Ok(Self::merge_sections(response, false))
    }

    async fn search_news(
        &self,
        query: &str,
        count: usize,
        opts: &SearchOpts,
    ) -> Result<Vec<SearchResult>, SearchError> {
        let mut news_opts = opts.clone();
        if news_opts.freshness.is_none() {
            news_opts.freshness = Some("day".to_string());
        }

        let response = self.query(query, count, &news_opts).await?;
        Ok(Self::merge_sections(response, true))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_web_and_news_results() {
        let response: YouComResponse = serde_json::from_str(
            r#"{
                "results": {
                    "web": [
                        {
                            "title": "Web Result",
                            "url": "https://example.com/web",
                            "description": "Web snippet",
                            "snippets": ["one", "two"],
                            "page_age": "2026-07-20T00:00:00"
                        }
                    ],
                    "news": [
                        {
                            "title": "News Result",
                            "url": "https://example.com/news",
                            "description": "News snippet"
                        }
                    ]
                }
            }"#,
        )
        .unwrap();

        let merged = YouCom::merge_sections(response, false);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].title, "Web Result");
        assert_eq!(merged[0].snippet, "one ... two");
        assert_eq!(merged[0].published.as_deref(), Some("2026-07-20T00:00:00"));
        assert_eq!(merged[1].source, "youcom_news");
    }

    #[test]
    fn prefers_news_for_news_mode() {
        let response: YouComResponse = serde_json::from_str(
            r#"{
                "results": {
                    "web": [
                        {
                            "title": "Web Result",
                            "url": "https://example.com/web"
                        }
                    ],
                    "news": [
                        {
                            "title": "News Result",
                            "url": "https://example.com/news"
                        }
                    ]
                }
            }"#,
        )
        .unwrap();

        let merged = YouCom::merge_sections(response, true);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].title, "News Result");
        assert_eq!(merged[0].source, "youcom_news");
    }
}
