//! Rule 12's search: the pages search (general, or news) with `social` and
//! `community` run beside it, and their posts returned with the pages.
//!
//! The added modes are whole searches of their own ([`engine::run_capped`]),
//! not extra providers in the pages' fusion: a hosted call carries one
//! mode, and rank fusion would put a post only one platform returned under
//! every page two engines agree on. Each part keeps its own ranking. The
//! pages keep every row `-c` asked for; the posts are extra rows
//! ([`shares`]), dealt in among the pages so the top of the list holds
//! both. (0.13 gave the posts half of `-c` instead, which cost five pages
//! in ten.)

use crate::cache;
use crate::context::AppContext;
use crate::engine::{self, Hosting};
use crate::errors::{redact_secrets, SearchError};
use crate::hosted::HOSTED_SPEC;
use crate::providers;
use crate::registry;
use crate::router;
use crate::types::{
    Answer, CostSummary, FailureCategory, Mode, ProviderFailure, ResponseMetadata, ResponseStatus,
    Routing, SearchOpts, SearchResponse, SearchResult, ENVELOPE_VERSION,
};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long an added mode may run when its calls go straight to their
/// providers: the social wait, which keeps 96% of X's answers (see
/// `registry::SOCIAL_WAIT`). X is billed when sent, so a shorter cap would
/// pay for posts and then drop them; the price is that a search with posts
/// added takes X's time, about 24 s. An added mode with a hosted call keeps
/// its own deadline (`engine::run_capped`).
pub const LANE_CAP: Duration = registry::SOCIAL_WAIT;

/// The merged search and the added modes that were sent.
pub struct Blend {
    pub response: SearchResponse,
    pub ran: Vec<Mode>,
}

type Run = Result<SearchResponse, SearchError>;

/// Timeouts in a row after which X is left out of added searches.
const X_STRIKES: u32 = 2;

/// How long X is left out. The first search after that asks it again, and
/// one more timeout rests it for as long again.
const X_REST: Duration = Duration::from_secs(600);

/// Whether X is left out of this added search. X is the one added source
/// that is slow and paid for by the call: when its API is at capacity
/// (20 Nov 2026: 8 of 12 searches) every search waits the whole
/// [`LANE_CAP`] for it and may be billed for an answer nobody read.
fn x_resting(strikes: cache::Strikes, now: u64) -> bool {
    strikes.count >= X_STRIKES && now.saturating_sub(strikes.at) < X_REST.as_secs()
}

/// Whether X answered in the added social search: `Some(false)` when it
/// ran out of time, `Some(true)` when it answered (with posts or without),
/// `None` when it was not asked or failed some other way (a refused key
/// or a 429 comes back at once and costs nothing to try again).
fn x_answered(run: &Run) -> Option<bool> {
    let timed_out = |failures: &[ProviderFailure]| {
        failures
            .iter()
            .any(|f| f.provider == "xai" && f.category == FailureCategory::Timeout)
    };
    match run {
        Ok(r) if timed_out(&r.metadata.provider_failures) => Some(false),
        Ok(r) if r.metadata.providers_timed_out.iter().any(|p| p == "xai") => Some(false),
        Ok(r) if r.metadata.provider_results.contains_key("xai") => Some(true),
        Err(SearchError::AllProvidersFailed { failed, .. }) if timed_out(failed) => Some(false),
        _ => None,
    }
}

/// The record after this search, when it changes.
fn next_strikes(prev: cache::Strikes, answered: Option<bool>, now: u64) -> Option<cache::Strikes> {
    match answered {
        Some(false) => Some(cache::Strikes {
            count: prev.count.saturating_add(1),
            at: now,
        }),
        Some(true) if prev.count > 0 => Some(cache::Strikes::default()),
        _ => None,
    }
}

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
    // X resting: the social search asks the other platform of its pick
    // alone, or is not run when X was all of it (breaking news).
    let strikes = cache::x_strikes();
    let resting = lanes.contains(&Mode::Social) && x_resting(strikes, cache::now());
    let without_x: Vec<String> = router::pick_social_platforms(&posts_query)
        .into_iter()
        .filter(|p| *p != "xai")
        .map(str::to_string)
        .collect();
    // A mode with no rows to give is not run: a search for one row is
    // answered by one page, and X would be paid for posts nobody sees.
    let share = shares(count, lanes.len());
    let lanes: Vec<Mode> = lanes
        .iter()
        .zip(&share[1..])
        .filter(|(_, rows)| **rows > 0)
        .map(|(m, _)| *m)
        .filter(|m| !(resting && *m == Mode::Social && without_x.is_empty()))
        .collect();
    let lanes = lanes.as_slice();
    let lane = |mode: Option<Mode>| {
        let ctx = ctx.clone();
        let q = posts_query.as_str();
        let only = (resting && mode == Some(Mode::Social)).then_some(without_x.as_slice());
        async move {
            match mode {
                Some(m) => Some(
                    engine::run_capped(ctx, q, m, count, (only, LANE_CAP), opts, hosting).await,
                ),
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
    if !resting {
        let answered = added
            .iter()
            .find(|(m, _)| *m == Mode::Social)
            .and_then(|(_, run)| x_answered(run));
        if let Some(next) = next_strikes(strikes, answered, cache::now()) {
            cache::set_x_strikes(next);
        }
    }
    let mut blend = merge(
        query,
        base,
        count,
        pages,
        added,
        start.elapsed(),
        hosting.is_some(),
    )?;
    if resting {
        blend.response.metadata.notes.push(
            "X did not answer the last searches in time and was left out; it is asked again \
             within ten minutes"
                .to_string(),
        );
    }
    Ok(blend)
}

/// Rows per part for `-c count`: the pages keep all of `count`, and each
/// added mode brings posts on top, two for every five pages (four for ten;
/// none for a search of one row).
pub fn shares(count: usize, lanes: usize) -> Vec<usize> {
    let posts = (count * 2 + 2) / 5;
    let mut out = vec![count];
    out.extend(std::iter::repeat_n(posts, lanes));
    out
}

/// A post with nothing to read: no text (X returns the odd empty post), a
/// clip with no caption, whose title is only "@name on TikTok", or a
/// Hacker News item marked dead. Such a row would take a place and say
/// nothing.
fn hollow(row: &SearchResult) -> bool {
    row.snippet.trim().is_empty()
        || [" on TikTok", " on Instagram", " on Threads"]
            .iter()
            .any(|tail| row.title.ends_with(tail))
        // A Hacker News item its moderators removed.
        || row.title.contains("[dead]")
        || row.title.contains("[flagged]")
}

/// How X's summary reads when none of its posts are about the query ("No
/// relevant posts were returned", "None of the posts mention ..."). In a
/// graded sample of 25 searches (19 Nov 2026) eight summaries said so in
/// these words, and all eight had been graded off topic or empty. Softer
/// wording ("none mention the price") is left alone: it also appears over
/// posts that are on the subject. The wording changes from run to run, so
/// this is the fallback for a summary that comes without a typed verdict.
const DISOWNED: &[&str] = &[
    "no relevant posts",
    "no relevant x posts",
    "no posts matching",
    "no posts relevant",
    "returned no",
    "none directly",
    "none of the posts",
    "no apparent connection",
];

/// Words too common to tell whether a row is about the query.
const COMMON_WORDS: &[&str] = &[
    "the", "and", "for", "with", "how", "what", "why", "who", "are", "was", "can", "does", "not",
    "from", "that", "this", "you", "your", "about", "into", "best", "new",
];

/// The words of `query` that say what it is about: three letters or more,
/// not a number, not a common word.
fn key_words(query: &str) -> Vec<String> {
    let mut words: Vec<String> = Vec::new();
    for w in query.to_lowercase().split(|c: char| !c.is_alphanumeric()) {
        if w.chars().count() >= 3
            && !w.chars().all(|c| c.is_ascii_digit())
            && !COMMON_WORDS.contains(&w)
            && !words.iter().any(|k| k == w)
        {
            words.push(w.to_string());
        }
    }
    words
}

/// A clip's view count, when the platform gave one.
fn views(row: &SearchResult) -> Option<u64> {
    row.extra.as_ref()?.get("views")?.as_u64()
}

/// Clean an added mode's rows before they are dealt in. Each step answers
/// a defect found by grading 25 real searches source by source (19 Nov
/// 2026):
/// - **A post with nothing to read** is dropped ([`hollow`]): 28% of X's
///   rows were posts it cited without listing their text.
/// - **X said its posts miss**: its rows and summary are dropped and a
///   note says so. The call is paid either way; passing on posts X itself
///   calls unrelated only costs the reader. With your own key the verdict
///   is typed (`on_topic` in the provider's request: no rows come back);
///   through metasearch.sh it is read from the summary ([`DISOWNED`]).
/// - **Grok's own replies** and a **second post by one author** are
///   dropped: one account held five of ten rows.
/// - **A clip almost nobody watched** (under 100 views) goes after the
///   others: such rows outranked a 137,000-view video on the same subject.
/// - **Reddit cross-posts** (the same title twice) appear once.
/// - **Hacker News** matches loosely once a long query is cut short, and
///   five of its eight rows were off topic. A row is kept when it holds
///   the query's first key word (the subject comes first) and either the
///   second or half of them all; over the sixteen rows on file that keeps
///   every thread about the subject and one stray. Reddit leads, one
///   Hacker News row to every three.
fn polish(mode: Mode, query: &str, r: &mut SearchResponse, notes: &mut Vec<String>) {
    match mode {
        Mode::Social => {
            // X's verdict: with your own key it is typed (the provider
            // returns the summary and no rows at all); through
            // metasearch.sh it is read from the summary's wording.
            let answered = r.answers.iter().any(|a| a.provider == "xai");
            let rows = r.results.iter().filter(|row| row.source == "xai").count();
            let disowned = answered
                && (rows == 0
                    || r.answers.iter().any(|a| {
                        let text = a.text.to_lowercase();
                        a.provider == "xai" && DISOWNED.iter().any(|p| text.contains(p))
                    }));
            r.results.retain(|row| !hollow(row));
            if disowned {
                r.answers.retain(|a| a.provider != "xai");
                r.results.retain(|row| row.source != "xai");
                notes.push("X found no posts about this query".to_string());
            }
            let mut authors: HashSet<String> = HashSet::new();
            r.results.retain(|row| {
                row.source != "xai"
                    || (!row.title.eq_ignore_ascii_case("@grok")
                        && authors.insert(row.title.to_lowercase()))
            });
            let (watched, unwatched): (Vec<SearchResult>, Vec<SearchResult>) =
                std::mem::take(&mut r.results)
                    .into_iter()
                    .partition(|row| views(row).is_none_or(|v| v >= 100));
            r.results = watched;
            r.results.extend(unwatched);
        }
        Mode::Community => {
            r.results.retain(|row| !hollow(row));
            let keys = key_words(&router::people_query(query));
            let mut titles: HashSet<String> = HashSet::new();
            let (reddit, hn): (Vec<SearchResult>, Vec<SearchResult>) =
                std::mem::take(&mut r.results)
                    .into_iter()
                    .filter(|row| {
                        row.source != "reddit" || titles.insert(row.title.trim().to_lowercase())
                    })
                    .partition(|row| row.source != "hackernews");
            let mut hn = hn.into_iter().filter(|row| {
                let text = format!("{} {}", row.title, row.snippet).to_lowercase();
                let holds = |k: &String| text.contains(k.as_str());
                let Some(subject) = keys.first() else {
                    return true;
                };
                holds(subject)
                    && (keys.get(1).is_none_or(holds)
                        || keys.iter().filter(|k| holds(k)).count() * 2 >= keys.len())
            });
            for (i, row) in reddit.into_iter().enumerate() {
                if i > 0 && i % 3 == 0 {
                    r.results.extend(hn.next());
                }
                r.results.push(row);
            }
            r.results.extend(hn);
        }
        _ => {}
    }
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
    /// Logged in: a provider with a key variable may have been reached
    /// through metasearch.sh, so a failed one's transport is not known.
    logged_in: bool,
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
                _ if f.provider == HOSTED_SPEC.name => Some("hosted"),
                Some(spec) if spec.env_keys.is_empty() => Some("local"),
                Some(_) if !self.logged_in => Some("direct"),
                _ => None,
            };
            if let Some(reached) = reached {
                self.transport
                    .entry(f.provider.clone())
                    .or_insert_with(|| reached.to_string());
            }
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
/// from any part, or with pages that did not answer and posts that came
/// back empty, the error is the pages run's: "no providers" when no web
/// engine is configured (a setup problem, unless a failed added call was
/// billed and has to be reported), else every failure with every cost.
pub fn merge(
    query: &str,
    base: Mode,
    count: usize,
    pages: Run,
    added: Vec<(Mode, Run)>,
    elapsed: Duration,
    logged_in: bool,
) -> Result<Blend, SearchError> {
    let mut t = Totals {
        logged_in,
        ..Totals::default()
    };
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
            Ok(mut r) => {
                ran.push(mode);
                polish(mode, query, &mut r, &mut t.notes);
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

    // No answer at all, or the pages did not answer and the posts that did
    // are empty: the pages' failure is the result (a setup or provider
    // error with its exit code), not "no results".
    let rows = parts.iter().any(|p| !p.is_empty()) || !t.answers.is_empty();
    if !t.answered || ((no_pages.is_some() || pages_failed) && !rows) {
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

/// Deal the parts (pages first) into one list: up to its share from each
/// ([`shares`]). Each row goes to the part furthest behind its share, pages
/// first on a tie, so for ten pages and two added modes the list opens
/// page, post, post, page, page, post, post. A URL the pages and a post
/// list both hold appears once, at its page rank, as the post's row (it has
/// the votes and comment counts).
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
    loop {
        // The part with rows left that has had the least of its share.
        let behind = |p: usize| taken[p] as f64 / share[p] as f64;
        let Some(p) = (0..parts.len())
            .filter(|&p| next[p] < parts[p].len() && taken[p] < share[p])
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
            snippet: "text".to_string(),
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
    fn pages_keep_their_rows_and_posts_come_on_top() {
        assert_eq!(shares(10, 2), [10, 4, 4]);
        assert_eq!(shares(5, 2), [5, 2, 2]);
        assert_eq!(shares(3, 2), [3, 1, 1]);
        assert_eq!(shares(20, 2), [20, 8, 8]);
        assert_eq!(shares(10, 1), [10, 4]);
        assert_eq!(shares(10, 0), [10]);
        // A search for one row is one page: no post, so no added mode runs.
        assert_eq!(shares(1, 2), [1, 0, 0]);
    }

    /// -c 10: all ten pages, with four social posts and four community
    /// threads dealt in so the top of the list holds each kind.
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
            false,
        )
        .unwrap();
        let r = &b.response;
        let order: String = hosts(r)
            .iter()
            .map(|s| match *s {
                "serper" => 'P',
                "xai" => 'S',
                _ => 'C',
            })
            .collect();
        assert_eq!(order, "PSCPPSCPPPSCPPSCPP");
        assert_eq!(b.ran, [Mode::Social, Mode::Community]);
        assert_eq!((r.mode.as_str(), r.status.as_str()), ("general", "success"));
        assert_eq!(r.metadata.providers_queried, ["serper", "xai", "reddit"]);
        assert_eq!(r.metadata.result_count, 18);
        assert_eq!(r.metadata.elapsed_ms, 1200);
        let cost = r.metadata.cost.as_ref().unwrap();
        assert_eq!((cost.micros, cost.basis.as_str()), (46_000, "estimate"));
        assert_eq!(cost.by_provider["xai"], 45_000);
    }

    /// A part with fewer rows than its share gives what it has; nothing
    /// else grows to fill the gap.
    #[test]
    fn a_short_part_gives_what_it_has() {
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
            false,
        )
        .unwrap();
        let got = hosts(&b.response);
        assert_eq!(got.len(), 11);
        assert_eq!(got.iter().filter(|s| **s == "serper").count(), 10);
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
            false,
        )
        .unwrap();
        let r = &b.response;
        assert_eq!(r.results[0].source, "reddit");
        assert_eq!(r.results[0].extra.as_ref().unwrap()["score"], 120);
        let abc = r.results.iter().filter(|x| x.url.contains("/abc/")).count();
        assert_eq!(abc, 1);
        // Five pages (one of them the thread) and Reddit's other two.
        assert_eq!(r.results.len(), 7);
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
            false,
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
            false,
        )
        .unwrap();
        assert_eq!(hosts(&b.response), ["reddit"; 2]);
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
            false,
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
            false,
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
            false,
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
            false,
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
            false,
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
        still.reason = "xai was still running when the search stopped waiting".to_string();
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
            false,
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
            false,
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

    /// At -c 1 the one row is a page: no added mode has a slot, so none is
    /// worth running (X would be paid for posts that cannot appear).
    #[test]
    fn a_mode_with_no_slot_has_no_share() {
        assert_eq!(shares(1, 2)[1..], [0, 0]);
        assert_eq!(shares(2, 2)[1..], [1, 1]);
    }

    fn social_answer(rows: Vec<SearchResult>, summary: &str) -> Run {
        let mut r = answered(Mode::Social, "xai", 45_000, "estimate", rows).unwrap();
        r.answers.push(Answer {
            provider: "xai".to_string(),
            text: summary.to_string(),
        });
        Ok(r)
    }

    fn post(source: &str, title: &str, url: &str) -> SearchResult {
        let mut r = row(source, url);
        r.title = title.to_string();
        r
    }

    fn blend_one(mode: Mode, query: &str, lane: Run) -> Blend {
        merge(
            query,
            Mode::General,
            10,
            answered(
                Mode::General,
                "serper",
                1_000,
                "estimate",
                rows("serper", "web.example", 2),
            ),
            vec![(mode, lane)],
            Duration::ZERO,
            false,
        )
        .unwrap()
    }

    /// When X says none of its posts are about the query, its rows and
    /// its summary are left out and a note says so; the other platform's
    /// rows stay.
    #[test]
    fn posts_x_calls_unrelated_are_left_out() {
        let lane = social_answer(
            vec![
                post("xai", "@a", "https://x.com/a/status/1"),
                post("youtube", "Talk", "https://www.youtube.com/watch?v=1"),
                post("xai", "@b", "https://x.com/b/status/2"),
            ],
            "The returned posts discuss gaming mice. None directly reference Game-TARS.",
        );
        let b = blend_one(Mode::Social, "game-tars input model", lane);
        let r = &b.response;
        assert!(
            r.results.iter().all(|x| x.source != "xai"),
            "{:?}",
            hosts(r)
        );
        assert!(r.results.iter().any(|x| x.source == "youtube"));
        assert!(r.answers.is_empty());
        assert!(r
            .metadata
            .notes
            .iter()
            .any(|n| n.contains("X found no posts")));
        // The call was made and is still paid for and reported.
        assert_eq!(r.metadata.cost.as_ref().unwrap().by_provider["xai"], 45_000);
        assert_eq!(b.ran, [Mode::Social]);

        // The typed verdict (own key): X's summary arrives with no row.
        let lane = social_answer(
            vec![post("youtube", "Talk", "https://www.youtube.com/watch?v=1")],
            "The posts are about unrelated incidents.",
        );
        let b = blend_one(Mode::Social, "shade of eranikus", lane);
        assert!(b.response.answers.is_empty());
        assert!(b
            .response
            .metadata
            .notes
            .iter()
            .any(|n| n.contains("X found no posts")));
        assert!(b.response.results.iter().any(|x| x.source == "youtube"));

        // A summary that stands by its posts keeps them, whatever else it
        // says is missing.
        let lane = social_answer(
            vec![post("xai", "@a", "https://x.com/a/status/1")],
            "Users report the desk is steady. None mention the price.",
        );
        let b = blend_one(Mode::Social, "uplift desk wobble", lane);
        assert!(b.response.results.iter().any(|x| x.source == "xai"));
        assert_eq!(b.response.answers.len(), 1);
    }

    /// One row per X author, none from Grok itself, and a clip almost
    /// nobody watched goes after the others.
    #[test]
    fn social_rows_are_one_per_author_with_watched_clips_first() {
        let mut quiet = post("youtube", "Quiet", "https://www.youtube.com/watch?v=q");
        quiet.extra = Some(serde_json::json!({"views": 12}));
        let mut loud = post("youtube", "Loud", "https://www.youtube.com/watch?v=l");
        loud.extra = Some(serde_json::json!({"views": 137_000}));
        let lane = social_answer(
            vec![
                quiet,
                post("xai", "@a", "https://x.com/a/status/1"),
                post("xai", "@grok", "https://x.com/grok/status/9"),
                post("xai", "@A", "https://x.com/a/status/2"),
                loud,
            ],
            "Users discuss the desk.",
        );
        let b = blend_one(Mode::Social, "uplift desk", lane);
        let got: Vec<&str> = b
            .response
            .results
            .iter()
            .filter(|x| x.source != "serper")
            .map(|x| x.title.as_str())
            .collect();
        assert_eq!(got, ["@a", "Loud", "Quiet"]);
    }

    /// Reddit leads the community rows; a Hacker News row is kept only when
    /// it holds half of the query's key words, and a cross-post appears
    /// once.
    #[test]
    fn community_rows_lead_with_reddit_and_drop_loose_hacker_news() {
        let mut on = post(
            "hackernews",
            "Rapamycin dosing in mice",
            "https://news.ycombinator.com/item?id=1",
        );
        on.snippet = "weekly dosing and side effects".to_string();
        // Half of the key words, but not the subject.
        let mut off = post(
            "hackernews",
            "Show HN: a dosing pump",
            "https://news.ycombinator.com/item?id=2",
        );
        off.snippet = "dosing with no side effects".to_string();
        // The subject and the next word are enough.
        let mut two = post(
            "hackernews",
            "Rapamycin dosing",
            "https://news.ycombinator.com/item?id=3",
        );
        two.snippet = "a thread".to_string();
        let lane = answered(
            Mode::Community,
            "reddit",
            0,
            "estimate",
            vec![
                on,
                off,
                two,
                post(
                    "reddit",
                    "Rapamycin: my protocol",
                    "https://www.reddit.com/r/a/comments/1/x/",
                ),
                post(
                    "reddit",
                    "rapamycin: my protocol",
                    "https://www.reddit.com/r/b/comments/2/x/",
                ),
                post(
                    "reddit",
                    "Side effects?",
                    "https://www.reddit.com/r/a/comments/3/x/",
                ),
            ],
        );
        let b = blend_one(
            Mode::Community,
            "rapamycin dosing protocol side effects",
            lane,
        );
        let got: Vec<&str> = b
            .response
            .results
            .iter()
            .filter(|x| x.source != "serper")
            .map(|x| x.title.as_str())
            .collect();
        assert_eq!(
            got,
            [
                "Rapamycin: my protocol",
                "Side effects?",
                "Rapamycin dosing in mice",
                "Rapamycin dosing"
            ]
        );
        assert_eq!(
            key_words("What is the best 27 inch monitor for you"),
            ["inch", "monitor"]
        );
    }

    /// Two timeouts running rest X for ten minutes; an answer clears the
    /// record; a search that did not ask X, or that X refused at once,
    /// changes nothing.
    #[test]
    fn x_rests_after_two_timeouts_and_returns_on_an_answer() {
        let clean = cache::Strikes::default();
        let one = next_strikes(clean, Some(false), 1_000).unwrap();
        assert_eq!((one.count, one.at), (1, 1_000));
        assert!(!x_resting(one, 1_001));
        let two = next_strikes(one, Some(false), 1_100).unwrap();
        assert!(x_resting(two, 1_101));
        assert!(x_resting(two, 1_100 + 599));
        assert!(!x_resting(two, 1_100 + 600), "asked again after the rest");
        // Still down when asked again: rested for as long again.
        let three = next_strikes(two, Some(false), 1_800).unwrap();
        assert!(x_resting(three, 1_801));
        assert_eq!(next_strikes(three, Some(true), 2_500), Some(clean));
        assert_eq!(next_strikes(clean, Some(true), 2_500), None);
        assert_eq!(next_strikes(two, None, 2_500), None);

        let timeout = |category| ProviderFailure {
            provider: "xai".to_string(),
            category,
            http_status: None,
            code: "http_error".to_string(),
            reason: "error sending request".to_string(),
            retryable: true,
        };
        // X timed out beside a platform that answered.
        let mut r = answered(Mode::Social, "youtube", 1_880, "estimate", Vec::new()).unwrap();
        r.metadata
            .provider_failures
            .push(timeout(FailureCategory::Timeout));
        assert_eq!(x_answered(&Ok(r)), Some(false));
        // X answered.
        assert_eq!(
            x_answered(&answered(
                Mode::Social,
                "xai",
                45_000,
                "estimate",
                Vec::new()
            )),
            Some(true)
        );
        // At capacity (429): refused at once, nothing was waited for.
        let mut r = answered(Mode::Social, "youtube", 1_880, "estimate", Vec::new()).unwrap();
        r.metadata
            .provider_failures
            .push(timeout(FailureCategory::RateLimit));
        assert_eq!(x_answered(&Ok(r)), None);
        // The whole social search failed on X's timeout.
        let failed: Run = Err(SearchError::AllProvidersFailed {
            failed: vec![timeout(FailureCategory::Timeout)],
            cost: None,
        });
        assert_eq!(x_answered(&failed), Some(false));
    }

    /// A post with nothing to read is left out: X's empty post, a clip
    /// with no caption (its title is only "@name on TikTok").
    #[test]
    fn posts_with_nothing_to_read_are_left_out() {
        let mut empty = row("xai", "https://x.com/a/status/1");
        empty.title = "Post on X".to_string();
        empty.snippet = String::new();
        let mut clip = row("tiktok", "https://www.tiktok.com/@a/video/2");
        clip.title = "@a on TikTok".to_string();
        clip.snippet = "160 views · 1 likes".to_string();
        let good = row("xai", "https://x.com/b/status/3");
        let b = merge(
            "q",
            Mode::General,
            10,
            answered(
                Mode::General,
                "serper",
                1_000,
                "estimate",
                rows("serper", "web.example", 2),
            ),
            vec![(
                Mode::Social,
                answered(
                    Mode::Social,
                    "xai",
                    45_000,
                    "estimate",
                    vec![empty, clip, good],
                ),
            )],
            Duration::ZERO,
            false,
        )
        .unwrap();
        let urls: Vec<&str> = b.response.results.iter().map(|r| r.url.as_str()).collect();
        assert!(urls.contains(&"https://x.com/b/status/3"), "{urls:?}");
        assert_eq!(urls.len(), 3, "{urls:?}");
    }

    /// Pages that did not answer plus posts that came back empty is the
    /// pages' error, not "no results": no web engine stays a setup error.
    #[test]
    fn empty_posts_do_not_hide_that_the_pages_did_not_answer() {
        let empty = || answered(Mode::Community, "reddit", 0, "estimate", Vec::new());
        let err = merge(
            "q",
            Mode::General,
            4,
            Err(SearchError::NoProviders("general".to_string())),
            vec![(Mode::Community, empty())],
            Duration::ZERO,
            false,
        )
        .err()
        .unwrap();
        assert!(matches!(err, SearchError::NoProviders(_)), "{err}");
        let err = merge(
            "q",
            Mode::General,
            4,
            all_failed("serper", None),
            vec![(Mode::Community, empty())],
            Duration::ZERO,
            false,
        )
        .err()
        .unwrap();
        assert!(
            matches!(err, SearchError::AllProvidersFailed { .. }),
            "{err}"
        );
    }

    /// Logged in, a failed provider that has a key variable may have been
    /// reached through metasearch.sh: its transport is left out, not guessed.
    #[test]
    fn a_failed_keyed_provider_has_no_guessed_transport_when_logged_in() {
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
            vec![(Mode::Social, all_failed("tiktok", None))],
            Duration::ZERO,
            true,
        )
        .unwrap();
        assert!(!b.response.metadata.transport.contains_key("tiktok"));
        // A provider's own timeout is not "still running at the deadline".
        assert!(b.response.metadata.providers_timed_out.is_empty());
    }
}
