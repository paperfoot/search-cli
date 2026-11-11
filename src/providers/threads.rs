//! `threads`: Threads post search, and a Threads post URL read whole with
//! its replies. See [`super::social_api`].

use super::social_api::{self as api, Page, Remark, Stats};
use super::{Adapter, Call, Output, ProviderSpec};
use crate::context::AppContext;
use crate::errors::SearchError;
use crate::types::{FailureCategory, Mode, SearchResult};
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;

pub static SPEC: ProviderSpec = ProviderSpec {
    name: "threads",
    env_keys: api::ENV_KEYS,
    modes: &[Mode::Social, Mode::Extract],
    filters: api::filters(true),
    max_count: 10,
    timeout: api::TIMEOUT,
    billed_on_send: true,
    price: api::PRICE,
};

pub struct Threads {
    ctx: Arc<AppContext>,
}

impl Threads {
    pub fn new(ctx: Arc<AppContext>) -> Self {
        Self { ctx }
    }
}

fn user(p: &Value) -> String {
    let u = api::text(p, "/user/username");
    if u.is_empty() {
        String::new()
    } else {
        format!("@{u}")
    }
}

fn post_url(p: &Value) -> String {
    api::non_empty(api::text(p, "/url")).unwrap_or_else(|| {
        format!(
            "https://www.threads.com/{}/post/{}",
            user(p),
            api::text(p, "/code")
        )
    })
}

fn stats(p: &Value) -> Stats {
    Stats {
        author: user(p),
        likes: api::num(p, "/like_count"),
        comments: api::num(p, "/text_post_app_info/direct_reply_count"),
        shares: api::num(p, "/text_post_app_info/reshare_count"),
        ..Stats::default()
    }
}

/// Posts of `/v1/threads/search`.
pub fn parse_search(body: &Value) -> Vec<SearchResult> {
    body["posts"]
        .as_array()
        .map(|posts| {
            posts
                .iter()
                .map(|p| {
                    let text = api::text(p, "/caption/text");
                    let s = stats(p);
                    let title = api::title_from(&text, &format!("{} on Threads", s.author));
                    api::hit(
                        "threads",
                        title,
                        post_url(p),
                        &text,
                        api::iso(p, "/taken_at"),
                        None,
                        &s,
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

/// A post and its replies from `/v1/threads/post`.
pub fn parse_post(url: &str, body: &Value) -> Page {
    let p = &body["post"];
    let text = api::text(p, "/caption/text");
    let stats = stats(p);
    Page {
        title: api::title_from(&text, &format!("{} on Threads", stats.author)),
        url: api::non_empty(post_url(p)).unwrap_or_else(|| url.to_string()),
        published: api::iso(p, "/taken_at"),
        stats,
        caption: text,
        comments: body["comments"]
            .as_array()
            .map(|list| {
                list.iter()
                    .map(|c| Remark {
                        author: user(c),
                        likes: api::num(c, "/like_count"),
                        text: api::text(c, "/caption/text"),
                    })
                    .filter(|r| !r.text.is_empty())
                    .collect()
            })
            .unwrap_or_default(),
        ..Page::default()
    }
}

#[async_trait]
impl Adapter for Threads {
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
                let start = api::since(call.opts.freshness.as_deref()).unwrap_or_default();
                let r = api::get(
                    self,
                    &self.ctx,
                    key,
                    "/v1/threads/search",
                    &[("query", &query), ("start_date", &start), ("trim", "true")],
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
                let r = api::get(
                    self,
                    &self.ctx,
                    key,
                    "/v1/threads/post",
                    &[("url", url), ("trim", "true")],
                    call.timeout,
                )
                .await?;
                let mut page = parse_post(url, &r.body);
                page.credits = r.credits;
                Ok(page.output("threads"))
            }
            other => Err(super::direct::unsupported("threads", other)),
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
    fn search_hits_carry_text_author_and_likes() {
        let hits = parse_search(&fixture("threads_search"));
        assert_eq!(hits.len(), 3);
        let h = &hits[0];
        assert_eq!(
            h.url,
            "https://www.threads.com/@dad.life.gaming/post/DfQ_LTsm841"
        );
        assert!(
            h.snippet.contains("Mechanical Gaming Keyboard"),
            "{}",
            h.snippet
        );
        assert_eq!(h.extra.as_ref().unwrap()["author"], "@dad.life.gaming");
        assert!(h.published.as_deref().unwrap().starts_with("2026-"));
    }

    #[test]
    fn a_post_reads_with_its_replies() {
        let page = parse_post(
            "https://www.threads.com/@dad.life.gaming/post/DfQ_LTsm841",
            &fixture("threads_post"),
        );
        assert_eq!(page.comments.len(), 1);
        assert!(page.comments[0].text.contains("rgb"));
        assert!(page.render().contains("## Top comments"));
    }
}
