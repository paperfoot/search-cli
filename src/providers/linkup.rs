//! Linkup: `POST /v1/search` with `outputType: "searchResults"`, the cheaper
//! of its output types ($0.005 standard, $0.05 deep; sourcedAnswer costs more
//! and is not used). News has no vertical: it means the last seven days
//! unless -f says otherwise.
//! https://docs.linkup.so/pages/documentation/development/pricing

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

const ENDPOINT: &str = "https://api.linkup.so/v1/search";
/// `depth: "deep"` with searchResults.
const DEEP_COST: u64 = 50_000;

pub static SPEC: ProviderSpec = ProviderSpec {
    name: "linkup",
    env_keys: &["LINKUP_API_KEY"],
    modes: &[Mode::General, Mode::News, Mode::Deep],
    filters: FilterSupport {
        freshness: true,
        domains: true,
        country: false,
        lang: false,
    },
    max_count: 20,
    // Deep searches run several rounds and take tens of seconds; a timeout
    // is still billed, so give them room. The mode deadline bounds the rest.
    timeout: Duration::from_secs(40),
    billed_on_send: true,
    price: Price {
        per_call: 5_000,
        per_extra_result: 0,
        included_results: 0,
    },
};

fn cost(mode: Mode) -> u64 {
    if mode == Mode::Deep {
        DEEP_COST
    } else {
        SPEC.price.estimate(0)
    }
}

pub struct Linkup {
    ctx: Arc<AppContext>,
}

impl Linkup {
    pub fn new(ctx: Arc<AppContext>) -> Self {
        Self { ctx }
    }
}

fn body(call: &Call<'_>) -> Value {
    let opts = call.opts;
    let mut b = json!({
        "q": call.query,
        "depth": if call.mode == Mode::Deep { "deep" } else { "standard" },
        "outputType": "searchResults",
        "maxResults": call.count,
    });
    if !opts.include_domains.is_empty() {
        b["includeDomains"] = json!(opts.include_domains);
    }
    if !opts.exclude_domains.is_empty() {
        b["excludeDomains"] = json!(opts.exclude_domains);
    }
    let days = opts
        .freshness
        .as_deref()
        .and_then(super::freshness_days)
        .or((call.mode == Mode::News).then_some(7));
    if let Some(days) = days {
        b["fromDate"] = json!(super::date_days_ago(days));
    }
    b
}

#[derive(Deserialize)]
struct LinkupResponse {
    #[serde(default)]
    results: Vec<LinkupResult>,
}

#[derive(Deserialize)]
struct LinkupResult {
    #[serde(rename = "type")]
    result_type: Option<String>,
    name: Option<String>,
    url: Option<String>,
    content: Option<String>,
}

fn parse(data: LinkupResponse, mode: Mode) -> Output {
    let results = data
        .results
        .into_iter()
        .filter(|r| r.result_type.as_deref() != Some("image"))
        .filter_map(|r| {
            Some(SearchResult {
                url: r.url.filter(|u| u.starts_with("http"))?,
                title: r.name.unwrap_or_default(),
                snippet: r.content.unwrap_or_default().trim().to_string(),
                source: "linkup".to_string(),
                published: None,
                image_url: None,
                extra: None,
            })
        })
        .collect();
    Output {
        results,
        answers: Vec::new(),
        cost_micros: Some(cost(mode)),
    }
}

#[async_trait]
impl Adapter for Linkup {
    fn spec(&self) -> &'static ProviderSpec {
        &SPEC
    }

    fn keys(&self) -> Vec<String> {
        super::keys_for(&self.ctx.config, &SPEC)
    }

    fn is_configured(&self) -> bool {
        !self.keys().is_empty()
    }

    /// An empty balance (429 `INSUFFICIENT_FUNDS_CREDITS`) and the key's own
    /// spending cap (429 `EXCEED_BUDGET_LIMIT`) are billing, not rate limits:
    /// never retried, and agents are never told to wait.
    fn classify(&self, status: u16, body: &str) -> Option<FailureCategory> {
        (status == 429
            && (body.contains("INSUFFICIENT_FUNDS") || body.contains("EXCEED_BUDGET_LIMIT")))
        .then_some(FailureCategory::BillingQuota)
    }

    fn sent_cost(&self, mode: Mode) -> u64 {
        cost(mode)
    }

    async fn call(&self, call: &Call<'_>) -> Result<Output, SearchError> {
        if !self.is_configured() {
            return Err(SearchError::AuthMissing { provider: "linkup" });
        }
        let body = body(call);
        let client = &self.ctx.client;
        let key = call.key;
        let resp = match super::send(self, call.timeout, || {
            client.post(ENDPOINT).bearer_auth(key).json(&body)
        })
        .await
        {
            Ok(resp) => resp,
            Err(e) => return no_result_is_empty(e),
        };
        let data: LinkupResponse = resp.json().await?;
        Ok(parse(data, call.mode))
    }
}

/// Linkup answers a query with nothing to find with 400
/// `SEARCH_QUERY_NO_RESULT` and does not bill it. That is an empty result,
/// not a malformed request (exit 0 `no_results`, not exit 1 `bad_request`).
fn no_result_is_empty(e: SearchError) -> Result<Output, SearchError> {
    match e {
        SearchError::Upstream {
            status: 400,
            ref message,
            ..
        } if message.contains("SEARCH_QUERY_NO_RESULT") => Ok(Output {
            cost_micros: Some(0),
            ..Output::default()
        }),
        e => Err(e),
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

    fn adapter() -> Linkup {
        Linkup::new(Arc::new(AppContext::new(AppConfig::default())))
    }

    #[test]
    fn no_result_answer_is_an_empty_unbilled_result() {
        let body = r#"{"statusCode":400,"error":{"code":"SEARCH_QUERY_NO_RESULT","message":"The query did not yield any result","details":[]}}"#;
        let upstream = |status| SearchError::Upstream {
            provider: "linkup",
            status,
            category: FailureCategory::BadRequest,
            message: super::super::http_error_message(status, body),
        };
        let out = no_result_is_empty(upstream(400)).expect("empty result, not an error");
        assert!(out.results.is_empty());
        assert_eq!(out.cost_micros, Some(0));
        let other = SearchError::Upstream {
            provider: "linkup",
            status: 400,
            category: FailureCategory::BadRequest,
            message: "HTTP 400: bad depth".into(),
        };
        assert!(no_result_is_empty(other).is_err());
    }

    #[test]
    fn request_pins_search_results_depth_and_filters() {
        let opts = SearchOpts {
            include_domains: vec!["tokio.rs".into()],
            exclude_domains: vec!["reddit.com".into()],
            freshness: Some("month".into()),
            ..SearchOpts::default()
        };
        let b = body(&call(Mode::General, &opts));
        assert_eq!(b["outputType"], "searchResults");
        assert_eq!(b["depth"], "standard");
        assert_eq!(b["maxResults"], 10);
        assert_eq!(b["includeDomains"], json!(["tokio.rs"]));
        assert_eq!(b["excludeDomains"], json!(["reddit.com"]));
        assert_eq!(b["fromDate"], json!(super::super::date_days_ago(30)));
        assert_eq!(body(&call(Mode::Deep, &opts))["depth"], "deep");
        let news = body(&call(Mode::News, &SearchOpts::default()));
        assert_eq!(news["fromDate"], json!(super::super::date_days_ago(7)));
        assert!(body(&call(Mode::General, &SearchOpts::default()))
            .get("fromDate")
            .is_none());
    }

    #[test]
    fn parses_text_results_and_prices_by_depth() {
        let data: LinkupResponse = serde_json::from_str(include_str!(
            "../../tests/fixtures/linkup/search_results.json"
        ))
        .unwrap();
        let out = parse(data, Mode::General);
        assert_eq!(out.results.len(), 2, "image and URL-less rows are dropped");
        assert_eq!(out.results[0].url, "https://tokio.rs/tokio/tutorial");
        assert_eq!(out.results[0].title, "Tutorial | Tokio");
        assert_eq!(out.cost_micros, Some(5_000));
        let data: LinkupResponse = serde_json::from_str(include_str!(
            "../../tests/fixtures/linkup/search_results.json"
        ))
        .unwrap();
        assert_eq!(parse(data, Mode::Deep).cost_micros, Some(50_000));
        let a = adapter();
        assert_eq!(a.sent_cost(Mode::General), 5_000);
        assert_eq!(a.sent_cost(Mode::Deep), 50_000);
    }

    #[test]
    fn out_of_funds_429_is_billing_quota() {
        let body = include_str!("../../tests/fixtures/linkup/insufficient_funds_429.json");
        let a = adapter();
        assert_eq!(a.classify(429, body), Some(FailureCategory::BillingQuota));
        assert_eq!(
            classify_http(a.classify(429, body), 429, body),
            FailureCategory::BillingQuota
        );
        // A plain throttle stays a rate limit.
        let throttle = r#"{"error":{"code":"TOO_MANY_REQUESTS","message":"Slow down"}}"#;
        assert_eq!(a.classify(429, throttle), None);
        assert_eq!(
            classify_http(a.classify(429, throttle), 429, throttle),
            FailureCategory::RateLimit
        );
    }

    /// The key's own spending cap (body from Linkup's official SDK tests,
    /// `LinkupBudgetLimitExceededError`) is billing too: waiting won't lift it.
    #[test]
    fn budget_limit_429_is_billing_quota() {
        let body = include_str!("../../tests/fixtures/linkup/budget_limit_429.json");
        let a = adapter();
        assert_eq!(a.classify(429, body), Some(FailureCategory::BillingQuota));
        assert_eq!(
            classify_http(a.classify(429, body), 429, body),
            FailureCategory::BillingQuota
        );
    }

    #[tokio::test]
    async fn out_of_funds_is_sent_once_and_exits_2() {
        billing_429_is_sent_once_and_exits_2(include_str!(
            "../../tests/fixtures/linkup/insufficient_funds_429.json"
        ))
        .await;
    }

    #[tokio::test]
    async fn budget_limit_is_sent_once_and_exits_2() {
        billing_429_is_sent_once_and_exits_2(include_str!(
            "../../tests/fixtures/linkup/budget_limit_429.json"
        ))
        .await;
    }

    /// End to end through the shared `send`: a billing 429 is tried once
    /// (no retry even with Retry-After), and as the only failure it exits 2.
    async fn billing_429_is_sent_once_and_exits_2(body: &'static str) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let reply: &'static str = Box::leak(
            format!(
                "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 0\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )
            .into_boxed_str(),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = hits.clone();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                let _ = sock.write_all(reply.as_bytes()).await;
                let _ = sock.shutdown().await;
            }
        });
        let a = adapter();
        let c = reqwest::Client::new();
        let err = super::super::send(&a, Duration::from_secs(5), || c.get(&url))
            .await
            .unwrap_err();
        assert_eq!(err.category(), FailureCategory::BillingQuota);
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
        let failed = vec![err.to_provider_failure("linkup")];
        assert_eq!(
            SearchError::AllProvidersFailed { failed, cost: None }.exit_code(),
            2
        );
    }
}
