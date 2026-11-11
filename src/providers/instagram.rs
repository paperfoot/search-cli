//! `instagram`: Instagram reels search, and an Instagram post or reel URL
//! read whole (caption, stats, transcript, top comments). See
//! [`super::social_api`].

use super::social_api::{self as api, Page, Remark, Stats};
use super::{Adapter, Call, FilterSupport, Output, ProviderSpec};
use crate::context::AppContext;
use crate::errors::SearchError;
use crate::types::{FailureCategory, Mode, SearchOpts, SearchResult};
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;

pub static SPEC: ProviderSpec = ProviderSpec {
    name: "instagram",
    env_keys: api::ENV_KEYS,
    modes: &[Mode::Social, Mode::Extract],
    filters: api::filters(true),
    max_count: 10,
    timeout: api::TIMEOUT,
    billed_on_send: true,
    price: api::PRICE,
};

pub struct Instagram {
    ctx: Arc<AppContext>,
}

impl Instagram {
    pub fn new(ctx: Arc<AppContext>) -> Self {
        Self { ctx }
    }
}

/// Reels search windows: a week, a month or a year. There is no day window.
fn window(freshness: Option<&str>) -> &'static str {
    match freshness {
        Some("week") => "last-week",
        Some("month") => "last-month",
        Some("year") => "last-year",
        _ => "",
    }
}

fn owner(v: &Value) -> String {
    let u = api::text(v, "/owner/username");
    if u.is_empty() {
        String::new()
    } else {
        format!("@{u}")
    }
}

/// Hits of `/v2/instagram/reels/search`.
pub fn parse_search(body: &Value) -> Vec<SearchResult> {
    body["reels"]
        .as_array()
        .map(|reels| {
            reels
                .iter()
                .map(|r| {
                    let caption = api::text(r, "/caption");
                    let stats = Stats {
                        author: owner(r),
                        followers: api::num(r, "/owner/follower_count"),
                        views: api::num(r, "/video_play_count")
                            .or_else(|| api::num(r, "/video_view_count"))
                            .filter(|n| *n > 0),
                        likes: api::num(r, "/like_count"),
                        comments: api::num(r, "/comment_count"),
                        shares: None,
                        duration_s: api::float(r, "/video_duration"),
                    };
                    let url = api::non_empty(api::text(r, "/url")).unwrap_or_else(|| {
                        format!(
                            "https://www.instagram.com/reel/{}/",
                            api::text(r, "/shortcode")
                        )
                    });
                    let title =
                        api::title_from(&caption, &format!("{} on Instagram", stats.author));
                    api::hit(
                        "instagram",
                        title,
                        url,
                        &caption,
                        api::non_empty(api::text(r, "/taken_at")),
                        api::non_empty(api::text(r, "/thumbnail_src")),
                        &stats,
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

/// A post read whole from `/v1/instagram/post` and, for a video,
/// `/v2/instagram/media/transcript`.
pub fn parse_post(url: &str, post: &Value, transcript: Option<&Value>) -> Page {
    // Trimmed answers put the media at the top; full ones under `data`.
    let m = post
        .get("xdt_shortcode_media")
        .or_else(|| post.pointer("/data/xdt_shortcode_media"))
        .unwrap_or(&Value::Null);
    let caption = api::text(m, "/edge_media_to_caption/edges/0/node/text");
    let stats = Stats {
        author: owner(m),
        followers: api::num(m, "/owner/edge_followed_by/count"),
        views: api::num(m, "/video_play_count")
            .or_else(|| api::num(m, "/video_view_count"))
            .filter(|n| *n > 0),
        likes: api::num(m, "/edge_media_preview_like/count"),
        comments: api::num(m, "/edge_media_to_parent_comment/count"),
        shares: None,
        duration_s: api::float(m, "/video_duration"),
    };
    let comments = m
        .pointer("/edge_media_to_parent_comment/edges")
        .and_then(Value::as_array)
        .map(|edges| {
            edges
                .iter()
                .map(|e| Remark {
                    author: format!("@{}", api::text(e, "/node/owner/username")),
                    likes: api::num(e, "/node/edge_liked_by/count"),
                    text: api::text(e, "/node/text"),
                })
                .filter(|r| !r.text.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let transcript = transcript
        .and_then(|t| t["transcripts"].as_array())
        .map(|list| {
            list.iter()
                .filter_map(|t| t["text"].as_str())
                .map(api::squash)
                .collect::<Vec<_>>()
                .join("\n\n")
        })
        .filter(|t| !t.is_empty());
    let shortcode = api::text(m, "/shortcode");
    Page {
        title: api::title_from(&caption, &format!("{} on Instagram", stats.author)),
        url: if shortcode.is_empty() {
            url.to_string()
        } else {
            format!("https://www.instagram.com/p/{shortcode}/")
        },
        published: api::iso(m, "/taken_at_timestamp"),
        image: api::non_empty(api::text(m, "/thumbnail_src"))
            .or_else(|| api::non_empty(api::text(m, "/display_url"))),
        stats,
        caption,
        transcript,
        comments,
        ..Page::default()
    }
}

#[async_trait]
impl Adapter for Instagram {
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

    /// Reels search has no one-day window, so `-f day` is reported as not
    /// applied.
    fn applied_filters(&self, opts: &SearchOpts) -> FilterSupport {
        api::filters(opts.freshness.as_deref() != Some("day"))
    }

    async fn call(&self, call: &Call<'_>) -> Result<Output, SearchError> {
        let key = call.key;
        match call.mode {
            Mode::Social => {
                let query = api::clean_query(call.query);
                let r = api::get(
                    self,
                    &self.ctx,
                    key,
                    "/v2/instagram/reels/search",
                    &[
                        ("query", &query),
                        ("date_posted", window(call.opts.freshness.as_deref())),
                    ],
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
                let post = api::get(
                    self,
                    &self.ctx,
                    key,
                    "/v1/instagram/post",
                    &[("url", url), ("trim", "true")],
                    call.timeout,
                );
                // AI transcription, flat price up to two minutes of video.
                let transcript = api::get(
                    self,
                    &self.ctx,
                    key,
                    "/v2/instagram/media/transcript",
                    &[("url", url)],
                    call.timeout,
                );
                let (post, transcript) = tokio::join!(post, transcript);
                let post = post?;
                let transcript = api::side("transcript", transcript);
                let mut page =
                    parse_post(url, &post.body, transcript.body.as_ref()).with(&transcript);
                page.credits += post.credits;
                Ok(page.output("instagram"))
            }
            other => Err(super::direct::unsupported("instagram", other)),
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
    fn reels_search_hits_carry_caption_owner_and_stats() {
        let hits = parse_search(&fixture("instagram_search"));
        assert_eq!(hits.len(), 3);
        let h = &hits[0];
        assert_eq!(h.url, "https://www.instagram.com/reel/DeUhhCNpC9T/");
        assert_eq!(
            h.title,
            "The Most SILENT Mechanical Keyboard I’ve Ever Tried"
        );
        let e = h.extra.as_ref().unwrap();
        assert_eq!(e["author"], "@austinnotduncan");
        assert_eq!(e["likes"], 815);
        assert_eq!(e["num_comments"], 13);
        assert!(h.published.is_some());
    }

    #[test]
    fn a_reel_reads_whole_with_transcript_and_comments() {
        let page = parse_post(
            "https://www.instagram.com/reel/DeUhhCNpC9T/",
            &fixture("instagram_post"),
            Some(&fixture("instagram_transcript")),
        );
        assert_eq!(page.url, "https://www.instagram.com/p/DeUhhCNpC9T/");
        assert!(page.transcript.as_deref().unwrap().contains("Lenovo Yoga"));
        assert_eq!(page.comments.len(), 3);
        assert_eq!(page.stats.likes, Some(815));
        assert_eq!(page.stats.views, Some(26_038));
        assert!(page.published.as_deref().unwrap().starts_with("2026-"));
        assert_eq!(window(Some("day")), "");
        assert_eq!(window(Some("week")), "last-week");
    }
}
