//! `reddit`: Reddit search and threads, keyless.
//!
//! - **Live:** Reddit's own server-rendered partials (`/svc/shreddit/...`),
//!   fetched from this machine with a Chrome TLS fingerprint (wreq), then a
//!   Safari one when the first is refused. A 200 that is a captcha page is a
//!   failure, never content. Requests are paced by a token bucket shared by
//!   every `search` process on the machine (about 10 a minute); after a
//!   captcha the live route rests for 7 minutes.
//! - **Archive:** Arctic Shift (`arctic-shift.photon-reddit.com`), which
//!   keeps every thread's full comment tree. A thread extract reads both at
//!   once: the archive gives the post and the whole tree, the live page the
//!   current scores of the top comments. When the live route fails, the
//!   archive alone serves it. Archive search needs a subreddit
//!   (`r/<name>` in the query).
//!
//! - **Global search fallback:** when the live route refuses a search that
//!   names no subreddit, the social platforms' upstream (ScrapeCreators'
//!   Reddit search) answers it: directly with `SCRAPECREATORS_API_KEY`, else
//!   through metasearch.sh when logged in and the server offers it.
//!
//! `SEARCH_REDDIT_ROUTE=archive` skips the live route; `live` skips the
//! archive.

use super::spec::{FilterSupport, Price};
use super::thread::{self, Comment, Post};
use super::{Adapter, Call, Output, ProviderSpec};
use crate::context::AppContext;
use crate::errors::SearchError;
use crate::types::{FailureCategory, Mode, SearchResult};
use async_trait::async_trait;
use dom_query::Document;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const LIVE: &str = "https://www.reddit.com";
const ARCHIVE: &str = "https://arctic-shift.photon-reddit.com/api";

pub static SPEC: ProviderSpec = ProviderSpec {
    name: "reddit",
    env_keys: &[],
    modes: &[Mode::Community, Mode::Extract],
    filters: FilterSupport {
        freshness: false,
        domains: false,
        country: false,
        lang: false,
    },
    max_count: 25,
    timeout: Duration::from_secs(20),
    billed_on_send: false,
    price: Price {
        per_call: 0,
        per_extra_result: 0,
        included_results: 0,
    },
};

pub struct Reddit {
    ctx: Arc<AppContext>,
}

impl Reddit {
    pub fn new(ctx: Arc<AppContext>) -> Self {
        Self { ctx }
    }
}

// ---------------------------------------------------------------------------
// Thread URLs

/// `(subreddit, post id)` of a Reddit thread URL:
/// `reddit.com/r/<sub>/comments/<id>/...` on www, old, new, np or m.
pub fn thread_ref(url: &str) -> Option<(String, String)> {
    let u = url::Url::parse(url.trim()).ok()?;
    let host = u.host_str()?.to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    let host = ["old.", "new.", "np.", "m."]
        .iter()
        .find_map(|p| host.strip_prefix(p))
        .unwrap_or(host);
    if host != "reddit.com" {
        return None;
    }
    let parts: Vec<&str> = u.path().split('/').filter(|s| !s.is_empty()).collect();
    match parts.as_slice() {
        ["r", sub, "comments", id, ..] if valid_sub(sub) && valid_id(id) => {
            Some((sub.to_string(), id.to_ascii_lowercase()))
        }
        _ => None,
    }
}

fn valid_sub(s: &str) -> bool {
    (2..=21).contains(&s.len()) && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn valid_id(s: &str) -> bool {
    (1..=12).contains(&s.len()) && s.chars().all(|c| c.is_ascii_alphanumeric())
}

/// An `r/<name>` named in a search query, and the query without it.
fn subreddit_in(query: &str) -> Option<(String, String)> {
    let words: Vec<&str> = query.split_whitespace().collect();
    let at = words.iter().position(|w| {
        let w = w.trim_start_matches('/');
        w.len() > 2
            && w[..2].eq_ignore_ascii_case("r/")
            && valid_sub(w[2..].trim_end_matches(['/', ',', '.']))
    })?;
    let sub = words[at].trim_start_matches('/')[2..]
        .trim_end_matches(['/', ',', '.'])
        .to_string();
    let rest: Vec<&str> = words
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != at)
        .map(|(_, w)| *w)
        .collect();
    Some((sub, rest.join(" ")))
}

// ---------------------------------------------------------------------------
// Pacing: a token bucket on disk, shared by every search process.

/// Burst allowance; Reddit served a captcha after 14 requests in 10 s.
const BURST: f64 = 5.0;
/// Sustained rate: 10 requests a minute held for an hour in testing.
const PER_SEC: f64 = 10.0 / 60.0;
/// How long the live route rests after a captcha (it cleared in about 7 min).
const COOLDOWN_SECS: f64 = 420.0;

#[derive(Debug, Clone, Copy, PartialEq, Deserialize, serde::Serialize)]
pub struct Bucket {
    pub tokens: f64,
    /// Unix seconds of the last update.
    pub at: f64,
    /// Unix seconds until which the live route rests (after a captcha).
    #[serde(default)]
    pub cooldown_until: f64,
}

impl Bucket {
    pub fn full(now: f64) -> Self {
        Self {
            tokens: BURST,
            at: now,
            cooldown_until: 0.0,
        }
    }

    /// Take one token at `now`, or say how long until one is free.
    pub fn take(&mut self, now: f64) -> Result<(), Duration> {
        let elapsed = (now - self.at).max(0.0);
        self.tokens = (self.tokens + elapsed * PER_SEC).min(BURST);
        self.at = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            Ok(())
        } else {
            Err(Duration::from_secs_f64((1.0 - self.tokens) / PER_SEC))
        }
    }

    pub fn resting(&self, now: f64) -> Option<Duration> {
        (self.cooldown_until > now).then(|| Duration::from_secs_f64(self.cooldown_until - now))
    }
}

static PACE: Mutex<()> = Mutex::new(());

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

fn pace_path() -> std::path::PathBuf {
    crate::cache::cache_dir().join("reddit-pace.json")
}

fn load_bucket(now: f64) -> Bucket {
    std::fs::read_to_string(pace_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| Bucket::full(now))
}

fn save_bucket(b: &Bucket) {
    let path = pace_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = path.with_extension("pace");
    if let Ok(text) = serde_json::to_string(b) {
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

/// Wait for a live-request token, at most `max_wait`. `Err` says why not:
/// resting after a captcha, or the next token is too far off.
async fn acquire(max_wait: Duration) -> Result<(), String> {
    loop {
        let wait = {
            let _guard = PACE.lock().unwrap_or_else(|e| e.into_inner());
            let now = now_secs();
            let mut b = load_bucket(now);
            if let Some(rest) = b.resting(now) {
                return Err(format!(
                    "reddit served a captcha recently; live pages rest for {} more s",
                    rest.as_secs()
                ));
            }
            match b.take(now) {
                Ok(()) => {
                    save_bucket(&b);
                    return Ok(());
                }
                Err(wait) => wait,
            }
        };
        if wait > max_wait {
            return Err(format!(
                "paced: about 10 Reddit requests a minute; the next is free in {} s",
                wait.as_secs() + 1
            ));
        }
        tokio::time::sleep(wait).await;
    }
}

fn start_cooldown() {
    let _guard = PACE.lock().unwrap_or_else(|e| e.into_inner());
    let now = now_secs();
    let mut b = load_bucket(now);
    b.cooldown_until = now + COOLDOWN_SECS;
    b.tokens = 0.0;
    b.at = now;
    save_bucket(&b);
}

// ---------------------------------------------------------------------------
// Live fetches

/// The TLS fingerprints the live route tries, in order. Reddit's edge
/// blocks some fingerprints outright (403) and not others; a captcha is per
/// IP, so only a 403 moves on to the next profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    #[cfg_attr(not(feature = "stealth"), allow(dead_code))]
    Chrome,
    #[cfg_attr(not(feature = "stealth"), allow(dead_code))]
    Safari,
    /// Built without the `stealth` feature: the plain HTTP client.
    #[cfg_attr(feature = "stealth", allow(dead_code))]
    Plain,
}

pub fn live_profiles() -> &'static [Profile] {
    #[cfg(feature = "stealth")]
    {
        &[Profile::Chrome, Profile::Safari]
    }
    #[cfg(not(feature = "stealth"))]
    {
        &[Profile::Plain]
    }
}

/// What a live response turned out to be.
#[derive(Debug, PartialEq, Eq)]
pub enum Live {
    /// The page holds the expected elements.
    Content(String),
    /// A real page with none of them (no comments, no hits).
    Empty,
    /// A captcha or interstitial, whatever the status.
    Challenge,
    /// Refused outright by status (403, 429, ...).
    Refused(u16),
}

/// Judge a live response. `marker` is an element every real page of this
/// kind contains when it has anything to show.
pub fn judge(status: u16, body: &str, marker: &str) -> Live {
    if status == 200 && body.contains(marker) {
        return Live::Content(body.to_string());
    }
    let lower = body.to_ascii_lowercase();
    let captcha = [
        "recaptcha",
        "g-recaptcha",
        "hcaptcha",
        "blocked by network security",
        "whoa there, pardner",
        "verify you are human",
        "prove your humanity",
    ]
    .iter()
    .any(|m| lower.contains(m));
    match status {
        200 if captcha => Live::Challenge,
        200 => Live::Empty,
        s => Live::Refused(s),
    }
}

async fn get_live(
    ctx: &AppContext,
    profile: Profile,
    url: &str,
    timeout: Duration,
) -> Result<(u16, String), String> {
    match profile {
        #[cfg(feature = "stealth")]
        Profile::Chrome | Profile::Safari => {
            let emulation = match profile {
                Profile::Safari => wreq_util::Profile::Safari26,
                _ => wreq_util::Profile::Chrome149,
            };
            let client = wreq::Client::builder()
                .emulation(emulation)
                .cookie_store(true)
                .connect_timeout(Duration::from_secs(5))
                .build()
                .map_err(|e| e.to_string())?;
            let fut = async {
                let resp = client.get(url).send().await.map_err(|e| e.to_string())?;
                let status = resp.status().as_u16();
                let body = resp.text().await.map_err(|e| e.to_string())?;
                Ok::<_, String>((status, body))
            };
            tokio::time::timeout(timeout, fut)
                .await
                .map_err(|_| "timed out".to_string())?
        }
        _ => {
            let resp = ctx
                .client
                .get(url)
                .timeout(timeout)
                .send()
                .await
                .map_err(|e| e.without_url().to_string())?;
            let status = resp.status().as_u16();
            let body = resp.text().await.map_err(|e| e.to_string())?;
            Ok((status, body))
        }
    }
}

/// One live page through the profiles, paced. `Ok(None)` is a real page
/// with nothing on it.
async fn live_page(
    ctx: &AppContext,
    url: &str,
    marker: &str,
    timeout: Duration,
) -> Result<Option<String>, String> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut why = String::new();
    for profile in live_profiles() {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        acquire(
            left.saturating_sub(Duration::from_secs(3))
                .min(Duration::from_secs(8)),
        )
        .await?;
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return Err("timed out".into());
        }
        let (status, body) = get_live(ctx, *profile, url, left).await?;
        match judge(status, &body, marker) {
            Live::Content(body) => return Ok(Some(body)),
            Live::Empty => return Ok(None),
            Live::Challenge => {
                start_cooldown();
                return Err(
                    "challenge: reddit served a captcha page; live pages rest for 7 min".into(),
                );
            }
            Live::Refused(s) => why = format!("HTTP {s} from reddit ({profile:?} fingerprint)"),
        }
        if !matches!(why.as_str(), w if w.starts_with("HTTP 403")) {
            break;
        }
    }
    Err(why)
}

/// Which routes a call may use, from `SEARCH_REDDIT_ROUTE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Routes {
    pub live: bool,
    pub archive: bool,
}

pub fn routes_from(value: Option<&str>) -> Routes {
    match value.map(|v| v.trim().to_ascii_lowercase()).as_deref() {
        Some("archive") => Routes {
            live: false,
            archive: true,
        },
        Some("live") => Routes {
            live: true,
            archive: false,
        },
        _ => Routes {
            live: true,
            archive: true,
        },
    }
}

fn routes() -> Routes {
    routes_from(std::env::var("SEARCH_REDDIT_ROUTE").ok().as_deref())
}

// ---------------------------------------------------------------------------
// Parsing the live partials

/// Every `<shreddit-comment>` of a comments partial, in page order.
pub fn parse_live_comments(html: &str) -> Vec<Comment> {
    let doc = Document::from(html);
    let mut out = Vec::new();
    for node in doc.select("shreddit-comment").iter() {
        let attr = |name: &str| node.attr(name).map(|v| v.to_string()).unwrap_or_default();
        let thing = attr("thingid");
        let Some(id) = thing.strip_prefix("t1_").map(str::to_string) else {
            continue;
        };
        let parent = node
            .attr("parentid")
            .and_then(|p| p.strip_prefix("t1_").map(str::to_string));
        let body_sel = format!("div[id=\"{thing}-comment-rtjson-content\"]");
        let body_node = doc.select(&body_sel);
        let body = if body_node.exists() {
            thread::html_to_text(&body_node.html())
        } else {
            "[deleted]".to_string()
        };
        out.push(Comment {
            id,
            parent,
            author: attr("author"),
            score: attr("score").parse().ok(),
            depth: attr("depth").parse().unwrap_or(0),
            created: attr("created").replace("+0000", "Z"),
            body,
        });
    }
    out
}

#[derive(Debug, Deserialize, Default)]
struct Tracking {
    #[serde(default)]
    post: TrackPost,
    #[serde(default)]
    profile: TrackName,
    #[serde(default)]
    subreddit: TrackName,
    #[serde(default)]
    search: TrackSearch,
}

#[derive(Debug, Deserialize, Default)]
struct TrackPost {
    #[serde(default)]
    id: String,
}

#[derive(Debug, Deserialize, Default)]
struct TrackName {
    #[serde(default)]
    name: String,
}

#[derive(Debug, Deserialize, Default)]
struct TrackSearch {
    #[serde(default)]
    snippet: String,
}

/// Posts of a search partial.
pub fn parse_live_search(html: &str) -> Vec<SearchResult> {
    let doc = Document::from(html);
    // The tracking context names the author, subreddit and snippet per post.
    let mut tracking: HashMap<String, Tracking> = HashMap::new();
    for node in doc.select("search-telemetry-tracker").iter() {
        let Some(raw) = node.attr("data-faceplate-tracking-context") else {
            continue;
        };
        if let Ok(t) = serde_json::from_str::<Tracking>(&raw) {
            if !t.post.id.is_empty() {
                let entry = tracking.entry(t.post.id.clone()).or_default();
                if entry.search.snippet.is_empty() {
                    entry.search.snippet = t.search.snippet;
                }
                if entry.profile.name.is_empty() {
                    entry.profile.name = t.profile.name;
                }
                if entry.subreddit.name.is_empty() {
                    entry.subreddit.name = t.subreddit.name;
                }
            }
        }
    }
    let mut out = Vec::new();
    for unit in doc.select("[data-testid=\"sdui-post-unit\"]").iter() {
        let title = unit.select("a[data-testid=\"post-title-text\"]");
        let Some(href) = title.attr("href") else {
            continue;
        };
        let Some((sub, id)) = thread_ref(&format!("{LIVE}{href}")) else {
            continue;
        };
        let numbers: Vec<i64> = unit
            .select("[data-testid=\"search-counter-row\"] faceplate-number")
            .iter()
            .filter_map(|n| n.attr("number").and_then(|v| v.parse().ok()))
            .collect();
        let published = unit
            .select("faceplate-timeago")
            .attr("ts")
            .map(|t| t.replace("+0000", "Z"));
        let t = tracking.remove(&format!("t3_{id}")).unwrap_or_default();
        let subreddit = if t.subreddit.name.is_empty() {
            sub
        } else {
            t.subreddit.name
        };
        let mut extra = serde_json::json!({ "subreddit": subreddit });
        let obj = extra.as_object_mut().expect("object");
        if let Some(score) = numbers.first() {
            obj.insert("score".into(), (*score).into());
        }
        if let Some(n) = numbers.get(1) {
            obj.insert("num_comments".into(), (*n).into());
        }
        if !t.profile.name.is_empty() {
            obj.insert("author".into(), t.profile.name.into());
        }
        let snippet = if t.search.snippet.is_empty() {
            format!("r/{subreddit}")
        } else {
            t.search.snippet
        };
        out.push(SearchResult {
            title: title.text().trim().to_string(),
            url: format!("{LIVE}{href}"),
            snippet: thread::clip(&snippet, 500),
            source: "reddit".into(),
            published,
            image_url: None,
            extra: Some(extra),
        });
    }
    out
}

// ---------------------------------------------------------------------------
// Parsing the archive

#[derive(Debug, Deserialize)]
struct ArchiveList<T> {
    #[serde(default = "Vec::new")]
    data: Vec<T>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ArchivePost {
    #[serde(default)]
    title: String,
    #[serde(default)]
    author: String,
    #[serde(default)]
    score: Option<i64>,
    #[serde(default)]
    num_comments: Option<u64>,
    #[serde(default)]
    subreddit: String,
    #[serde(default)]
    created_utc: Option<f64>,
    #[serde(default)]
    permalink: String,
    #[serde(default)]
    selftext: String,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    is_self: Option<bool>,
}

fn archive_post(p: ArchivePost) -> Post {
    let url = format!("{LIVE}{}", p.permalink);
    let link = p
        .url
        .filter(|u| p.is_self != Some(true) && !u.is_empty() && *u != url);
    Post {
        title: p.title,
        url,
        author: p.author,
        score: p.score,
        num_comments: p.num_comments,
        community: format!("r/{}", p.subreddit),
        created: p
            .created_utc
            .map(|t| thread::iso_from_unix(t as i64))
            .unwrap_or_default(),
        body: if matches!(p.selftext.as_str(), "[removed]" | "[deleted]") {
            p.selftext
        } else {
            thread::html_to_text(&p.selftext)
        },
        link,
    }
}

fn archive_error(e: impl std::fmt::Display) -> String {
    format!("archive: {e}")
}

fn parse_archive_post(body: &str) -> Result<Post, String> {
    let list: ArchiveList<ArchivePost> = serde_json::from_str(body).map_err(archive_error)?;
    if let Some(e) = list.error {
        return Err(archive_error(e));
    }
    list.data
        .into_iter()
        .next()
        .map(archive_post)
        .ok_or_else(|| archive_error("no such post"))
}

#[derive(Debug, Deserialize)]
struct ArchiveNode {
    kind: String,
    data: serde_json::Value,
}

/// Every comment of an archive tree, with parent links; `more` stubs (the
/// rest of a long thread the archive did not send) are counted, not kept.
fn parse_archive_tree(body: &str) -> Result<(Vec<Comment>, u64), String> {
    let list: ArchiveList<ArchiveNode> = serde_json::from_str(body).map_err(archive_error)?;
    if let Some(e) = list.error {
        return Err(archive_error(e));
    }
    let mut out = Vec::new();
    let mut more = 0u64;
    let mut stack: Vec<(ArchiveNode, usize)> =
        list.data.into_iter().rev().map(|n| (n, 0)).collect();
    while let Some((node, depth)) = stack.pop() {
        if node.kind != "t1" {
            more += node.data["count"].as_u64().unwrap_or(0);
            continue;
        }
        let d = node.data;
        let s = |k: &str| d[k].as_str().unwrap_or("").to_string();
        let parent = d["parent_id"]
            .as_str()
            .and_then(|p| p.strip_prefix("t1_"))
            .map(str::to_string);
        out.push(Comment {
            id: s("id"),
            parent,
            author: s("author"),
            score: d["score"].as_i64(),
            depth,
            created: d["created_utc"]
                .as_f64()
                .map(|t| thread::iso_from_unix(t as i64))
                .unwrap_or_default(),
            body: thread::html_to_text(&s("body")),
        });
        if let Some(children) = d["replies"]["data"]["children"].as_array() {
            for child in children.iter().rev() {
                if let Ok(n) = serde_json::from_value::<ArchiveNode>(child.clone()) {
                    stack.push((n, depth + 1));
                }
            }
        }
    }
    Ok((out, more))
}

/// Posts of the global search fallback (`/v1/reddit/search`).
pub fn parse_global_search(body: &serde_json::Value) -> Vec<SearchResult> {
    use super::social_api as api;
    body["posts"]
        .as_array()
        .map(|posts| {
            posts
                .iter()
                .filter_map(|p| {
                    let url = api::non_empty(api::text(p, "/url")).or_else(|| {
                        api::non_empty(api::text(p, "/permalink")).map(|l| format!("{LIVE}{l}"))
                    })?;
                    thread_ref(&url)?;
                    let sub = api::text(p, "/subreddit");
                    let body = thread::html_to_text(&api::text(p, "/selftext"));
                    Some(SearchResult {
                        title: api::text(p, "/title"),
                        url,
                        snippet: if body.is_empty() {
                            format!("r/{sub}")
                        } else {
                            thread::clip(&body, 500)
                        },
                        source: "reddit".into(),
                        published: api::iso(p, "/created_utc"),
                        image_url: None,
                        extra: Some(serde_json::json!({
                            "subreddit": sub,
                            "score": api::num(p, "/score"),
                            "num_comments": api::num(p, "/num_comments"),
                            "author": api::text(p, "/author"),
                            "route": "global",
                        })),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn parse_archive_search(body: &str) -> Result<Vec<SearchResult>, String> {
    let list: ArchiveList<ArchivePost> = serde_json::from_str(body).map_err(archive_error)?;
    if let Some(e) = list.error {
        return Err(archive_error(e));
    }
    Ok(list
        .data
        .into_iter()
        .map(|p| {
            let post = archive_post(p);
            let snippet = if post.body.is_empty() {
                post.community.clone()
            } else {
                thread::clip(&post.body, 500)
            };
            SearchResult {
                title: post.title.clone(),
                url: post.url.clone(),
                snippet,
                source: "reddit".into(),
                published: (!post.created.is_empty()).then(|| post.created.clone()),
                image_url: None,
                extra: Some(serde_json::json!({
                    "subreddit": post.community.trim_start_matches("r/"),
                    "score": post.score,
                    "num_comments": post.num_comments,
                    "author": post.author,
                    "route": "archive",
                })),
            }
        })
        .collect())
}

// ---------------------------------------------------------------------------
// Merging a thread

/// The thread from whatever answered: the archive tree as the skeleton, the
/// live page's current scores and bodies over it, live comments the archive
/// lacks added. Returns the ordered comments and the route that served it.
pub fn merge(
    live: Option<Vec<Comment>>,
    archive: Option<Vec<Comment>>,
) -> Option<(Vec<Comment>, &'static str)> {
    let route = match (&live, &archive) {
        (Some(_), Some(_)) => "live+archive",
        (Some(_), None) => "live",
        (None, Some(_)) => "archive",
        (None, None) => return None,
    };
    let mut by_id: HashMap<String, Comment> = HashMap::new();
    for c in archive.into_iter().flatten() {
        by_id.insert(c.id.clone(), c);
    }
    for c in live.into_iter().flatten() {
        by_id.insert(c.id.clone(), c);
    }
    Some((thread::order(by_id.into_values().collect()), route))
}

fn slug_title(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|u| {
            u.path_segments()
                .and_then(|mut s| s.nth(4).map(|t| t.replace('_', " ")))
        })
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| "Reddit thread".to_string())
}

// ---------------------------------------------------------------------------
// Calls

impl Reddit {
    async fn archive_get(&self, path: &str, timeout: Duration) -> Result<String, String> {
        let url = format!("{ARCHIVE}/{path}");
        let resp = super::send(self, timeout, || self.ctx.client.get(&url))
            .await
            .map_err(archive_error)?;
        resp.text().await.map_err(archive_error)
    }

    async fn thread(&self, url: &str, timeout: Duration) -> Result<Output, SearchError> {
        let Some((sub, id)) = thread_ref(url) else {
            return Err(SearchError::Api {
                provider: "reddit",
                code: "not_a_thread",
                message: "reddit extracts only reddit.com/r/<sub>/comments/<id> thread URLs".into(),
                status: None,
            });
        };
        let routes = routes();
        let live_url = format!("{LIVE}/svc/shreddit/comments/r/{sub}/t3_{id}");
        let live = async {
            if !routes.live {
                return Err("live route off (SEARCH_REDDIT_ROUTE=archive)".to_string());
            }
            live_page(&self.ctx, &live_url, "<shreddit-comment", timeout)
                .await
                .map(|page| page.map(|p| parse_live_comments(&p)).unwrap_or_default())
        };
        let post = async {
            if !routes.archive {
                return Err("archive off (SEARCH_REDDIT_ROUTE=live)".to_string());
            }
            self.archive_get(&format!("posts/ids?ids={id}"), timeout)
                .await
                .and_then(|b| parse_archive_post(&b))
        };
        let tree = async {
            if !routes.archive {
                return Err("archive off (SEARCH_REDDIT_ROUTE=live)".to_string());
            }
            self.archive_get(
                &format!("comments/tree?link_id=t3_{id}&limit=9999"),
                timeout,
            )
            .await
            .and_then(|b| parse_archive_tree(&b))
        };
        let (live, post, tree) = tokio::join!(live, post, tree);
        let mut notes: Vec<String> = Vec::new();
        let live_err = live.as_ref().err().cloned();
        let (tree, more) = match tree {
            Ok((c, more)) => (Some(c), more),
            Err(e) => {
                if routes.archive {
                    notes.push(format!("archive tree unavailable: {e}"));
                }
                (None, 0)
            }
        };
        if let Some(e) = &live_err {
            if tree.is_some() {
                notes.push(format!("live page not used ({e}); scores are the archive's, captured when it saved each comment"));
            }
        } else if tree.is_some() {
            notes.push("scores of comments past the live first page are the archive's, captured when it saved them".into());
        }
        if more > 0 {
            notes.push(format!(
                "{more} more comments exist that neither source returned"
            ));
        }
        let Some((comments, route)) = merge(live.ok(), tree) else {
            let message = live_err.unwrap_or_else(|| "no source answered".into());
            let category = if message.starts_with("challenge") {
                FailureCategory::Rejected
            } else if message.starts_with("paced") {
                FailureCategory::RateLimit
            } else {
                FailureCategory::Network
            };
            return Err(SearchError::Upstream {
                provider: "reddit",
                status: 0,
                category,
                message: format!("{message}; {}", notes.join("; ")),
            });
        };
        let post = match post {
            Ok(p) => p,
            Err(e) => {
                notes.push(format!("post details unavailable: {e}"));
                let canonical = format!("{LIVE}/r/{sub}/comments/{id}/");
                Post {
                    title: slug_title(url),
                    url: canonical,
                    community: format!("r/{sub}"),
                    ..Post::default()
                }
            }
        };
        Ok(Output {
            results: vec![thread::result("reddit", &post, &comments, route, &notes)],
            ..Output::default()
        })
    }

    async fn search(&self, call: &Call<'_>) -> Result<Output, SearchError> {
        let routes = routes();
        let mut why = Vec::new();
        if routes.live {
            let q: String = url::form_urlencoded::byte_serialize(call.query.as_bytes()).collect();
            let url = format!("{LIVE}/svc/shreddit/search/?q={q}&type=posts");
            match live_page(&self.ctx, &url, "post-title-text", call.timeout).await {
                Ok(page) => {
                    let mut results = page.map(|p| parse_live_search(&p)).unwrap_or_default();
                    results.truncate(call.count as usize);
                    return Ok(Output {
                        results,
                        ..Output::default()
                    });
                }
                Err(e) => why.push(e),
            }
        }
        let live_refused = routes.live && !why.is_empty();
        if routes.archive {
            match subreddit_in(call.query) {
                Some((sub, rest)) => {
                    let title: String =
                        url::form_urlencoded::byte_serialize(rest.as_bytes()).collect();
                    let path = format!(
                        "posts/search?subreddit={sub}&title={title}&limit={}&sort=desc",
                        call.count
                    );
                    match self
                        .archive_get(&path, call.timeout)
                        .await
                        .and_then(|b| parse_archive_search(&b))
                    {
                        Ok(results) => {
                            return Ok(Output {
                                results,
                                ..Output::default()
                            })
                        }
                        Err(e) => why.push(e),
                    }
                }
                None => why.push(
                    "archive search needs a subreddit: add r/<name> to the query".to_string(),
                ),
            }
        }
        // Reddit refused and the query names no subreddit: global search.
        if live_refused && subreddit_in(call.query).is_none() {
            match self.global_search(call).await {
                Ok(out) => return Ok(out),
                Err(e) => why.push(e),
            }
        }
        let message = why.join("; ");
        let category = if message.starts_with("challenge") {
            FailureCategory::Rejected
        } else if message.starts_with("paced") || message.starts_with("reddit served a captcha") {
            FailureCategory::RateLimit
        } else {
            FailureCategory::Network
        };
        Err(SearchError::Upstream {
            provider: "reddit",
            status: 0,
            category,
            message,
        })
    }
}

impl Reddit {
    /// The global search fallback: direct with the platforms' key, else
    /// hosted when metasearch.sh offers Reddit.
    async fn global_search(&self, call: &Call<'_>) -> Result<Output, String> {
        use super::social_api as api;
        // The platforms' keys, fallbacks included: a key refused or out of
        // credit hands the call to the next, as in the engine.
        let keys = super::keys_for(&self.ctx.config, &super::tiktok::SPEC);
        let mut refused: Vec<String> = Vec::new();
        for (i, key) in keys.iter().enumerate() {
            // -f is already one of day, week, month, year.
            let timeframe = call.opts.freshness.as_deref().unwrap_or_default();
            let r = api::get(
                self,
                &self.ctx,
                key,
                "/v1/reddit/search",
                &[
                    ("query", call.query),
                    ("timeframe", timeframe),
                    ("trim", "true"),
                ],
                call.timeout,
            )
            .await;
            match r {
                Ok(r) => {
                    return Ok(api::search_output(
                        parse_global_search(&r.body),
                        call.count,
                        r.credits,
                    ))
                }
                Err(e) if i + 1 < keys.len() && super::next_key_may_help(e.category()) => {
                    refused.push(format!("key {}: {e}", i + 1));
                }
                Err(e) if refused.is_empty() => return Err(format!("global search: {e}")),
                Err(e) => {
                    return Err(format!(
                        "global search: key {}: {e}; refused before it: {}",
                        i + 1,
                        refused.join("; ")
                    ))
                }
            }
        }
        let client = crate::hosted::MetasearchClient::new(&self.ctx)
            .ok()
            .flatten()
            .ok_or("global search needs `search login`")?;
        let offered = client
            .manifest()
            .await
            .is_some_and(|m| m.offer("reddit", Mode::Community).is_some());
        if !offered {
            return Err("global search is not offered by metasearch.sh yet".into());
        }
        let idempotency = uuid::Uuid::new_v4().to_string();
        let body = crate::hosted::search_body(
            Mode::Community,
            call.query,
            call.count,
            &super::NO_FILTERS,
            &crate::hosted::BodyOpts {
                providers: Some(&["reddit"]),
                allowed: &[],
                rerank: None,
                fresh: false,
            },
        );
        let (mut out, _) = client
            .search("reddit", &body, &idempotency, call.timeout)
            .await
            .map_err(|e| format!("global search through metasearch.sh: {e}"))?;
        for r in &mut out.results {
            if let Some(obj) = r
                .extra
                .get_or_insert_with(|| serde_json::json!({}))
                .as_object_mut()
            {
                obj.insert("route".into(), "hosted".into());
            }
        }
        Ok(out)
    }
}

#[async_trait]
impl Adapter for Reddit {
    fn spec(&self) -> &'static ProviderSpec {
        &SPEC
    }

    fn is_configured(&self) -> bool {
        true // keyless
    }

    async fn call(&self, call: &Call<'_>) -> Result<Output, SearchError> {
        match call.mode {
            Mode::Community => self.search(call).await,
            Mode::Extract | Mode::Scrape => self.thread(call.query, call.timeout).await,
            other => Err(super::direct::unsupported("reddit", other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMMENTS: &str = include_str!("../../tests/fixtures/reddit/comments.html");
    const SEARCH: &str = include_str!("../../tests/fixtures/reddit/search.html");
    const ARCHIVE_POST: &str = include_str!("../../tests/fixtures/reddit/arctic_post.json");
    const ARCHIVE_TREE: &str = include_str!("../../tests/fixtures/reddit/arctic_tree.json");
    const CAPTCHA: &str = include_str!("../../tests/fixtures/reddit/captcha.html");

    #[test]
    fn live_comments_carry_author_score_depth_parent_and_time() {
        let comments = parse_live_comments(COMMENTS);
        assert!(comments.len() >= 8, "{}", comments.len());
        let first = &comments[0];
        assert_eq!(first.id, "kcxj4ur");
        assert_eq!(first.author, "Vova_xX");
        assert_eq!(first.score, Some(66));
        assert_eq!(first.depth, 0);
        assert_eq!(first.parent, None);
        assert!(first.created.starts_with("2023-12-11T18:59:35"));
        assert!(first.body.contains("keychron V3"), "{}", first.body);
        // A reply's body is its own, not its parent's.
        let reply = &comments[1];
        assert_eq!(reply.parent.as_deref(), Some("kcxj4ur"));
        assert_eq!(reply.depth, 1);
        assert!(
            !reply.body.contains("keychron V3, as 60%"),
            "{}",
            reply.body
        );
    }

    #[test]
    fn live_search_reads_posts_with_votes_comments_and_subreddit() {
        let results = parse_live_search(SEARCH);
        assert!(results.len() >= 3, "{}", results.len());
        let r = &results[0];
        assert!(r.url.starts_with("https://www.reddit.com/r/"), "{}", r.url);
        assert!(!r.title.is_empty());
        let extra = r.extra.as_ref().unwrap();
        assert!(extra["score"].is_i64(), "{extra}");
        assert!(extra["num_comments"].is_i64(), "{extra}");
        assert!(extra["subreddit"].is_string());
        assert!(r.published.is_some());
    }

    #[test]
    fn a_captcha_page_is_a_challenge_never_content() {
        assert_eq!(judge(200, CAPTCHA, "<shreddit-comment"), Live::Challenge);
        assert_eq!(judge(403, CAPTCHA, "<shreddit-comment"), Live::Refused(403));
        assert!(matches!(
            judge(200, COMMENTS, "<shreddit-comment"),
            Live::Content(_)
        ));
        // A real page with nothing on it is empty, not a challenge.
        assert_eq!(
            judge(200, "<html><body>ok</body></html>", "<shreddit-comment"),
            Live::Empty
        );
        // A thread that mentions recaptcha in a comment is still content.
        let talk = format!("{COMMENTS}<p>how do I add reCAPTCHA?</p>");
        assert!(matches!(
            judge(200, &talk, "<shreddit-comment"),
            Live::Content(_)
        ));
    }

    #[test]
    fn the_bucket_allows_a_burst_then_about_ten_a_minute() {
        let mut b = Bucket::full(1000.0);
        for _ in 0..5 {
            assert!(b.take(1000.0).is_ok());
        }
        let wait = b.take(1000.0).unwrap_err();
        assert!((5.5..=6.5).contains(&wait.as_secs_f64()), "{wait:?}");
        // Six seconds later one more token is free, and only one.
        assert!(b.take(1006.1).is_ok());
        assert!(b.take(1006.2).is_err());
        // A minute of quiet refills about ten, capped at the burst.
        let mut taken = 0;
        let mut b = Bucket {
            tokens: 0.0,
            at: 0.0,
            cooldown_until: 0.0,
        };
        let mut t = 0.0;
        while t < 60.0 {
            if b.take(t).is_ok() {
                taken += 1;
            }
            t += 0.5;
        }
        assert!((9..=11).contains(&taken), "{taken}");
        let resting = Bucket {
            cooldown_until: 500.0,
            ..Bucket::full(100.0)
        };
        assert!(resting.resting(100.0).is_some());
        assert!(resting.resting(501.0).is_none());
    }

    #[test]
    fn archive_post_and_tree_parse() {
        let post = parse_archive_post(ARCHIVE_POST).unwrap();
        assert_eq!(post.title, "Keyboards that you recommend for programming?");
        assert_eq!(post.community, "r/webdev");
        assert_eq!(post.num_comments, Some(222));
        let (tree, _) = parse_archive_tree(ARCHIVE_TREE).unwrap();
        assert!(tree.len() >= 10, "{}", tree.len());
        assert_eq!(tree[0].id, "kcxj4ur");
        assert!(tree.iter().any(|c| c.depth == 1 && c.parent.is_some()));
        assert!(parse_archive_post("{\"data\":null,\"error\":\"Timeout\"}").is_err());
    }

    /// The live route first, then the archive; the merged thread keeps
    /// live scores, and the archive alone serves it when live fails.
    #[test]
    fn fallback_order_and_merge() {
        #[cfg(feature = "stealth")]
        assert_eq!(live_profiles(), &[Profile::Chrome, Profile::Safari]);
        #[cfg(not(feature = "stealth"))]
        assert_eq!(live_profiles(), &[Profile::Plain]);

        let live = parse_live_comments(COMMENTS);
        let (archive, _) = parse_archive_tree(ARCHIVE_TREE).unwrap();
        let live_score = live[0].score;
        let (both, route) = merge(Some(live.clone()), Some(archive.clone())).unwrap();
        assert_eq!(route, "live+archive");
        assert!(both.len() >= live.len().max(archive.len()));
        let first = both.iter().find(|c| c.id == "kcxj4ur").unwrap();
        assert_eq!(first.score, live_score);

        let (only_archive, route) = merge(None, Some(archive.clone())).unwrap();
        assert_eq!(route, "archive");
        assert_eq!(only_archive.len(), archive.len());
        assert_eq!(merge(Some(live), None).unwrap().1, "live");
        assert!(merge(None, None).is_none());

        assert_eq!(
            routes_from(None),
            Routes {
                live: true,
                archive: true
            }
        );
        assert_eq!(
            routes_from(Some("archive")),
            Routes {
                live: false,
                archive: true
            }
        );
        assert_eq!(
            routes_from(Some("LIVE")),
            Routes {
                live: true,
                archive: false
            }
        );
    }

    /// When Reddit refuses a search that names no subreddit, the global
    /// search answers; its posts read like the live ones.
    #[test]
    fn global_search_posts_parse_like_live_ones() {
        let body: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/social/reddit_search.json"
        ))
        .unwrap();
        let hits = parse_global_search(&body);
        assert_eq!(hits.len(), 3);
        let h = &hits[0];
        assert!(thread_ref(&h.url).is_some(), "{}", h.url);
        let e = h.extra.as_ref().unwrap();
        assert_eq!(e["subreddit"], "developersIndia");
        assert_eq!(e["num_comments"], 49);
        assert_eq!(e["route"], "global");
        assert!(h.published.is_some());
    }

    #[test]
    fn thread_urls_and_subreddit_cues() {
        let want = Some(("webdev".to_string(), "18g0wcw".to_string()));
        for u in [
            "https://www.reddit.com/r/webdev/comments/18g0wcw/keyboards_that_you_recommend_for_programming/",
            "https://old.reddit.com/r/webdev/comments/18g0wcw/",
            "https://reddit.com/r/webdev/comments/18g0wcw",
        ] {
            assert_eq!(thread_ref(u), want, "{u}");
        }
        assert_eq!(thread_ref("https://www.reddit.com/r/webdev/"), None);
        assert_eq!(thread_ref("https://evilreddit.com/r/a/comments/b/"), None);
        assert_eq!(
            subreddit_in("r/rust async runtimes"),
            Some(("rust".into(), "async runtimes".into()))
        );
        assert_eq!(subreddit_in("rust async"), None);
        assert_eq!(
            slug_title("https://www.reddit.com/r/webdev/comments/18g0wcw/keyboards_that_you/"),
            "keyboards that you"
        );
    }
}
