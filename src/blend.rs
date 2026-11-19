//! Rule 12's search: the pages search (general, or news) with `social` and
//! `community` run beside it, and their posts given a share of the result
//! slots.
//!
//! The added modes are whole searches of their own ([`engine::run_capped`]),
//! not extra providers in the general fusion: a hosted call carries one
//! mode, and rank fusion would put a post only one platform returned under
//! every page two engines agree on. Each part keeps its own ranking; the
//! parts are then dealt into one list by share ([`shares`]), so the top of
//! the list holds pages and posts alike.

use crate::context::AppContext;
use crate::engine::{self, Hosting};
use crate::errors::{redact_secrets, SearchError};
use crate::hosted::HOSTED_SPEC;
use crate::providers;
use crate::registry;
use crate::router;
use crate::types::{
    Answer, CostSummary, Mode, ProviderFailure, ResponseMetadata, ResponseStatus, Routing,
    SearchOpts, SearchResponse, SearchResult, ENVELOPE_VERSION,
};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long an added mode may run: the social wait, which keeps 96% of X's
/// answers (see `registry::SOCIAL_WAIT`). X is billed when sent, so a
/// shorter cap would pay for posts and then drop them; the price is that a
/// search with posts added takes X's time, about 24 s, not general's 3 s.
pub const LANE_CAP: Duration = registry::SOCIAL_WAIT;

/// The merged search and the added modes that were sent.
pub struct Blend {
    pub response: SearchResponse,
    pub ran: Vec<Mode>,
}

type Run = Result<SearchResponse, SearchError>;

/// `base` (general or news) for `query`, and each of `lanes` (two at most)
/// for the query without its question frame, all at once.
pub async fn run(
    ctx: Arc<AppContext>,
    query: &str,
    base: Mode,
    count: usize,
    opts: &SearchOpts,
    hosting: Option<&Hosting>,
    lanes: &[Mode],
) -> Result<Blend, SearchError> {
    let start = Instant::now();
    let posts_query = router::people_query(query);
    let lane = |mode: Option<Mode>| {
        let ctx = ctx.clone();
        let q = posts_query.as_str();
        async move {
            match mode {
                Some(m) => {
                    Some(engine::run_capped(ctx, q, m, count, opts, hosting, LANE_CAP).await)
                }
                None => None,
            }
        }
    };
    let (pages, a, b) = tokio::join!(
        engine::run(ctx.clone(), query, base, count, &None, opts, hosting),
        lane(lanes.first().copied()),
        lane(lanes.get(1).copied()),
    );
    let added: Vec<(Mode, Run)> = lanes
        .iter()
        .copied()
        .zip([a, b].into_iter().flatten())
        .collect();
    merge(query, base, count, pages, added, start.elapsed())
}

/// Result slots per part for `count` rows: the pages keep half (the odd one
/// too), the added modes split the rest, the first taking any odd one.
/// A part that returns fewer rows than its share leaves them to the others.
pub fn shares(count: usize, lanes: usize) -> Vec<usize> {
    let posts = if lanes == 0 { 0 } else { count / 2 };
    let mut out = vec![count - posts];
    for i in 0..lanes {
        out.push(posts / lanes + usize::from(i < posts % lanes));
    }
    out
}

/// What the parts add up to, apart from their rows.
#[derive(Default)]
struct Totals {
    answers: Vec<Answer>,
    queried: Vec<String>,
    timed_out: Vec<String>,
    transport: BTreeMap<String, String>,
    provider_results: BTreeMap<String, usize>,
    warnings: Vec<String>,
    failures: Vec<ProviderFailure>,
    costs: Vec<CostSummary>,
    notes: Vec<String>,
    /// A part answered, with or without rows.
    answered: bool,
}

/// Add `name` once: two parts can each have made a hosted call, and both
/// are named `metasearch` when they fail.
fn add_name(list: &mut Vec<String>, name: &str) {
    if !list.iter().any(|n| n == name) {
        list.push(name.to_string());
    }
}

impl Totals {
    fn take(&mut self, r: SearchResponse) -> Vec<SearchResult> {
        let m = r.metadata;
        self.answered = true;
        self.answers.extend(r.answers);
        for name in &m.providers_queried {
            add_name(&mut self.queried, name);
        }
        for name in &m.providers_timed_out {
            add_name(&mut self.timed_out, name);
        }
        self.transport.extend(m.transport);
        for (name, n) in m.provider_results {
            *self.provider_results.entry(name).or_default() += n;
        }
        for w in m.warnings {
            if !self.warnings.contains(&w) {
                self.warnings.push(w);
            }
        }
        self.failures.extend(m.provider_failures);
        self.costs.extend(m.cost);
        r.results
    }

    /// A part whose every provider failed: its calls were still sent. The
    /// error carries the failures and the cost only, so which calls were
    /// still running and how each was reached are read from the failures.
    fn take_failed(&mut self, failed: Vec<ProviderFailure>, cost: Option<CostSummary>) {
        for f in &failed {
            add_name(&mut self.queried, &f.provider);
            if engine::still_running(f) {
                add_name(&mut self.timed_out, &f.provider);
            }
            let reached = match providers::find(&f.provider) {
                _ if f.provider == HOSTED_SPEC.name => "hosted",
                Some(spec) if spec.env_keys.is_empty() => "local",
                _ => "direct",
            };
            self.transport
                .entry(f.provider.clone())
                .or_insert_with(|| reached.to_string());
        }
        self.failures.extend(failed);
        self.costs.extend(cost);
    }

    fn cost(&self) -> Option<CostSummary> {
        let first = self.costs.first()?;
        let basis = if self.costs.iter().all(|c| c.basis == first.basis) {
            first.basis.clone()
        } else {
            "mixed".to_string()
        };
        let mut by_provider: BTreeMap<String, u64> = BTreeMap::new();
        for c in &self.costs {
            for (name, micros) in &c.by_provider {
                *by_provider.entry(name.clone()).or_default() += micros;
            }
        }
        Some(CostSummary::with_basis(by_provider, &basis))
    }
}

/// One envelope from the pages run and the added modes' runs. A part that
/// could not run or failed never fails the search while another part
/// answered: it leaves a note or its `provider_failures`. With no answer
/// from any part the error is the pages run's: "no providers" when no web
/// engine is configured (a setup problem, unless a failed added call was
/// billed and has to be reported), else every failure with every cost.
pub fn merge(
    query: &str,
    base: Mode,
    count: usize,
    pages: Run,
    added: Vec<(Mode, Run)>,
    elapsed: Duration,
) -> Result<Blend, SearchError> {
    let mut t = Totals::default();
    let mut routing = Routing::default();
    let mut no_pages: Option<SearchError> = None;
    let mut pages_failed = false;
    let mut parts: Vec<Vec<SearchResult>> = Vec::new();
    parts.push(match pages {
        Ok(r) => {
            routing = r.metadata.routing.clone().unwrap_or_default();
            t.take(r)
        }
        Err(SearchError::AllProvidersFailed { failed, cost }) => {
            pages_failed = true;
            t.take_failed(failed, cost);
            Vec::new()
        }
        Err(e @ SearchError::NoProviders(_)) => {
            no_pages = Some(e);
            Vec::new()
        }
        // Bad input and the like: the search itself is wrong.
        Err(e) => return Err(e),
    });
    let mut ran: Vec<Mode> = Vec::new();
    for (mode, run) in added {
        parts.push(match run {
            Ok(r) => {
                ran.push(mode);
                t.take(r)
            }
            Err(SearchError::AllProvidersFailed { failed, cost }) => {
                ran.push(mode);
                t.take_failed(failed, cost);
                Vec::new()
            }
            Err(SearchError::NoProviders(_)) => {
                t.notes.push(format!(
                    "{mode} was not added: its sources need `search login` or a key"
                ));
                Vec::new()
            }
            Err(e) => {
                t.notes.push(format!(
                    "{mode} was not added: {}",
                    redact_secrets(&e.to_string())
                ));
                Vec::new()
            }
        });
    }

    if !t.answered {
        return Err(match no_pages {
            Some(e) if t.cost().is_none_or(|c| c.micros == 0) => e,
            _ => SearchError::AllProvidersFailed {
                cost: t.cost(),
                failed: t.failures,
            },
        });
    }
    if no_pages.is_some() {
        t.notes
            .push("posts only: no web engine has a key here (log in or set one)".to_string());
    } else if pages_failed {
        t.notes
            .push("posts only: every web engine failed (see provider_failures)".to_string());
    }

    let results = deal(parts, count);
    let mut providers_failed: Vec<String> = Vec::new();
    for f in &t.failures {
        add_name(&mut providers_failed, &f.provider);
    }
    let status = ResponseStatus::classify(
        results.is_empty() && t.answers.is_empty(),
        !providers_failed.is_empty(),
    );
    let cost = t.cost();
    Ok(Blend {
        ran,
        response: SearchResponse {
            version: ENVELOPE_VERSION.to_string(),
            status: status.as_str().to_string(),
            query: query.to_string(),
            mode: base.to_string(),
            metadata: ResponseMetadata {
                elapsed_ms: elapsed.as_millis(),
                result_count: results.len(),
                providers_queried: t.queried,
                providers_failed,
                providers_cancelled: Vec::new(),
                providers_timed_out: t.timed_out,
                provider_results: t.provider_results,
                warnings: t.warnings,
                cached: false,
                cache_age_secs: None,
                provider_failures: t.failures,
                transport: t.transport,
                cost,
                routing: (routing != Routing::default()).then_some(routing),
                notes: t.notes,
            },
            results,
            answers: t.answers,
        },
    })
}

/// Deal the parts (pages first) into one list of `count` rows. Each row goes
/// to the part furthest behind its share, pages first on a tie, so for ten
/// rows and two added modes the order is page, post, post, page, post,
/// page, post, page, post, page. A URL the pages and a post list both hold
/// appears once, at its page rank, as the post's row (it has the votes and
/// comment counts).
fn deal(mut parts: Vec<Vec<SearchResult>>, count: usize) -> Vec<SearchResult> {
    let share = shares(count, parts.len().saturating_sub(1));
    let mut posts: HashMap<String, (usize, usize)> = HashMap::new();
    for (p, rows) in parts.iter().enumerate().skip(1) {
        for (i, row) in rows.iter().enumerate() {
            let key = engine::normalize_url(&row.url);
            if !key.is_empty() {
                posts.entry(key).or_insert((p, i));
            }
        }
    }
    let mut moved: HashSet<(usize, usize)> = HashSet::new();
    if let Some((pages, rest)) = parts.split_first_mut() {
        for row in pages.iter_mut() {
            if let Some(&(p, i)) = posts.get(&engine::normalize_url(&row.url)) {
                *row = rest[p - 1][i].clone();
                moved.insert((p, i));
            }
        }
    }
    for (p, rows) in parts.iter_mut().enumerate().skip(1) {
        let mut i = 0;
        rows.retain(|_| {
            i += 1;
            !moved.contains(&(p, i - 1))
        });
    }

    let mut next = vec![0usize; parts.len()];
    let mut taken = vec![0usize; parts.len()];
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<SearchResult> = Vec::new();
    while out.len() < count {
        // The part with rows left that has had the least of its share.
        let behind = |p: usize| match share[p] {
            0 => f64::INFINITY,
            s => taken[p] as f64 / s as f64,
        };
        let Some(p) = (0..parts.len())
            .filter(|&p| next[p] < parts[p].len())
            .min_by(|&a, &b| behind(a).total_cmp(&behind(b)))
        else {
            break;
        };
        let row = parts[p][next[p]].clone();
        next[p] += 1;
        let key = engine::normalize_url(&row.url);
        if !key.is_empty() && !seen.insert(key) {
            continue;
        }
        taken[p] += 1;
        out.push(row);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::FailureCategory;

    fn row(source: &str, url: &str) -> SearchResult {
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

    fn rows(source: &str, host: &str, n: usize) -> Vec<SearchResult> {
        (1..=n)
            .map(|i| row(source, &format!("https://{host}/{i}")))
            .collect()
    }

    fn answered(
        mode: Mode,
        provider: &str,
        micros: u64,
        basis: &str,
        results: Vec<SearchResult>,
    ) -> Run {
        Ok(SearchResponse {
            version: ENVELOPE_VERSION.to_string(),
            status: "success".to_string(),
            query: "q".to_string(),
            mode: mode.to_string(),
            metadata: ResponseMetadata {
                elapsed_ms: 1,
                result_count: results.len(),
                providers_queried: vec![provider.to_string()],
                providers_failed: Vec::new(),
                providers_cancelled: Vec::new(),
                providers_timed_out: Vec::new(),
                provider_results: BTreeMap::from([(provider.to_string(), results.len())]),
                warnings: Vec::new(),
                cached: false,
                cache_age_secs: None,
                provider_failures: Vec::new(),
                transport: BTreeMap::from([(provider.to_string(), "direct".to_string())]),
                cost: Some(CostSummary::with_basis(
                    BTreeMap::from([(provider.to_string(), micros)]),
                    basis,
                )),
                routing: None,
                notes: Vec::new(),
            },
            results,
            answers: Vec::new(),
        })
    }

    fn failure(provider: &str, category: FailureCategory) -> ProviderFailure {
        ProviderFailure {
            provider: provider.to_string(),
            category,
            http_status: None,
            code: "api_error".to_string(),
            reason: "refused".to_string(),
            retryable: false,
        }
    }

    fn all_failed(provider: &str, micros: Option<u64>) -> Run {
        Err(SearchError::AllProvidersFailed {
            failed: vec![failure(provider, FailureCategory::Timeout)],
            cost: micros.map(|m| {
                CostSummary::with_basis(BTreeMap::from([(provider.to_string(), m)]), "estimate")
            }),
        })
    }

    fn hosts(r: &SearchResponse) -> Vec<&str> {
        r.results.iter().map(|r| r.source.as_str()).collect()
    }

    #[test]
    fn shares_keep_half_for_pages() {
        assert_eq!(shares(10, 2), [5, 3, 2]);
        assert_eq!(shares(5, 2), [3, 1, 1]);
        assert_eq!(shares(3, 2), [2, 1, 0]);
        assert_eq!(shares(1, 2), [1, 0, 0]);
        assert_eq!(shares(10, 1), [5, 5]);
        assert_eq!(shares(10, 0), [10]);
    }

    /// Ten rows: five pages, three social posts, two community threads,
    /// dealt so the top of the list holds each kind.
    #[test]
    fn rows_are_dealt_by_share_with_pages_first() {
        let b = merge(
            "q",
            Mode::General,
            10,
            answered(
                Mode::General,
                "serper",
                1_000,
                "estimate",
                rows("serper", "web.example", 10),
            ),
            vec![
                (
                    Mode::Social,
                    answered(
                        Mode::Social,
                        "xai",
                        45_000,
                        "estimate",
                        rows("xai", "x.com", 10),
                    ),
                ),
                (
                    Mode::Community,
                    answered(
                        Mode::Community,
                        "reddit",
                        0,
                        "estimate",
                        rows("reddit", "reddit.com", 10),
                    ),
                ),
            ],
            Duration::from_millis(1200),
        )
        .unwrap();
        let r = &b.response;
        assert_eq!(
            hosts(r),
            [
                "serper", "xai", "reddit", "serper", "xai", "serper", "reddit", "serper", "xai",
                "serper"
            ]
        );
        assert_eq!(b.ran, [Mode::Social, Mode::Community]);
        assert_eq!((r.mode.as_str(), r.status.as_str()), ("general", "success"));
        assert_eq!(r.metadata.providers_queried, ["serper", "xai", "reddit"]);
        assert_eq!(r.metadata.result_count, 10);
        assert_eq!(r.metadata.elapsed_ms, 1200);
        let cost = r.metadata.cost.as_ref().unwrap();
        assert_eq!((cost.micros, cost.basis.as_str()), (46_000, "estimate"));
        assert_eq!(cost.by_provider["xai"], 45_000);
    }

    /// A part with fewer rows than its share leaves them to the others.
    #[test]
    fn a_short_part_leaves_its_slots_to_the_others() {
        let b = merge(
            "q",
            Mode::General,
            10,
            answered(
                Mode::General,
                "serper",
                1_000,
                "estimate",
                rows("serper", "web.example", 10),
            ),
            vec![
                (
                    Mode::Social,
                    answered(
                        Mode::Social,
                        "xai",
                        45_000,
                        "estimate",
                        rows("xai", "x.com", 1),
                    ),
                ),
                (
                    Mode::Community,
                    answered(Mode::Community, "reddit", 0, "estimate", Vec::new()),
                ),
            ],
            Duration::ZERO,
        )
        .unwrap();
        let got = hosts(&b.response);
        assert_eq!(got.len(), 10);
        assert_eq!(got.iter().filter(|s| **s == "serper").count(), 9);
        assert_eq!(got[1], "xai");
    }

    /// A thread the web engines and Reddit both return appears once, at its
    /// page rank, as Reddit's row.
    #[test]
    fn a_url_in_pages_and_posts_appears_once_as_the_post() {
        let mut pages = rows("serper", "web.example", 4);
        pages.insert(
            0,
            row(
                "serper",
                "https://www.reddit.com/r/desks/comments/abc/x/?utm_source=g",
            ),
        );
        let mut threads = rows("reddit", "reddit.com", 2);
        let mut rich = row("reddit", "https://www.reddit.com/r/desks/comments/abc/x/");
        rich.extra = Some(serde_json::json!({"score": 120}));
        threads.push(rich);
        let b = merge(
            "q",
            Mode::General,
            6,
            answered(Mode::General, "serper", 1_000, "estimate", pages),
            vec![(
                Mode::Community,
                answered(Mode::Community, "reddit", 0, "estimate", threads),
            )],
            Duration::ZERO,
        )
        .unwrap();
        let r = &b.response;
        assert_eq!(r.results[0].source, "reddit");
        assert_eq!(r.results[0].extra.as_ref().unwrap()["score"], 120);
        let abc = r.results.iter().filter(|x| x.url.contains("/abc/")).count();
        assert_eq!(abc, 1);
        assert_eq!(r.results.len(), 6);
    }

    /// An added mode that fails or cannot run never fails the search: its
    /// failures and spend are reported, or a note says why it was left out.
    #[test]
    fn a_failed_or_unconfigured_added_mode_is_reported_not_fatal() {
        let b = merge(
            "q",
            Mode::General,
            4,
            answered(
                Mode::General,
                "serper",
                1_000,
                "charged",
                rows("serper", "web.example", 4),
            ),
            vec![
                (Mode::Social, all_failed("xai", Some(45_000))),
                (
                    Mode::Community,
                    Err(SearchError::NoProviders("community".to_string())),
                ),
            ],
            Duration::ZERO,
        )
        .unwrap();
        let r = &b.response;
        assert_eq!(r.status, "partial_success");
        assert_eq!(hosts(r), ["serper"; 4]);
        // Social was sent (and billed); community never ran.
        assert_eq!(b.ran, [Mode::Social]);
        assert_eq!(r.metadata.providers_failed, ["xai"]);
        assert_eq!(r.metadata.providers_queried, ["serper", "xai"]);
        let cost = r.metadata.cost.as_ref().unwrap();
        assert_eq!((cost.micros, cost.basis.as_str()), (46_000, "mixed"));
        assert_eq!(
            r.metadata.notes,
            ["community was not added: its sources need `search login` or a key"]
        );
    }

    /// No web engine, but the posts answered: a result, with a note.
    #[test]
    fn posts_alone_still_answer_when_no_web_engine_is_configured() {
        let b = merge(
            "q",
            Mode::General,
            4,
            Err(SearchError::NoProviders("general".to_string())),
            vec![(
                Mode::Community,
                answered(
                    Mode::Community,
                    "reddit",
                    0,
                    "estimate",
                    rows("reddit", "reddit.com", 6),
                ),
            )],
            Duration::ZERO,
        )
        .unwrap();
        assert_eq!(hosts(&b.response), ["reddit"; 4]);
        assert_eq!(b.response.metadata.notes.len(), 1);
    }

    /// Nothing answered anywhere: an error that carries every failure and
    /// every billed call, or the general run's own error when nothing ran.
    #[test]
    fn no_answer_anywhere_is_an_error_with_every_failure_and_cost() {
        let err = merge(
            "q",
            Mode::General,
            4,
            all_failed("serper", None),
            vec![(Mode::Social, all_failed("xai", Some(45_000)))],
            Duration::ZERO,
        )
        .err()
        .unwrap();
        match err {
            SearchError::AllProvidersFailed { failed, cost } => {
                assert_eq!(failed.len(), 2);
                assert_eq!(cost.unwrap().micros, 45_000);
            }
            e => panic!("{e}"),
        }
        let err = merge(
            "q",
            Mode::General,
            4,
            Err(SearchError::NoProviders("general".to_string())),
            vec![(
                Mode::Social,
                Err(SearchError::NoProviders("social".to_string())),
            )],
            Duration::ZERO,
        )
        .err()
        .unwrap();
        assert!(matches!(err, SearchError::NoProviders(_)), "{err}");
        // Bad input is the search's own error, whatever the posts did.
        let err = merge(
            "q",
            Mode::General,
            4,
            Err(SearchError::InvalidInput {
                message: "bad".to_string(),
            }),
            vec![(
                Mode::Community,
                answered(
                    Mode::Community,
                    "reddit",
                    0,
                    "estimate",
                    rows("reddit", "reddit.com", 2),
                ),
            )],
            Duration::ZERO,
        )
        .err()
        .unwrap();
        assert!(matches!(err, SearchError::InvalidInput { .. }), "{err}");
    }

    /// The config problem wins when nothing was spent: no web engine and
    /// Reddit unreachable is "no providers" (exit 2), as it was before
    /// posts were added, not a transient failure. A billed call that failed
    /// is still reported.
    #[test]
    fn no_web_engine_stays_a_config_error_unless_a_failed_call_was_billed() {
        let err = merge(
            "q",
            Mode::General,
            4,
            Err(SearchError::NoProviders("general".to_string())),
            vec![(Mode::Community, all_failed("reddit", None))],
            Duration::ZERO,
        )
        .err()
        .unwrap();
        assert!(matches!(err, SearchError::NoProviders(_)), "{err}");
        let err = merge(
            "q",
            Mode::General,
            4,
            Err(SearchError::NoProviders("general".to_string())),
            vec![(Mode::Social, all_failed("xai", Some(45_000)))],
            Duration::ZERO,
        )
        .err()
        .unwrap();
        match err {
            SearchError::AllProvidersFailed { cost, .. } => {
                assert_eq!(cost.unwrap().micros, 45_000)
            }
            e => panic!("{e}"),
        }
    }

    /// A part that failed whole keeps what its error does not carry: which
    /// call was still running, and how each provider was reached.
    #[test]
    fn a_failed_part_keeps_its_timed_out_calls_and_transport() {
        let mut still = failure("xai", FailureCategory::Timeout);
        still.code = "timeout".to_string();
        let b = merge(
            "q",
            Mode::News,
            4,
            answered(
                Mode::News,
                "serper",
                1_000,
                "estimate",
                rows("serper", "web.example", 4),
            ),
            vec![
                (
                    Mode::Social,
                    Err(SearchError::AllProvidersFailed {
                        failed: vec![still],
                        cost: None,
                    }),
                ),
                (Mode::Community, all_failed("reddit", None)),
            ],
            Duration::ZERO,
        )
        .unwrap();
        let m = &b.response.metadata;
        assert_eq!(b.response.mode, "news");
        assert_eq!(m.providers_timed_out, ["xai"]);
        assert_eq!(m.transport["xai"], "direct");
        assert_eq!(m.transport["reddit"], "local");
        assert_eq!(m.transport["serper"], "direct");
    }

    /// Two hosted calls that both fail are both `metasearch`: it is named
    /// once, each failure is kept, and posts that came back alone say so.
    #[test]
    fn two_failed_hosted_calls_name_metasearch_once() {
        let b = merge(
            "q",
            Mode::General,
            4,
            all_failed("metasearch", None),
            vec![
                (Mode::Social, all_failed("metasearch", None)),
                (
                    Mode::Community,
                    answered(
                        Mode::Community,
                        "reddit",
                        0,
                        "estimate",
                        rows("reddit", "reddit.com", 3),
                    ),
                ),
            ],
            Duration::ZERO,
        )
        .unwrap();
        let m = &b.response.metadata;
        assert_eq!(m.providers_failed, ["metasearch"]);
        assert_eq!(m.providers_queried, ["metasearch", "reddit"]);
        assert_eq!(m.provider_failures.len(), 2);
        assert_eq!(m.transport["metasearch"], "hosted");
        assert_eq!(b.response.status, "partial_success");
        assert_eq!(
            m.notes,
            ["posts only: every web engine failed (see provider_failures)"]
        );
    }
}
