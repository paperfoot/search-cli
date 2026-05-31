use crate::classify::classify_intent;
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
        Mode::Auto | Mode::General => &[
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

    // Speculative execution: in Auto mode, launch the most likely providers
    // (Brave, Serper) before classification finishes, to cut latency. Gate on
    // the RESOLVED key (is_configured uses resolve_key), NOT raw config — an
    // env-only key must still speculate, otherwise the provider is neither
    // speculated nor added to the active set below and silently never runs.
    let mut speculative_set = JoinSet::new();
    let is_auto = mode == Mode::Auto;
    let mut speculated: HashSet<&'static str> = HashSet::new();

    if is_auto && only_providers.is_none() {
        let brave = providers::brave::Brave::new(ctx.clone());
        if brave.is_configured() {
            let (q, c, o, tout) = (query_arc.clone(), count, opts.clone(), brave.timeout());
            speculative_set
                .spawn(async move { ("brave", timeout(tout, brave.search(&q, c, &o)).await) });
            speculated.insert("brave");
        }
        let serper = providers::serper::Serper::new(ctx.clone());
        if serper.is_configured() {
            let (q, c, o, tout) = (query_arc.clone(), count, opts.clone(), serper.timeout());
            speculative_set
                .spawn(async move { ("serper", timeout(tout, serper.search(&q, c, &o)).await) });
            speculated.insert("serper");
        }
    }

    let resolved_mode = if is_auto {
        classify_intent(query)
    } else {
        mode
    };

    // If Auto resolved to an intent where generic web results aren't wanted,
    // abort speculation so they don't pollute intent-specific results.
    // (classify_intent never yields Deep — explicit -m deep sets is_auto=false —
    // so Deep isn't listed here.)
    let spec_compatible = matches!(resolved_mode, Mode::Auto | Mode::General);
    if !spec_compatible {
        speculative_set.abort_all();
        while speculative_set.join_next().await.is_some() {}
        speculated.clear();
    }

    let all_providers = providers::build_providers(&ctx);
    let wanted = providers_for_mode(resolved_mode);

    let active: Vec<Box<dyn Provider>> = all_providers
        .into_iter()
        .filter(|p| {
            let name = p.name();
            // Don't restart a provider already launched speculatively.
            if speculated.contains(name) {
                return false;
            }
            let in_mode_set = wanted.contains(&name);
            (in_mode_set || only_providers.is_some())
                && provider_allowed(name, only_providers)
                && p.is_configured()
        })
        .collect();

    if active.is_empty() && speculative_set.is_empty() {
        return Err(SearchError::NoProviders(resolved_mode.to_string()));
    }

    let mut set = JoinSet::new();
    let mut providers_queried = Vec::new();

    // Track speculative providers still in flight (deterministic order).
    if speculated.contains("brave") {
        providers_queried.push("brave".to_string());
    }
    if speculated.contains("serper") {
        providers_queried.push("serper".to_string());
    }

    // For Deep mode, also launch Brave's LLM Context API alongside Brave web
    // search — querying brave twice (web + grounding) is intentional, so
    // `brave` + `brave_llm_context` both appearing in providers_queried is
    // expected, not a bug.
    if resolved_mode == Mode::Deep {
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

        match resolved_mode {
            Mode::News => {
                set.spawn(async move {
                    let result = timeout(tout, provider.search_news(&q, c, &sopts)).await;
                    (name, result)
                });
            }
            _ => {
                set.spawn(async move {
                    let result = timeout(tout, provider.search(&q, c, &sopts)).await;
                    (name, result)
                });
            }
        }
    }

    let mut all_results = Vec::new();
    let mut providers_failed = Vec::new();
    let mut provider_failures: Vec<ProviderFailure> = Vec::new();
    let mut unique_urls = HashSet::new();

    // Process speculative results first (they had a head start). Same 3-level
    // shape as the main set, since speculative calls are now timeout-wrapped.
    while let Some(res) = speculative_set.join_next().await {
        match res {
            Ok((_name, Ok(Ok(items)))) => {
                for item in items {
                    if unique_urls.insert(normalize_url(&item.url)) {
                        all_results.push(item);
                    }
                }
            }
            Ok((name, Ok(Err(e)))) => {
                tracing::warn!("{name} speculative failed: {e}");
                provider_failures.push(e.to_provider_failure(name));
                providers_failed.push(name.to_string());
            }
            Ok((name, Err(_))) => {
                tracing::warn!("{name} speculative timed out");
                provider_failures.push(timeout_failure(name));
                providers_failed.push(name.to_string());
            }
            Err(e) => {
                if !e.is_cancelled() {
                    tracing::error!("speculative join error: {e}");
                }
            }
        }
    }

    // Process the rest
    while let Some(join_result) = set.join_next().await {
        match join_result {
            Ok((_name, Ok(Ok(items)))) => {
                for item in items {
                    let normalized = normalize_url(&item.url);
                    if unique_urls.insert(normalized) {
                        all_results.push(item);
                    }
                }
                // Enough results: cancel still-pending providers, but DON'T
                // break — keep draining already-finished tasks so their
                // paid-for, deduped results aren't thrown away. Aborted tasks
                // come back as cancelled JoinErrors (handled by the Err arm).
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
                // JoinError from abort — not a real failure
                if !e.is_cancelled() {
                    tracing::error!("join error: {e}");
                }
            }
        }
    }

    // Trim to exact requested count
    all_results.truncate(count);
    let result_count = all_results.len();
    let elapsed = start.elapsed();

    // Total failure is an error, not a success-shaped envelope — so agents can
    // branch on the `error` block and read the per-provider reasons.
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
        mode: resolved_mode.to_string(),
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
    // For Auto mode, check if it would resolve to a special mode.
    // If so, route to execute_special with the resolved mode.
    // Otherwise, pass Mode::Auto to execute_search so speculative execution works.
    let mut response = if mode == Mode::Auto {
        let resolved = classify_intent(query);
        match resolved {
            Mode::Scholar
            | Mode::Patents
            | Mode::Images
            | Mode::Places
            | Mode::People
            | Mode::Similar
            | Mode::Scrape
            | Mode::Extract
            | Mode::Social => {
                execute_special(ctx, query, resolved, count, only_providers, opts).await?
            }
            // Pass Auto to execute_search — it handles speculation + classification internally
            _ => execute_search(ctx, query, Mode::Auto, count, only_providers, opts).await?,
        }
    } else {
        match mode {
            Mode::Scholar
            | Mode::Patents
            | Mode::Images
            | Mode::Places
            | Mode::People
            | Mode::Similar
            | Mode::Scrape
            | Mode::Extract
            | Mode::Social => {
                execute_special(ctx, query, mode, count, only_providers, opts).await?
            }
            _ => execute_search(ctx, query, mode, count, only_providers, opts).await?,
        }
    };

    response.metadata.result_count = response.results.len();
    Ok(response)
}
