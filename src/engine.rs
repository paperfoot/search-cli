//! One executor for every mode. The registry names a mode's providers, its
//! merge kind and its deadline; the engine runs them:
//! - **fused**: every provider at once, all collected until they finish or
//!   the deadline passes, then reciprocal-rank fusion;
//! - **chain**: in order until one returns content the mode accepts;
//! - **single**: one call.
//!
//! A call that has been sent is paid for, so nothing is cancelled before the
//! mode deadline; whatever is still running then is reported in
//! `metadata.providers_timed_out`.

use crate::context::AppContext;
use crate::errors::{redact_secrets, SearchError};
use crate::hosted::{HostedAdapter, MetasearchClient, RerankItem, V1Manifest, RERANK_MAX};
use crate::providers::{self, Adapter, Call, FilterSupport, Output, ProviderSpec};
use crate::registry::{self, MergeKind};
use crate::types::{
    Answer, CostSummary, FailureCategory, Mode, ProviderFailure, ResponseMetadata, ResponseStatus,
    Routing, SearchOpts, SearchResponse, SearchResult, Transport, ENVELOPE_VERSION,
};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::task::JoinSet;

/// Reciprocal-rank-fusion constant (standard k=60): dampens the gap between
/// rank 1 and rank 2 so cross-provider consensus outweighs any single
/// provider's top pick.
const RRF_K: f64 = 60.0;

/// The metasearch.sh side of a run: the logged-in client and its manifest.
pub struct Hosting {
    pub client: Arc<MetasearchClient>,
    /// `None` when the manifest could not be fetched and no copy is cached:
    /// every provider is then offered for the modes it serves locally, and
    /// the server answers 503 for any it does not.
    pub manifest: Option<V1Manifest>,
    /// `metasearch.max_providers`; default 3 per fused mode, deep 6.
    pub max_providers: Option<u64>,
    /// JEV rerank of fused results (off with `--no-rerank`).
    pub rerank: bool,
}

impl Hosting {
    /// Hosted fan-out cap for a fused mode without `-p`.
    fn cap(&self, mode: Mode) -> usize {
        match self.max_providers {
            Some(n) => n.max(1) as usize,
            None if mode == Mode::Deep => 6,
            None => 3,
        }
    }
}

/// Main entry point (called by main.rs): run `mode` over the configured
/// providers within the mode's deadline. Each provider runs with your own key
/// (direct), keyless on this machine (local), or through metasearch.sh when
/// you are logged in and have no key for it (hosted).
pub async fn run(
    ctx: Arc<AppContext>,
    query: &str,
    mode: Mode,
    count: usize,
    only_providers: &Option<Vec<String>>,
    opts: &SearchOpts,
    hosting: Option<&Hosting>,
) -> Result<SearchResponse, SearchError> {
    let request = Request {
        query,
        mode,
        count,
        only: only_providers.as_deref(),
        opts,
        deadline: registry::spec(mode).deadline,
        hosted_cap: hosting.map(|h| h.cap(mode)),
        reranker: hosting.filter(|h| h.rerank).map(|h| h.client.as_ref()),
    };
    // Specs list extract, never its alias.
    let served = match mode {
        Mode::Scrape => Mode::Extract,
        m => m,
    };
    let adapters = route(providers::build_providers(&ctx), served, hosting);
    execute_with(adapters, &request).await
}

/// Swap every provider you have no key for, and that metasearch.sh offers in
/// `mode`, for its hosted twin. Own keys always win (env > SEARCH_KEYS_* >
/// file); keyless providers stay local.
fn route(
    adapters: Vec<Box<dyn Adapter>>,
    mode: Mode,
    hosting: Option<&Hosting>,
) -> Vec<Box<dyn Adapter>> {
    let Some(h) = hosting else {
        return adapters;
    };
    adapters
        .into_iter()
        .map(|a| {
            let spec = a.spec();
            if a.is_configured() || spec.env_keys.is_empty() || !spec.modes.contains(&mode) {
                return a;
            }
            match offer(h.manifest.as_ref(), spec, mode) {
                Some((filters, max_count)) => Box::new(HostedAdapter {
                    spec,
                    client: h.client.clone(),
                    filters,
                    max_count,
                }) as Box<dyn Adapter>,
                None => a,
            }
        })
        .collect()
}

/// What metasearch.sh offers for (provider, mode): its filters and count cap.
fn offer(
    manifest: Option<&V1Manifest>,
    spec: &ProviderSpec,
    mode: Mode,
) -> Option<(Vec<String>, u16)> {
    match manifest {
        Some(m) => m.offer(spec.name, mode).map(|p| {
            (
                p.filters.clone(),
                p.max_count.unwrap_or(spec.max_count).clamp(1, 20),
            )
        }),
        None => {
            let f = spec.filters;
            let filters = [
                (f.freshness, "freshness"),
                (f.domains, "include_domains"),
                (f.domains, "exclude_domains"),
                (f.country, "country"),
                (f.lang, "language"),
            ]
            .into_iter()
            .filter(|(on, _)| *on)
            .map(|(_, name)| name.to_string())
            .collect();
            Some((filters, spec.max_count.clamp(1, 20)))
        }
    }
}

/// One search, independent of where adapters come from (tests pass fakes).
pub(crate) struct Request<'a> {
    pub query: &'a str,
    pub mode: Mode,
    pub count: usize,
    pub only: Option<&'a [String]>,
    pub opts: &'a SearchOpts,
    pub deadline: Duration,
    /// Most hosted providers a fused mode queries without `-p`.
    pub hosted_cap: Option<usize>,
    /// Rerank fused results through metasearch.sh (logged in, not `--no-rerank`).
    pub reranker: Option<&'a MetasearchClient>,
}

/// True for modes whose fused results a logged-in run reranks.
pub fn reranks(mode: Mode) -> bool {
    RERANK_MODES.contains(&mode)
}

/// Modes whose results JEV may reorder (ROUTING.md "Rerank").
const RERANK_MODES: &[Mode] = &[
    Mode::General,
    Mode::News,
    Mode::Academic,
    Mode::Scholar,
    Mode::People,
    Mode::Deep,
];

/// What one provider call produced.
enum Outcome {
    Done(Output),
    /// A 2xx the mode would not accept (chain); paid for all the same.
    Rejected(ProviderFailure, Output),
    Failed(ProviderFailure),
    /// Still running at the mode deadline.
    TimedOut,
}

pub(crate) async fn execute_with(
    adapters: Vec<Box<dyn Adapter>>,
    req: &Request<'_>,
) -> Result<SearchResponse, SearchError> {
    let start = Instant::now();
    let deadline = start + req.deadline;
    // `scrape` is an alias: route it exactly like `extract`.
    let mode = match req.mode {
        Mode::Scrape => Mode::Extract,
        m => m,
    };
    let mode_spec = registry::spec(mode);
    let mut plan = plan(adapters, mode, mode_spec.providers, req.only)?;
    if plan.is_empty() {
        return Err(no_providers_error(mode, req.only));
    }
    let mut warnings: Vec<String> = Vec::new();
    // Every hosted call is charged, so a fused mode without -p sends only the
    // first few in registry order and names the rest.
    if let (MergeKind::Fused, None, Some(cap)) = (mode_spec.merge, req.only, req.hosted_cap) {
        let hosted: Vec<&'static str> = plan
            .iter()
            .filter(|a| a.transport() == Transport::Hosted)
            .map(|a| a.name())
            .collect();
        if hosted.len() > cap {
            let (kept, left) = hosted.split_at(cap);
            plan.retain(|a| a.transport() != Transport::Hosted || kept.contains(&a.name()));
            warnings.push(format!(
                "hosted: queried {}; add -p to choose others (not queried: {})",
                kept.join(", "),
                left.join(", ")
            ));
        }
    }

    let names: Vec<&'static str> = plan.iter().map(|a| a.name()).collect();
    // The filters each call really sends (a hosted call: the manifest's list).
    let applied: HashMap<&'static str, FilterSupport> = plan
        .iter()
        .map(|a| (a.name(), a.applied_filters(req.opts)))
        .collect();
    // What a sent-but-unanswered call costs in this mode: billed providers
    // bill on send. Taken now, before the adapters move into their tasks.
    let sent_costs: HashMap<&'static str, u64> = plan
        .iter()
        .filter(|a| a.spec().billed_on_send)
        .map(|a| (a.name(), a.sent_cost(mode)))
        .collect();
    let mut transport: BTreeMap<String, String> = plan
        .iter()
        .map(|a| (a.name().to_string(), a.transport().as_str().to_string()))
        .collect();

    let mut outcomes: Vec<(&'static str, Outcome)> = match mode_spec.merge {
        MergeKind::Fused => run_fused(plan, req, mode, deadline).await,
        MergeKind::Chain | MergeKind::Single => run_chain(plan, req, mode, deadline).await,
    };
    // Plan order, not completion order, so fusion ties break the same way
    // whichever provider answered first.
    outcomes.sort_by_key(|(n, _)| names.iter().position(|x| x == n));

    let mut buckets: Vec<(String, Vec<SearchResult>)> = Vec::new();
    let mut answers: Vec<Answer> = Vec::new();
    let mut failures: Vec<ProviderFailure> = Vec::new();
    let mut timed_out: Vec<String> = Vec::new();
    let mut by_provider: BTreeMap<String, u64> = BTreeMap::new();
    let mut queried: Vec<String> = Vec::new();
    for (name, outcome) in outcomes {
        queried.push(name.to_string());
        let spec = providers::find(name);
        let sent_cost = sent_costs.get(name).copied();
        // An answered call: the upstream's own figure, else the estimate.
        let answered_cost = |out: &Output| {
            out.cost_micros
                .unwrap_or_else(|| spec.map_or(0, |s| s.price.estimate(out.results.len() as u16)))
        };
        match outcome {
            Outcome::Done(out) => {
                by_provider.insert(name.to_string(), answered_cost(&out));
                answers.extend(out.answers);
                buckets.push((name.to_string(), out.results));
            }
            Outcome::Rejected(f, out) => {
                tracing::warn!("{name}: {}", redact_secrets(&f.reason));
                by_provider.insert(name.to_string(), answered_cost(&out));
                failures.push(f);
            }
            Outcome::Failed(f) => {
                tracing::warn!("{name}: {}", redact_secrets(&f.reason));
                if providers::billed_failure(f.category) {
                    if let Some(c) = sent_cost {
                        by_provider.insert(name.to_string(), c);
                    }
                }
                failures.push(f);
            }
            Outcome::TimedOut => {
                tracing::warn!("{name}: still running at the mode deadline");
                if let Some(c) = sent_cost {
                    by_provider.insert(name.to_string(), c);
                }
                timed_out.push(name.to_string());
                failures.push(timeout_failure(name));
            }
        }
    }

    // A chain that stopped early never reached the rest.
    transport.retain(|name, _| queried.contains(name));
    let cost = (!by_provider.is_empty()).then(|| {
        let hosted = transport.values().filter(|t| *t == "hosted").count();
        let basis = match hosted {
            0 => "estimate",
            n if n == transport.len() => "charged",
            _ => "mixed",
        };
        CostSummary::with_basis(by_provider, basis)
    });

    let provider_results: BTreeMap<String, usize> = buckets
        .iter()
        .filter(|(_, items)| !items.is_empty())
        .map(|(name, items)| (name.clone(), items.len()))
        .collect();

    // Only fused modes merge across providers. Chain and single keep the
    // answering provider's rows and order as sent: rows may share a URL
    // (images from one page) or have none (places).
    let mut results = match mode_spec.merge {
        MergeKind::Fused => fuse_rrf(buckets),
        MergeKind::Chain | MergeKind::Single => {
            buckets.into_iter().flat_map(|(_, items)| items).collect()
        }
    };

    // Total failure is an error, not a success-shaped envelope. Zero results
    // with at least one clean answer is `no_results`.
    if results.is_empty()
        && answers.is_empty()
        && !failures.is_empty()
        && failures.len() == queried.len()
    {
        return Err(SearchError::AllProvidersFailed {
            failed: failures,
            cost,
        });
    }

    // Rerank the fused pool once, before -c cuts it: a pinned single
    // provider keeps its own order.
    let pinned_to_one = req.only.is_some_and(|l| l.len() == 1);
    let mut reranked = None;
    if let Some(client) = req.reranker {
        if RERANK_MODES.contains(&mode) && results.len() >= 3 && !pinned_to_one {
            reranked = Some(rerank(client, req.query, &mut results).await);
        }
    }
    results.truncate(req.count);

    if mode == Mode::Extract {
        // Scraped page content is untrusted input for the consuming agent —
        // label it so downstream prompts can treat it as data, not instructions.
        for r in &mut results {
            let extra = r.extra.get_or_insert_with(|| serde_json::json!({}));
            if let Some(obj) = extra.as_object_mut() {
                obj.insert(
                    "content_origin".to_string(),
                    serde_json::json!("untrusted_web_content"),
                );
            }
        }
    }

    let providers_failed: Vec<String> = failures.iter().map(|f| f.provider.clone()).collect();
    let status = ResponseStatus::classify(
        results.is_empty() && answers.is_empty(),
        !providers_failed.is_empty(),
    );
    Ok(SearchResponse {
        version: ENVELOPE_VERSION.to_string(),
        status: status.as_str().to_string(),
        query: req.query.to_string(),
        mode: req.mode.to_string(),
        metadata: ResponseMetadata {
            elapsed_ms: start.elapsed().as_millis(),
            result_count: results.len(),
            warnings: {
                warnings.extend(filter_warnings(mode, &queried, &applied, req.opts));
                warnings
            },
            providers_queried: queried,
            providers_failed,
            providers_cancelled: Vec::new(),
            providers_timed_out: timed_out,
            provider_results,
            cached: false,
            cache_age_secs: None,
            provider_failures: failures,
            transport,
            cost,
            routing: reranked.map(|r| Routing {
                auto: None,
                reranked: Some(r),
            }),
            notes: Vec::new(),
        },
        results,
        answers,
    })
}

/// The mode's configured providers in registry order, narrowed by `-p`.
/// A `-p` name that does not serve the mode is bad input (exit 3), so a
/// typo never silently runs nothing or something else.
fn plan(
    adapters: Vec<Box<dyn Adapter>>,
    mode: Mode,
    order: &[&str],
    only: Option<&[String]>,
) -> Result<Vec<Box<dyn Adapter>>, SearchError> {
    if let Some(list) = only {
        for want in list {
            let serves = adapters
                .iter()
                .any(|a| a.name().eq_ignore_ascii_case(want) && a.spec().modes.contains(&mode));
            if !serves {
                let known = adapters.iter().any(|a| a.name().eq_ignore_ascii_case(want));
                let why = if known {
                    format!("provider '{want}' does not serve mode '{mode}'")
                } else {
                    format!("unknown provider '{want}'")
                };
                return Err(SearchError::InvalidInput {
                    message: format!(
                        "{why}; mode '{mode}' is served by [{}]",
                        servers(&adapters, mode).join(", ")
                    ),
                });
            }
        }
    }
    let rank = |name: &str| order.iter().position(|o| *o == name).unwrap_or(order.len());
    let mut picked: Vec<Box<dyn Adapter>> = adapters
        .into_iter()
        .filter(|a| match only {
            Some(list) => list.iter().any(|w| w.eq_ignore_ascii_case(a.name())),
            None => order.contains(&a.name()),
        })
        .filter(|a| a.spec().modes.contains(&mode) && a.is_configured())
        .collect();
    picked.sort_by_key(|a| rank(a.name()));
    Ok(picked)
}

fn servers(adapters: &[Box<dyn Adapter>], mode: Mode) -> Vec<&'static str> {
    adapters
        .iter()
        .filter(|a| a.spec().modes.contains(&mode))
        .map(|a| a.name())
        .collect()
}

fn per_call_count(adapter: &dyn Adapter, count: usize) -> u16 {
    (count.min(u16::MAX as usize) as u16).clamp(1, adapter.spec().max_count)
}

/// A direct call gets min(provider timeout, deadline left). A hosted call
/// gets the whole deadline left: the server runs the provider under its own
/// timeout and its deadline is 5 s shorter than ours.
fn time_left(deadline: Instant, adapter: &dyn Adapter) -> Duration {
    let left = deadline.saturating_duration_since(Instant::now());
    match adapter.transport() {
        Transport::Hosted => left,
        _ => adapter.spec().timeout.min(left),
    }
}

/// Reorder `results` by JEV relevance through metasearch.sh. Fail-open: on any
/// failure the order stays and the answer is false.
async fn rerank(client: &MetasearchClient, query: &str, results: &mut Vec<SearchResult>) -> bool {
    let head = results.len().min(RERANK_MAX);
    let items: Vec<RerankItem> = results[..head]
        .iter()
        .enumerate()
        .map(|(i, r)| RerankItem {
            id: format!("r{i}"),
            title: r.title.clone(),
            snippet: r.snippet.clone(),
            url: r.url.clone(),
        })
        .collect();
    let Some(answer) = client.rerank(query, &items).await else {
        return false;
    };
    let mut pool: Vec<Option<SearchResult>> = results.drain(..head).map(Some).collect();
    let mut ordered: Vec<(usize, SearchResult)> = Vec::with_capacity(head);
    for id in &answer.order {
        let i = id.strip_prefix('r').and_then(|n| n.parse::<usize>().ok());
        if let Some((i, r)) = i.and_then(|i| pool.get_mut(i).and_then(Option::take).map(|r| (i, r)))
        {
            ordered.push((i, r));
        }
    }
    // Anything the answer left out keeps its fused order, after the rest.
    ordered.extend(
        pool.into_iter()
            .enumerate()
            .filter_map(|(i, r)| r.map(|r| (i, r))),
    );
    let mut ordered: Vec<SearchResult> = ordered
        .into_iter()
        .map(|(i, mut r)| {
            if let Some(p) = answer.relevance.get(&format!("r{i}")) {
                let extra = r.extra.get_or_insert_with(|| serde_json::json!({}));
                if let Some(obj) = extra.as_object_mut() {
                    obj.insert(
                        "relevance".to_string(),
                        serde_json::json!((p.clamp(0.0, 1.0) * 100.0).round() / 100.0),
                    );
                }
            }
            r
        })
        .collect();
    ordered.append(results);
    *results = ordered;
    true
}

async fn run_one(
    adapter: &dyn Adapter,
    mode: Mode,
    query: &str,
    count: usize,
    opts: &SearchOpts,
    deadline: Instant,
) -> Outcome {
    let call = Call {
        mode,
        query,
        count: per_call_count(adapter, count),
        opts,
        timeout: time_left(deadline, adapter),
    };
    match tokio::time::timeout_at(deadline.into(), adapter.call(&call)).await {
        Ok(Ok(out)) => Outcome::Done(out),
        Ok(Err(e)) => Outcome::Failed(e.to_provider_failure(adapter.name())),
        Err(_) => Outcome::TimedOut,
    }
}

/// Every provider at once; collect until all finish or the deadline passes.
async fn run_fused(
    plan: Vec<Box<dyn Adapter>>,
    req: &Request<'_>,
    mode: Mode,
    deadline: Instant,
) -> Vec<(&'static str, Outcome)> {
    let mut set: JoinSet<(&'static str, Outcome)> = JoinSet::new();
    let mut pending: Vec<&'static str> = Vec::new();
    let mut task_names: HashMap<tokio::task::Id, &'static str> = HashMap::new();
    for adapter in plan {
        let name = adapter.name();
        pending.push(name);
        let query = req.query.to_string();
        let opts = req.opts.clone();
        let count = req.count;
        let handle = set.spawn(async move {
            let outcome = run_one(adapter.as_ref(), mode, &query, count, &opts, deadline).await;
            (name, outcome)
        });
        task_names.insert(handle.id(), name);
    }

    let mut outcomes = Vec::new();
    loop {
        match tokio::time::timeout_at(deadline.into(), set.join_next()).await {
            Ok(Some(Ok((name, outcome)))) => {
                pending.retain(|n| *n != name);
                outcomes.push((name, outcome));
            }
            // A crashed adapter is a failure of that provider, not a timeout,
            // and its cost is unknown, so nothing is charged for it.
            Ok(Some(Err(e))) => {
                if let Some(name) = task_names.get(&e.id()).copied() {
                    pending.retain(|n| *n != name);
                    outcomes.push((
                        name,
                        Outcome::Failed(ProviderFailure {
                            provider: name.to_string(),
                            category: FailureCategory::Other,
                            http_status: None,
                            code: "crashed".to_string(),
                            reason: format!("{name} crashed inside the CLI"),
                            retryable: false,
                        }),
                    ));
                }
            }
            Ok(None) => break,
            Err(_) => {
                // Deadline: nothing more will be waited for.
                set.abort_all();
                break;
            }
        }
    }
    outcomes.extend(pending.into_iter().map(|n| (n, Outcome::TimedOut)));
    outcomes
}

/// In order; the first output with results that the mode accepts wins.
async fn run_chain(
    plan: Vec<Box<dyn Adapter>>,
    req: &Request<'_>,
    mode: Mode,
    deadline: Instant,
) -> Vec<(&'static str, Outcome)> {
    let mut outcomes = Vec::new();
    for adapter in plan {
        let name = adapter.name();
        if Instant::now() >= deadline {
            break; // never sent, so neither a failure nor a cost
        }
        // Hosted rungs do not run the local adapters' gate, so a URL direct
        // found blocked, paywalled (402) or gone (404/410) is checked here.
        // The gate's errors are BadRequest or Rejected: never billed.
        if let Err(e) = registry::rung_gate(name, req.query) {
            outcomes.push((name, Outcome::Failed(e.to_provider_failure(name))));
            continue;
        }
        match run_one(
            adapter.as_ref(),
            mode,
            req.query,
            req.count,
            req.opts,
            deadline,
        )
        .await
        {
            Outcome::Done(out) if out.results.is_empty() => {
                outcomes.push((name, Outcome::Done(out)));
            }
            Outcome::Done(out) => match registry::accept(mode, &out) {
                Ok(()) => {
                    outcomes.push((name, Outcome::Done(out)));
                    break;
                }
                Err(reason) => {
                    outcomes.push((name, Outcome::Rejected(rejected(name, reason), out)))
                }
            },
            other => outcomes.push((name, other)),
        }
    }
    outcomes
}

fn rejected(provider: &str, reason: &str) -> ProviderFailure {
    ProviderFailure {
        provider: provider.to_string(),
        category: FailureCategory::Rejected,
        http_status: None,
        code: "rejected".to_string(),
        reason: format!("{provider} returned {reason}"),
        retryable: false,
    }
}

/// Failure record for a provider still running at the mode deadline.
fn timeout_failure(provider: &str) -> ProviderFailure {
    ProviderFailure {
        provider: provider.to_string(),
        category: FailureCategory::Timeout,
        http_status: None,
        code: "timeout".to_string(),
        reason: format!("{provider} was still running at the mode deadline"),
        retryable: true,
    }
}

/// Zero providers ran. With `-p` that's the caller's filter selecting only
/// unconfigured providers — a bad-input error naming the mode's real provider
/// set. Without `-p` it's a configuration problem.
fn no_providers_error(mode: Mode, only: Option<&[String]>) -> SearchError {
    match only {
        Some(list) => SearchError::InvalidInput {
            message: format!(
                "no provider ran: mode '{}' is served by [{}], but -p {} selected none of them that are configured",
                mode,
                registry::spec(mode).providers.join(", "),
                list.join(",")
            ),
        },
        None => SearchError::NoProviders(mode.to_string()),
    }
}

/// Reciprocal rank fusion across provider buckets. Each URL scores
/// Σ 1/(RRF_K + rank) over the providers that returned it, so a result two
/// engines agree on outranks any single engine's top hit. Deterministic:
/// independent of provider completion order. Duplicates keep the first-seen
/// content (by bucket order), enriched with missing fields and an
/// `also_found_by` list in `extra`. A row without a URL (a citation or a
/// place with no link) is its own result: nothing proves two such rows match.
fn fuse_rrf(buckets: Vec<(String, Vec<SearchResult>)>) -> Vec<SearchResult> {
    struct Fused {
        result: SearchResult,
        score: f64,
        insertion: usize,
    }
    let mut map: HashMap<String, Fused> = HashMap::new();
    let mut insertion = 0usize;
    for (provider, items) in buckets {
        for (rank, item) in items.into_iter().enumerate() {
            let key = match normalize_url(&item.url) {
                k if k.is_empty() => format!("\0{insertion}"),
                k => k,
            };
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
                    let extra = f.result.extra.get_or_insert_with(|| serde_json::json!({}));
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

/// Honesty notes for filtered searches: one per queried provider that drops
/// a requested filter, so filtered results stay auditable. Never excludes a
/// provider — the caller decides whether unfiltered results are acceptable.
fn filter_warnings(
    mode: Mode,
    queried: &[String],
    applied: &HashMap<&'static str, FilterSupport>,
    opts: &SearchOpts,
) -> Vec<String> {
    let wants_freshness = opts.freshness.is_some();
    let wants_domains = !opts.include_domains.is_empty() || !opts.exclude_domains.is_empty();
    let wants_country = opts.country.is_some();
    let wants_lang = opts.lang.is_some();
    if !(wants_freshness || wants_domains || wants_country || wants_lang) {
        return Vec::new();
    }
    if !registry::mode_applies_filters(mode) {
        return vec![format!(
            "mode '{mode}' does not apply -f/-d/--country/--lang filters; results are unfiltered"
        )];
    }
    let mut warnings = Vec::new();
    for name in queried {
        let Some(f) = applied.get(name.as_str()).copied() else {
            continue;
        };
        let missing: Vec<&str> = [
            (wants_freshness && !f.freshness, "-f freshness"),
            (wants_domains && !f.domains, "-d/--exclude-domain"),
            (wants_country && !f.country, "--country"),
            (wants_lang && !f.lang, "--lang"),
        ]
        .into_iter()
        .filter_map(|(drop, label)| drop.then_some(label))
        .collect();
        if !missing.is_empty() {
            warnings.push(format!(
                "{name} does not apply {}; its results are unfiltered",
                missing.join(" or ")
            ));
        }
        // The caveat describes what happens to domains that were sent.
        if wants_domains && f.domains {
            if let Some(note) = registry::filter_support(name).note {
                warnings.push(note.to_string());
            }
        }
    }
    warnings
}

/// Dedup key for a URL (PLAN.md §3.7; the server's `canonicalUrl` pins the
/// same vectors): scheme and a leading `www.` dropped, host lowercased, path
/// and query case kept, trailing `/` and `#fragment` dropped, tracking
/// parameters removed and the rest sorted.
pub(crate) fn normalize_url(url: &str) -> String {
    let url = url.trim();
    let rest = strip_prefix_ci(url, "https://")
        .or_else(|| strip_prefix_ci(url, "http://"))
        .unwrap_or(url);
    let rest = rest.split('#').next().unwrap_or(rest);
    let (before_query, query) = match rest.split_once('?') {
        Some((b, q)) => (b, Some(q)),
        None => (rest, None),
    };
    let (host, path) = match before_query.find('/') {
        Some(i) => before_query.split_at(i),
        None => (before_query, ""),
    };
    let host = host.to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    let mut key = String::with_capacity(url.len());
    key.push_str(host);
    key.push_str(path.trim_end_matches('/'));
    if let Some(q) = query {
        let mut params: Vec<&str> = q
            .split('&')
            .filter(|p| !p.is_empty() && !is_tracker(p))
            .collect();
        params.sort_unstable();
        if !params.is_empty() {
            key.push('?');
            key.push_str(&params.join("&"));
        }
    }
    key
}

fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let head = s.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &s[prefix.len()..])
}

fn is_tracker(param: &str) -> bool {
    let name = param.split('=').next().unwrap_or(param);
    name.starts_with("utm_")
        || matches!(name, "gclid" | "fbclid" | "msclkid" | "mc_eid" | "ref_src")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{brave, exa, firecrawl, jina, linkup, parallel, perplexity, serpapi};
    use crate::providers::{serper, tavily, xai, ProviderSpec};
    use std::sync::Mutex;

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

    /// What a fake adapter does when called.
    #[derive(Clone)]
    enum Act {
        /// Answer after a delay with these URLs (and optional answer/cost).
        Ok {
            delay_ms: u64,
            urls: Vec<String>,
            answer: Option<&'static str>,
            cost: Option<u64>,
            snippet: &'static str,
        },
        /// Fail with this category.
        Fail(FailureCategory),
    }

    type Log = Arc<Mutex<Vec<(String, u16)>>>;

    struct Fake {
        spec: &'static ProviderSpec,
        act: Act,
        log: Log,
        /// Overrides `Adapter::sent_cost` (as Tavily and Brave deep do).
        sent: Option<u64>,
    }

    #[async_trait::async_trait]
    impl Adapter for Fake {
        fn spec(&self) -> &'static ProviderSpec {
            self.spec
        }
        fn is_configured(&self) -> bool {
            true
        }
        fn sent_cost(&self, _mode: Mode) -> u64 {
            self.sent.unwrap_or(self.spec.price.estimate(0))
        }
        async fn call(&self, call: &Call<'_>) -> Result<Output, SearchError> {
            self.log
                .lock()
                .unwrap()
                .push((self.spec.name.to_string(), call.count));
            match &self.act {
                Act::Ok {
                    delay_ms,
                    urls,
                    answer,
                    cost,
                    snippet,
                } => {
                    tokio::time::sleep(Duration::from_millis(*delay_ms)).await;
                    let mut results: Vec<SearchResult> =
                        urls.iter().map(|u| result(u, self.spec.name)).collect();
                    for r in &mut results {
                        r.snippet = snippet.to_string();
                    }
                    Ok(Output {
                        results,
                        answers: answer
                            .map(|t| Answer {
                                provider: self.spec.name.to_string(),
                                text: t.to_string(),
                            })
                            .into_iter()
                            .collect(),
                        cost_micros: *cost,
                    })
                }
                Act::Fail(category) => Err(SearchError::Upstream {
                    provider: self.spec.name,
                    status: 500,
                    category: *category,
                    message: "boom".into(),
                }),
            }
        }
    }

    fn ok(delay_ms: u64, n: usize, tag: &str) -> Act {
        Act::Ok {
            delay_ms,
            urls: (0..n)
                .map(|i| format!("https://{tag}.example/{i}"))
                .collect(),
            answer: None,
            cost: None,
            snippet: "a real page about something",
        }
    }

    fn fakes(specs: &[(&'static ProviderSpec, Act)]) -> (Vec<Box<dyn Adapter>>, Log) {
        let log: Log = Arc::new(Mutex::new(Vec::new()));
        let adapters = specs
            .iter()
            .map(|(spec, act)| {
                Box::new(Fake {
                    spec,
                    act: act.clone(),
                    log: log.clone(),
                    sent: None,
                }) as Box<dyn Adapter>
            })
            .collect();
        (adapters, log)
    }

    async fn search(
        adapters: Vec<Box<dyn Adapter>>,
        mode: Mode,
        count: usize,
        only: Option<&[String]>,
        deadline_ms: u64,
    ) -> Result<SearchResponse, SearchError> {
        let opts = SearchOpts::default();
        let req = Request {
            query: "q",
            mode,
            count,
            only,
            opts: &opts,
            deadline: Duration::from_millis(deadline_ms),
            hosted_cap: None,
            reranker: None,
        };
        execute_with(adapters, &req).await
    }

    fn called(log: &Log) -> Vec<String> {
        let mut v: Vec<String> = log.lock().unwrap().iter().map(|(n, _)| n.clone()).collect();
        v.sort();
        v
    }

    #[tokio::test]
    async fn fused_waits_for_slow_providers_and_reports_the_stuck_one() {
        let (adapters, _) = fakes(&[
            (&serper::SPEC, ok(5, 3, "fast")),
            (&exa::SPEC, ok(250, 3, "slow")),
            (&jina::SPEC, ok(30_000, 3, "stuck")),
        ]);
        let started = Instant::now();
        let r = search(adapters, Mode::General, 3, None, 600).await.unwrap();
        let took = started.elapsed();
        // Early stop would have dropped exa once serper filled -c 3.
        assert!(r.metadata.provider_results.contains_key("exa"));
        assert!(r.metadata.provider_results.contains_key("serper"));
        assert_eq!(r.metadata.providers_timed_out, vec!["jina".to_string()]);
        assert!(r.metadata.providers_cancelled.is_empty());
        assert_eq!(r.status, "partial_success");
        assert!(
            took >= Duration::from_millis(550),
            "returned before the deadline"
        );
        assert!(took < Duration::from_secs(5), "waited past the deadline");
        // The stuck call was sent, so it counts as spend.
        let cost = r.metadata.cost.unwrap();
        assert_eq!(cost.by_provider["jina"], jina::SPEC.price.per_call as u64);
    }

    #[tokio::test]
    async fn p_restricts_every_call_in_deep() {
        let all = || {
            fakes(&[
                (&exa::SPEC, ok(1, 2, "exa")),
                (&serper::SPEC, ok(1, 2, "serper")),
                (&brave::SPEC, ok(1, 2, "brave")),
                (&linkup::SPEC, ok(1, 2, "linkup")),
                (&parallel::SPEC, ok(1, 2, "parallel")),
                (&tavily::SPEC, ok(1, 2, "tavily")),
                (&perplexity::SPEC, ok(1, 2, "perplexity")),
                (&xai::SPEC, ok(1, 2, "xai")),
            ])
        };
        for only in ["brave", "exa"] {
            let (adapters, log) = all();
            let list = vec![only.to_string()];
            let r = search(adapters, Mode::Deep, 10, Some(&list), 2_000)
                .await
                .unwrap();
            assert_eq!(called(&log), vec![only.to_string()]);
            assert_eq!(r.metadata.providers_queried, vec![only.to_string()]);
        }
        // Without -p every deep provider runs.
        let (adapters, log) = all();
        search(adapters, Mode::Deep, 10, None, 2_000).await.unwrap();
        assert_eq!(called(&log).len(), 8);
    }

    #[tokio::test]
    async fn scholar_fuses_serper_and_serpapi_concurrently() {
        let (adapters, _) = fakes(&[
            (&serper::SPEC, ok(200, 5, "serper")),
            (&serpapi::SPEC, ok(200, 5, "serpapi")),
        ]);
        let started = Instant::now();
        let r = search(adapters, Mode::Scholar, 4, None, 2_000)
            .await
            .unwrap();
        assert!(
            started.elapsed() < Duration::from_millis(390),
            "ran serially"
        );
        assert_eq!(r.metadata.provider_results.len(), 2);
        assert_eq!(r.results.len(), 4);
        // RRF interleaves the two top-ranked lists.
        let sources: Vec<&str> = r.results.iter().map(|x| x.source.as_str()).collect();
        assert!(sources.contains(&"serper") && sources.contains(&"serpapi"));
    }

    #[tokio::test]
    async fn count_is_clamped_per_provider() {
        let (adapters, log) = fakes(&[(&serper::SPEC, ok(1, 1, "a")), (&exa::SPEC, ok(1, 1, "b"))]);
        search(adapters, Mode::General, 50, None, 2_000)
            .await
            .unwrap();
        let mut counts = log.lock().unwrap().clone();
        counts.sort();
        assert_eq!(
            counts,
            vec![("exa".to_string(), 20), ("serper".to_string(), 10)]
        );
    }

    #[tokio::test]
    async fn answers_do_not_take_result_slots() {
        let act = Act::Ok {
            delay_ms: 1,
            urls: vec!["https://a.example/1".into(), "https://a.example/2".into()],
            answer: Some("the answer"),
            cost: None,
            snippet: "x",
        };
        let (adapters, _) = fakes(&[(&perplexity::SPEC, act)]);
        let r = search(adapters, Mode::General, 2, None, 2_000)
            .await
            .unwrap();
        assert_eq!(r.results.len(), 2);
        assert_eq!(r.metadata.result_count, 2);
        assert_eq!(r.answers.len(), 1);
        assert_eq!(r.answers[0].provider, "perplexity");
    }

    #[tokio::test]
    async fn cost_is_an_estimate_per_provider() {
        let reported = Act::Ok {
            delay_ms: 1,
            urls: vec!["https://t.example".into()],
            answer: None,
            cost: Some(16_000),
            snippet: "x",
        };
        let (adapters, _) = fakes(&[
            (&exa::SPEC, ok(1, 12, "exa")),
            (&tavily::SPEC, reported),
            (&serper::SPEC, Act::Fail(FailureCategory::Auth)),
        ]);
        let r = search(adapters, Mode::General, 20, None, 2_000)
            .await
            .unwrap();
        let cost = r.metadata.cost.unwrap();
        assert_eq!(cost.basis, "estimate");
        // exa: 7,000 + 2 results over 10 × 1,000; tavily reported its own.
        assert_eq!(cost.by_provider["exa"], 9_000);
        assert_eq!(cost.by_provider["tavily"], 16_000);
        // A rejected key is not billed.
        assert!(!cost.by_provider.contains_key("serper"));
        assert_eq!(cost.micros, 25_000);
        assert!((cost.usd - 0.025).abs() < 1e-9);
        assert_eq!(r.metadata.transport["exa"], "direct");
    }

    /// A paid call whose 200 the mode rejected (a challenge page) was still
    /// paid for: it counts, at the adapter's reported cost or the estimate.
    #[tokio::test]
    async fn rejected_billed_output_counts_as_spend() {
        let challenge = Act::Ok {
            delay_ms: 1,
            urls: vec!["https://site.example/page".into()],
            answer: None,
            cost: Some(700),
            snippet: "Just a moment...",
        };
        let (adapters, _) = fakes(&[(&jina::SPEC, challenge), (&firecrawl::SPEC, ok(1, 1, "fc"))]);
        let r = search(adapters, Mode::Extract, 1, None, 2_000)
            .await
            .unwrap();
        let cost = r.metadata.cost.unwrap();
        assert_eq!(cost.by_provider["jina"], 700);
        assert_eq!(cost.by_provider["firecrawl"], 5_000);
    }

    /// A 2xx whose body did not parse was billed upstream.
    #[tokio::test]
    async fn parse_failure_on_a_billed_provider_counts_as_spend() {
        let (adapters, _) = fakes(&[
            (&serper::SPEC, ok(1, 2, "s")),
            (&exa::SPEC, Act::Fail(FailureCategory::Parse)),
        ]);
        let r = search(adapters, Mode::General, 5, None, 2_000)
            .await
            .unwrap();
        let cost = r.metadata.cost.unwrap();
        assert_eq!(cost.by_provider["exa"], 7_000);
        assert_eq!(cost.by_provider["serper"], 1_000);
    }

    /// A call still running at the deadline is priced at what was sent for
    /// the mode (Tavily always sends the advanced depth: 16,000), not the
    /// base rate.
    #[tokio::test]
    async fn timeouts_are_priced_at_the_adapters_sent_cost() {
        let log: Log = Arc::new(Mutex::new(Vec::new()));
        let adapters: Vec<Box<dyn Adapter>> = vec![
            Box::new(Fake {
                spec: &serper::SPEC,
                act: ok(1, 2, "s"),
                log: log.clone(),
                sent: None,
            }),
            Box::new(Fake {
                spec: &tavily::SPEC,
                act: ok(5_000, 2, "t"),
                log,
                sent: Some(16_000),
            }),
        ];
        let r = search(adapters, Mode::General, 5, None, 300).await.unwrap();
        assert_eq!(r.metadata.providers_timed_out, vec!["tavily".to_string()]);
        assert_eq!(r.metadata.cost.unwrap().by_provider["tavily"], 16_000);
    }

    fn urls(urls: &[&str], snippet: &'static str) -> Act {
        Act::Ok {
            delay_ms: 1,
            urls: urls.iter().map(|u| u.to_string()).collect(),
            answer: None,
            cost: None,
            snippet,
        }
    }

    /// Serper places rows carry no `link`, so every url is "": a single
    /// provider's rows come back as sent, never merged into one.
    #[tokio::test]
    async fn places_rows_without_url_are_all_kept() {
        let (adapters, _) = fakes(&[(&serper::SPEC, urls(&[""; 5], "a cafe"))]);
        let r = search(adapters, Mode::Places, 5, None, 2_000)
            .await
            .unwrap();
        assert_eq!(r.results.len(), 5);
    }

    /// Images from the same source page share a `link`: single mode keeps
    /// the provider's rows and order.
    #[tokio::test]
    async fn single_mode_keeps_rows_that_share_a_url() {
        let page = "https://site.example/gallery";
        let (adapters, _) = fakes(&[(&serper::SPEC, urls(&[page, page, page], "img"))]);
        let r = search(adapters, Mode::Images, 10, None, 2_000)
            .await
            .unwrap();
        assert_eq!(r.results.len(), 3);
    }

    /// Fusion never treats two link-less rows as the same result.
    #[tokio::test]
    async fn fused_rows_without_url_are_not_merged() {
        let (adapters, _) = fakes(&[
            (&serper::SPEC, urls(&["https://a.org/1", "", ""], "paper")),
            (&serpapi::SPEC, urls(&["", "https://b.org/2"], "paper")),
        ]);
        let r = search(adapters, Mode::Scholar, 10, None, 2_000)
            .await
            .unwrap();
        assert_eq!(r.results.len(), 5);
        assert!(r
            .results
            .iter()
            .filter(|x| x.url.is_empty())
            .all(|x| x.extra.is_none()));
    }

    #[tokio::test]
    async fn p_outside_the_mode_is_bad_input() {
        let (adapters, log) = fakes(&[
            (&serper::SPEC, ok(1, 1, "a")),
            (&firecrawl::SPEC, ok(1, 1, "b")),
        ]);
        let list = vec!["firecrawl".to_string()];
        let err = search(adapters, Mode::General, 5, Some(&list), 2_000)
            .await
            .unwrap_err();
        assert_eq!(err.exit_code(), 3);
        assert!(err.to_string().contains("does not serve mode 'general'"));
        assert!(log.lock().unwrap().is_empty(), "nothing may run");
    }

    #[tokio::test]
    async fn chain_moves_past_rejected_output() {
        let challenge = Act::Ok {
            delay_ms: 1,
            urls: vec!["https://site.example/page".into()],
            answer: None,
            cost: None,
            snippet: "Just a moment...",
        };
        let (adapters, log) =
            fakes(&[(&jina::SPEC, challenge), (&firecrawl::SPEC, ok(1, 1, "fc"))]);
        let r = search(adapters, Mode::Scrape, 1, None, 2_000)
            .await
            .unwrap();
        assert_eq!(r.mode, "scrape");
        assert_eq!(r.results[0].source, "firecrawl");
        assert_eq!(
            r.metadata.provider_failures[0].category,
            FailureCategory::Rejected
        );
        assert_eq!(r.status, "partial_success");
        assert_eq!(
            called(&log),
            vec!["firecrawl".to_string(), "jina".to_string()]
        );
        assert_eq!(
            r.results[0].extra.as_ref().unwrap()["content_origin"],
            "untrusted_web_content"
        );
    }

    #[tokio::test]
    async fn chain_gates_every_rung_on_a_url_direct_saw_blocked() {
        let url = "https://chain-gate.example/blocked";
        registry::note_direct(url, registry::DirectSaw::Blocked);
        let (adapters, log) = fakes(&[
            (&jina::SPEC, ok(1, 1, "j")),
            (&firecrawl::SPEC, ok(1, 1, "fc")),
        ]);
        let opts = SearchOpts::default();
        let req = Request {
            query: url,
            mode: Mode::Extract,
            count: 1,
            only: None,
            opts: &opts,
            deadline: Duration::from_millis(2_000),
            hosted_cap: None,
            reranker: None,
        };
        let _ = execute_with(adapters, &req).await;
        assert!(called(&log).is_empty(), "a gated rung was sent the URL");
    }

    #[tokio::test]
    async fn chain_stops_at_the_first_accepted_output() {
        let (adapters, log) = fakes(&[
            (&jina::SPEC, ok(1, 1, "j")),
            (&firecrawl::SPEC, ok(1, 1, "fc")),
        ]);
        let r = search(adapters, Mode::Extract, 1, None, 2_000)
            .await
            .unwrap();
        assert_eq!(called(&log), vec!["jina".to_string()]);
        assert_eq!(r.metadata.transport.len(), 1);
    }

    #[tokio::test]
    async fn empty_results_with_one_failure_is_no_results_not_total_failure() {
        let (adapters, _) = fakes(&[
            (&serper::SPEC, ok(1, 0, "a")),
            (&exa::SPEC, Act::Fail(FailureCategory::Server)),
        ]);
        let r = search(adapters, Mode::General, 5, None, 2_000)
            .await
            .unwrap();
        assert_eq!(r.status, "no_results");

        let (adapters, _) = fakes(&[
            (&serper::SPEC, Act::Fail(FailureCategory::Auth)),
            (&exa::SPEC, Act::Fail(FailureCategory::BillingQuota)),
        ]);
        let err = search(adapters, Mode::General, 5, None, 2_000)
            .await
            .unwrap_err();
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn url_vectors_match_the_server() {
        // PLAN.md §3.7, the same five vectors the server's canonicalUrl pins.
        assert_eq!(
            normalize_url("https://WWW.Example.com/Post/?utm_source=x#a"),
            "example.com/Post"
        );
        assert_eq!(
            normalize_url("https://example.com/post/"),
            normalize_url("https://example.com/post")
        );
        assert_ne!(
            normalize_url("https://youtube.com/watch?v=AbC"),
            normalize_url("https://youtube.com/watch?v=abc")
        );
        assert_eq!(
            normalize_url("https://example.com/p?b=2&a=1&gclid=z"),
            "example.com/p?a=1&b=2"
        );
        assert_eq!(
            normalize_url("http://example.com/x"),
            normalize_url("https://example.com/x")
        );
    }

    #[test]
    fn url_key_keeps_paths_and_queries_intact() {
        assert!(normalize_url("https://site.com/files/www.report.pdf").contains("www.report.pdf"));
        assert!(normalize_url("https://a.com/x?redirect=http://b.com")
            .contains("redirect=http://b.com"));
        assert_ne!(
            normalize_url("https://x.com/r?page=1"),
            normalize_url("https://x.com/r?page=2")
        );
        assert_eq!(normalize_url("https://www.example.com/"), "example.com");
    }

    #[test]
    fn rrf_consensus_beats_single_provider_top_rank() {
        let buckets = vec![
            (
                "a".to_string(),
                vec![
                    result("https://only-a.com", "a"),
                    result("https://both.com", "a"),
                ],
            ),
            (
                "b".to_string(),
                vec![
                    result("https://only-b.com", "b"),
                    result("https://both.com", "b"),
                ],
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
                    vec![
                        result("https://x.com", first),
                        result("https://y.com", first),
                    ],
                ),
                (
                    second.to_string(),
                    vec![
                        result("https://y.com", second),
                        result("https://z.com", second),
                    ],
                ),
            ]
        };
        let top = |b| fuse_rrf(b).into_iter().next().unwrap().url;
        assert_eq!(top(mk("a", "b")), "https://y.com");
        assert_eq!(top(mk("b", "a")), "https://y.com");
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
    fn filter_warnings_name_providers_that_drop_filters() {
        let opts = SearchOpts {
            freshness: Some("week".into()),
            country: Some("gb".into()),
            ..SearchOpts::default()
        };
        let specs: HashMap<&'static str, FilterSupport> = providers::all()
            .iter()
            .map(|s| (s.name, s.filters))
            .collect();
        let w = filter_warnings(
            Mode::General,
            &["jina".into(), "serper".into()],
            &specs,
            &opts,
        );
        assert_eq!(w.len(), 1);
        assert!(w[0].starts_with("jina does not apply -f freshness or --country"));
        let w = filter_warnings(Mode::Places, &["serper".into()], &specs, &opts);
        assert!(w[0].contains("mode 'places' does not apply"));
        assert!(filter_warnings(
            Mode::General,
            &["jina".into()],
            &specs,
            &SearchOpts::default()
        )
        .is_empty());
    }

    #[test]
    fn filter_warnings_follow_what_was_sent_not_the_spec() {
        // xai deep through metasearch.sh: the manifest lists freshness only,
        // so -d is dropped and the X-handle caveat would be false.
        let opts = SearchOpts {
            include_domains: vec!["nature.com".into()],
            ..SearchOpts::default()
        };
        let sent = FilterSupport {
            freshness: true,
            domains: false,
            country: false,
            lang: false,
        };
        let applied = HashMap::from([("xai", sent)]);
        let w = filter_warnings(Mode::Deep, &["xai".into()], &applied, &opts);
        assert_eq!(
            w,
            vec!["xai does not apply -d/--exclude-domain; its results are unfiltered".to_string()]
        );
    }
}
