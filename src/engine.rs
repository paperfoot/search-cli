use crate::context::AppContext;
use crate::errors::SearchError;
use crate::providers::{self, Provider};
use crate::registry;
use crate::types::{
    Answer, FailureCategory, Mode, ProviderFailure, ResponseMetadata, ResponseStatus, SearchOpts,
    SearchResponse, SearchResult, ENVELOPE_VERSION,
};
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::task::JoinSet;
use tokio::time::timeout;

/// Once enough unique results have arrived, in-flight providers get this long
/// to land before being cancelled. Long enough for the second/third-fastest
/// providers to contribute (so fusion has consensus to rank on), short enough
/// that one slow provider doesn't hold the response hostage.
const EARLY_STOP_GRACE: Duration = Duration::from_millis(1500);

/// Reciprocal-rank-fusion constant (standard k=60): dampens the gap between
/// rank 1 and rank 2 so cross-provider consensus outweighs any single
/// provider's top pick.
const RRF_K: f64 = 60.0;

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
    let wanted = registry::spec(mode).providers;
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

    // Per-provider result buckets, fused after collection. Early-stop: once
    // enough unique results exist, in-flight providers get EARLY_STOP_GRACE
    // to land, then are cancelled. Deep mode never cancels — coverage is its
    // entire purpose.
    let mut buckets: Vec<(String, Vec<SearchResult>)> = Vec::new();
    let mut providers_failed = Vec::new();
    let mut provider_failures: Vec<ProviderFailure> = Vec::new();
    let mut unique_urls = HashSet::new();
    let mut deadline: Option<tokio::time::Instant> = None;

    loop {
        let join_result = match deadline {
            Some(d) => match tokio::time::timeout_at(d, set.join_next()).await {
                Ok(r) => r,
                Err(_) => {
                    // Grace expired: cancel stragglers, then drain without a
                    // deadline so already-finished tasks aren't lost.
                    set.abort_all();
                    deadline = None;
                    continue;
                }
            },
            None => set.join_next().await,
        };
        let Some(join_result) = join_result else { break };
        match join_result {
            Ok((name, Ok(Ok(items)))) => {
                for item in &items {
                    unique_urls.insert(normalize_url(&item.url));
                }
                buckets.push((name.to_string(), items));
                if mode != Mode::Deep && deadline.is_none() && unique_urls.len() >= count {
                    deadline = Some(tokio::time::Instant::now() + EARLY_STOP_GRACE);
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

    // Synthetic AI answers (non-http pseudo-URLs like perplexity://answer)
    // move to the answers field: they aren't fetchable web results and must
    // not consume result slots or pollute `.results[].url` pipelines.
    let mut answers: Vec<Answer> = Vec::new();
    for (_, items) in &mut buckets {
        items.retain(|r| {
            if is_synthetic_answer(r) {
                answers.push(Answer {
                    provider: r.source.clone(),
                    text: r.snippet.clone(),
                });
                false
            } else {
                true
            }
        });
    }

    let provider_results: BTreeMap<String, usize> = buckets
        .iter()
        .filter(|(_, items)| !items.is_empty())
        .map(|(name, items)| (name.clone(), items.len()))
        .collect();

    // Cancelled = started but neither returned nor failed.
    let providers_cancelled: Vec<String> = providers_queried
        .iter()
        .filter(|q| {
            !buckets.iter().any(|(n, _)| n == *q) && !providers_failed.iter().any(|f| f == *q)
        })
        .cloned()
        .collect();

    let mut all_results = fuse_rrf(buckets);
    all_results.truncate(count);
    let result_count = all_results.len();
    let warnings = filter_warnings(&providers_queried, opts);
    let elapsed = start.elapsed();

    // Total failure is an error, not a success-shaped envelope.
    if all_results.is_empty() && answers.is_empty() && !provider_failures.is_empty() {
        return Err(SearchError::AllProvidersFailed {
            failed: provider_failures,
        });
    }

    let status = ResponseStatus::classify(
        all_results.is_empty() && answers.is_empty(),
        !providers_failed.is_empty(),
    );

    Ok(SearchResponse {
        version: ENVELOPE_VERSION.to_string(),
        status: status.as_str().to_string(),
        query: query.to_string(),
        mode: mode.to_string(),
        results: all_results,
        answers,
        metadata: ResponseMetadata {
            elapsed_ms: elapsed.as_millis(),
            result_count,
            providers_queried,
            providers_failed,
            providers_cancelled,
            provider_results,
            warnings,
            cached: false,
            cache_age_secs: None,
            provider_failures,
        },
    })
}

/// True for provider-synthesized answers smuggled in as results under a
/// non-web pseudo-URL scheme (perplexity://answer, tavily://answer).
fn is_synthetic_answer(r: &SearchResult) -> bool {
    !(r.url.starts_with("http://") || r.url.starts_with("https://"))
}

/// Reciprocal rank fusion across provider buckets. Each URL scores
/// Σ 1/(RRF_K + rank) over the providers that returned it, so a result two
/// engines agree on outranks any single engine's top hit. Deterministic:
/// independent of provider completion order. Duplicates keep the first-seen
/// content (by bucket order), enriched with missing fields and an
/// `also_found_by` list in `extra`.
fn fuse_rrf(buckets: Vec<(String, Vec<SearchResult>)>) -> Vec<SearchResult> {
    struct Fused {
        result: SearchResult,
        score: f64,
        insertion: usize,
    }
    let mut map: std::collections::HashMap<String, Fused> = std::collections::HashMap::new();
    let mut insertion = 0usize;
    for (provider, items) in buckets {
        for (rank, item) in items.into_iter().enumerate() {
            let key = normalize_url(&item.url);
            let add = 1.0 / (RRF_K + rank as f64 + 1.0);
            match map.get_mut(&key) {
                Some(f) => {
                    f.score += add;
                    if f.result.published.is_none() {
                        f.result.published = item.published;
                    }
                    if f.result.image_url.is_none() {
                        f.result.image_url = item.image_url;
                    }
                    let extra = f
                        .result
                        .extra
                        .get_or_insert_with(|| serde_json::json!({}));
                    if let Some(obj) = extra.as_object_mut() {
                        let list = obj
                            .entry("also_found_by")
                            .or_insert_with(|| serde_json::json!([]));
                        if let Some(arr) = list.as_array_mut() {
                            arr.push(serde_json::json!(provider.clone()));
                        }
                    }
                }
                None => {
                    insertion += 1;
                    map.insert(
                        key,
                        Fused {
                            result: item,
                            score: add,
                            insertion,
                        },
                    );
                }
            }
        }
    }
    let mut fused: Vec<Fused> = map.into_values().collect();
    fused.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.insertion.cmp(&b.insertion))
    });
    fused.into_iter().map(|f| f.result).collect()
}

/// One warning per active provider that will silently not apply a requested
/// -f/-d filter, so filtered searches stay auditable. Never excludes a
/// provider — the caller decides whether unfiltered results are acceptable.
fn filter_warnings(active: &[String], opts: &SearchOpts) -> Vec<String> {
    let wants_freshness = opts.freshness.is_some();
    let wants_domains = !opts.include_domains.is_empty() || !opts.exclude_domains.is_empty();
    if !wants_freshness && !wants_domains {
        return Vec::new();
    }
    let mut warnings = Vec::new();
    for name in active {
        let support = registry::filter_support(name);
        let mut missing = Vec::new();
        if wants_freshness && !support.freshness {
            missing.push("-f freshness");
        }
        if wants_domains && !support.domains {
            missing.push("-d/--exclude-domain");
        }
        if !missing.is_empty() {
            warnings.push(format!(
                "{name} does not apply {}; its results are unfiltered",
                missing.join(" or ")
            ));
        }
        if wants_domains {
            if let Some(note) = support.note {
                warnings.push(note.to_string());
            }
        }
    }
    warnings
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
    let mut provider_results: BTreeMap<String, usize> = BTreeMap::new();

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
                &mut provider_results,
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

    // Filter honesty: apart from social (xai, which remaps domains to X
    // handles), no special-mode provider call forwards -f/-d filters.
    let mut warnings = Vec::new();
    let wants_filters = opts.freshness.is_some()
        || !opts.include_domains.is_empty()
        || !opts.exclude_domains.is_empty();
    if wants_filters {
        if mode == Mode::Social {
            if let Some(note) = registry::filter_support("xai").note {
                warnings.push(note.to_string());
            }
        } else {
            warnings.push(format!(
                "mode '{mode}' does not apply -f/-d filters; results are unfiltered"
            ));
        }
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
        answers: Vec::new(),
        metadata: ResponseMetadata {
            elapsed_ms: elapsed.as_millis(),
            result_count,
            providers_queried,
            providers_failed,
            providers_cancelled: Vec::new(),
            provider_results,
            warnings,
            cached: false,
            cache_age_secs: None,
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
    counts: &mut BTreeMap<String, usize>,
) {
    match outcome {
        Ok(Ok(items)) => {
            if !items.is_empty() {
                counts.insert(provider.to_string(), items.len());
            }
            results.extend(items);
        }
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
    use super::{fuse_rrf, is_synthetic_answer, normalize_url};
    use crate::types::SearchResult;

    fn result(url: &str, source: &str) -> SearchResult {
        SearchResult {
            title: url.to_string(),
            url: url.to_string(),
            snippet: String::new(),
            source: source.to_string(),
            published: None,
            image_url: None,
            extra: None,
        }
    }

    #[test]
    fn rrf_consensus_beats_single_provider_top_rank() {
        // URL found by two providers (at ranks 2 and 2) must outrank a URL
        // only one provider returned at rank 1.
        let buckets = vec![
            (
                "a".to_string(),
                vec![result("https://only-a.com", "a"), result("https://both.com", "a")],
            ),
            (
                "b".to_string(),
                vec![result("https://only-b.com", "b"), result("https://both.com", "b")],
            ),
        ];
        let fused = fuse_rrf(buckets);
        assert_eq!(fused[0].url, "https://both.com");
        assert_eq!(fused.len(), 3);
    }

    #[test]
    fn rrf_is_independent_of_bucket_arrival_order() {
        let mk = |first: &str, second: &str| {
            vec![
                (
                    first.to_string(),
                    vec![result("https://x.com", first), result("https://y.com", first)],
                ),
                (
                    second.to_string(),
                    vec![result("https://y.com", second), result("https://z.com", second)],
                ),
            ]
        };
        let urls = |buckets| {
            fuse_rrf(buckets)
                .into_iter()
                .map(|r| r.url)
                .collect::<Vec<_>>()
        };
        // y.com has consensus and wins in both arrival orders.
        assert_eq!(urls(mk("a", "b"))[0], "https://y.com");
        assert_eq!(urls(mk("b", "a"))[0], "https://y.com");
    }

    #[test]
    fn rrf_records_consensus_in_extra() {
        let buckets = vec![
            ("a".to_string(), vec![result("https://both.com", "a")]),
            ("b".to_string(), vec![result("https://both.com", "b")]),
        ];
        let fused = fuse_rrf(buckets);
        let also = fused[0].extra.as_ref().unwrap()["also_found_by"].clone();
        assert_eq!(also, serde_json::json!(["b"]));
    }

    #[test]
    fn synthetic_answers_are_detected_by_scheme() {
        assert!(is_synthetic_answer(&result("perplexity://answer", "perplexity")));
        assert!(is_synthetic_answer(&result("tavily://answer", "tavily")));
        assert!(!is_synthetic_answer(&result("https://x.com/search?q=a", "xai")));
    }

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
