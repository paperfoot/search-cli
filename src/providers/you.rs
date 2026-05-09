use crate::context::AppContext;
use crate::errors::SearchError;
use crate::providers::augment_query;
use crate::types::{map_freshness, SearchOpts, SearchResult};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

pub struct You {
    ctx: Arc<AppContext>,
}

impl You {
    pub fn new(ctx: Arc<AppContext>) -> Self {
        Self { ctx }
    }

    fn api_key(&self) -> String {
        super::resolve_key(&self.ctx.config.keys.you, "YOU_API_KEY")
    }

    async fn do_search(
        &self,
        query: &str,
        count: usize,
        opts: &SearchOpts,
        include_news: bool,
    ) -> Result<Vec<SearchResult>, SearchError> {
        if self.api_key().is_empty() {
            return Err(SearchError::AuthMissing { provider: "you" });
        }

        let q = augment_query(query, opts);
        let mut req = self
            .ctx
            .client
            .get("https://ydc-index.io/v1/search")
            .header("X-API-Key", self.api_key())
            .query(&[
                ("query", q.as_str()),
                ("count", &count.to_string()),
                ("country", "US"),
                ("safesearch", "moderate"),
            ]);

        if let Some(f) = opts.freshness.as_deref().map(map_freshness) {
            req = req.query(&[("freshness", f)]);
        }

        // Live crawl — fetch full page content for LLM-ready results.
        // Defaults to "none" (no live crawl) when not requested.
        let livecrawl = opts
            .extra
            .as_ref()
            .and_then(|e| e.get("livecrawl"))
            .and_then(|v| v.as_str())
            .unwrap_or("none");
        if livecrawl != "none" {
            req = req.query(&[("livecrawl", livecrawl)]);
            if let Some(fmts) = opts.extra.as_ref().and_then(|e| e.get("livecrawl_formats")) {
                if let Some(arr) = fmts.as_array() {
                    // POST accepts JSON array; for GET, repeat the param.
                    for fmt in arr {
                        if let Some(s) = fmt.as_str() {
                            req = req.query(&[("livecrawl_formats", s)]);
                        }
                    }
                } else if let Some(s) = fmts.as_str() {
                    req = req.query(&[("livecrawl_formats", s)]);
                }
            }
            if let Some(timeout) = opts.extra.as_ref().and_then(|e| e.get("crawl_timeout")) {
                if let Some(n) = timeout.as_u64() {
                    req = req.query(&[("crawl_timeout", &n.to_string())]);
                } else if let Some(s) = timeout.as_str() {
                    req = req.query(&[("crawl_timeout", s)]);
                }
            }
        }

        let resp = super::retry_request(|| {
            let req = req.try_clone().ok_or_else(|| SearchError::Config("failed to clone request".into()));
            async move {
                let req = req?;
                let r = req.send().await?;
                if r.status() == 429 {
                    return Err(SearchError::RateLimited { provider: "you" });
                }
                if !r.status().is_success() {
                    return Err(SearchError::Api {
                        provider: "you",
                        code: "api_error",
                        message: format!("HTTP {}", r.status()),
                    });
                }
                Ok(r.json::<YouResponse>().await?)
            }
        })
        .await?;

        let mut out = Vec::new();
        let web = resp.results.as_ref().and_then(|r| r.web.as_ref());

        for hit in web.into_iter().flatten() {
            // Build snippet: combine description with the first snippet or join all.
            let snippet = if let Some(ref snippets) = hit.snippets {
                if let Some(first) = snippets.first() {
                    first.clone()
                } else {
                    String::new()
                }
            } else {
                hit.description.clone().unwrap_or_default()
            };

            out.push(SearchResult {
                title: hit.title.clone().unwrap_or_default(),
                url: hit.url.clone().unwrap_or_default(),
                snippet,
                source: "you".to_string(),
                published: None,
                image_url: hit.favicon_url.clone(),
                extra: if let Some(ref contents) = hit.contents {
                    Some(json!({
                        "contents": {
                            "markdown": contents.markdown,
                            "html": contents.html,
                        }
                    }))
                } else {
                    None
                },
            });
        }

        if include_news {
            let news = resp.results.as_ref().and_then(|r| r.news.as_ref());
            for item in news.into_iter().flatten() {
                let snippet = if let Some(ref snippets) = item.snippets {
                    if let Some(first) = snippets.first() {
                        first.clone()
                    } else {
                        String::new()
                    }
                } else {
                    item.description.clone().unwrap_or_default()
                };

                out.push(SearchResult {
                    title: item.title.clone().unwrap_or_default(),
                    url: item.url.clone().unwrap_or_default(),
                    snippet,
                    source: "you_news".to_string(),
                    published: item.age.clone(),
                    image_url: item.favicon_url.clone(),
                    extra: if let Some(ref contents) = item.contents {
                        Some(json!({
                            "contents": {
                                "markdown": contents.markdown,
                                "html": contents.html,
                            }
                        }))
                    } else {
                        None
                    },
                });
            }
        }

        Ok(out)
    }
}

// ── Response types matching actual You.com Search API JSON ──

#[derive(Deserialize)]
struct YouResponse {
    results: Option<YouResults>,
    // metadata is present but we don't need it for result extraction
}

#[derive(Deserialize)]
struct YouResults {
    web: Option<Vec<YouResultItem>>,
    news: Option<Vec<YouResultItem>>,
}

#[derive(Deserialize)]
struct YouResultItem {
    title: Option<String>,
    url: Option<String>,
    description: Option<String>,
    snippets: Option<Vec<String>>,
    favicon_url: Option<String>,
    /// Present when `livecrawl` is enabled. Contains full page content.
    contents: Option<YouContents>,
    /// Age string for news results (e.g. "2h", "1d")
    age: Option<String>,
}

#[derive(Deserialize)]
struct YouContents {
    markdown: Option<String>,
    html: Option<String>,
}

// ── Provider trait implementation ──

#[async_trait]
impl super::Provider for You {
    fn name(&self) -> &'static str {
        "you"
    }
    fn capabilities(&self) -> &[&'static str] {
        &["general", "news", "deep"]
    }
    fn env_keys(&self) -> &[&'static str] {
        &["YOU_API_KEY", "SEARCH_KEYS_YOU"]
    }
    fn is_configured(&self) -> bool {
        !self.api_key().is_empty()
    }

    async fn search(
        &self,
        query: &str,
        count: usize,
        opts: &SearchOpts,
    ) -> Result<Vec<SearchResult>, SearchError> {
        self.do_search(query, count, opts, false).await
    }

    async fn search_news(
        &self,
        query: &str,
        count: usize,
        opts: &SearchOpts,
    ) -> Result<Vec<SearchResult>, SearchError> {
        self.do_search(query, count, opts, true).await
    }
}

// ── Tests ──

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_you_response_deserialize_web_only() {
        let json = r#"{
            "results": {
                "web": [
                    {
                        "title": "Rust Language",
                        "url": "https://rust-lang.org",
                        "description": "A systems programming language",
                        "snippets": ["Rust is blazingly fast", "Memory-safe without GC"],
                        "favicon_url": "https://you.com/favicon?domain=rust-lang.org"
                    }
                ]
            }
        }"#;

        let resp: YouResponse = serde_json::from_str(json).unwrap();
        let results = resp.results.unwrap();
        let web = results.web.unwrap();
        assert_eq!(web.len(), 1);
        assert_eq!(web[0].title.as_deref(), Some("Rust Language"));
        assert_eq!(web[0].url.as_deref(), Some("https://rust-lang.org"));
        assert_eq!(
            web[0].description.as_deref(),
            Some("A systems programming language")
        );
        assert_eq!(web[0].snippets.as_ref().unwrap().len(), 2);
        assert!(results.news.is_none());
    }

    #[test]
    fn test_you_response_deserialize_news_only() {
        let json = r#"{
            "results": {
                "news": [
                    {
                        "title": "Breaking News",
                        "url": "https://news.example.com",
                        "description": "Something happened",
                        "snippets": ["Details emerging"],
                        "age": "2h"
                    }
                ]
            }
        }"#;

        let resp: YouResponse = serde_json::from_str(json).unwrap();
        let results = resp.results.unwrap();
        let news = results.news.unwrap();
        assert_eq!(news.len(), 1);
        assert_eq!(news[0].title.as_deref(), Some("Breaking News"));
        assert_eq!(news[0].age.as_deref(), Some("2h"));
        assert!(results.web.is_none());
    }

    #[test]
    fn test_you_response_deserialize_empty() {
        let json = r#"{}"#;
        let resp: YouResponse = serde_json::from_str(json).unwrap();
        assert!(resp.results.is_none());
    }

    #[test]
    fn test_you_response_deserialize_with_contents() {
        let json = r##"{
            "results": {
                "web": [
                    {
                        "title": "Page with full content",
                        "url": "https://example.com",
                        "description": "A page description",
                        "snippets": ["Snippet text"],
                        "favicon_url": "https://you.com/favicon?domain=example.com",
                        "contents": {
                            "markdown": "# Page Title\n\nFull page content in markdown.",
                            "html": "<h1>Page Title</h1><p>Full page content in HTML.</p>"
                        }
                    }
                ]
            }
        }"##;

        let resp: YouResponse = serde_json::from_str(json).unwrap();
        let results = resp.results.unwrap();
        let web = results.web.unwrap();
        let contents = web[0].contents.as_ref().unwrap();
        assert!(contents.markdown.is_some());
        assert!(contents.html.is_some());
    }
}