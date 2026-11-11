//! `youtube`: YouTube video and shorts search, and a YouTube video URL read
//! whole (description, stats, transcript, top comments). See
//! [`super::social_api`].

use super::social_api::{self as api, Page, Remark, Stats};
use super::{Adapter, Call, Output, ProviderSpec};
use crate::context::AppContext;
use crate::errors::SearchError;
use crate::types::{FailureCategory, Mode, SearchResult};
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;

pub static SPEC: ProviderSpec = ProviderSpec {
    name: "youtube",
    env_keys: api::ENV_KEYS,
    modes: &[Mode::Social, Mode::Extract],
    filters: api::filters(true),
    max_count: 20,
    timeout: api::TIMEOUT,
    billed_on_send: true,
    price: api::PRICE,
};

pub struct YouTube {
    ctx: Arc<AppContext>,
}

impl YouTube {
    pub fn new(ctx: Arc<AppContext>) -> Self {
        Self { ctx }
    }
}

fn channel(v: &Value) -> String {
    let handle = api::text(v, "/channel/handle");
    if !handle.is_empty() && !handle.starts_with("channel/") {
        return format!("@{}", handle.trim_start_matches('@'));
    }
    api::text(v, "/channel/title")
}

fn hit(v: &Value) -> SearchResult {
    let stats = Stats {
        author: channel(v),
        views: api::num(v, "/viewCountInt"),
        duration_s: api::num(v, "/lengthSeconds").map(|s| s as f64),
        ..Stats::default()
    };
    let title = api::text(v, "/title");
    let id = api::text(v, "/id");
    let url = api::non_empty(api::text(v, "/url"))
        .unwrap_or_else(|| format!("https://www.youtube.com/watch?v={id}"));
    let mut r = api::hit(
        "youtube",
        title.clone(),
        url,
        "",
        api::non_empty(api::text(v, "/publishedTime")),
        api::non_empty(api::text(v, "/thumbnail")),
        &stats,
    );
    // Search sends no description: the snippet says who posted it and how
    // it did.
    let line = stats.line();
    r.snippet = [stats.author.as_str(), line.as_str()]
        .iter()
        .filter(|s| !s.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join(" · ");
    if let Some(obj) = r.extra.as_mut().and_then(Value::as_object_mut) {
        obj.insert("kind".into(), Value::from(api::text(v, "/type")));
    }
    r
}

/// Videos, then shorts, of `/v1/youtube/search`.
pub fn parse_search(body: &Value) -> Vec<SearchResult> {
    ["videos", "shorts"]
        .iter()
        .filter_map(|k| body[*k].as_array())
        .flatten()
        .map(hit)
        .collect()
}

/// A video read whole from `/v1/youtube/video`, `/v1/youtube/video/transcript`
/// and `/v1/youtube/video/comments`.
pub fn parse_video(
    url: &str,
    video: &Value,
    transcript: Option<&Value>,
    comments: Option<&Value>,
) -> Page {
    let stats = Stats {
        author: api::non_empty(api::text(video, "/ownerByline/name"))
            .unwrap_or_else(|| channel(video)),
        followers: None,
        views: api::num(video, "/viewCountInt"),
        likes: api::num(video, "/likeCountInt"),
        comments: api::num(video, "/commentCountInt"),
        shares: None,
        duration_s: api::num(video, "/durationMs").map(|ms| ms as f64 / 1000.0),
    };
    let mut page = Page {
        title: api::non_empty(api::text(video, "/title")).unwrap_or_else(|| "YouTube video".into()),
        url: api::non_empty(api::text(video, "/url")).unwrap_or_else(|| url.to_string()),
        published: api::non_empty(api::text(video, "/publishDate")),
        image: api::non_empty(api::text(video, "/thumbnail")),
        caption: api::text(video, "/description"),
        transcript: transcript
            .and_then(|t| t["transcript_only_text"].as_str())
            .map(api::squash)
            .filter(|t| !t.is_empty()),
        comments: comments
            .and_then(|c| c["comments"].as_array())
            .map(|list| {
                list.iter()
                    .filter(|c| api::num(c, "/replyLevel").unwrap_or(0) == 0)
                    .map(|c| Remark {
                        author: api::text(c, "/author/name"),
                        likes: api::num(c, "/engagement/likes"),
                        text: api::text(c, "/content"),
                    })
                    .filter(|r| !r.text.is_empty())
                    .collect()
            })
            .unwrap_or_default(),
        stats,
        ..Page::default()
    };
    let subs = api::text(video, "/ownerByline/subscriberCountText");
    if !subs.is_empty() {
        page.stats.author = format!("{} ({subs})", page.stats.author);
    }
    if transcript.is_some() && page.transcript.is_none() {
        page.notes.push("this video has no captions to read".into());
    }
    page
}

#[async_trait]
impl Adapter for YouTube {
    fn spec(&self) -> &'static ProviderSpec {
        &SPEC
    }

    fn keys(&self) -> Vec<String> {
        super::keys_for(&self.ctx.config, &SPEC)
    }

    fn is_configured(&self) -> bool {
        !self.keys().is_empty()
    }

    fn classify(&self, status: u16, _body: &str) -> Option<FailureCategory> {
        api::classify(status)
    }

    async fn call(&self, call: &Call<'_>) -> Result<Output, SearchError> {
        let key = call.key;
        match call.mode {
            Mode::Social => {
                let query = api::clean_query(call.query);
                let upload = match call.opts.freshness.as_deref() {
                    Some("day") => "today",
                    Some("week") => "this_week",
                    Some("month") => "this_month",
                    Some("year") => "this_year",
                    _ => "",
                };
                let r = api::get(
                    self,
                    &self.ctx,
                    key,
                    "/v1/youtube/search",
                    &[("query", &query), ("uploadDate", upload)],
                    call.timeout,
                )
                .await?;
                Ok(api::search_output(
                    parse_search(&r.body),
                    call.count,
                    r.credits,
                ))
            }
            Mode::Extract | Mode::Scrape => {
                let url = call.query.trim();
                let get = |path: &'static str| {
                    api::get(self, &self.ctx, key, path, &[("url", url)], call.timeout)
                };
                // A missing caption track costs nothing.
                let (video, transcript, comments) = tokio::join!(
                    get("/v1/youtube/video"),
                    get("/v1/youtube/video/transcript"),
                    get("/v1/youtube/video/comments"),
                );
                let video = video?;
                let transcript = api::side("transcript", transcript);
                let comments = api::side("comments", comments);
                let mut page = parse_video(
                    url,
                    &video.body,
                    transcript.body.as_ref(),
                    comments.body.as_ref(),
                )
                .with(&transcript)
                .with(&comments);
                page.credits += video.credits;
                Ok(page.output("youtube"))
            }
            other => Err(super::direct::unsupported("youtube", other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Value {
        let path = format!(
            "{}/tests/fixtures/social/{name}.json",
            env!("CARGO_MANIFEST_DIR")
        );
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn search_returns_videos_then_shorts_with_views_and_channel() {
        let hits = parse_search(&fixture("youtube_search"));
        assert_eq!(hits.len(), 5);
        let h = &hits[0];
        assert_eq!(h.url, "https://www.youtube.com/watch?v=3rKPzh3Pr0o");
        assert_eq!(h.title, "Full-Sized Keyboards are FINALLY Good in 2026.");
        let e = h.extra.as_ref().unwrap();
        assert_eq!(e["views"], 431_431);
        assert_eq!(e["author"], "@SwitchandClickOfficial");
        assert_eq!(e["kind"], "video");
        assert_eq!(hits[3].extra.as_ref().unwrap()["kind"], "short");
        assert!(h.snippet.contains("431.4K views"), "{}", h.snippet);
    }

    #[test]
    fn a_video_reads_whole_with_its_transcript() {
        let page = parse_video(
            "https://www.youtube.com/watch?v=3rKPzh3Pr0o",
            &fixture("youtube_video"),
            Some(&fixture("youtube_transcript")),
            Some(&fixture("youtube_comments")),
        );
        assert!(page
            .transcript
            .as_deref()
            .unwrap()
            .starts_with("Full-size keyboards should be the best"));
        assert_eq!(page.stats.likes, Some(7843));
        assert_eq!(page.stats.duration_s, Some(811.0));
        assert!(!page.comments.is_empty());
        assert!(page.stats.author.contains("1.45M subscribers"));
        let text = page.render();
        assert!(text.contains("## Transcript") && text.contains("## Top comments"));
    }
}
