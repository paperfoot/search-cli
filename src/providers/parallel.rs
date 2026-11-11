//! Parallel Search v1: `POST /v1/search`. `mode: "fast"` ($1 per 1,000) for
//! general and news, `"advanced"` ($5 per 1,000) for deep, plus $1 per 1,000
//! results above ten. v1 rejects unknown fields (`additionalProperties:
//! false`), so the body carries only documented ones; `v1beta` is legacy.
//! https://docs.parallel.ai/api-reference/search/search

use super::perplexity::host_in;
use super::spec::{FilterSupport, Price};
use super::{Adapter, Call, Output, ProviderSpec};
use crate::context::AppContext;
use crate::errors::SearchError;
use crate::types::{Mode, SearchResult};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

const ENDPOINT: &str = "https://api.parallel.ai/v1/search";
const EXCERPT_CHARS: u32 = 1_500;

pub static SPEC: ProviderSpec = ProviderSpec {
    name: "parallel",
    env_keys: &["PARALLEL_API_KEY"],
    modes: &[Mode::General, Mode::News, Mode::Deep],
    filters: FilterSupport {
        freshness: true,
        domains: true,
        country: true,
        lang: false,
    },
    max_count: 20,
    // Advanced (deep) searches take longer than fast ones; the mode deadline
    // still bounds general and news.
    timeout: Duration::from_secs(30),
    billed_on_send: true,
    price: FAST,
};

/// `mode: "fast"`: general and news.
const FAST: Price = Price {
    per_call: 1_000,
    per_extra_result: 1_000,
    included_results: 10,
};

/// `mode: "advanced"`: deep.
const ADVANCED: Price = Price {
    per_call: 5_000,
    per_extra_result: 1_000,
    included_results: 10,
};

fn price(mode: Mode) -> Price {
    if mode == Mode::Deep {
        ADVANCED
    } else {
        FAST
    }
}

pub struct Parallel {
    ctx: Arc<AppContext>,
}

impl Parallel {
    pub fn new(ctx: Arc<AppContext>) -> Self {
        Self { ctx }
    }
}

/// The v1 request: `objective`, `search_queries`, `mode` and
/// `advanced_settings`, nothing else.
fn body(call: &Call<'_>) -> Value {
    let opts = call.opts;
    // include wins upstream (v1 ignores exclude when include is set); the
    // exclude list is then applied to the results in `parse`.
    let mut source_policy = serde_json::Map::new();
    if !opts.include_domains.is_empty() {
        source_policy.insert("include_domains".into(), json!(opts.include_domains));
    } else if !opts.exclude_domains.is_empty() {
        source_policy.insert("exclude_domains".into(), json!(opts.exclude_domains));
    }
    if let Some(days) = opts.freshness.as_deref().and_then(super::freshness_days) {
        source_policy.insert("after_date".into(), json!(super::date_days_ago(days)));
    }
    let mut advanced = json!({
        "max_results": call.count,
        "excerpt_settings": { "max_chars_per_result": EXCERPT_CHARS },
    });
    if !source_policy.is_empty() {
        advanced["source_policy"] = Value::Object(source_policy);
    }
    if let Some(country) = &opts.country {
        advanced["location"] = json!(country.to_ascii_lowercase());
    }
    json!({
        "objective": call.query,
        "search_queries": [call.query],
        "mode": if call.mode == Mode::Deep { "advanced" } else { "fast" },
        "advanced_settings": advanced,
    })
}

#[derive(Deserialize)]
struct ParallelResponse {
    #[serde(default)]
    results: Vec<ParallelResult>,
}

#[derive(Deserialize)]
struct ParallelResult {
    url: Option<String>,
    title: Option<String>,
    publish_date: Option<String>,
    excerpts: Option<Vec<String>>,
}

/// Results with an http(s) URL, minus any host on the exclude list when the
/// include list took the source policy.
fn parse(data: ParallelResponse, call: &Call<'_>) -> Output {
    let opts = call.opts;
    let drop: &[String] = if opts.include_domains.is_empty() {
        &[]
    } else {
        &opts.exclude_domains
    };
    let results: Vec<SearchResult> = data
        .results
        .into_iter()
        .filter_map(|r| {
            let url = r.url.filter(|u| u.starts_with("http"))?;
            if host_in(&url, drop) {
                return None;
            }
            Some(SearchResult {
                title: r.title.unwrap_or_default(),
                url,
                snippet: r
                    .excerpts
                    .unwrap_or_default()
                    .join("\n\n")
                    .trim()
                    .to_string(),
                source: "parallel".to_string(),
                published: r.publish_date.filter(|d| !d.is_empty()),
                image_url: None,
                extra: None,
            })
        })
        .collect();
    // Billed on what was asked for: max_results sets the extra-result fee.
    let cost_micros = Some(price(call.mode).estimate(call.count));
    Output {
        results,
        answers: Vec::new(),
        cost_micros,
    }
}

#[async_trait]
impl Adapter for Parallel {
    fn spec(&self) -> &'static ProviderSpec {
        &SPEC
    }

    fn keys(&self) -> Vec<String> {
        super::keys_for(&self.ctx.config, &SPEC)
    }

    fn is_configured(&self) -> bool {
        !self.keys().is_empty()
    }

    fn sent_cost(&self, mode: Mode) -> u64 {
        price(mode).estimate(0)
    }

    async fn call(&self, call: &Call<'_>) -> Result<Output, SearchError> {
        if !self.is_configured() {
            return Err(SearchError::AuthMissing {
                provider: "parallel",
            });
        }
        let body = body(call);
        let client = &self.ctx.client;
        let api_key = call.key;
        let resp = super::send(self, call.timeout, || {
            client
                .post(ENDPOINT)
                .header("x-api-key", api_key)
                .json(&body)
        })
        .await?;
        let data: ParallelResponse = resp.json().await?;
        Ok(parse(data, call))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;
    use crate::types::SearchOpts;

    fn call<'a>(mode: Mode, count: u16, opts: &'a SearchOpts) -> Call<'a> {
        Call {
            mode,
            query: "rust async runtime",
            count,
            opts,
            timeout: Duration::from_secs(5),
            key: "",
        }
    }

    fn fixture() -> ParallelResponse {
        serde_json::from_str(include_str!("../../tests/fixtures/parallel/search_v1.json")).unwrap()
    }

    #[test]
    fn request_uses_the_v1_field_layout_only() {
        let opts = SearchOpts {
            exclude_domains: vec!["pinterest.com".into()],
            freshness: Some("week".into()),
            country: Some("GB".into()),
            ..SearchOpts::default()
        };
        let b = body(&call(Mode::General, 15, &opts));
        let mut top: Vec<&str> = b.as_object().unwrap().keys().map(|k| k.as_str()).collect();
        top.sort();
        assert_eq!(
            top,
            ["advanced_settings", "mode", "objective", "search_queries"]
        );
        assert_eq!(b["mode"], "fast");
        assert_eq!(b["search_queries"], json!(["rust async runtime"]));
        let a = &b["advanced_settings"];
        let mut keys: Vec<&str> = a.as_object().unwrap().keys().map(|k| k.as_str()).collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "excerpt_settings",
                "location",
                "max_results",
                "source_policy"
            ]
        );
        assert_eq!(a["max_results"], 15);
        assert_eq!(a["excerpt_settings"]["max_chars_per_result"], 1_500);
        assert_eq!(a["location"], "gb");
        assert_eq!(
            a["source_policy"]["exclude_domains"],
            json!(["pinterest.com"])
        );
        assert_eq!(
            a["source_policy"]["after_date"],
            json!(super::super::date_days_ago(7))
        );
    }

    #[test]
    fn deep_sends_advanced_and_include_takes_the_source_policy() {
        let opts = SearchOpts {
            include_domains: vec!["docs.rs".into()],
            exclude_domains: vec!["github.com".into()],
            ..SearchOpts::default()
        };
        let b = body(&call(Mode::Deep, 10, &opts));
        assert_eq!(b["mode"], "advanced");
        let sp = &b["advanced_settings"]["source_policy"];
        assert_eq!(sp["include_domains"], json!(["docs.rs"]));
        assert!(sp.get("exclude_domains").is_none());
        assert!(b["advanced_settings"].get("location").is_none());
    }

    #[test]
    fn parses_v1_results_with_dates_and_drops_bad_urls() {
        let opts = SearchOpts::default();
        let out = parse(fixture(), &call(Mode::General, 10, &opts));
        assert_eq!(out.results.len(), 2, "the non-http row is dropped");
        let r = &out.results[0];
        assert_eq!(r.url, "https://tokio.rs/blog/2026-10-tokio-1-48");
        assert_eq!(r.published.as_deref(), Some("2026-10-21"));
        assert!(r.snippet.contains("\n\n"), "excerpts are joined");
        assert_eq!(out.results[1].published, None);
    }

    #[test]
    fn exclude_list_is_applied_locally_when_include_wins_upstream() {
        let opts = SearchOpts {
            include_domains: vec!["tokio.rs".into(), "github.com".into()],
            exclude_domains: vec!["github.com".into()],
            ..SearchOpts::default()
        };
        let out = parse(fixture(), &call(Mode::General, 10, &opts));
        assert!(out.results.iter().all(|r| !r.url.contains("github.com")));
        assert_eq!(out.results.len(), 1);
    }

    #[test]
    fn cost_follows_mode_and_requested_count() {
        let opts = SearchOpts::default();
        let p = Parallel::new(Arc::new(AppContext::new(AppConfig::default())));
        assert_eq!(
            parse(fixture(), &call(Mode::General, 10, &opts)).cost_micros,
            Some(1_000)
        );
        assert_eq!(
            parse(fixture(), &call(Mode::News, 20, &opts)).cost_micros,
            Some(11_000)
        );
        assert_eq!(
            parse(fixture(), &call(Mode::Deep, 10, &opts)).cost_micros,
            Some(5_000)
        );
        assert_eq!(
            parse(fixture(), &call(Mode::Deep, 15, &opts)).cost_micros,
            Some(10_000)
        );
        assert_eq!(p.sent_cost(Mode::General), 1_000);
        assert_eq!(p.sent_cost(Mode::Deep), 5_000);
    }
}
