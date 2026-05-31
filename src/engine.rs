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
        Mode::Auto | Mode::General => &["parallel", "brave", "serper", "exa", "jina", "tavily", "perplexity"],
        Mode::News => &["parallel", "brave", "serper", "tavily", "perplexity"],
        Mode::Academic => &["exa", "serper", "tavily", "perplexity"],
        Mode::Deep => &["parallel", "brave", "exa", "serper", "tavily", "perplexity", "xai"],
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

    // Speculative Execution: If in Auto mode, we don't wait for classification 
    // to start the most likely providers (Brave, Serper).
    let mut speculative_set = JoinSet::new();
    let is_auto = mode == Mode::Auto;
    
    if is_auto && only_providers.is_none() {
        // Only speculate if we have keys and it's not a filtered provider list
        if !ctx.config.keys.brave.is_empty() {
            let q = query_arc.clone();
            let c = count;
            let o = opts.clone();
            let p = providers::brave::Brave::new(ctx.clone());
            speculative_set.spawn(async move { ("brave", p.search(&q, c, &o).await) });
        }
        if !ctx.config.keys.serper.is_empty() {
            let q = query_arc.clone();
            let c = count;
            let o = opts.clone();
            let p = providers::serper::Serper::new(ctx.clone());
            speculative_set.spawn(async move { ("serper", p.search(&q, c, &o).await) });
        }
    }

    let resolved_mode = if is_auto {
        classify_intent(query)
    } else {
        mode
    };

    // If auto resolved to a mode where Brave/Serper aren't wanted,
    // abort speculative tasks to avoid mixing generic web results into
    // intent-specific searches (e.g. news, social, academic).
    let spec_compatible = matches!(
        resolved_mode,
        Mode::Auto | Mode::General | Mode::Deep
    );
    if !spec_compatible {
        speculative_set.abort_all();
        // Drain aborted tasks so they don't merge later
        while speculative_set.join_next().await.is_some() {}
    }

    let all_providers = providers::build_providers(&ctx);
    let wanted = providers_for_mode(resolved_mode);

    let active: Vec<Box<dyn Provider>> = all_providers
        .into_iter()
        .filter(|p| {
            let name = p.name();
            // Don't restart speculative ones (they already launched above)
            if is_auto && only_providers.is_none() && (name == "brave" || name == "serper") { return false; }
            
            let in_mode_set = wanted.contains(&name);
            let in_filter = only_providers
                .as_ref()
                .map(|list| list.iter().any(|f| f.eq_ignore_ascii_case(name)))
                .unwrap_or(true);
            (in_mode_set || only_providers.is_some()) && in_filter && p.is_configured()
        })
        .collect();

    if active.is_empty() && speculative_set.is_empty() {
        return Err(SearchError::NoProviders(resolved_mode.to_string()));
    }

    let mut set = JoinSet::new();
    let mut providers_queried = Vec::new();

    // Re-add speculative ones to the tracking list (only if they weren't aborted)
    if is_auto && only_providers.is_none() && spec_compatible {
        if !ctx.config.keys.brave.is_empty() { providers_queried.push("brave".to_string()); }
        if !ctx.config.keys.serper.is_empty() { providers_queried.push("serper".to_string()); }
    }

    // For Deep mode, also launch Brave LLM Context API in parallel
    if resolved_mode == Mode::Deep && !ctx.config.keys.brave.is_empty() {
        let q = query_arc.clone();
        let c = count;
        let o = opts.clone();
        let brave = providers::brave::Brave::new(ctx.clone());
        set.spawn(async move {
            let result = timeout(Duration::from_secs(15), brave.search_llm_context(&q, c, &o)).await;
            ("brave_llm_context", result)
        });
        providers_queried.push("brave_llm_context".to_string());
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

    // Process speculative results first (they had a head start)
    while let Some(res) = speculative_set.join_next().await {
        match res {
            Ok((_name, Ok(items))) => {
                for item in items {
                    if unique_urls.insert(normalize_url(&item.url)) {
                        all_results.push(item);
                    }
                }
            }
            Ok((name, Err(e))) => {
                tracing::warn!("{name} speculative failed: {e}");
                provider_failures.push(e.to_provider_failure(name));
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
                // If we already have enough results, cancel slow providers
                if all_results.len() >= count {
                    set.abort_all();
                    break;
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

fn normalize_url(url: &str) -> String {
    url.trim_end_matches('/')
        .replace("http://", "https://")
        .replace("www.", "")
        .to_lowercase()
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
            Mode::Scholar | Mode::Patents | Mode::Images | Mode::Places | Mode::People
            | Mode::Similar | Mode::Scrape | Mode::Extract | Mode::Social => {
                execute_special(ctx, query, resolved, count, only_providers, opts).await?
            }
            // Pass Auto to execute_search — it handles speculation + classification internally
            _ => execute_search(ctx, query, Mode::Auto, count, only_providers, opts).await?,
        }
    } else {
        match mode {
            Mode::Scholar | Mode::Patents | Mode::Images | Mode::Places | Mode::People
            | Mode::Similar | Mode::Scrape | Mode::Extract | Mode::Social => {
                execute_special(ctx, query, mode, count, only_providers, opts).await?
            }
            _ => execute_search(ctx, query, mode, count, only_providers, opts).await?,
        }
    };

    response.metadata.result_count = response.results.len();
    Ok(response)
}
