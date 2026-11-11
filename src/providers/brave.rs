use super::spec::{FilterSupport, Price};
use super::{Adapter, Call, Output, ProviderSpec};
use crate::context::AppContext;
use crate::errors::SearchError;
use crate::types::{FailureCategory, Mode, SearchResult};
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
}

const BASE: &str = "https://api.search.brave.com/res/v1";

#[derive(Deserialize)]
struct BraveSearch {
    web: Option<BraveWeb>,
    /// News search answers with its results at the top level.
    results: Option<Vec<BraveItem>>,
}

#[derive(Deserialize)]
struct BraveWeb {
    results: Vec<BraveItem>,
}

#[derive(Deserialize)]
struct BraveItem {
    title: Option<String>,
    url: Option<String>,
    description: Option<String>,
    extra_snippets: Option<Vec<String>>,
    age: Option<String>,
    page_age: Option<String>,
}

#[derive(Deserialize)]
struct BraveContext {
    grounding: Option<Grounding>,
}

#[derive(Deserialize)]
struct Grounding {
    generic: Option<Vec<GroundingItem>>,
}

#[derive(Deserialize)]
struct GroundingItem {
    title: Option<String>,
    url: Option<String>,
    snippets: Option<Vec<String>>,
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

    fn keys(&self) -> Vec<String> {
        super::keys_for(&self.ctx.config, &SPEC)
    }

    fn is_configured(&self) -> bool {
        !self.keys().is_empty()
    }

    /// Brave names the cause in `error.code`. Its per-second 429 also lists
    /// the monthly quota in `meta`, so only the code tells a rate limit
    /// (retry shortly) from an exhausted plan (add credit). A dead key is a
    /// 422, which the status alone would call a bad request.
    fn classify(&self, _status: u16, body: &str) -> Option<FailureCategory> {
        let by_code = serde_json::from_str::<serde_json::Value>(body)
            .ok()
            .and_then(|v| match v.pointer("/error/code")?.as_str()? {
                "RATE_LIMITED" => Some(FailureCategory::RateLimit),
                "QUOTA_LIMITED" | "OPTION_NOT_IN_PLAN" => Some(FailureCategory::BillingQuota),
                "SUBSCRIPTION_TOKEN_INVALID" | "SUBSCRIPTION_NOT_FOUND" => {
                    Some(FailureCategory::Auth)
                }
                _ => None,
            });
        by_code.or_else(|| {
            let b = body.to_ascii_lowercase();
            (b.contains("token is invalid") || b.contains("subscription_token_invalid"))
                .then_some(FailureCategory::Auth)
        })
    }

    /// Deep sends web search and LLM Context together: two billed requests.
    fn sent_cost(&self, mode: Mode) -> u64 {
        let per_call = SPEC.price.per_call as u64;
        if mode == Mode::Deep {
            2 * per_call
        } else {
            per_call
        }
    }

    async fn call(&self, call: &Call<'_>) -> Result<Output, SearchError> {
        if !self.is_configured() {
            return Err(SearchError::AuthMissing { provider: "brave" });
        }
        let per_call = SPEC.price.per_call as u64;
        let key = call.key;
        let client = &self.ctx.client;
        match call.mode {
            Mode::News => {
                let body: BraveSearch = self.get(call, || news_request(client, key, call)).await?;
                Ok(Output {
                    results: news_results(body),
                    cost_micros: Some(per_call),
                    ..Output::default()
                })
            }
            // Deep = web search plus LLM Context, concurrently, inside one
            // provider call, so `-p` governs both and the cost is one line.
            Mode::Deep => {
                let (web, grounding) = tokio::join!(
                    self.get::<BraveSearch, _>(call, || web_request(client, key, call)),
                    self.get::<BraveContext, _>(call, || context_request(client, key, call)),
                );
                // Each half that reached Brave is billed, answered or not.
                let billed = billed_half(&web) + billed_half(&grounding);
                let results = match (web.map(web_results), grounding.map(context_results)) {
                    (Ok(w), Ok(g)) => merge(w, g),
                    (Ok(w), Err(e)) => {
                        let e = crate::errors::redact_secrets(&e.to_string());
                        tracing::warn!("brave llm context: {e}");
                        w
                    }
                    (Err(e), Ok(g)) => {
                        let e = crate::errors::redact_secrets(&e.to_string());
                        tracing::warn!("brave web: {e}");
                        g
                    }
                    (Err(w), Err(g)) => return Err(both_failed(w, g)),
                };
                Ok(Output {
                    results,
                    answers: Vec::new(),
                    cost_micros: Some(per_call * billed),
                })
            }
            _ => {
                let body: BraveSearch = self.get(call, || web_request(client, key, call)).await?;
                Ok(Output {
                    results: web_results(body),
                    cost_micros: Some(per_call),
                    ..Output::default()
                })
            }
        }
    }
}

impl Brave {
    async fn get<T, F>(&self, call: &Call<'_>, build: F) -> Result<T, SearchError>
    where
        T: serde::de::DeserializeOwned,
        F: Fn() -> reqwest::RequestBuilder,
    {
        let resp = super::send(self, call.timeout, build).await?;
        Ok(resp.json().await?)
    }
}

/// Both deep halves failed: report a billed half's error when there is one,
/// so the engine books the spend instead of pricing the call at 0.
fn both_failed(web: SearchError, grounding: SearchError) -> SearchError {
    if !super::billed_failure(web.category()) && super::billed_failure(grounding.category()) {
        grounding
    } else {
        web
    }
}

/// 1 when one half of a deep call was billed: it answered, or it reached
/// Brave and timed out or came back unparseable.
fn billed_half<T>(half: &Result<T, SearchError>) -> u64 {
    match half {
        Ok(_) => 1,
        Err(e) => super::billed_failure(e.category()) as u64,
    }
}

/// Web results keep their rank; grounding text for a URL web search already
/// listed rides along in `extra.context`, other grounding URLs follow.
fn merge(web: Vec<SearchResult>, grounding: Vec<SearchResult>) -> Vec<SearchResult> {
    let key = |u: &str| u.trim_end_matches('/').to_string();
    let mut out = web;
    let mut index: std::collections::HashMap<String, usize> = out
        .iter()
        .enumerate()
        .map(|(i, r)| (key(&r.url), i))
        .collect();
    for g in grounding {
        match index.get(&key(&g.url)) {
            Some(&i) => {
                if !g.snippet.is_empty() {
                    out[i].extra = Some(serde_json::json!({ "context": g.snippet }));
                }
            }
            None => {
                index.insert(key(&g.url), out.len());
                out.push(g);
            }
        }
    }
    out
}

fn base(
    client: &reqwest::Client,
    key: &str,
    path: &str,
    call: &Call<'_>,
) -> reqwest::RequestBuilder {
    let opts = call.opts;
    let mut req = client
        .get(format!("{BASE}/{path}"))
        .header("X-Subscription-Token", key)
        .header("Accept", "application/json")
        .query(&[("q", super::augment_query(call.query, opts))]);
    if let Some(f) = opts.freshness.as_deref().map(map_freshness) {
        req = req.query(&[("freshness", f)]);
    }
    if let Some(country) = &opts.country {
        req = req.query(&[("country", country.to_uppercase())]);
    }
    if let Some(lang) = &opts.lang {
        req = req.query(&[("search_lang", lang.to_lowercase())]);
    }
    req
}

fn count(call: &Call<'_>) -> String {
    call.count.clamp(1, SPEC.max_count).to_string()
}

fn web_request(client: &reqwest::Client, key: &str, call: &Call<'_>) -> reqwest::RequestBuilder {
    base(client, key, "web/search", call)
        .query(&[("count", count(call).as_str()), ("extra_snippets", "true")])
}

fn news_request(client: &reqwest::Client, key: &str, call: &Call<'_>) -> reqwest::RequestBuilder {
    base(client, key, "news/search", call).query(&[("count", count(call))])
}

/// LLM Context is sized by tokens; Brave advises leaving `count` unset.
fn context_request(
    client: &reqwest::Client,
    key: &str,
    call: &Call<'_>,
) -> reqwest::RequestBuilder {
    base(client, key, "llm/context", call).query(&[
        ("maximum_number_of_tokens", "16384"),
        ("context_threshold_mode", "balanced"),
    ])
}

fn is_http(u: &str) -> bool {
    let l = u.get(..8).unwrap_or(u).to_ascii_lowercase();
    l.starts_with("https://") || l.starts_with("http://")
}

/// Brave marks query terms with `<strong>` and escapes entities.
fn plain(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.replace("&quot;", "\"")
        .replace("&#x27;", "'")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

fn item_result(r: BraveItem, source: &str, with_extras: bool) -> Option<SearchResult> {
    let url = r.url.filter(|u| is_http(u))?;
    let mut parts = vec![plain(r.description.as_deref().unwrap_or_default())];
    if with_extras {
        parts.extend(
            r.extra_snippets
                .unwrap_or_default()
                .iter()
                .map(|s| plain(s)),
        );
    }
    parts.retain(|p| !p.is_empty());
    Some(SearchResult {
        title: plain(r.title.as_deref().unwrap_or_default()),
        url,
        snippet: parts.join("\n"),
        source: source.to_string(),
        published: r.page_age.or(r.age),
        image_url: None,
        extra: None,
    })
}

fn web_results(body: BraveSearch) -> Vec<SearchResult> {
    body.web
        .map(|w| w.results)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|r| item_result(r, "brave", true))
        .collect()
}

fn news_results(body: BraveSearch) -> Vec<SearchResult> {
    body.results
        .unwrap_or_default()
        .into_iter()
        .filter_map(|r| item_result(r, "brave_news", false))
        .collect()
}

fn context_results(body: BraveContext) -> Vec<SearchResult> {
    body.grounding
        .and_then(|g| g.generic)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|g| {
            let url = g.url.filter(|u| is_http(u))?;
            Some(SearchResult {
                title: plain(g.title.as_deref().unwrap_or_default()),
                url,
                snippet: g.snippets.unwrap_or_default().join("\n"),
                source: "brave_llm_context".to_string(),
                published: None,
                image_url: None,
                extra: None,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;
    use crate::types::SearchOpts;

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

    /// Deep sends two requests: a timed-out or unparseable half was billed,
    /// a rate-limited one was not.
    #[test]
    fn deep_prices_every_half_that_was_sent() {
        let b = brave();
        assert_eq!(b.sent_cost(Mode::Deep), 10_000);
        assert_eq!(b.sent_cost(Mode::General), 5_000);
        let ok: Result<(), SearchError> = Ok(());
        let timed_out: Result<(), SearchError> = Err(super::super::timeout_error("brave"));
        let parse: Result<(), SearchError> =
            Err(serde_json::from_str::<u8>("x").unwrap_err().into());
        let limited: Result<(), SearchError> = Err(SearchError::Upstream {
            provider: "brave",
            status: 429,
            category: FailureCategory::RateLimit,
            message: "m".into(),
        });
        assert_eq!(billed_half(&ok) + billed_half(&timed_out), 2);
        assert_eq!(billed_half(&parse), 1);
        assert_eq!(billed_half(&limited), 0);
    }

    /// A 429 on the web half and a timeout on LLM Context: Brave billed the
    /// timed-out half, so the error the engine prices must be the billed one.
    #[test]
    fn both_halves_failing_report_the_billed_one() {
        let limited = || SearchError::Upstream {
            provider: "brave",
            status: 429,
            category: FailureCategory::RateLimit,
            message: "slow down".into(),
        };
        let timed_out = || super::super::timeout_error("brave");
        let e = both_failed(limited(), timed_out());
        assert!(super::super::billed_failure(e.category()));
        let e = both_failed(timed_out(), limited());
        assert!(super::super::billed_failure(e.category()));
    }

    fn query(req: &reqwest::Request) -> Vec<(String, String)> {
        req.url().query_pairs().into_owned().collect()
    }

    fn call_with<'a>(mode: Mode, count: u16, opts: &'a SearchOpts) -> Call<'a> {
        Call {
            mode,
            query: "rank fusion",
            count,
            opts,
            timeout: Duration::from_secs(1),
            key: "",
        }
    }

    #[test]
    fn requests_cap_count_and_llm_context_sends_none() {
        let opts = SearchOpts {
            freshness: Some("week".into()),
            country: Some("gb".into()),
            include_domains: vec!["example.com".into()],
            ..SearchOpts::default()
        };
        let c = reqwest::Client::new();
        let call = call_with(Mode::Deep, 50, &opts);

        let web = web_request(&c, "test-key", &call).build().unwrap();
        assert_eq!(web.url().path(), "/res/v1/web/search");
        assert!(web.headers().contains_key("x-subscription-token"));
        let q = query(&web);
        assert!(q.contains(&("q".into(), "rank fusion site:example.com".into())));
        assert!(q.contains(&("count".into(), "20".into())));
        assert!(q.contains(&("extra_snippets".into(), "true".into())));
        assert!(q.contains(&("freshness".into(), "pw".into())));
        assert!(q.contains(&("country".into(), "GB".into())));

        let ctx = context_request(&c, "test-key", &call).build().unwrap();
        assert_eq!(ctx.url().path(), "/res/v1/llm/context");
        let q = query(&ctx);
        assert!(q.iter().all(|(k, _)| k != "count"), "{q:?}");
        assert!(q.contains(&("maximum_number_of_tokens".into(), "16384".into())));
        assert!(q.contains(&("context_threshold_mode".into(), "balanced".into())));

        let news = news_request(&c, "test-key", &call_with(Mode::News, 5, &opts))
            .build()
            .unwrap();
        assert_eq!(news.url().path(), "/res/v1/news/search");
        assert!(query(&news).contains(&("count".into(), "5".into())));
    }

    #[test]
    fn web_and_news_parse_plain_text_and_dates() {
        let web: BraveSearch =
            serde_json::from_str(include_str!("../../tests/fixtures/brave/web.json")).unwrap();
        let web = web_results(web);
        assert_eq!(web.len(), 2, "a javascript: URL is dropped");
        assert_eq!(
            web[0].title,
            "Reciprocal rank fusion | Elasticsearch Reference"
        );
        assert_eq!(
            web[0].snippet,
            "Reciprocal rank fusion (RRF) is a method for combining multiple result sets.\nRRF requires no tuning.\nThe constant k defaults to 60."
        );
        assert_eq!(web[0].published.as_deref(), Some("2026-09-14T08:12:00"));
        assert_eq!(web[1].snippet, "Cormack, Clarke & Buettcher, SIGIR 2009.");

        // News results sit at the top level, not under `news`.
        let news: BraveSearch =
            serde_json::from_str(include_str!("../../tests/fixtures/brave/news.json")).unwrap();
        let news = news_results(news);
        assert_eq!(news.len(), 1);
        assert_eq!(news[0].source, "brave_news");
        assert_eq!(
            news[0].snippet,
            "The new endpoint returns grounding snippets for AI agents."
        );
        assert_eq!(news[0].published.as_deref(), Some("2026-11-02T09:00:00"));
    }

    #[test]
    fn deep_merges_grounding_into_web_results() {
        let web: BraveSearch =
            serde_json::from_str(include_str!("../../tests/fixtures/brave/web.json")).unwrap();
        let ctx: BraveContext =
            serde_json::from_str(include_str!("../../tests/fixtures/brave/llm-context.json"))
                .unwrap();
        let out = merge(web_results(web), context_results(ctx));
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].source, "brave");
        assert_eq!(
            out[0].extra.as_ref().unwrap()["context"],
            "RRF score = sum of 1 / (k + rank).\nk is 60 by default."
        );
        assert_eq!(out[2].url, "https://en.wikipedia.org/wiki/Rank_fusion");
        assert_eq!(out[2].source, "brave_llm_context");
    }

    #[test]
    fn dead_key_fixture_is_auth() {
        let b = brave();
        let body = include_str!("../../tests/fixtures/brave/auth-422.json");
        assert_eq!(b.classify(422, body), Some(FailureCategory::Auth));
        assert_eq!(
            b.classify(422, "The provided subscription token is invalid."),
            Some(FailureCategory::Auth)
        );
        let limited = include_str!("../../tests/fixtures/brave/rate-limited-429.json");
        assert_eq!(b.classify(429, limited), Some(FailureCategory::RateLimit));
        assert_eq!(
            crate::errors::classify_http(b.classify(422, body), 422, body),
            FailureCategory::Auth
        );
    }
}
