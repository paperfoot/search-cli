pub mod brave;
pub mod browserless;
pub mod direct;
pub mod exa;
pub mod firecrawl;
pub mod jina;
pub mod linkup;
pub mod parallel;
pub mod perplexity;
pub mod serpapi;
pub mod serper;
pub mod spec;
#[cfg(feature = "stealth")]
pub mod stealth;
pub mod tavily;
pub mod xai;

pub use spec::{Adapter, Call, Output, ProviderSpec};

use crate::config::AppConfig;
use crate::context::AppContext;
use crate::errors::{classify_http, quota_text, SearchError};
use crate::types::FailureCategory;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Every provider the binary knows, in display order. The one provider list
/// in the crate: routing, config, doctor, usage and stats derive from it.
static ALL: &[&ProviderSpec] = &[
    &serper::SPEC,
    &exa::SPEC,
    &jina::SPEC,
    &brave::SPEC,
    &linkup::SPEC,
    &parallel::SPEC,
    &tavily::SPEC,
    &perplexity::SPEC,
    &serpapi::SPEC,
    &xai::SPEC,
    &firecrawl::SPEC,
    &browserless::SPEC,
    #[cfg(feature = "stealth")]
    &stealth::SPEC,
    &direct::SPEC,
];

pub fn all() -> &'static [&'static ProviderSpec] {
    ALL
}

pub fn find(name: &str) -> Option<&'static ProviderSpec> {
    ALL.iter()
        .copied()
        .find(|s| s.name.eq_ignore_ascii_case(name))
}

pub fn build_providers(ctx: &Arc<AppContext>) -> Vec<Box<dyn Adapter>> {
    vec![
        Box::new(serper::Serper::new(ctx.clone())),
        Box::new(exa::Exa::new(ctx.clone())),
        Box::new(jina::Jina::new(ctx.clone())),
        Box::new(brave::Brave::new(ctx.clone())),
        Box::new(linkup::Linkup::new(ctx.clone())),
        Box::new(parallel::Parallel::new(ctx.clone())),
        Box::new(tavily::Tavily::new(ctx.clone())),
        Box::new(perplexity::Perplexity::new(ctx.clone())),
        Box::new(serpapi::SerpApi::new(ctx.clone())),
        Box::new(xai::Xai::new(ctx.clone())),
        Box::new(firecrawl::Firecrawl::new(ctx.clone())),
        Box::new(browserless::Browserless::new(ctx.clone())),
        #[cfg(feature = "stealth")]
        Box::new(stealth::Stealth::new(ctx.clone())),
        Box::new(direct::Direct::new(ctx.clone())),
    ]
}

/// Resolve a provider API key. **Environment variable wins over the config
/// file** — CI/session credentials and a freshly-exported key must be able to
/// override stale local config. (Previously config won, so an out-of-date
/// `keys.brave` silently shadowed a working `BRAVE_API_KEY` and every brave
/// request failed.) Because figment already merges `SEARCH_KEYS_*` env over
/// TOML into `config_value`, the full precedence is:
/// bare env var (e.g. `BRAVE_API_KEY`) > `SEARCH_KEYS_*` env > config file.
pub fn resolve_key(config_value: &str, env_var: &str) -> String {
    if let Ok(v) = std::env::var(env_var) {
        if !v.is_empty() {
            return v;
        }
    }
    config_value.to_string()
}

/// The effective key for a provider (empty when unset or keyless).
pub fn key_for(config: &AppConfig, spec: &ProviderSpec) -> String {
    match (spec.env_keys.first(), config.keys.get(spec.name)) {
        (Some(env), Some(file)) => resolve_key(file, env),
        (Some(env), None) => resolve_key("", env),
        _ => String::new(),
    }
}

/// A failure that still cost a billed call: the request reached the
/// provider and was answered too late (timeout) or with a 2xx that did not
/// parse. Auth, quota, rate-limit, bad-request and server answers are not
/// billed; a connection error never left.
pub fn billed_failure(category: FailureCategory) -> bool {
    matches!(category, FailureCategory::Timeout | FailureCategory::Parse)
}

/// How many times a request may be re-sent, and the backoff between tries.
const MAX_RETRIES: u32 = 2;
const BACKOFF_MIN: Duration = Duration::from_millis(200);
const BACKOFF_MAX: Duration = Duration::from_millis(800);
/// Non-2xx bodies are read up to this size for classification.
const ERROR_BODY_CAP: usize = 64 * 1024;

/// Send one provider request with the shared retry policy and return the 2xx
/// response; every non-2xx becomes a classified [`SearchError::Upstream`].
///
/// `timeout` is the whole budget for this call (the engine passes
/// `min(spec.timeout, deadline left)`); each attempt gets what is left of it.
/// A request is re-sent only when it can't have been billed or the provider
/// asked for it:
/// - connection errors (the request never left);
/// - 429/503 carrying a `Retry-After` that fits in the budget, whose body is
///   not a quota message.
///
/// A read timeout is never re-sent: the provider got the request and bills it.
pub async fn send<F>(
    adapter: &dyn Adapter,
    timeout: Duration,
    build: F,
) -> Result<reqwest::Response, SearchError>
where
    F: Fn() -> reqwest::RequestBuilder,
{
    let provider = adapter.name();
    let deadline = Instant::now() + timeout;
    let mut attempt = 0u32;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(timeout_error(provider));
        }
        let resp = match build().timeout(left).send().await {
            Ok(resp) => resp,
            Err(e) => {
                if e.is_connect() && attempt < MAX_RETRIES {
                    let pause = backoff(attempt);
                    if pause < deadline.saturating_duration_since(Instant::now()) {
                        tokio::time::sleep(pause).await;
                        attempt += 1;
                        continue;
                    }
                }
                // Strip the URL: SerpApi authenticates with `?api_key=`.
                return Err(SearchError::Http(e.without_url()));
            }
        };
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }
        let code = status.as_u16();
        let retry_after = resp
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.trim().parse::<u64>().ok())
            .map(Duration::from_secs);
        let body = read_capped(resp, ERROR_BODY_CAP).await;
        let category = classify_http(adapter.classify(code, &body), code, &body);

        // A quota or auth body is classified as such, so it never retries;
        // a 5xx keeps its Server category, but quota text in it still means
        // a retry would only be refused again.
        let transient = matches!(code, 429 | 503)
            && matches!(
                category,
                FailureCategory::RateLimit | FailureCategory::Server
            )
            && !quota_text(&body);
        if transient && attempt < MAX_RETRIES {
            if let Some(wait) = retry_after {
                let pause = wait.max(backoff(attempt));
                // Leave room for the retried request itself.
                if pause + BACKOFF_MAX < deadline.saturating_duration_since(Instant::now()) {
                    tokio::time::sleep(pause).await;
                    attempt += 1;
                    continue;
                }
            }
        }
        return Err(SearchError::Upstream {
            provider,
            status: code,
            category,
            message: http_error_message(code, &body),
        });
    }
}

fn backoff(attempt: u32) -> Duration {
    (BACKOFF_MIN * 2u32.pow(attempt)).min(BACKOFF_MAX)
}

/// The call's time budget ran out before an answer arrived.
pub fn timeout_error(provider: &'static str) -> SearchError {
    SearchError::Upstream {
        provider,
        status: 408,
        category: FailureCategory::Timeout,
        message: "timed out before the provider answered".to_string(),
    }
}

async fn read_capped(mut resp: reqwest::Response, cap: usize) -> String {
    let mut buf: Vec<u8> = Vec::new();
    while buf.len() < cap {
        match resp.chunk().await {
            Ok(Some(chunk)) => buf.extend_from_slice(&chunk),
            _ => break,
        }
    }
    buf.truncate(cap);
    String::from_utf8_lossy(&buf).into_owned()
}

/// Format `HTTP <status>` with a sanitized one-line body excerpt when present.
pub fn http_error_message(status: u16, body: &str) -> String {
    let excerpt = sanitize_excerpt(body, 200);
    if excerpt.is_empty() {
        format!("HTTP {status}")
    } else {
        format!("HTTP {status}: {excerpt}")
    }
}

/// Options for calls that apply no filters (vertical endpoints).
pub static NO_FILTERS: crate::types::SearchOpts = crate::types::SearchOpts {
    include_domains: Vec::new(),
    exclude_domains: Vec::new(),
    freshness: None,
    country: None,
    lang: None,
};

/// Append `site:` / `-site:` operators for providers without native domain filters.
/// Google-style engines AND bare `site:` terms, so include domains are joined
/// with `OR` (as the server's `withSiteOperators` does); excludes stay `-site:`.
pub fn augment_query(query: &str, opts: &crate::types::SearchOpts) -> String {
    let mut q = String::with_capacity(
        query.len() + 16 * (opts.include_domains.len() + opts.exclude_domains.len()),
    );
    q.push_str(query);
    for (i, d) in opts.include_domains.iter().enumerate() {
        q.push_str(if i == 0 { " site:" } else { " OR site:" });
        q.push_str(d);
    }
    for d in &opts.exclude_domains {
        q.push_str(" -site:");
        q.push_str(d);
    }
    q
}

/// UTC date `days` ago as `YYYY-MM-DD`, without a date-crate dependency.
/// Used to translate the CLI's freshness words into provider date floors.
pub fn date_days_ago(days: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let total_days = now.saturating_sub(days * 86400) / 86400;
    // Howard Hinnant's civil-from-days algorithm.
    let z = total_days as i64 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

/// Days of lookback for a freshness word (day/week/month/year).
pub fn freshness_days(freshness: &str) -> Option<u64> {
    match freshness {
        "day" => Some(1),
        "week" => Some(7),
        "month" => Some(30),
        "year" => Some(365),
        _ => None,
    }
}

/// Collapse whitespace and clip to `max` chars (char-safe, never panics on UTF-8).
fn sanitize_excerpt(body: &str, max: usize) -> String {
    let collapsed = body.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out: String = collapsed.chars().take(max).collect();
    if collapsed.chars().count() > max {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn augment_query_ors_include_domains_and_negates_excludes() {
        let opts = crate::types::SearchOpts {
            include_domains: vec!["a.org".into(), "b.com".into(), "c.net".into()],
            exclude_domains: vec!["x.com".into(), "y.com".into()],
            ..crate::types::SearchOpts::default()
        };
        assert_eq!(
            augment_query("q", &opts),
            "q site:a.org OR site:b.com OR site:c.net -site:x.com -site:y.com"
        );
        let one = crate::types::SearchOpts {
            include_domains: vec!["a.org".into()],
            ..crate::types::SearchOpts::default()
        };
        assert_eq!(augment_query("q", &one), "q site:a.org");
        assert_eq!(augment_query("q", &NO_FILTERS), "q");
    }

    // Uses uniquely-named env vars so parallel tests don't race on shared state.
    #[test]
    fn env_var_wins_over_config() {
        let var = "SEARCH_TEST_KEY_ENV_WINS";
        std::env::set_var(var, "from-env");
        assert_eq!(resolve_key("from-config", var), "from-env");
        std::env::remove_var(var);
    }

    #[test]
    fn falls_back_to_config_when_env_unset() {
        let var = "SEARCH_TEST_KEY_FALLBACK";
        std::env::remove_var(var);
        assert_eq!(resolve_key("from-config", var), "from-config");
    }

    #[test]
    fn empty_env_does_not_shadow_config() {
        let var = "SEARCH_TEST_KEY_EMPTY";
        std::env::set_var(var, "");
        assert_eq!(resolve_key("from-config", var), "from-config");
        std::env::remove_var(var);
    }

    #[test]
    fn empty_when_neither_set() {
        let var = "SEARCH_TEST_KEY_NONE";
        std::env::remove_var(var);
        assert_eq!(resolve_key("", var), "");
    }

    #[test]
    fn one_spec_per_provider_and_every_adapter_listed() {
        let names: Vec<&str> = all().iter().map(|s| s.name).collect();
        let mut sorted = names.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len(), "duplicate provider spec");
        let ctx = Arc::new(AppContext::new(AppConfig::default()));
        let built: Vec<&str> = build_providers(&ctx).iter().map(|a| a.name()).collect();
        assert_eq!(built, names, "build_providers and all() disagree");
        for s in all() {
            assert!(!s.modes.is_empty(), "{} serves no mode", s.name);
            assert!(!s.modes.contains(&crate::types::Mode::Scrape));
            assert!(s.max_count >= 1);
            if s.env_keys.is_empty() {
                assert!(!s.billed_on_send, "{} is keyless but billed", s.name);
            }
        }
    }

    /// A stub adapter around a real spec, for driving `send` directly.
    struct Probe(&'static ProviderSpec);

    #[async_trait::async_trait]
    impl Adapter for Probe {
        fn spec(&self) -> &'static ProviderSpec {
            self.0
        }
        fn is_configured(&self) -> bool {
            true
        }
        async fn call(&self, _call: &Call<'_>) -> Result<Output, SearchError> {
            Ok(Output::default())
        }
    }

    /// A local server that counts connections and answers each with `reply`
    /// (or never answers when `reply` is None). Every reply closes the
    /// connection, so connections == requests.
    async fn server(reply: Option<&'static str>) -> (String, Arc<AtomicUsize>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                counter.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let _ = sock.read(&mut buf).await;
                    match reply {
                        Some(r) => {
                            let _ = sock.write_all(r.as_bytes()).await;
                            let _ = sock.shutdown().await;
                        }
                        // Hold the socket open and never answer.
                        None => tokio::time::sleep(Duration::from_secs(30)).await,
                    }
                });
            }
        });
        (format!("http://{addr}/"), hits)
    }

    fn client() -> reqwest::Client {
        reqwest::Client::builder().build().unwrap()
    }

    #[tokio::test]
    async fn billed_read_timeout_is_sent_exactly_once() {
        let (url, hits) = server(None).await;
        let probe = Probe(&exa::SPEC);
        assert!(probe.spec().billed_on_send);
        let c = client();
        let err = send(&probe, Duration::from_millis(600), || c.get(&url))
            .await
            .unwrap_err();
        assert_eq!(err.category(), FailureCategory::Timeout);
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "a billed timeout was re-sent"
        );
    }

    #[tokio::test]
    async fn quota_429_is_billing_and_not_retried() {
        let (url, hits) = server(Some(
            "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 0\r\nConnection: close\r\nContent-Length: 63\r\n\r\n{\"code\":\"INSUFFICIENT_FUNDS_CREDITS\",\"msg\":\"Remaining funds: 0\"}",
        ))
        .await;
        let probe = Probe(&linkup::SPEC);
        let c = client();
        let err = send(&probe, Duration::from_secs(5), || c.get(&url))
            .await
            .unwrap_err();
        assert_eq!(err.category(), FailureCategory::BillingQuota);
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn rate_limit_with_retry_after_is_retried_twice() {
        let (url, hits) = server(Some(
            "HTTP/1.1 503 Service Unavailable\r\nRetry-After: 0\r\nConnection: close\r\nContent-Length: 4\r\n\r\nbusy",
        ))
        .await;
        let probe = Probe(&serper::SPEC);
        let c = client();
        let err = send(&probe, Duration::from_secs(5), || c.get(&url))
            .await
            .unwrap_err();
        assert_eq!(err.category(), FailureCategory::Server);
        assert_eq!(hits.load(Ordering::SeqCst), 3);
    }

    /// End to end through `send` with the real Brave adapter: the free
    /// plan's per-second 429 is a rate limit (exit 4), not billing (exit 2).
    #[tokio::test]
    async fn brave_per_second_429_is_rate_limit_not_billing() {
        let (url, hits) = server(Some("HTTP/1.1 429 Too Many Requests\r\nConnection: close\r\nContent-Length: 227\r\n\r\n{\"type\":\"ErrorResponse\",\"error\":{\"id\":\"4083cd30\",\"status\":429,\"detail\":\"Request rate limit exceeded for plan\",\"meta\":{\"plan\":\"Free\",\"rate_limit\":1,\"rate_current\":1,\"quota_limit\":2000,\"quota_current\":342},\"code\":\"RATE_LIMITED\"}}")).await;
        let ctx = Arc::new(AppContext::new(AppConfig::default()));
        let adapter = brave::Brave::new(ctx);
        let c = client();
        let err = send(&adapter, Duration::from_secs(5), || c.get(&url))
            .await
            .unwrap_err();
        assert_eq!(err.category(), FailureCategory::RateLimit);
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        let failed = vec![err.to_provider_failure("brave")];
        assert_eq!(
            SearchError::AllProvidersFailed { failed, cost: None }.exit_code(),
            4
        );
    }

    #[tokio::test]
    async fn quota_503_is_not_retried() {
        let (url, hits) = server(Some(
            "HTTP/1.1 503 Service Unavailable\r\nRetry-After: 0\r\nConnection: close\r\nContent-Length: 25\r\n\r\nmonthly quota is used up.",
        ))
        .await;
        let probe = Probe(&serper::SPEC);
        let c = client();
        let err = send(&probe, Duration::from_secs(5), || c.get(&url))
            .await
            .unwrap_err();
        assert_eq!(err.category(), FailureCategory::Server);
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn rate_limit_without_retry_after_is_not_retried() {
        let (url, hits) = server(Some(
            "HTTP/1.1 429 Too Many Requests\r\nConnection: close\r\nContent-Length: 9\r\n\r\nslow down",
        ))
        .await;
        let probe = Probe(&serper::SPEC);
        let c = client();
        let err = send(&probe, Duration::from_secs(5), || c.get(&url))
            .await
            .unwrap_err();
        assert_eq!(err.category(), FailureCategory::RateLimit);
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }
}
