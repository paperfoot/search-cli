pub mod brave;
pub mod browserless;
pub mod direct;
pub mod exa;
pub mod facebook;
pub mod firecrawl;
pub mod hackernews;
pub mod instagram;
pub mod jina;
pub mod linkup;
pub mod parallel;
pub mod perplexity;
pub mod reddit;
pub mod serpapi;
pub mod serper;
pub mod social_api;
pub mod spec;
#[cfg(feature = "stealth")]
pub mod stealth;
pub mod tavily;
pub mod thread;
pub mod threads;
pub mod tiktok;
pub mod xai;
pub mod youtube;

pub use spec::{Adapter, Call, FilterSupport, Output, ProviderSpec};

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
    &reddit::SPEC,
    &hackernews::SPEC,
    &tiktok::SPEC,
    &instagram::SPEC,
    &youtube::SPEC,
    &facebook::SPEC,
    &threads::SPEC,
    &firecrawl::SPEC,
    &browserless::SPEC,
    #[cfg(feature = "stealth")]
    &stealth::SPEC,
    &direct::SPEC,
];

/// How many search engines the binary has: providers serving any mode but
/// extract and community, other than the social platforms. The README, both SKILL.md copies, the plugin and
/// marketplace entries, the crate description and `--help` say it in words
/// ("ten"); a test fails when either drifts.
pub const SEARCH_ENGINES: usize = 10;

/// The community sources (`community` mode): Reddit and Hacker News. Counted
/// apart from the search engines; the public text names them.
pub const COMMUNITY_SOURCES: &[&str] = &["reddit", "hackernews"];

/// The social platforms (`social` mode, beside X): TikTok, Instagram,
/// YouTube, Facebook and Threads. Counted apart from the search engines;
/// they run through metasearch.sh.
pub const SOCIAL_PLATFORMS: &[&str] = social_api::PLATFORMS;

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
        Box::new(reddit::Reddit::new(ctx.clone())),
        Box::new(hackernews::HackerNews::new(ctx.clone())),
        Box::new(tiktok::TikTok::new(ctx.clone())),
        Box::new(instagram::Instagram::new(ctx.clone())),
        Box::new(youtube::YouTube::new(ctx.clone())),
        Box::new(facebook::Facebook::new(ctx.clone())),
        Box::new(threads::Threads::new(ctx.clone())),
        Box::new(firecrawl::Firecrawl::new(ctx.clone())),
        Box::new(browserless::Browserless::new(ctx.clone())),
        #[cfg(feature = "stealth")]
        Box::new(stealth::Stealth::new(ctx.clone())),
        Box::new(direct::Direct::new(ctx.clone())),
    ]
}

/// A provider's keys in the order they are tried, empty when unset or
/// keyless. **The environment wins over the config file**: CI/session
/// credentials and a freshly-exported key must be able to override stale
/// local config (a stale `keys.brave` once shadowed a working
/// `BRAVE_API_KEY`). It replaces the file's keys as a whole:
/// `<PROVIDER>_API_KEY`, then `<PROVIDER>_API_KEY_2`, `_3`, … in number
/// order; when none of those is set, `keys.<provider>` (a string or a list,
/// from the file or `SEARCH_KEYS_<PROVIDER>`).
pub fn keys_for(config: &AppConfig, spec: &ProviderSpec) -> Vec<String> {
    let Some(env) = spec.env_keys.first() else {
        return Vec::new();
    };
    let from_env = env_keys(env);
    if !from_env.is_empty() {
        return from_env;
    }
    config
        .keys
        .get(spec.name)
        .map(<[String]>::to_vec)
        .unwrap_or_default()
}

/// Every key of every provider, fallbacks included: the values redaction
/// scrubs from any text the CLI prints.
pub fn every_key(config: &AppConfig) -> Vec<String> {
    ALL.iter().flat_map(|s| keys_for(config, s)).collect()
}

/// `var`, then `var_2`, `var_3`, … in number order. The numbered names leave
/// `var` itself one key, as the provider's own SDKs read it.
fn env_keys(var: &str) -> Vec<String> {
    let mut numbered: Vec<(u32, String)> = std::env::vars_os()
        .filter_map(|(k, v)| {
            let n = k.to_str()?.strip_prefix(var)?.strip_prefix('_')?;
            // `_2`, `_3`, …: digits, no leading zero, so no two names share a place.
            if n.is_empty() || n.starts_with('0') || !n.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            Some((n.parse().ok()?, v.into_string().ok()?))
        })
        .collect();
    numbered.sort_by_key(|(n, _)| *n);
    let first = std::env::var(var).ok();
    crate::config::clean_keys(
        first
            .into_iter()
            .chain(numbered.into_iter().map(|(_, v)| v)),
    )
}

/// A failure that the next key may not hit: the provider refused this key
/// (auth), its account is out of credit or quota, or it is rate limited.
/// Keys are separate accounts, so each has its own credit and rate limit.
/// Timeouts, server errors, bad requests and parse failures would fail the
/// same way with any key, and a timeout or parse failure was already billed.
pub fn next_key_may_help(category: FailureCategory) -> bool {
    matches!(
        category,
        FailureCategory::Auth | FailureCategory::BillingQuota | FailureCategory::RateLimit
    )
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
    fn search_engine_count_matches_the_provider_list() {
        use crate::types::Mode;
        let engines = ALL
            .iter()
            .filter(|s| !SOCIAL_PLATFORMS.contains(&s.name))
            .filter(|s| {
                s.modes
                    .iter()
                    .any(|m| !matches!(m, Mode::Extract | Mode::Community))
            })
            .count();
        assert_eq!(engines, SEARCH_ENGINES);
        let social: Vec<&str> = ALL
            .iter()
            .filter(|s| s.modes.contains(&Mode::Social) && s.name != "xai")
            .map(|s| s.name)
            .collect();
        assert_eq!(social, SOCIAL_PLATFORMS);
        let community: Vec<&str> = ALL
            .iter()
            .filter(|s| s.modes.contains(&Mode::Community))
            .map(|s| s.name)
            .collect();
        assert_eq!(community, COMMUNITY_SOURCES);
    }

    /// Every "<number> engines" / "<number> search engines" phrase in the
    /// public text states [`SEARCH_ENGINES`], so the count cannot drift
    /// between copies again.
    #[test]
    fn every_public_engine_count_is_the_canonical_one() {
        const SEARCH_ENGINES_WORD: &str = "ten";
        const NUMBERS: &[&str] = &[
            "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten", "eleven",
            "twelve", "thirteen", "fourteen", "fifteen",
        ];
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let files = [
            "README.md",
            "SKILL.md",
            "Cargo.toml",
            "src/cli.rs",
            ".claude-plugin/marketplace.json",
            "plugins/search-cli/.claude-plugin/plugin.json",
            "plugins/search-cli/skills/search/SKILL.md",
            "plugins/metasearch/.claude-plugin/plugin.json",
        ];
        let digits = SEARCH_ENGINES.to_string();
        let mut seen = 0;
        for f in files {
            let text = std::fs::read_to_string(root.join(f))
                .unwrap()
                .to_lowercase();
            let words: Vec<String> = text
                .split(|c: char| !c.is_alphanumeric())
                .filter(|w| !w.is_empty())
                .map(str::to_string)
                .collect();
            for i in 0..words.len() {
                if words[i] != "engines" && words[i] != "providers" {
                    continue;
                }
                let j = if i >= 2 && words[i - 1] == "search" {
                    i - 2
                } else {
                    i.wrapping_sub(1)
                };
                let Some(n) = words.get(j) else { continue };
                let is_number = NUMBERS.contains(&n.as_str()) || n.parse::<u32>().is_ok();
                if !is_number {
                    continue;
                }
                assert!(
                    n == SEARCH_ENGINES_WORD || *n == digits,
                    "{f}: \"{n} {}\" should say {SEARCH_ENGINES_WORD}",
                    words[i]
                );
                seen += 1;
            }
        }
        assert!(
            seen >= 6,
            "the count should appear in each description ({seen})"
        );
    }

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

    /// `<VAR>`, then `<VAR>_2`, `_3`, … in number order (not name order),
    /// skipping empty values, repeats and names that are not numbered.
    #[test]
    fn numbered_env_keys_follow_the_bare_one_in_number_order() {
        let var = "SEARCH_TEST_ORDER_API_KEY";
        let vars = [
            (var.to_string(), "first"),
            (format!("{var}_10"), "fourth"),
            (format!("{var}_2"), "second"),
            (format!("{var}_3"), "third"),
            (format!("{var}_4"), ""),
            (format!("{var}_5"), "second"),
            (format!("{var}_X"), "not a key"),
            (format!("{var}_+6"), "not a key"),
            (format!("{var}_07"), "not a key"),
        ];
        for (k, v) in &vars {
            std::env::set_var(k, v);
        }
        assert_eq!(env_keys(var), ["first", "second", "third", "fourth"]);
        // Numbered keys work without the bare one.
        std::env::remove_var(var);
        assert_eq!(env_keys(var), ["second", "third", "fourth"]);
        for (k, _) in &vars {
            std::env::remove_var(k);
        }
    }

    /// The environment replaces the file's keys as a whole, as it always
    /// replaced the one key; a file list is used when no variable is set.
    #[test]
    fn env_keys_replace_the_file_list() {
        static SPEC: ProviderSpec = ProviderSpec {
            env_keys: &["SEARCH_TEST_REPLACE_API_KEY"],
            ..exa::SPEC
        };
        let mut config = AppConfig::default();
        config.keys.exa = crate::config::KeyList(vec!["file-1".into(), "file-2".into()]);
        std::env::remove_var("SEARCH_TEST_REPLACE_API_KEY");
        std::env::remove_var("SEARCH_TEST_REPLACE_API_KEY_2");
        assert_eq!(keys_for(&config, &SPEC), ["file-1", "file-2"]);
        // An empty variable does not shadow the file.
        std::env::set_var("SEARCH_TEST_REPLACE_API_KEY", "");
        assert_eq!(keys_for(&config, &SPEC), ["file-1", "file-2"]);
        std::env::set_var("SEARCH_TEST_REPLACE_API_KEY_2", "env-2");
        assert_eq!(keys_for(&config, &SPEC), ["env-2"]);
        std::env::remove_var("SEARCH_TEST_REPLACE_API_KEY");
        std::env::remove_var("SEARCH_TEST_REPLACE_API_KEY_2");
        // Keyless providers have none, whatever the file says.
        assert!(keys_for(&config, &direct::SPEC).is_empty());
    }

    /// Fallback keys are registered for redaction like the first key.
    #[test]
    fn every_key_includes_the_fallbacks() {
        for v in ["BROWSERLESS_API_KEY", "BROWSERLESS_API_KEY_2"] {
            std::env::remove_var(v);
        }
        let mut config = AppConfig::default();
        config.keys.browserless =
            crate::config::KeyList(vec!["bl-first-0001".into(), "bl-second-0002".into()]);
        let all = every_key(&config);
        assert!(all.contains(&"bl-first-0001".to_string()));
        assert!(all.contains(&"bl-second-0002".to_string()));
    }

    #[test]
    fn only_key_failures_move_to_the_next_key() {
        use FailureCategory::*;
        for c in [Auth, BillingQuota, RateLimit] {
            assert!(next_key_may_help(c), "{c:?}");
        }
        for c in [
            Timeout, Server, BadRequest, Parse, Network, Config, Rejected, Other,
        ] {
            assert!(!next_key_may_help(c), "{c:?}");
        }
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
