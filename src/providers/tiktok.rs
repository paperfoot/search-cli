//! `tiktok`: TikTok keyword search, and a TikTok video URL read whole
//! (caption, stats, transcript, top comments). See [`super::social_api`].

use super::social_api::{self as api, Page, Remark, Stats};
use super::{Adapter, Call, Output, ProviderSpec};
use crate::context::AppContext;
use crate::errors::SearchError;
use crate::types::{FailureCategory, Mode, SearchResult};
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;

pub static SPEC: ProviderSpec = ProviderSpec {
    name: "tiktok",
    env_keys: api::ENV_KEYS,
    modes: &[Mode::Social, Mode::Extract],
    filters: api::filters(true),
    max_count: 10,
    timeout: api::TIMEOUT,
    billed_on_send: true,
    price: api::PRICE,
};

pub struct TikTok {
    ctx: Arc<AppContext>,
}

impl TikTok {
    pub fn new(ctx: Arc<AppContext>) -> Self {
        Self { ctx }
    }
}

fn stats(item: &Value) -> Stats {
    let author = api::text(item, "/author/unique_id");
    Stats {
        author: if author.is_empty() {
            String::new()
        } else {
            format!("@{author}")
        },
        followers: api::num(item, "/author/follower_count").filter(|n| *n > 0),
        views: api::num(item, "/statistics/play_count"),
        likes: api::num(item, "/statistics/digg_count"),
        comments: api::num(item, "/statistics/comment_count"),
        shares: api::num(item, "/statistics/share_count"),
        duration_s: api::num(item, "/video/duration")
            .filter(|d| *d > 0)
            .map(|ms| ms as f64 / 1000.0),
    }
}

fn video_url(item: &Value) -> String {
    let url = api::text(item, "/url");
    if !url.is_empty() {
        return url;
    }
    format!(
        "https://www.tiktok.com/@{}/video/{}",
        api::text(item, "/author/unique_id"),
        api::text(item, "/aweme_id")
    )
}

fn published(item: &Value) -> Option<String> {
    api::non_empty(api::text(item, "/create_time_utc")).or_else(|| api::iso(item, "/create_time"))
}

fn cover(item: &Value) -> Option<String> {
    api::non_empty(api::text(item, "/video/cover/url_list/0"))
}

/// Hits of `/v1/tiktok/search/keyword`.
pub fn parse_search(body: &Value) -> Vec<SearchResult> {
    let items = body["search_item_list"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let mut seen = std::collections::HashSet::new();
    items
        .iter()
        .map(|i| i.get("aweme_info").unwrap_or(i))
        // TikTok repeats videos within one page.
        .filter(|i| seen.insert(api::text(i, "/aweme_id")))
        .map(|i| {
            let caption = api::text(i, "/desc");
            let s = stats(i);
            let title = api::title_from(&caption, &format!("{} on TikTok", s.author));
            api::hit(
                "tiktok",
                title,
                video_url(i),
                &caption,
                published(i),
                cover(i),
                &s,
            )
        })
        .collect()
}

/// A video read whole from `/v2/tiktok/video` (with its transcript) and
/// `/v1/tiktok/video/comments`.
pub fn parse_video(video: &Value, comments: Option<&Value>) -> Page {
    let item = &video["aweme_detail"];
    let caption = api::text(item, "/desc");
    let stats = stats(item);
    let transcript = video["transcript"]
        .as_str()
        .map(api::vtt_to_text)
        .filter(|t| !t.is_empty());
    let remarks = comments
        .and_then(|c| c["comments"].as_array())
        .map(|list| {
            list.iter()
                .map(|c| Remark {
                    author: format!("@{}", api::text(c, "/user/unique_id")),
                    likes: api::num(c, "/digg_count"),
                    text: api::text(c, "/text"),
                })
                .filter(|r| !r.text.is_empty())
                .collect()
        })
        .unwrap_or_default();
    Page {
        title: api::title_from(&caption, &format!("{} on TikTok", stats.author)),
        url: video_url(item),
        published: published(item),
        image: cover(item),
        stats,
        caption,
        transcript,
        comments: remarks,
        ..Page::default()
    }
}

#[async_trait]
impl Adapter for TikTok {
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
                let date = match call.opts.freshness.as_deref() {
                    Some("day") => "yesterday",
                    Some("week") => "this-week",
                    Some("month") => "this-month",
                    // No year window: six months is the widest one inside it.
                    Some("year") => "last-6-months",
                    _ => "",
                };
                let r = api::get(
                    self,
                    &self.ctx,
                    key,
                    "/v1/tiktok/search/keyword",
                    &[("query", &query), ("date_posted", date), ("trim", "true")],
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
                let video = api::get(
                    self,
                    &self.ctx,
                    key,
                    "/v2/tiktok/video",
                    &[("url", url), ("get_transcript", "true"), ("trim", "true")],
                    call.timeout,
                );
                let comments = api::get(
                    self,
                    &self.ctx,
                    key,
                    "/v1/tiktok/video/comments",
                    &[("url", url), ("trim", "true")],
                    call.timeout,
                );
                let (video, comments) = tokio::join!(video, comments);
                let video = video?;
                let comments = api::side("comments", comments);
                let mut page = parse_video(&video.body, comments.body.as_ref()).with(&comments);
                page.credits += video.credits;
                Ok(page.output("tiktok"))
            }
            other => Err(super::direct::unsupported("tiktok", other)),
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
    fn search_hits_carry_caption_stats_and_url() {
        let hits = parse_search(&fixture("tiktok_search"));
        assert_eq!(hits.len(), 3);
        let h = &hits[0];
        assert_eq!(
            h.url,
            "https://www.tiktok.com/@jerryytech/video/7681548583702695181"
        );
        assert!(
            h.title.starts_with("I wanna know your keyboard size"),
            "{}",
            h.title
        );
        let e = h.extra.as_ref().unwrap();
        assert_eq!(e["views"], 72_671);
        assert_eq!(e["likes"], 1555);
        assert_eq!(e["author"], "@jerryytech");
        assert_eq!(e["duration_s"], 38.4);
        assert!(h.published.as_deref().unwrap().starts_with("2026-"));
        assert_eq!(h.source, "tiktok");
    }

    #[test]
    fn a_video_reads_whole_with_transcript_and_comments() {
        let page = parse_video(&fixture("tiktok_video"), Some(&fixture("tiktok_comments")));
        assert!(page
            .transcript
            .as_deref()
            .unwrap()
            .starts_with("What your keyboard size says"));
        assert_eq!(page.comments.len(), 3);
        assert_eq!(page.comments[0].text, "75% for life");
        assert_eq!(page.comments[0].likes, Some(339));
        let r = page.into_result("tiktok");
        assert!(r.snippet.contains("## Transcript"), "{}", r.snippet);
        assert!(
            r.snippet.contains("[339] @adam070513: 75% for life"),
            "{}",
            r.snippet
        );
        assert_eq!(r.extra.as_ref().unwrap()["transcript"], true);
        assert_eq!(r.extra.as_ref().unwrap()["verdict"], "ok");
    }
}
