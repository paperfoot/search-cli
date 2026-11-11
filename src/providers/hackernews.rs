//! `hackernews`: Hacker News through Algolia's free HN API (no key).
//! `community` searches stories and comments; extract reads a
//! `news.ycombinator.com/item?id=N` thread with every comment.

use super::spec::{FilterSupport, Price};
use super::thread::{self, Comment, Post};
use super::{Adapter, Call, Output, ProviderSpec};
use crate::context::AppContext;
use crate::errors::SearchError;
use crate::types::{FailureCategory, Mode, SearchResult};
use async_trait::async_trait;
use serde::Deserialize;
use std::sync::Arc;
use std::time::Duration;

const API: &str = "https://hn.algolia.com/api/v1";

pub static SPEC: ProviderSpec = ProviderSpec {
    name: "hackernews",
    env_keys: &[],
    modes: &[Mode::Community, Mode::Extract],
    filters: FilterSupport {
        freshness: true,
        domains: false,
        country: false,
        lang: false,
    },
    max_count: 50,
    timeout: Duration::from_secs(10),
    billed_on_send: false,
    price: Price {
        per_call: 0,
        per_extra_result: 0,
        included_results: 0,
    },
};

pub struct HackerNews {
    ctx: Arc<AppContext>,
}

impl HackerNews {
    pub fn new(ctx: Arc<AppContext>) -> Self {
        Self { ctx }
    }
}

/// The item id of a Hacker News thread URL, if `url` is one.
pub fn item_id(url: &str) -> Option<u64> {
    let u = url::Url::parse(url.trim()).ok()?;
    let host = u.host_str()?.to_ascii_lowercase();
    if host != "news.ycombinator.com" || u.path() != "/item" {
        return None;
    }
    u.query_pairs()
        .find(|(k, _)| k == "id")
        .and_then(|(_, v)| v.parse().ok())
}

fn item_url(id: &str) -> String {
    format!("https://news.ycombinator.com/item?id={id}")
}

#[derive(Debug, Deserialize)]
struct SearchBody {
    #[serde(default)]
    hits: Vec<Hit>,
}

#[derive(Debug, Deserialize)]
struct Hit {
    #[serde(rename = "objectID")]
    object_id: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    author: Option<String>,
    #[serde(default)]
    points: Option<i64>,
    #[serde(default)]
    num_comments: Option<u64>,
    #[serde(default)]
    story_text: Option<String>,
    #[serde(default)]
    comment_text: Option<String>,
    #[serde(default)]
    story_id: Option<u64>,
    #[serde(default)]
    story_title: Option<String>,
    #[serde(default)]
    created_at: Option<String>,
    #[serde(default, rename = "_tags")]
    tags: Vec<String>,
}

fn parse_search(body: &str) -> Result<Output, SearchError> {
    let parsed: SearchBody = serde_json::from_str(body).map_err(|e| parse_error(&e))?;
    let results = parsed
        .hits
        .into_iter()
        .map(|h| {
            let is_comment = h.tags.iter().any(|t| t == "comment") || h.comment_text.is_some();
            let mut extra = serde_json::json!({
                "author": h.author,
                "type": if is_comment { "comment" } else { "story" },
            });
            let obj = extra.as_object_mut().expect("object");
            if let Some(p) = h.points {
                obj.insert("points".into(), p.into());
            }
            if is_comment {
                let story = h.story_title.clone().unwrap_or_default();
                if let Some(id) = h.story_id {
                    obj.insert("story_url".into(), item_url(&id.to_string()).into());
                }
                SearchResult {
                    title: format!("Comment on: {story}"),
                    url: item_url(&h.object_id),
                    snippet: thread::clip(
                        &thread::html_to_text(h.comment_text.as_deref().unwrap_or("")),
                        500,
                    ),
                    source: "hackernews".into(),
                    published: h.created_at,
                    image_url: None,
                    extra: Some(extra),
                }
            } else {
                if let Some(n) = h.num_comments {
                    obj.insert("num_comments".into(), n.into());
                }
                if let Some(link) = h.url.as_deref().filter(|l| !l.is_empty()) {
                    obj.insert("link".into(), link.into());
                }
                let text = thread::html_to_text(h.story_text.as_deref().unwrap_or(""));
                let snippet = if text.is_empty() {
                    format!(
                        "{} points, {} comments{}",
                        h.points.unwrap_or(0),
                        h.num_comments.unwrap_or(0),
                        h.url
                            .as_deref()
                            .filter(|l| !l.is_empty())
                            .map(|l| format!(" · {l}"))
                            .unwrap_or_default()
                    )
                } else {
                    thread::clip(&text, 500)
                };
                SearchResult {
                    title: h.title.unwrap_or_default(),
                    url: item_url(&h.object_id),
                    snippet,
                    source: "hackernews".into(),
                    published: h.created_at,
                    image_url: None,
                    extra: Some(extra),
                }
            }
        })
        .collect();
    Ok(Output {
        results,
        ..Output::default()
    })
}

#[derive(Debug, Deserialize)]
struct Item {
    id: u64,
    #[serde(default)]
    author: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    points: Option<i64>,
    #[serde(default)]
    created_at_i: Option<i64>,
    #[serde(default)]
    parent_id: Option<u64>,
    #[serde(default)]
    children: Vec<Item>,
}

fn parse_item(body: &str) -> Result<Output, SearchError> {
    let root: Item = serde_json::from_str(body).map_err(|e| parse_error(&e))?;
    let mut comments = Vec::new();
    // Algolia already sends children in HN's ranking order: keep it.
    let mut stack: Vec<(&Item, usize)> = root.children.iter().rev().map(|c| (c, 0)).collect();
    while let Some((item, depth)) = stack.pop() {
        let body = thread::html_to_text(item.text.as_deref().unwrap_or(""));
        // A deleted or dead comment has no author and no text.
        if item.author.is_some() || !body.is_empty() {
            comments.push(Comment {
                id: item.id.to_string(),
                parent: item
                    .parent_id
                    .filter(|p| *p != root.id)
                    .map(|p| p.to_string()),
                author: item.author.clone().unwrap_or_else(|| "[deleted]".into()),
                score: item.points,
                depth,
                created: item
                    .created_at_i
                    .map(thread::iso_from_unix)
                    .unwrap_or_default(),
                body,
            });
        }
        stack.extend(item.children.iter().rev().map(|c| (c, depth + 1)));
    }
    let post = Post {
        title: root.title.clone().unwrap_or_default(),
        url: item_url(&root.id.to_string()),
        author: root.author.clone().unwrap_or_default(),
        score: root.points,
        num_comments: Some(comments.len() as u64),
        community: "Hacker News".into(),
        created: root
            .created_at_i
            .map(thread::iso_from_unix)
            .unwrap_or_default(),
        body: thread::html_to_text(root.text.as_deref().unwrap_or("")),
        link: root.url.clone().filter(|l| !l.is_empty()),
    };
    Ok(Output {
        results: vec![thread::result(
            "hackernews",
            &post,
            &comments,
            "algolia",
            &[],
        )],
        ..Output::default()
    })
}

fn parse_error(e: &serde_json::Error) -> SearchError {
    SearchError::Upstream {
        provider: "hackernews",
        status: 200,
        category: FailureCategory::Parse,
        message: format!("unreadable Algolia response: {e}"),
    }
}

/// The query string for a community search.
fn search_params(call: &Call<'_>) -> Vec<(&'static str, String)> {
    // An `r/<name>` names a subreddit for the reddit provider; HN has none.
    let query: Vec<&str> = call
        .query
        .split_whitespace()
        .filter(|w| !w.trim_start_matches('/').starts_with("r/"))
        .collect();
    let mut params = vec![
        ("query", query.join(" ")),
        ("tags", "(story,comment)".to_string()),
        ("hitsPerPage", call.count.to_string()),
    ];
    if let Some(days) = call
        .opts
        .freshness
        .as_deref()
        .and_then(super::freshness_days)
    {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        params.push((
            "numericFilters",
            format!("created_at_i>{}", now.saturating_sub(days * 86_400)),
        ));
    }
    params
}

#[async_trait]
impl Adapter for HackerNews {
    fn spec(&self) -> &'static ProviderSpec {
        &SPEC
    }

    fn is_configured(&self) -> bool {
        true // keyless
    }

    async fn call(&self, call: &Call<'_>) -> Result<Output, SearchError> {
        match call.mode {
            Mode::Community => {
                let params = search_params(call);
                let url = format!("{API}/search");
                let resp = super::send(self, call.timeout, || {
                    self.ctx.client.get(&url).query(&params)
                })
                .await?;
                let body = resp.text().await.map_err(SearchError::Http)?;
                parse_search(&body)
            }
            Mode::Extract | Mode::Scrape => {
                let Some(id) = item_id(call.query) else {
                    return Err(SearchError::Api {
                        provider: "hackernews",
                        code: "not_a_thread",
                        message: "hackernews extracts only news.ycombinator.com/item?id=N URLs"
                            .into(),
                        status: None,
                    });
                };
                let url = format!("{API}/items/{id}");
                let resp = super::send(self, call.timeout, || self.ctx.client.get(&url)).await?;
                let body = resp.text().await.map_err(SearchError::Http)?;
                parse_item(&body)
            }
            other => Err(super::direct::unsupported("hackernews", other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_maps_stories_and_comments_to_discussion_urls() {
        let out =
            parse_search(include_str!("../../tests/fixtures/hackernews/search.json")).unwrap();
        assert!(out.results.len() >= 4);
        let story = &out.results[0];
        assert_eq!(story.url, "https://news.ycombinator.com/item?id=26406989");
        assert_eq!(story.title, "Why asynchronous Rust doesn't work");
        let extra = story.extra.as_ref().unwrap();
        assert_eq!(extra["points"], 603);
        assert_eq!(extra["num_comments"], 482);
        assert_eq!(extra["author"], "tazjin");
        assert_eq!(extra["type"], "story");
        let comment = out
            .results
            .iter()
            .find(|r| r.extra.as_ref().unwrap()["type"] == "comment")
            .expect("a comment hit");
        assert!(comment.title.starts_with("Comment on: "));
        assert!(!comment.snippet.contains("&#x2F;"), "{}", comment.snippet);
    }

    #[test]
    fn item_extract_keeps_every_comment_with_depth_and_parent() {
        let out = parse_item(include_str!("../../tests/fixtures/hackernews/item.json")).unwrap();
        let r = &out.results[0];
        assert_eq!(r.title, "Futurelock: A subtle risk in async Rust");
        let comments = r.extra.as_ref().unwrap()["comments"].as_array().unwrap();
        assert!(comments.len() >= 5, "{}", comments.len());
        assert_eq!(comments[0]["depth"], 0);
        assert!(comments[0]["parent"].is_null());
        let reply = comments.iter().find(|c| c["depth"] == 1).unwrap();
        assert!(reply["parent"].is_string());
        assert!(r.snippet.contains("(depth 1): "), "{}", r.snippet);
    }

    #[test]
    fn a_subreddit_cue_is_dropped_from_the_hn_query() {
        let opts = crate::types::SearchOpts::default();
        let call = Call {
            mode: Mode::Community,
            query: "mechanical keyboard r/webdev",
            count: 5,
            opts: &opts,
            timeout: Duration::from_secs(1),
            key: "",
        };
        assert_eq!(search_params(&call)[0].1, "mechanical keyboard");
    }

    #[test]
    fn only_item_urls_are_threads() {
        assert_eq!(
            item_id("https://news.ycombinator.com/item?id=45774086"),
            Some(45774086)
        );
        assert_eq!(item_id("https://news.ycombinator.com/news"), None);
        assert_eq!(item_id("https://example.com/item?id=1"), None);
    }
}
