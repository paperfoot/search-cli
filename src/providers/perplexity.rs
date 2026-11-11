//! Perplexity Search API: `POST /search` returns ranked web results with page
//! text at a flat $5 per 1,000 requests. It replaces the Sonar chat call,
//! whose support ended on 27 Sep 2026; there is no synthetic answer any more.
//! The Search API has no academic option, so academic runs the same call.
//! `search_domain_filter` takes an allow list or a deny list, never both:
//! given both, the allow list goes upstream and the deny list is applied to
//! the results here. https://docs.perplexity.ai/api-reference/search-post

use super::spec::{FilterSupport, Price};
use super::{Adapter, Call, Output, ProviderSpec};
use crate::context::AppContext;
use crate::errors::SearchError;
use crate::types::{FailureCategory, Mode, SearchResult};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

const ENDPOINT: &str = "https://api.perplexity.ai/search";
/// The API caps `search_domain_filter` at 20 entries.
const MAX_DOMAINS: usize = 20;
const SNIPPET_MAX: usize = 2_000;

pub static SPEC: ProviderSpec = ProviderSpec {
    name: "perplexity",
    env_keys: &["PERPLEXITY_API_KEY"],
    modes: &[Mode::General, Mode::News, Mode::Academic, Mode::Deep],
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

pub struct Perplexity {
    ctx: Arc<AppContext>,
}

impl Perplexity {
    pub fn new(ctx: Arc<AppContext>) -> Self {
        Self { ctx }
    }
}

fn body(call: &Call<'_>) -> Value {
    let opts = call.opts;
    let mut b = json!({ "query": call.query, "max_results": call.count });
    if !opts.include_domains.is_empty() {
        let allow: Vec<&str> = opts
            .include_domains
            .iter()
            .take(MAX_DOMAINS)
            .map(String::as_str)
            .collect();
        b["search_domain_filter"] = json!(allow);
    } else if !opts.exclude_domains.is_empty() {
        let deny: Vec<String> = opts
            .exclude_domains
            .iter()
            .take(MAX_DOMAINS)
            .map(|d| format!("-{d}"))
            .collect();
        b["search_domain_filter"] = json!(deny);
    }
    // News has no vertical here: without -f it means the last week, as on
    // linkup and the hosted API.
    let recency = opts
        .freshness
        .as_deref()
        .or((call.mode == Mode::News).then_some("week"));
    if let Some(r) = recency {
        b["search_recency_filter"] = json!(r);
    }
    if let Some(country) = opts.country.as_deref().filter(|c| c.len() == 2) {
        b["country"] = json!(country.to_ascii_uppercase());
    }
    if let Some(lang) = opts.lang.as_deref().filter(|l| l.len() == 2) {
        b["search_language_filter"] = json!([lang.to_ascii_lowercase()]);
    }
    b
}

#[derive(Deserialize)]
struct PerplexityResponse {
    #[serde(default)]
    results: Vec<PerplexityResult>,
}

#[derive(Deserialize)]
struct PerplexityResult {
    title: Option<String>,
    url: Option<String>,
    snippet: Option<String>,
    date: Option<String>,
}

/// True when the URL's host is one of `domains` or a subdomain of one.
pub(super) fn host_in(url: &str, domains: &[String]) -> bool {
    if domains.is_empty() {
        return false;
    }
    let Some(host) = url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_ascii_lowercase))
    else {
        return false;
    };
    domains.iter().any(|d| {
        let d = d.trim().trim_start_matches("www.").to_ascii_lowercase();
        let h = host.trim_start_matches("www.");
        !d.is_empty() && (h == d || h.ends_with(&format!(".{d}")))
    })
}

fn clip(s: &str) -> String {
    let s = s.trim();
    if s.chars().count() <= SNIPPET_MAX {
        return s.to_string();
    }
    let mut out: String = s.chars().take(SNIPPET_MAX - 1).collect();
    out.push('…');
    out
}

fn parse(data: PerplexityResponse, call: &Call<'_>) -> Output {
    let opts = call.opts;
    // The whole deny list is applied here too: upstream gets at most 20
    // entries, and none when the allow list took the slot.
    let drop: &[String] = &opts.exclude_domains;
    let results = data
        .results
        .into_iter()
        .filter_map(|r| {
            let url = r.url.filter(|u| u.starts_with("http"))?;
            if host_in(&url, drop) {
                return None;
            }
            Some(SearchResult {
                title: r.title.unwrap_or_default(),
                snippet: clip(r.snippet.as_deref().unwrap_or_default()),
                url,
                source: "perplexity".to_string(),
                published: r.date.filter(|d| !d.is_empty()),
                image_url: None,
                extra: None,
            })
        })
        .collect();
    Output {
        results,
        answers: Vec::new(),
        cost_micros: Some(SPEC.price.estimate(0)),
    }
}

#[async_trait]
impl Adapter for Perplexity {
    fn spec(&self) -> &'static ProviderSpec {
        &SPEC
    }

    fn keys(&self) -> Vec<String> {
        super::keys_for(&self.ctx.config, &SPEC)
    }

    fn is_configured(&self) -> bool {
        !self.keys().is_empty()
    }

    /// An empty balance is a 401 "You exceeded your current quota", not a
    /// bad key.
    fn classify(&self, status: u16, body: &str) -> Option<FailureCategory> {
        (status == 401 && body.to_ascii_lowercase().contains("quota"))
            .then_some(FailureCategory::BillingQuota)
    }

    async fn call(&self, call: &Call<'_>) -> Result<Output, SearchError> {
        if !self.is_configured() {
            return Err(SearchError::AuthMissing {
                provider: "perplexity",
            });
        }
        let body = body(call);
        let client = &self.ctx.client;
        let key = call.key;
        let resp = super::send(self, call.timeout, || {
            client.post(ENDPOINT).bearer_auth(key).json(&body)
        })
        .await?;
        let data: PerplexityResponse = resp.json().await?;
        Ok(parse(data, call))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;
    use crate::errors::classify_http;
    use crate::types::SearchOpts;

    fn call<'a>(mode: Mode, opts: &'a SearchOpts) -> Call<'a> {
        Call {
            mode,
            query: "rust async runtime",
            count: 10,
            opts,
            timeout: Duration::from_secs(5),
            key: "",
        }
    }

    fn fixture() -> PerplexityResponse {
        serde_json::from_str(include_str!("../../tests/fixtures/perplexity/search.json")).unwrap()
    }

    fn adapter() -> Perplexity {
        Perplexity::new(Arc::new(AppContext::new(AppConfig::default())))
    }

    #[test]
    fn request_is_the_search_api_with_an_allow_list_only() {
        let opts = SearchOpts {
            include_domains: vec!["docs.rs".into()],
            exclude_domains: vec!["reddit.com".into()],
            country: Some("gb".into()),
            lang: Some("EN".into()),
            ..SearchOpts::default()
        };
        let b = body(&call(Mode::Academic, &opts));
        assert_eq!(b["query"], "rust async runtime");
        assert_eq!(b["max_results"], 10);
        assert_eq!(b["search_domain_filter"], json!(["docs.rs"]));
        assert_eq!(b["country"], "GB");
        assert_eq!(b["search_language_filter"], json!(["en"]));
        for gone in ["model", "messages", "search_mode", "search_recency_filter"] {
            assert!(b.get(gone).is_none(), "{gone} must not be sent");
        }
    }

    #[test]
    fn deny_list_is_prefixed_and_news_defaults_to_a_week() {
        let opts = SearchOpts {
            exclude_domains: vec!["reddit.com".into(), "quora.com".into()],
            ..SearchOpts::default()
        };
        let b = body(&call(Mode::News, &opts));
        assert_eq!(
            b["search_domain_filter"],
            json!(["-reddit.com", "-quora.com"])
        );
        assert_eq!(b["search_recency_filter"], "week");
        let opts = SearchOpts {
            freshness: Some("day".into()),
            ..SearchOpts::default()
        };
        assert_eq!(
            body(&call(Mode::News, &opts))["search_recency_filter"],
            "day"
        );
        assert!(body(&call(Mode::General, &SearchOpts::default()))
            .get("search_recency_filter")
            .is_none());
    }

    #[test]
    fn parses_results_with_snippets_and_dates_and_no_answer() {
        let opts = SearchOpts::default();
        let out = parse(fixture(), &call(Mode::General, &opts));
        assert!(out.answers.is_empty(), "the Search API has no answer");
        assert_eq!(out.results.len(), 3);
        let r = &out.results[0];
        assert_eq!(r.url, "https://tokio.rs/");
        assert_eq!(r.source, "perplexity");
        assert_eq!(r.published.as_deref(), Some("2026-10-21"));
        assert!(!r.snippet.is_empty());
        assert_eq!(out.results[2].published, None);
        assert_eq!(out.cost_micros, Some(5_000));
    }

    /// Upstream takes 20 deny entries; the 21st must still be filtered here.
    #[test]
    fn deny_entries_past_twenty_are_applied_locally() {
        let mut exclude: Vec<String> = (0..20).map(|i| format!("d{i}.com")).collect();
        exclude.push("reddit.com".into());
        let opts = SearchOpts {
            exclude_domains: exclude,
            ..SearchOpts::default()
        };
        let all = parse(fixture(), &call(Mode::General, &SearchOpts::default()));
        assert!(all.results.iter().any(|r| r.url.contains("reddit.com")));
        let out = parse(fixture(), &call(Mode::General, &opts));
        assert!(out.results.iter().all(|r| !r.url.contains("reddit.com")));
    }

    #[test]
    fn deny_list_is_applied_locally_when_the_allow_list_went_upstream() {
        let opts = SearchOpts {
            include_domains: vec!["tokio.rs".into(), "reddit.com".into()],
            exclude_domains: vec!["reddit.com".into()],
            ..SearchOpts::default()
        };
        let out = parse(fixture(), &call(Mode::General, &opts));
        assert!(out.results.iter().all(|r| !r.url.contains("reddit.com")));
        assert_eq!(out.results.len(), 2);
    }

    #[test]
    fn quota_401_is_billing_and_a_bad_key_stays_auth() {
        let body = include_str!("../../tests/fixtures/perplexity/insufficient_quota_401.json");
        let a = adapter();
        assert_eq!(a.classify(401, body), Some(FailureCategory::BillingQuota));
        assert_eq!(
            classify_http(a.classify(401, body), 401, body),
            FailureCategory::BillingQuota
        );
        let bad_key = r#"{"error":{"message":"Invalid API key provided","type":"invalid_api_key","code":401}}"#;
        assert_eq!(a.classify(401, bad_key), None);
        assert_eq!(
            classify_http(a.classify(401, bad_key), 401, bad_key),
            FailureCategory::Auth
        );
    }

    #[test]
    fn host_matching_covers_subdomains_only() {
        let d = vec!["reddit.com".to_string()];
        assert!(host_in("https://www.reddit.com/r/rust", &d));
        assert!(host_in("https://old.reddit.com/", &d));
        assert!(!host_in("https://notreddit.com/", &d));
        assert!(!host_in("not a url", &d));
    }
}
