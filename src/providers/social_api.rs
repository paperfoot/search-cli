//! What the social platform providers (`tiktok`, `instagram`, `youtube`,
//! `facebook`, `threads`) share: one upstream API, the request, the cost of a
//! call, URL recognition and the readable page an extract returns.
//!
//! The upstream is ScrapeCreators (`api.scrapecreators.com`, header
//! `x-api-key`); every answer reports `credits_charged`. Public text names the
//! platforms only, never this upstream: the providers are documented as
//! running through metasearch.sh, which holds the key. When
//! `SCRAPECREATORS_API_KEY` is set the CLI calls the upstream directly
//! (internal use; no doc, help or agent-info entry mentions the variable).

use super::spec::{FilterSupport, Price};
use super::thread;
use super::Adapter;
use crate::context::AppContext;
use crate::errors::SearchError;
use crate::types::{FailureCategory, SearchResult};
use serde_json::{json, Value};
use std::time::Duration;

const BASE: &str = "https://api.scrapecreators.com";

/// The env var the platform providers read their direct key from.
pub const ENV_KEYS: &[&str] = &["SCRAPECREATORS_API_KEY"];

/// The platform providers, in display order. Counted apart from the search
/// engines, like the community sources.
pub const PLATFORMS: &[&str] = &["tiktok", "instagram", "youtube", "facebook", "threads"];

/// Entry price: $1.88 per 1,000 credits, so 1,880 micro-USD per credit.
pub const MICROS_PER_CREDIT: u64 = 1_880;

/// One search or post lookup is one credit.
pub const PRICE: Price = Price {
    per_call: MICROS_PER_CREDIT as u32,
    per_extra_result: 0,
    included_results: 0,
};

/// Per-provider deadline. Measured on 4 Nov 2026: TikTok 4 to 13 s,
/// Threads 9 s, Instagram 11 to 14 s, YouTube 3 to 5 s; 20 s leaves room
/// for a slow Instagram page without letting one platform hold up the
/// social fan-out (whose wait for stragglers is the same 20 s).
pub const TIMEOUT: Duration = Duration::from_secs(20);

pub const fn filters(freshness: bool) -> FilterSupport {
    FilterSupport {
        freshness,
        domains: false,
        country: false,
        lang: false,
    }
}

/// One answered request: the body and the credits it cost.
pub struct Reply {
    pub body: Value,
    pub credits: u64,
}

/// `GET <path>?<params>` with the shared retry policy. Empty params are left
/// out. A 200 whose body says `success: false` is a failure of the call.
/// The URL is built before the future is returned, so `params` may be a
/// temporary.
pub fn get<'a>(
    adapter: &'a dyn Adapter,
    ctx: &'a AppContext,
    key: &'a str,
    path: &str,
    params: &[(&str, &str)],
    timeout: Duration,
) -> impl std::future::Future<Output = Result<Reply, SearchError>> + 'a {
    let mut url = url::Url::parse(BASE)
        .and_then(|b| b.join(path))
        .expect("static upstream path");
    url.query_pairs_mut()
        .extend_pairs(params.iter().filter(|(_, v)| !v.is_empty()));
    async move {
        let resp = super::send(adapter, timeout, || {
            ctx.client.get(url.as_str()).header("x-api-key", key)
        })
        .await?;
        let provider = adapter.name();
        let text = resp.text().await.map_err(|e| parse_error(provider, e))?;
        let body: Value = serde_json::from_str(&text).map_err(|e| parse_error(provider, e))?;
        if body["success"] == Value::Bool(false) {
            let message = body["message"]
                .as_str()
                .or_else(|| body["error"].as_str())
                .unwrap_or("the platform returned no data for this request");
            return Err(SearchError::Upstream {
                provider,
                status: 200,
                category: FailureCategory::BadRequest,
                message: thread::clip(message, 200),
            });
        }
        let credits = body["credits_charged"].as_u64().unwrap_or(0);
        Ok(Reply { body, credits })
    }
}

fn parse_error(provider: &'static str, e: impl std::fmt::Display) -> SearchError {
    SearchError::Upstream {
        provider,
        status: 200,
        category: FailureCategory::Parse,
        message: format!("unreadable answer: {e}"),
    }
}

/// Classify the upstream's non-2xx bodies: 402 is out of credits, 401 and
/// 403 a bad key.
pub fn classify(status: u16) -> Option<FailureCategory> {
    match status {
        402 => Some(FailureCategory::BillingQuota),
        401 | 403 => Some(FailureCategory::Auth),
        _ => None,
    }
}

/// The cost of `credits`, in micro-USD.
pub fn cost(credits: u64) -> u64 {
    credits * MICROS_PER_CREDIT
}

// ---------------------------------------------------------------------------
// Reading JSON

/// The string at a JSON pointer, or "".
pub fn text(v: &Value, ptr: &str) -> String {
    v.pointer(ptr)
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string()
}

/// The number at a JSON pointer: an integer, a float or a numeric string.
pub fn num(v: &Value, ptr: &str) -> Option<i64> {
    match v.pointer(ptr)? {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

pub fn float(v: &Value, ptr: &str) -> Option<f64> {
    v.pointer(ptr).and_then(Value::as_f64)
}

/// Unix seconds (number or numeric string) as ISO 8601.
pub fn iso(v: &Value, ptr: &str) -> Option<String> {
    num(v, ptr).filter(|t| *t > 0).map(thread::iso_from_unix)
}

pub fn non_empty(s: String) -> Option<String> {
    (!s.is_empty()).then_some(s)
}

/// A title from a caption: its first line, clipped.
pub fn title_from(caption: &str, fallback: &str) -> String {
    let first = caption.lines().map(str::trim).find(|l| !l.is_empty());
    match first {
        Some(line) => thread::clip(line, 120),
        None => fallback.to_string(),
    }
}

/// 72_671 -> "72.7K".
pub fn compact(n: i64) -> String {
    let f = n as f64;
    match n.unsigned_abs() {
        0..=999 => n.to_string(),
        1_000..=999_999 => format!("{:.1}K", f / 1e3),
        1_000_000..=999_999_999 => format!("{:.1}M", f / 1e6),
        _ => format!("{:.1}B", f / 1e9),
    }
}

/// A WebVTT caption track as plain text: cue text only, repeats dropped.
pub fn vtt_to_text(vtt: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for line in vtt.lines() {
        let l = line.trim();
        if l.is_empty()
            || l == "WEBVTT"
            || l.contains("-->")
            || l.chars().all(|c| c.is_ascii_digit())
            || l.starts_with("NOTE")
        {
            continue;
        }
        if out.last() != Some(&l) {
            out.push(l);
        }
    }
    out.join(" ")
}

/// Collapse runs of whitespace (transcripts arrive with double spaces).
pub fn squash(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

// ---------------------------------------------------------------------------
// Results

/// Platform stats, kept in a result's `extra`.
#[derive(Debug, Default, Clone)]
pub struct Stats {
    pub author: String,
    pub followers: Option<i64>,
    pub views: Option<i64>,
    pub likes: Option<i64>,
    pub comments: Option<i64>,
    pub shares: Option<i64>,
    pub duration_s: Option<f64>,
}

impl Stats {
    pub fn extra(&self, platform: &str) -> Value {
        let mut m = serde_json::Map::new();
        m.insert("platform".into(), json!(platform));
        if !self.author.is_empty() {
            m.insert("author".into(), json!(self.author));
        }
        for (k, v) in [
            ("followers", self.followers),
            ("views", self.views),
            ("likes", self.likes),
            ("num_comments", self.comments),
            ("shares", self.shares),
        ] {
            if let Some(v) = v {
                m.insert(k.into(), json!(v));
            }
        }
        if let Some(d) = self.duration_s {
            m.insert("duration_s".into(), json!((d * 10.0).round() / 10.0));
        }
        Value::Object(m)
    }

    /// "72.7K views · 1.6K likes · 263 comments · 75 shares · 38 s".
    pub fn line(&self) -> String {
        let mut parts: Vec<String> = [
            (self.views, "views"),
            (self.likes, "likes"),
            (self.comments, "comments"),
            (self.shares, "shares"),
        ]
        .into_iter()
        .filter_map(|(n, label)| n.map(|n| format!("{} {label}", compact(n))))
        .collect();
        if let Some(d) = self.duration_s {
            parts.push(format!("{} s", d.round() as i64));
        }
        parts.join(" · ")
    }
}

/// One search hit.
pub fn hit(
    platform: &str,
    title: String,
    url: String,
    caption: &str,
    published: Option<String>,
    image: Option<String>,
    stats: &Stats,
) -> SearchResult {
    let snippet = if caption.is_empty() {
        stats.line()
    } else {
        thread::clip(&squash(caption), 500)
    };
    SearchResult {
        title,
        url,
        snippet,
        source: platform.to_string(),
        published,
        image_url: image.filter(|u| !u.is_empty()),
        extra: Some(stats.extra(platform)),
    }
}

/// One comment of a post.
#[derive(Debug, Clone, PartialEq)]
pub struct Remark {
    pub author: String,
    pub likes: Option<i64>,
    pub text: String,
}

/// A post or page read whole: what an extract returns.
#[derive(Debug, Default)]
pub struct Page {
    pub title: String,
    pub url: String,
    pub published: Option<String>,
    pub image: Option<String>,
    pub stats: Stats,
    pub caption: String,
    pub transcript: Option<String>,
    pub comments: Vec<Remark>,
    /// Other lines worth reading (a page's details, its recent posts).
    pub sections: Vec<(String, String)>,
    pub notes: Vec<String>,
    pub credits: u64,
}

impl Page {
    /// The readable text: heading, byline, stats, caption, transcript,
    /// comments.
    pub fn render(&self) -> String {
        let mut out = format!("# {}\n\n{}\n", self.title, self.url);
        let mut byline = Vec::new();
        if !self.stats.author.is_empty() {
            byline.push(match self.stats.followers {
                Some(f) => format!("{} ({} followers)", self.stats.author, compact(f)),
                None => self.stats.author.clone(),
            });
        }
        if let Some(p) = &self.published {
            byline.push(p.clone());
        }
        if !byline.is_empty() {
            out.push_str(&format!("{}\n", byline.join(" · ")));
        }
        let line = self.stats.line();
        if !line.is_empty() {
            out.push_str(&format!("{line}\n"));
        }
        if !self.caption.is_empty() {
            out.push_str(&format!("\n{}\n", self.caption.trim()));
        }
        for (heading, body) in &self.sections {
            out.push_str(&format!("\n## {heading}\n\n{}\n", body.trim()));
        }
        if let Some(t) = &self.transcript {
            out.push_str(&format!("\n## Transcript\n\n{}\n", t.trim()));
        }
        if !self.comments.is_empty() {
            out.push_str("\n## Top comments\n\n");
            for c in &self.comments {
                let score = c.likes.map(|l| format!("[{l}] ")).unwrap_or_default();
                out.push_str(&format!("{score}{}: {}\n", c.author, squash(&c.text)));
            }
        }
        out
    }

    pub fn into_result(self, platform: &str) -> SearchResult {
        let snippet = self.render();
        let mut extra = self.stats.extra(platform);
        let obj = extra.as_object_mut().expect("object");
        obj.insert("transcript".into(), json!(self.transcript.is_some()));
        obj.insert(
            "comments".into(),
            json!(self
                .comments
                .iter()
                .map(|c| json!({ "author": c.author, "likes": c.likes, "text": c.text }))
                .collect::<Vec<_>>()),
        );
        obj.insert("credits".into(), json!(self.credits));
        obj.insert("verdict".into(), json!("ok"));
        if !self.notes.is_empty() {
            obj.insert("notes".into(), json!(self.notes));
        }
        SearchResult {
            title: self.title,
            url: self.url,
            snippet,
            source: platform.to_string(),
            published: self.published,
            image_url: self.image.filter(|u| !u.is_empty()),
            extra: Some(extra),
        }
    }
}

/// A secondary request of an extract (transcript, comments): its body and
/// credits, or a note saying why it is missing. It never fails the page.
pub struct Side {
    pub body: Option<Value>,
    pub credits: u64,
    pub note: Option<String>,
}

pub fn side(what: &str, reply: Result<Reply, SearchError>) -> Side {
    match reply {
        Ok(r) => Side {
            credits: r.credits,
            body: Some(r.body),
            note: None,
        },
        Err(e) => Side {
            body: None,
            credits: 0,
            note: Some(format!("{what} unavailable: {e}")),
        },
    }
}

impl Page {
    /// Count a side request in: its credits and, when it failed, its note.
    pub fn with(mut self, side: &Side) -> Self {
        self.credits += side.credits;
        self.notes.extend(side.note.clone());
        self
    }

    /// The extract's output: one result, priced from the credits charged.
    pub fn output(self, platform: &str) -> super::Output {
        let credits = self.credits;
        super::Output {
            results: vec![self.into_result(platform)],
            cost_micros: Some(cost(credits)),
            ..super::Output::default()
        }
    }
}

/// A search's output, priced from the credits charged.
pub fn search_output(mut results: Vec<SearchResult>, count: u16, credits: u64) -> super::Output {
    results.truncate(count as usize);
    super::Output {
        results,
        cost_micros: Some(cost(credits)),
        ..super::Output::default()
    }
}

// ---------------------------------------------------------------------------
// URLs

/// The host of `url` without `www.`/`m.`, lowercased, and its path segments.
fn host_and_path(url: &str) -> Option<(String, Vec<String>, url::Url)> {
    let u = url::Url::parse(url.trim()).ok()?;
    if !matches!(u.scheme(), "http" | "https") {
        return None;
    }
    let host = u.host_str()?.to_ascii_lowercase();
    let host = ["www.", "m.", "mobile.", "web.", "vm.", "vt."]
        .iter()
        .find_map(|p| host.strip_prefix(p))
        .unwrap_or(&host)
        .to_string();
    let parts = u
        .path()
        .split('/')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    Some((host, parts, u))
}

/// Which platform provider reads `url` whole, if it is a post, video or
/// page on one: TikTok videos, Instagram posts and reels, YouTube videos
/// and shorts, Facebook posts, reels and pages, Threads posts.
pub fn platform_of_url(url: &str) -> Option<&'static str> {
    let (host, parts, u) = host_and_path(url)?;
    let p: Vec<&str> = parts.iter().map(String::as_str).collect();
    match host.as_str() {
        "tiktok.com" => {
            let video =
                matches!(p.as_slice(), [user, "video" | "photo", _, ..] if user.starts_with('@'));
            // vm.tiktok.com/<code> short links (prefix stripped above).
            let short = u
                .host_str()
                .is_some_and(|h| h.starts_with("vm.") || h.starts_with("vt."))
                && p.len() == 1;
            let t = matches!(p.as_slice(), ["t", _]);
            (video || short || t).then_some("tiktok")
        }
        "instagram.com" => {
            let post = matches!(p.as_slice(), ["p" | "reel" | "reels" | "tv", _, ..])
                || matches!(p.as_slice(), [_, "p" | "reel", _, ..]);
            post.then_some("instagram")
        }
        "youtube.com" | "music.youtube.com" => {
            let watch = p.first() == Some(&"watch") && u.query_pairs().any(|(k, _)| k == "v");
            let other = matches!(p.as_slice(), ["shorts" | "live" | "embed", _, ..]);
            (watch || other).then_some("youtube")
        }
        "youtu.be" => (p.len() == 1).then_some("youtube"),
        "facebook.com" | "fb.com" => {
            const NOT_PAGES: &[&str] = &[
                "login",
                "login.php",
                "help",
                "policies",
                "privacy",
                "settings",
                "marketplace",
                "events",
                "gaming",
                "business",
                "ads",
                "pages",
                "search",
                "hashtag",
            ];
            match p.first() {
                None => None,
                Some(first) if NOT_PAGES.contains(first) => None,
                Some(_) => Some("facebook"),
            }
        }
        "fb.watch" => (!p.is_empty()).then_some("facebook"),
        "threads.net" | "threads.com" => {
            matches!(p.as_slice(), [user, "post", _, ..] if user.starts_with('@'))
                .then_some("threads")
        }
        _ => None,
    }
}

/// The query a platform searches: the cue words ("on tiktok", "instagram
/// reels") taken out, so TikTok is not asked for "tiktok".
pub fn clean_query(query: &str) -> String {
    let cleaned = crate::router::strip_platform_cues(query);
    if cleaned.trim().is_empty() {
        query.trim().to_string()
    } else {
        cleaned
    }
}

/// The date `days` ago, `YYYY-MM-DD`, for platforms that take a start date.
pub fn since(freshness: Option<&str>) -> Option<String> {
    freshness
        .and_then(super::freshness_days)
        .map(super::date_days_ago)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_route_to_their_platform() {
        let cases = [
            (
                "https://www.tiktok.com/@jerryytech/video/7681548583702695181",
                Some("tiktok"),
            ),
            ("https://vm.tiktok.com/ZMabc123/", Some("tiktok")),
            ("https://www.tiktok.com/@jerryytech", None),
            (
                "https://www.instagram.com/reel/DeUhhCNpC9T/",
                Some("instagram"),
            ),
            ("https://instagram.com/p/DeUhhCNpC9T", Some("instagram")),
            ("https://www.instagram.com/austinnotduncan/", None),
            (
                "https://www.youtube.com/watch?v=3rKPzh3Pr0o",
                Some("youtube"),
            ),
            ("https://youtu.be/3rKPzh3Pr0o", Some("youtube")),
            ("https://m.youtube.com/shorts/qehEJ2bZrRo", Some("youtube")),
            ("https://www.youtube.com/@SwitchandClickOfficial", None),
            ("https://www.facebook.com/keychron", Some("facebook")),
            (
                "https://www.facebook.com/reel/1535656380759655",
                Some("facebook"),
            ),
            ("https://www.facebook.com/login.php", None),
            (
                "https://www.threads.com/@dad.life.gaming/post/DfQ_LTsm841",
                Some("threads"),
            ),
            ("https://www.threads.net/@zuck/post/abc", Some("threads")),
            ("https://www.threads.net/@zuck", None),
            ("https://example.com/watch?v=1", None),
            ("https://evil-tiktok.com/@a/video/1", None),
        ];
        for (url, want) in cases {
            assert_eq!(platform_of_url(url), want, "{url}");
        }
    }

    #[test]
    fn webvtt_becomes_plain_text() {
        let vtt = "WEBVTT\n\n\n00:00:00.000 --> 00:00:01.980\nWhat your keyboard size says about you.\n\n00:00:01.981 --> 00:00:04.161\nYou try hard in every game.\n";
        assert_eq!(
            vtt_to_text(vtt),
            "What your keyboard size says about you. You try hard in every game."
        );
    }

    #[test]
    fn stats_read_as_a_line_and_an_extra() {
        let s = Stats {
            author: "@jerryytech".into(),
            followers: Some(7783),
            views: Some(72_671),
            likes: Some(1555),
            comments: Some(263),
            shares: Some(75),
            duration_s: Some(38.4),
        };
        assert_eq!(
            s.line(),
            "72.7K views · 1.6K likes · 263 comments · 75 shares · 38 s"
        );
        let e = s.extra("tiktok");
        assert_eq!(e["views"], 72_671);
        assert_eq!(e["num_comments"], 263);
        assert_eq!(e["followers"], 7783);
        assert_eq!(e["platform"], "tiktok");
        assert_eq!(compact(6_200_000), "6.2M");
        assert_eq!(cost(3), 5_640);
    }

    #[test]
    fn cue_words_leave_the_platform_query() {
        assert_eq!(
            clean_query("mechanical keyboard on tiktok"),
            "mechanical keyboard"
        );
        assert_eq!(clean_query("instagram reels latte art"), "latte art");
        assert_eq!(clean_query("tiktok"), "tiktok");
    }
}
