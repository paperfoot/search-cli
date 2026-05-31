use crate::context::AppContext;
use crate::errors::SearchError;
use crate::providers::{self, Provider};
use crate::types::{
    FailureCategory, Mode, ProviderFailure, ResponseMetadata, ResponseStatus, SearchOpts,
    SearchResponse, SearchResult, ENVELOPE_VERSION,
};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::task::JoinSet;
use tokio::time::timeout;

/// Which providers to query for each mode
fn providers_for_mode(mode: Mode) -> &'static [&'static str] {
    match mode {
        Mode::General => &[
            "parallel",
            "brave",
            "serper",
            "exa",
            "jina",
            "tavily",
            "perplexity",
        ],
        Mode::News => &["parallel", "brave", "serper", "tavily", "perplexity"],
        Mode::Academic => &["exa", "serper", "tavily", "perplexity"],
        Mode::Deep => &[
            "parallel",
            "brave",
            "exa",
            "serper",
            "tavily",
            "perplexity",
            "xai",
        ],
        Mode::Scholar => &["serper", "serpapi"],
        Mode::Patents => &["serper"],
        Mode::People => &["exa"],
        Mode::Images => &["serper"],
        Mode::Places => &["serper"],
        Mode::Extract | Mode::Scrape => &["stealth", "jina", "firecrawl", "browserless"],
        Mode::Similar => &["exa"],
        Mode::Social => &["xai"],
    }
}

pub async fn execute_search(
    ctx: Arc<AppContext>,
    query: &str,
    mode: Mode,
    count: usize,
    only_providers: &Option<Vec<String>>,
    opts: &SearchOpts,
) -> Result<SearchResponse, SearchError> {
    let start = Instant::now();
    let query_arc: Arc<str> = Arc::from(query);

    // Concrete mode in, concrete provider set out — no intent guessing, no
    // speculative pre-launch. The set is the mode's providers (or whatever -p
    // requested), intersected with what's actually configured.
    let wanted = providers_for_mode(mode);
    let active: Vec<Box<dyn Provider>> = providers::build_providers(&ctx)
        .into_iter()
        .filter(|p| {
            let name = p.name();
            let in_mode_set = wanted.contains(&name);
            (in_mode_set || only_providers.is_some())
                && provider_allowed(name, only_providers)
                && p.is_configured()
        })
        .collect();

    if active.is_empty() {
        return Err(SearchError::NoProviders(mode.to_string()));
    }

    let mut set = JoinSet::new();
    let mut providers_queried = Vec::new();

    // Deep mode also pulls Brave's LLM Context (grounding) API alongside Brave
    // web search — querying brave twice (web + grounding) is intentional.
    if mode == Mode::Deep {
        let brave = providers::brave::Brave::new(ctx.clone());
        if brave.is_configured() {
            let (q, c, o) = (query_arc.clone(), count, opts.clone());
            set.spawn(async move {
                let result =
                    timeout(Duration::from_secs(15), brave.search_llm_context(&q, c, &o)).await;
                ("brave_llm_context", result)
            });
            providers_queried.push("brave_llm_context".to_string());
        }
    }

    for provider in active {
        let q = query_arc.clone();
        let c = count;
        let name = provider.name();
        let tout = provider.timeout();
        let sopts = opts.clone();
        providers_queried.push(name.to_string());

        if mode == Mode::News {
            set.spawn(async move {
                let result = timeout(tout, provider.search_news(&q, c, &sopts)).await;
                (name, result)
            });
        } else {
            set.spawn(async move {
                let result = timeout(tout, provider.search(&q, c, &sopts)).await;
                (name, result)
            });
        }
    }

    let mut all_results = Vec::new();
    let mut providers_failed = Vec::new();
    let mut provider_failures: Vec<ProviderFailure> = Vec::new();
    let mut unique_urls = HashSet::new();

    while let Some(join_result) = set.join_next().await {
        match join_result {
            Ok((_name, Ok(Ok(items)))) => {
                for item in items {
                    if unique_urls.insert(normalize_url(&item.url)) {
                        all_results.push(item);
                    }
                }
                // Enough results: cancel still-pending providers, but keep
                // draining finished tasks so their paid-for results aren't lost.
                if all_results.len() >= count {
                    set.abort_all();
                }
            }
            Ok((name, Ok(Err(e)))) => {
                tracing::warn!("{name}: {e}");
                provider_failures.push(e.to_provider_failure(name));
                providers_failed.push(name.to_string());
            }
            Ok((name, Err(_))) => {
                tracing::warn!("{name}: timed out");
                provider_failures.push(timeout_failure(name));
                providers_failed.push(name.to_string());
            }
            Err(e) => {
                if !e.is_cancelled() {
                    tracing::error!("join error: {e}");
                }
            }
        }
    }

    all_results.truncate(count);
    let result_count = all_results.len();
    let elapsed = start.elapsed();

    // Total failure is an error, not a success-shaped envelope.
    if all_results.is_empty() && !provider_failures.is_empty() {
        return Err(SearchError::AllProvidersFailed {
            failed: provider_failures,
        });
    }

    let status = ResponseStatus::classify(all_results.is_empty(), !providers_failed.is_empty());

    Ok(SearchResponse {
        version: ENVELOPE_VERSION.to_string(),
        status: status.as_str().to_string(),
        query: query.to_string(),
        mode: mode.to_string(),
        results: all_results,
        metadata: ResponseMetadata {
            elapsed_ms: elapsed.as_millis(),
            result_count,
            providers_queried,
            providers_failed,
            provider_failures,
        },
    })
}

/// Dedup key for a URL. Strips scheme and a leading `www.` only — anchored, so
/// it can't corrupt a `www.`/`http://` substring inside a path or query (the
/// old unanchored `.replace()` collapsed `/files/www.x` and rewrote
/// `?redirect=http://`, causing false dedup collisions and dropped results).
/// The query string is preserved so paginated/parameterized URLs stay distinct.
fn normalize_url(url: &str) -> String {
    let lower = url.trim_end_matches('/').to_lowercase();
    let no_scheme = lower
        .strip_prefix("https://")
        .or_else(|| lower.strip_prefix("http://"))
        .unwrap_or(&lower);
    no_scheme
        .strip_prefix("www.")
        .unwrap_or(no_scheme)
        .to_string()
}

fn provider_allowed(name: &str, only: &Option<Vec<String>>) -> bool {
    only.as_ref()
        .map(|list| list.iter().any(|f| f.eq_ignore_ascii_case(name)))
        .unwrap_or(true)
}

/// Handle special modes that need direct provider method calls
pub async fn execute_special(
    ctx: Arc<AppContext>,
    query: &str,
    mode: Mode,
    count: usize,
    only_providers: &Option<Vec<String>>,
    opts: &SearchOpts,
) -> Result<SearchResponse, SearchError> {
    let start = Instant::now();
    let mut results = Vec::new();
    let mut providers_queried = Vec::new();
    let mut providers_failed = Vec::new();
    let mut provider_failures: Vec<ProviderFailure> = Vec::new();

    // Run one timed provider call, recording success/failure uniformly.
    macro_rules! query_provider {
        ($name:literal, $fut:expr, $secs:expr) => {{
            providers_queried.push($name.to_string());
            record_result(
                timeout(Duration::from_secs($secs), $fut).await,
                $name,
                &mut results,
                &mut providers_failed,
                &mut provider_failures,
            );
        }};
    }

    match mode {
        Mode::Scholar => {
            let serper = providers::serper::Serper::new(ctx.clone());
            if serper.is_configured() && provider_allowed("serper", only_providers) {
                query_provider!("serper", serper.search_scholar(query, count), 10);
            }
            let serpapi = providers::serpapi::SerpApi::new(ctx.clone());
            if serpapi.is_configured() && provider_allowed("serpapi", only_providers) {
                query_provider!("serpapi", serpapi.search_scholar(query, count), 10);
            }
        }
        Mode::Patents => {
            let serper = providers::serper::Serper::new(ctx.clone());
            if serper.is_configured() && provider_allowed("serper", only_providers) {
                query_provider!("serper", serper.search_patents(query, count), 10);
            }
        }
        Mode::Images => {
            let serper = providers::serper::Serper::new(ctx.clone());
            if serper.is_configured() && provider_allowed("serper", only_providers) {
                query_provider!("serper", serper.search_images(query, count), 10);
            }
        }
        Mode::Places => {
            let serper = providers::serper::Serper::new(ctx.clone());
            if serper.is_configured() && provider_allowed("serper", only_providers) {
                query_provider!("serper", serper.search_places(query, count), 10);
            }
        }
        Mode::People => {
            let exa = providers::exa::Exa::new(ctx.clone());
            if exa.is_configured() && provider_allowed("exa", only_providers) {
                query_provider!("exa", exa.search_people(query, count), 15);
            }
        }
        Mode::Similar => {
            let exa = providers::exa::Exa::new(ctx.clone());
            if exa.is_configured() && provider_allowed("exa", only_providers) {
                query_provider!("exa", exa.find_similar(query, count), 15);
            }
        }
        Mode::Social => {
            let xai = providers::xai::Xai::new(ctx.clone());
            if xai.is_configured() && provider_allowed("xai", only_providers) {
                query_provider!("xai", xai.search(query, count, opts), 60);
            }
        }
        Mode::Scrape | Mode::Extract => {
            // Try Stealth (local) first, then Jina reader, Firecrawl, Browserless.
            let stealth = providers::stealth::Stealth::new(ctx.clone());
            if provider_allowed("stealth", only_providers) {
                query_provider!("stealth", stealth.scrape_url(query), 30);
            }
            if results.is_empty() {
                let jina = providers::jina::Jina::new(ctx.clone());
                if jina.is_configured() && provider_allowed("jina", only_providers) {
                    query_provider!("jina", jina.read_url(query), 30);
                }
            }
            if results.is_empty() {
                let fc = providers::firecrawl::Firecrawl::new(ctx.clone());
                if fc.is_configured() && provider_allowed("firecrawl", only_providers) {
                    query_provider!("firecrawl", fc.scrape_url(query), 30);
                }
            }
            // Last resort: Browserless cloud browser (handles Cloudflare, JS rendering).
            if results.is_empty() {
                let bl = providers::browserless::Browserless::new(ctx.clone());
                if bl.is_configured() && provider_allowed("browserless", only_providers) {
                    query_provider!("browserless", bl.scrape_url(query), 30);
                }
            }
        }
        _ => {} // handled by execute_search
    }

    if results.is_empty() && providers_queried.is_empty() {
        return Err(SearchError::NoProviders(mode.to_string()));
    }
    if results.is_empty() && !provider_failures.is_empty() {
        return Err(SearchError::AllProvidersFailed {
            failed: provider_failures,
        });
    }

    let elapsed = start.elapsed();
    let result_count = results.len();
    let status = ResponseStatus::classify(results.is_empty(), !providers_failed.is_empty());

    Ok(SearchResponse {
        version: ENVELOPE_VERSION.to_string(),
        status: status.as_str().to_string(),
        query: query.to_string(),
        mode: mode.to_string(),
        results,
        metadata: ResponseMetadata {
            elapsed_ms: elapsed.as_millis(),
            result_count,
            providers_queried,
            providers_failed,
            provider_failures,
        },
    })
}

/// Extend `results` on success; capture a structured failure (reason + category)
/// on error or timeout. Shared by every special-mode provider call.
fn record_result(
    outcome: Result<Result<Vec<SearchResult>, SearchError>, tokio::time::error::Elapsed>,
    provider: &'static str,
    results: &mut Vec<SearchResult>,
    failed: &mut Vec<String>,
    failures: &mut Vec<ProviderFailure>,
) {
    match outcome {
        Ok(Ok(items)) => results.extend(items),
        Ok(Err(e)) => {
            tracing::warn!("{provider}: {e}");
            failures.push(e.to_provider_failure(provider));
            failed.push(provider.to_string());
        }
        Err(_) => {
            tracing::warn!("{provider}: timed out");
            failures.push(timeout_failure(provider));
            failed.push(provider.to_string());
        }
    }
}

/// Failure record for a provider that exceeded its timeout.
fn timeout_failure(provider: &str) -> ProviderFailure {
    ProviderFailure {
        provider: provider.to_string(),
        category: FailureCategory::Timeout,
        http_status: None,
        code: "timeout".to_string(),
        reason: format!("{provider} timed out"),
        retryable: true,
    }
}

/// Main dispatch: routes to execute_search or execute_special based on mode
pub async fn run(
    ctx: Arc<AppContext>,
    query: &str,
    mode: Mode,
    count: usize,
    only_providers: &Option<Vec<String>>,
    opts: &SearchOpts,
) -> Result<SearchResponse, SearchError> {
    // Routing is explicit: the caller chose `mode` (default General). No intent
    // guessing — agents read `agent-info` and pick the mode/providers. Special
    // modes use a provider's dedicated method; the rest aggregate the mode's
    // provider set.
    let mut response = match mode {
        Mode::Scholar
        | Mode::Patents
        | Mode::Images
        | Mode::Places
        | Mode::People
        | Mode::Similar
        | Mode::Scrape
        | Mode::Extract
        | Mode::Social => execute_special(ctx, query, mode, count, only_providers, opts).await?,
        _ => execute_search(ctx, query, mode, count, only_providers, opts).await?,
    };

    response.metadata.result_count = response.results.len();
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::normalize_url;

    #[test]
    fn dedupes_scheme_and_leading_www() {
        assert_eq!(
            normalize_url("https://www.example.com/"),
            normalize_url("http://example.com")
        );
    }

    #[test]
    fn preserves_query_string_so_paginated_urls_stay_distinct() {
        assert_ne!(
            normalize_url("https://x.com/r?page=1"),
            normalize_url("https://x.com/r?page=2")
        );
    }

    #[test]
    fn does_not_strip_www_inside_path() {
        assert!(normalize_url("https://site.com/files/www.report.pdf").contains("www.report.pdf"));
    }

    #[test]
    fn does_not_rewrite_http_inside_query() {
        assert!(normalize_url("https://a.com/x?redirect=http://b.com")
            .contains("redirect=http://b.com"));
    }
}
