//! SerpApi: Google Scholar. The key travels in the URL (`?api_key=`), so no
//! error built here may carry the request URL: transport and decode errors are
//! stripped with `without_url()`, and failure reasons are redacted centrally.
//! Identical queries hit SerpApi's cache, which is free, so `no_cache` stays off.

use super::spec::{FilterSupport, Price};
use super::{Adapter, Call, Output, ProviderSpec};
use crate::context::AppContext;
use crate::errors::SearchError;
use crate::types::{FailureCategory, Mode, SearchResult};
use async_trait::async_trait;
use serde_json::{Map, Value};
use std::sync::Arc;
use std::time::Duration;

pub static SPEC: ProviderSpec = ProviderSpec {
    name: "serpapi",
    env_keys: &["SERPAPI_API_KEY"],
    modes: &[Mode::Scholar],
    // Scholar sends no filters today (the engine warns).
    filters: FilterSupport {
        freshness: false,
        domains: false,
        country: false,
        lang: false,
    },
    max_count: 20,
    timeout: Duration::from_secs(15),
    billed_on_send: true,
    price: Price {
        per_call: 25_000,
        per_extra_result: 0,
        included_results: 0,
    },
};

pub struct SerpApi {
    ctx: Arc<AppContext>,
}

impl SerpApi {
    pub fn new(ctx: Arc<AppContext>) -> Self {
        Self { ctx }
    }
}

/// Google Scholar honours `num` from 1 to 20 (SerpApi's google_scholar
/// reference); the plain Google engine ignores it since Sept 2025, but this
/// adapter never calls that engine.
fn request(client: &reqwest::Client, key: &str, call: &Call<'_>) -> reqwest::RequestBuilder {
    let num = call.count.clamp(1, SPEC.max_count).to_string();
    client.get("https://serpapi.com/search").query(&[
        ("engine", "google_scholar"),
        ("q", call.query),
        ("num", num.as_str()),
        ("api_key", key),
    ])
}

fn text<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn is_http(u: &str) -> bool {
    let l = u.get(..8).unwrap_or(u).to_ascii_lowercase();
    l.starts_with("https://") || l.starts_with("http://")
}

/// One scholar row, with the same `extra` keys as Serper's scholar results so
/// consumers read one shape: publicationInfo, citedBy, pdfUrl, year.
fn scholar(item: &Value) -> Option<SearchResult> {
    let link = text(item, "link").filter(|u| is_http(u))?;
    let summary = item
        .pointer("/publication_info/summary")
        .and_then(Value::as_str);
    let pdf = item
        .get("resources")
        .and_then(Value::as_array)
        .and_then(|rs| rs.iter().find(|r| text(r, "file_format") == Some("PDF")))
        .and_then(|r| text(r, "link"));
    let cited = item
        .pointer("/inline_links/cited_by/total")
        .and_then(Value::as_u64);
    let year = summary.and_then(year_in);

    let mut extra = Map::new();
    if let Some(s) = summary {
        extra.insert("publicationInfo".into(), s.into());
    }
    if let Some(c) = cited {
        extra.insert("citedBy".into(), c.into());
    }
    if let Some(p) = pdf {
        extra.insert("pdfUrl".into(), p.into());
    }
    if let Some(y) = year {
        extra.insert("year".into(), y.into());
    }
    Some(SearchResult {
        title: text(item, "title").unwrap_or_default().to_string(),
        url: link.to_string(),
        snippet: text(item, "snippet").unwrap_or_default().to_string(),
        source: "serpapi_scholar".to_string(),
        published: None,
        image_url: None,
        extra: (!extra.is_empty()).then_some(Value::Object(extra)),
    })
}

/// The first 19xx/20xx year standing alone in a publication summary.
fn year_in(summary: &str) -> Option<u64> {
    summary
        .split(|c: char| !c.is_ascii_digit())
        .find(|w| w.len() == 4 && (w.starts_with("19") || w.starts_with("20")))
        .and_then(|w| w.parse().ok())
}

fn quota(body: &str) -> Option<FailureCategory> {
    let b = body.to_ascii_lowercase();
    if b.contains("run out of searches")
        || b.contains("searches per hour")
        || b.contains("plan limit")
    {
        Some(FailureCategory::BillingQuota)
    } else if b.contains("invalid api key") {
        Some(FailureCategory::Auth)
    } else {
        None
    }
}

/// SerpApi reports some failures in a 200 body; "no results" is an empty
/// answer, not a failure.
fn parse(body: &Value) -> Result<Output, SearchError> {
    if let Some(err) = text(body, "error") {
        if !err.contains("hasn't returned any results") {
            return Err(SearchError::Upstream {
                provider: "serpapi",
                status: 200,
                category: quota(err).unwrap_or(FailureCategory::Other),
                message: super::http_error_message(200, err),
            });
        }
    }
    let results = body
        .get("organic_results")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(scholar).collect())
        .unwrap_or_default();
    Ok(Output {
        results,
        ..Output::default()
    })
}

#[async_trait]
impl Adapter for SerpApi {
    fn spec(&self) -> &'static ProviderSpec {
        &SPEC
    }

    fn keys(&self) -> Vec<String> {
        super::keys_for(&self.ctx.config, &SPEC)
    }

    fn is_configured(&self) -> bool {
        !self.keys().is_empty()
    }

    fn classify(&self, _status: u16, body: &str) -> Option<FailureCategory> {
        quota(body)
    }

    async fn call(&self, call: &Call<'_>) -> Result<Output, SearchError> {
        if !self.is_configured() {
            return Err(SearchError::AuthMissing {
                provider: "serpapi",
            });
        }
        let key = call.key;
        let client = &self.ctx.client;
        let resp = super::send(self, call.timeout, || request(client, key, call)).await?;
        // A decode error would print the URL, key included.
        let body: Value = resp
            .json()
            .await
            .map_err(|e| SearchError::Http(e.without_url()))?;
        parse(&body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;
    use crate::types::SearchOpts;

    #[test]
    fn scholar_request_sends_engine_query_and_capped_num() {
        let opts = SearchOpts {
            country: Some("gb".into()),
            ..SearchOpts::default()
        };
        let call = Call {
            mode: Mode::Scholar,
            query: "rank fusion",
            count: 50,
            opts: &opts,
            timeout: Duration::from_secs(1),
            key: "",
        };
        let req = request(&reqwest::Client::new(), "test-key", &call)
            .build()
            .unwrap();
        let pairs: Vec<(String, String)> = req.url().query_pairs().into_owned().collect();
        let names: Vec<&str> = pairs.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(names, ["engine", "q", "num", "api_key"]);
        assert_eq!(pairs[0].1, "google_scholar");
        assert_eq!(pairs[1].1, "rank fusion");
        assert_eq!(pairs[2].1, "20");
        assert_eq!(req.url().path(), "/search");
    }

    #[test]
    fn scholar_rows_keep_citations_pdf_and_year() {
        let body: Value =
            serde_json::from_str(include_str!("../../tests/fixtures/serpapi/scholar.json"))
                .unwrap();
        let out = parse(&body).unwrap();
        assert_eq!(out.results.len(), 1, "a citation with no link is dropped");
        let r = &out.results[0];
        assert_eq!(r.url, "https://dl.acm.org/doi/abs/10.1145/1571941.1572114");
        let extra = r.extra.as_ref().unwrap();
        assert_eq!(extra["citedBy"], 2201);
        assert_eq!(extra["year"], 2009);
        assert_eq!(
            extra["pdfUrl"],
            "https://dl.acm.org/doi/pdf/10.1145/1571941.1572114"
        );
        assert_eq!(out.cost_micros, None);
    }

    #[test]
    fn invalid_key_is_auth_and_200_errors_fail() {
        let s = SerpApi::new(Arc::new(AppContext::new(AppConfig::default())));
        let dead = include_str!("../../tests/fixtures/serpapi/invalid-key-401.json");
        assert_eq!(s.classify(401, dead), Some(FailureCategory::Auth));
        assert_eq!(
            s.classify(429, r#"{"error":"Your account has run out of searches."}"#),
            Some(FailureCategory::BillingQuota)
        );
        let err = parse(&serde_json::json!({"error": "Your account has run out of searches."}))
            .unwrap_err();
        assert_eq!(err.category(), FailureCategory::BillingQuota);
        let empty = parse(&serde_json::json!({
            "error": "Google hasn't returned any results for this query."
        }))
        .unwrap();
        assert!(empty.results.is_empty());
    }

    /// A decode error stripped of its URL cannot leak `api_key`.
    #[tokio::test]
    async fn errors_never_carry_the_key() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 2048];
            let _ = sock.read(&mut buf).await;
            let _ = sock
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\nnot")
                .await;
        });
        let resp = reqwest::Client::new()
            .get(format!("http://{addr}/search?api_key=SECRET123"))
            .send()
            .await
            .unwrap();
        let err = resp
            .json::<Value>()
            .await
            .map_err(|e| SearchError::Http(e.without_url()))
            .unwrap_err();
        assert!(!err.to_string().contains("SECRET123"), "{err}");
        assert_eq!(err.category(), FailureCategory::Parse);
    }
}
