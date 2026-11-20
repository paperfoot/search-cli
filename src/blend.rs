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
use crate::providers::{self, xai};
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
    // A record dated after now (the clock was set back) rests nothing.
    strikes.count >= X_STRIKES && strikes.at <= now && now - strikes.at < X_REST.as_secs()
}

/// Whether X answered in the added social search: `Some(false)` when it
/// ran out of time, `Some(true)` when it answered (with posts or with
/// none: an answer with no rows is not in `provider_results`), `None` when
/// it was not asked or failed some other way (a refused key or a 429
/// comes back at once and costs nothing to try again).
fn x_answered(run: &Run) -> Option<bool> {
    let failure = |failures: &[ProviderFailure]| {
        failures
            .iter()
            .find(|f| f.provider == "xai")
            .map(|f| f.category)
    };
    match run {
        Ok(r) => {
            let m = &r.metadata;
            match failure(&m.provider_failures) {
                Some(FailureCategory::Timeout) => Some(false),
                Some(_) => None,
                None if m.providers_timed_out.iter().any(|p| p == "xai") => Some(false),
                None => m
                    .providers_queried
                    .iter()
                    .any(|p| p == "xai")
                    .then_some(true),
            }
        }
        Err(SearchError::AllProvidersFailed { failed, .. }) => {
            (failure(failed) == Some(FailureCategory::Timeout)).then_some(false)
        }
        Err(_) => None,
    }
}

/// The record after this search, when it changes. Timeouts count as
/// running only within twice the rest of each other: the search that asks
/// X again after a rest extends it on a timeout, and two timeouts a month
/// apart are two separate ones.
fn next_strikes(prev: cache::Strikes, answered: Option<bool>, now: u64) -> Option<cache::Strikes> {
    let running = prev.at <= now && now - prev.at < 2 * X_REST.as_secs();
    match answered {
        Some(false) => Some(cache::Strikes {
            count: if running {
                prev.count.saturating_add(1)
            } else {
                1
            },
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
    // A mode with no rows to give is not run: a search for one row is
    // answered by one page, and X would be paid for posts nobody sees.
    let share = shares(count, lanes.len());
    let mut lanes: Vec<Mode> = lanes
        .iter()
        .zip(&share[1..])
        .filter(|(_, rows)| **rows > 0)
        .map(|(m, _)| *m)
        .collect();
    // X resting: the social search asks the other platform of its pick
    // alone, or is not run when X was all of it (breaking news).
    let strikes = cache::x_strikes();
    let resting = lanes.contains(&Mode::Social) && x_resting(strikes, cache::now());
    let without_x: Vec<String> = router::pick_social_platforms(&posts_query)
        .into_iter()
        .filter(|p| *p != "xai")
        .map(str::to_string)
        .collect();
    if resting && without_x.is_empty() {
        lanes.retain(|m| *m != Mode::Social);
    }
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
    let mut added: Vec<(Mode, Run)> = lanes
        .iter()
        .copied()
        .zip([a, b].into_iter().flatten())
        .collect();
    if resting {
        // Without X the platform alone may have no key here: nothing to
        // report beyond the note that X was left out.
        added.retain(|(m, run)| {
            *m != Mode::Social || matches!(run, Ok(_) | Err(SearchError::AllProvidersFailed { .. }))
        });
    } else {
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
/// clip with no caption (its title is then the platform's own stand-in,
/// "@name on TikTok"), or a Hacker News item marked dead. Such a row would
/// take a place and say nothing. A real title that happens to end "on
/// TikTok" is kept, whatever its source.
fn hollow(row: &SearchResult) -> bool {
    if row.snippet.trim().is_empty() {
        return true;
    }
    let author = row
        .extra
        .as_ref()
        .and_then(|e| e.get("author"))
        .and_then(|a| a.as_str())
        .unwrap_or_default();
    let stand_in = |platform: &str| row.title == format!("{author} on {platform}");
    match row.source.as_str() {
        "tiktok" => stand_in("TikTok"),
        "instagram" => stand_in("Instagram"),
        "threads" => stand_in("Threads"),
        "hackernews" => row.title.contains("[dead]") || row.title.contains("[flagged]"),
        _ => false,
    }
}

/// How X's summary reads when none of its posts are about the query ("No
/// relevant posts were returned"). Read only for X through metasearch.sh,
/// whose reply carries no typed verdict yet; with your own key the
/// provider asks X outright (`xai::OFF_TOPIC`). Only wording that says
/// nothing was found: "none of the posts mention the price" also stands
/// over posts that are on the subject.
const DISOWNED: &[&str] = &[
    "no relevant posts",
    "no relevant x posts",
    "no posts matching",
    "no posts relevant",
    "no apparent connection",
];

/// Words too common to tell whether a row is about the query.
const COMMON_WORDS: &[&str] = &[
    "the", "and", "for", "with", "how", "what", "why", "who", "are", "was", "can", "does", "not",
    "from", "that", "this", "you", "your", "about", "into", "best", "new", "should", "would",
    "could", "will", "when", "where", "which", "have", "has", "had", "than", "then", "there",
    "their", "they", "them", "these", "those", "any", "all", "but", "get", "use", "using", "need",
    "want", "just", "still", "much", "many", "more", "most", "some",
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
///   is typed (`on_topic` in the provider's request); through
///   metasearch.sh it is read from the summary ([`DISOWNED`]).
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
            // X's verdict. With your own key it is typed: the provider
            // marks the answer (`xai::OFF_TOPIC`) and returns no rows.
            // Through metasearch.sh it is read from the summary's wording.
            let hosted = r
                .metadata
                .transport
                .get("xai")
                .is_some_and(|t| t == "hosted");
            let disowned = r.answers.iter().filter(|a| a.provider == "xai").any(|a| {
                a.text.starts_with(xai::OFF_TOPIC) || {
                    let text = a.text.to_lowercase();
                    hosted && DISOWNED.iter().any(|p| text.contains(p))
                }
            });
            r.results.retain(|row| !hollow(row));
            if disowned {
                r.answers.retain(|a| a.provider != "xai");
                r.results.retain(|row| row.source != "xai");
                notes.push("X found no posts about this query".to_string());
            }
            let mut authors: HashSet<String> = HashSet::new();
            r.results.retain(|row| {
                // A row with no author is titled "Post on X": not one author.
                row.source != "xai"
                    || !row.title.starts_with('@')
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

/// One envelope from the pages run and the added modes' runs. The pages
/// decide: when their run fails, its error is the search's, with what the
/// added modes spent counted in. An added mode that could not run or
/// failed never fails the search: it leaves a note or its
/// `provider_failures`.
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
    // The pages decide the search. When no web engine answers, it fails
    // exactly as it would without posts: the pages' own failures, so the
    // same exit code and advice, and nothing to cache. Posts are an
    // addition, never a stand-in: an empty balance must not read as a
    // working search because Reddit is free, and a rate-limited X must
    // not turn "top up" into "retry". What the added modes spent is
    // still counted.
    let pages = match pages {
        Ok(r) => r,
        Err(SearchError::AllProvidersFailed { failed, cost }) => {
            t.costs.extend(cost);
            for (_, run) in added {
                match run {
                    Ok(r) => t.costs.extend(r.metadata.cost),
                    Err(SearchError::AllProvidersFailed { cost, .. }) => t.costs.extend(cost),
                    Err(_) => {}
                }
            }
            return Err(SearchError::AllProvidersFailed {
                failed,
                cost: t.cost(),
            });
        }
        Err(e) => return Err(e),
    };
    let routing = pages.metadata.routing.clone().unwrap_or_default();
    let mut parts: Vec<Vec<SearchResult>> = vec![t.take(pages)];
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

    /// The pages decide: when no web engine answers, the search fails with
    /// the pages' own failures (their exit code, their advice), whatever
    /// the added modes did, and what those spent is still counted.
    #[test]
    fn failed_pages_fail_the_search_whatever_the_posts_did() {
        let billing = failure("metasearch", FailureCategory::BillingQuota);
        // The balance is empty: Reddit answering for free is not a result.
        let err = merge(
            "q",
            Mode::General,
            4,
            Err(SearchError::AllProvidersFailed {
                failed: vec![billing.clone()],
                cost: None,
            }),
            vec![
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
                (
                    Mode::Social,
                    Err(SearchError::AllProvidersFailed {
                        failed: vec![failure("xai", FailureCategory::RateLimit)],
                        cost: Some(CostSummary::with_basis(
                            BTreeMap::from([("xai".to_string(), 45_000)]),
                            "estimate",
                        )),
                    }),
                ),
            ],
            Duration::ZERO,
            true,
        )
        .err()
        .unwrap();
        assert_eq!(err.exit_code(), 2, "top up, not retry: {err}");
        match err {
            SearchError::AllProvidersFailed { failed, cost } => {
                assert_eq!(failed.len(), 1, "the pages' failures only");
                assert_eq!(failed[0].category, FailureCategory::BillingQuota);
                assert_eq!(cost.unwrap().micros, 45_000, "X was still paid for");
            }
            other => panic!("{other}"),
        }
        // No web engine at all, or a bad request: the pages' error as it is.
        let err = merge(
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
                    rows("reddit", "reddit.com", 3),
                ),
            )],
            Duration::ZERO,
            false,
        )
        .err()
        .unwrap();
        assert!(matches!(err, SearchError::NoProviders(_)), "{err}");
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
    /// rows stay. With your own key the verdict is typed; the summary's
    /// wording is read only for X through metasearch.sh.
    #[test]
    fn posts_x_calls_unrelated_are_left_out() {
        let x_rows = || {
            vec![
                post("xai", "@a", "https://x.com/a/status/1"),
                post("youtube", "Talk", "https://www.youtube.com/watch?v=1"),
                post("xai", "@b", "https://x.com/b/status/2"),
            ]
        };
        let has_x = |b: &Blend| b.response.results.iter().any(|x| x.source == "xai");
        let noted = |b: &Blend| {
            b.response
                .metadata
                .notes
                .iter()
                .any(|n| n.contains("X found no posts"))
        };

        // Own key, typed: the provider marks the answer and sends no rows.
        let lane = social_answer(
            vec![post("youtube", "Talk", "https://www.youtube.com/watch?v=1")],
            &format!(
                "{} The posts are about unrelated incidents.",
                xai::OFF_TOPIC
            ),
        );
        let b = blend_one(Mode::Social, "obscure boss mechanic", lane);
        assert!(b.response.answers.is_empty() && noted(&b) && !has_x(&b));
        assert!(b.response.results.iter().any(|x| x.source == "youtube"));
        // The call was made and is still paid for and reported.
        let cost = b.response.metadata.cost.as_ref().unwrap();
        assert_eq!(cost.by_provider["xai"], 45_000);
        assert_eq!(b.ran, [Mode::Social]);

        // Own key, on topic: wording never overrides the typed verdict.
        for summary in [
            "Users report wobble at full height. None of the posts mention the price.",
            "No relevant posts were returned on pricing, but several cover the wobble.",
        ] {
            let b = blend_one(
                Mode::Social,
                "uplift desk wobble",
                social_answer(x_rows(), summary),
            );
            assert!(has_x(&b) && !noted(&b), "{summary}");
            assert_eq!(b.response.answers.len(), 1);
        }
        // A summary whose posts were not listed is kept as the answer.
        let b = blend_one(
            Mode::Social,
            "uplift desk wobble",
            social_answer(Vec::new(), "Users report wobble at full height."),
        );
        assert_eq!(b.response.answers.len(), 1);
        assert!(!noted(&b));

        // Through metasearch.sh there is no typed verdict: wording that
        // says nothing was found drops X; softer wording does not.
        let hosted = |summary: &str| {
            let mut r = social_answer(x_rows(), summary).unwrap();
            r.metadata
                .transport
                .insert("xai".to_string(), "hosted".to_string());
            blend_one(Mode::Social, "acme x200 firmware", Ok(r))
        };
        let b = hosted("No relevant posts were returned by the search for the Acme X200.");
        assert!(!has_x(&b) && noted(&b) && b.response.answers.is_empty());
        assert!(b.response.results.iter().any(|x| x.source == "youtube"));
        let b = hosted("Users like the desk. None of the posts mention the price.");
        assert!(has_x(&b) && !noted(&b));
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
                // Two posts with no author are two posts.
                post("xai", "Post on X", "https://x.com/i/status/7"),
                post("xai", "Post on X", "https://x.com/i/status/8"),
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
        assert_eq!(got, ["@a", "Post on X", "Post on X", "Loud"]);
    }

    /// Reddit leads the community rows; a Hacker News row is kept only when
    /// it holds half of the query's key words, and a cross-post appears
    /// once.
    #[test]
    fn community_rows_lead_with_reddit_and_drop_loose_hacker_news() {
        let mut on = post(
            "hackernews",
            "Creatine loading in athletes",
            "https://news.ycombinator.com/item?id=1",
        );
        on.snippet = "weekly loading and side effects".to_string();
        // Half of the key words, but not the subject.
        let mut off = post(
            "hackernews",
            "Show HN: a loading spinner",
            "https://news.ycombinator.com/item?id=2",
        );
        off.snippet = "loading with no side effects".to_string();
        // The subject and the next word are enough.
        let mut two = post(
            "hackernews",
            "Creatine loading",
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
                    "Creatine: my protocol",
                    "https://www.reddit.com/r/a/comments/1/x/",
                ),
                post(
                    "reddit",
                    "creatine: my protocol",
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
            "creatine loading protocol side effects",
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
                "Creatine: my protocol",
                "Side effects?",
                "Creatine loading in athletes",
                "Creatine loading"
            ]
        );
        assert_eq!(
            key_words("What is the best 27 inch monitor for you"),
            ["inch", "monitor"]
        );
        // A question word is not the subject.
        assert_eq!(
            key_words("should I learn rust or go in 2026"),
            ["learn", "rust"]
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
        // Two timeouts a month apart are not two running.
        let later = next_strikes(one, Some(false), 1_000 + 30 * 86_400).unwrap();
        assert_eq!(later.count, 1);
        // A record from the future (the clock was set back) rests nothing
        // and does not count as the first of two.
        let ahead = cache::Strikes {
            count: 5,
            at: 9_000,
        };
        assert!(!x_resting(ahead, 1_000));
        assert_eq!(next_strikes(ahead, Some(false), 1_000).unwrap().count, 1);

        let x_failed = |category| failure("xai", category);
        let social = |queried: &[&str]| {
            let mut r = answered(Mode::Social, "youtube", 1_880, "estimate", Vec::new()).unwrap();
            r.metadata.providers_queried = queried.iter().map(|p| p.to_string()).collect();
            r
        };
        // X timed out beside a platform that answered.
        let mut r = social(&["xai", "youtube"]);
        r.metadata
            .provider_failures
            .push(x_failed(FailureCategory::Timeout));
        assert_eq!(x_answered(&Ok(r)), Some(false));
        // X answered with no rows (off topic): it is asked for, has no
        // failure and no entry in provider_results. Still an answer.
        let r = social(&["xai", "youtube"]);
        assert!(!r.metadata.provider_results.contains_key("xai"));
        assert_eq!(x_answered(&Ok(r)), Some(true));
        // X was not asked.
        assert_eq!(x_answered(&Ok(social(&["youtube"]))), None);
        // At capacity (429): refused at once, nothing was waited for.
        let mut r = social(&["xai", "youtube"]);
        r.metadata
            .provider_failures
            .push(x_failed(FailureCategory::RateLimit));
        assert_eq!(x_answered(&Ok(r)), None);
        // The whole social search failed on X's timeout.
        let failed: Run = Err(SearchError::AllProvidersFailed {
            failed: vec![x_failed(FailureCategory::Timeout)],
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
        clip.extra = Some(serde_json::json!({"author": "@a"}));
        // Real titles that end the same way are kept.
        let mut video = row("youtube", "https://www.youtube.com/watch?v=4");
        video.title = "How I got 1M views on TikTok".to_string();
        let mut own = row("tiktok", "https://www.tiktok.com/@c/video/5");
        own.title = "my first week on TikTok".to_string();
        own.extra = Some(serde_json::json!({"author": "@c"}));
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
                    vec![empty, clip, good, video, own],
                ),
            )],
            Duration::ZERO,
            false,
        )
        .unwrap();
        let urls: Vec<&str> = b.response.results.iter().map(|r| r.url.as_str()).collect();
        assert!(urls.contains(&"https://x.com/b/status/3"), "{urls:?}");
        assert!(
            urls.contains(&"https://www.youtube.com/watch?v=4"),
            "{urls:?}"
        );
        assert!(
            urls.contains(&"https://www.tiktok.com/@c/video/5"),
            "{urls:?}"
        );
        assert_eq!(urls.len(), 5, "{urls:?}");
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
