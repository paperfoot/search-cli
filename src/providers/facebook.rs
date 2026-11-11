//! `facebook`: Facebook page search by name, and a Facebook URL read whole:
//! a post or reel (text, stats, transcript, top comments) or a page (its
//! details and latest posts). Facebook has no public keyword search of
//! posts, so search finds pages only. See [`super::social_api`].

use super::social_api::{self as api, Page, Remark, Stats};
use super::{Adapter, Call, Output, ProviderSpec};
use crate::context::AppContext;
use crate::errors::SearchError;
use crate::types::{FailureCategory, Mode, SearchResult};
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;

pub static SPEC: ProviderSpec = ProviderSpec {
    name: "facebook",
    env_keys: api::ENV_KEYS,
    modes: &[Mode::Social, Mode::Extract],
    filters: api::filters(false),
    max_count: 15,
    timeout: api::TIMEOUT,
    billed_on_send: true,
    price: api::PRICE,
};

pub struct Facebook {
    ctx: Arc<AppContext>,
}

impl Facebook {
    pub fn new(ctx: Arc<AppContext>) -> Self {
        Self { ctx }
    }
}

/// True for a post, reel, video or share link; false for a page.
pub fn is_post_url(url: &str) -> bool {
    let Ok(u) = url::Url::parse(url.trim()) else {
        return false;
    };
    if u.host_str().is_some_and(|h| h.ends_with("fb.watch")) {
        return true;
    }
    let path = u.path().to_ascii_lowercase();
    [
        "/posts/",
        "/videos/",
        "/reel/",
        "/reels/",
        "/watch",
        "/permalink.php",
        "/story.php",
        "/share/",
        "/photo",
    ]
    .iter()
    .any(|p| path.contains(p))
}

/// Pages of `/v1/facebook/adLibrary/search/companies`.
pub fn parse_search(body: &Value) -> Vec<SearchResult> {
    body["searchResults"]
        .as_array()
        .map(|list| {
            list.iter()
                .filter(|p| p["page_is_deleted"] != Value::Bool(true))
                .map(|p| {
                    let alias = api::text(p, "/page_alias");
                    let id = api::text(p, "/page_id");
                    let url = format!(
                        "https://www.facebook.com/{}",
                        if alias.is_empty() { &id } else { &alias }
                    );
                    let name = api::text(p, "/name");
                    let category = api::text(p, "/category");
                    let stats = Stats {
                        author: name.clone(),
                        followers: api::num(p, "/likes"),
                        ..Stats::default()
                    };
                    let mut about = vec![category];
                    if let Some(likes) = stats.followers {
                        about.push(format!("{} page likes", api::compact(likes)));
                    }
                    let ig = api::text(p, "/ig_username");
                    if !ig.is_empty() {
                        about.push(format!("Instagram @{ig}"));
                    }
                    if p["verification"].as_str() == Some("BLUE_VERIFIED") {
                        about.push("verified".into());
                    }
                    let about: Vec<String> = about.into_iter().filter(|s| !s.is_empty()).collect();
                    let mut r = api::hit(
                        "facebook",
                        name,
                        url,
                        &about.join(" · "),
                        None,
                        api::non_empty(api::text(p, "/image_uri")),
                        &stats,
                    );
                    if let Some(obj) = r.extra.as_mut().and_then(Value::as_object_mut) {
                        obj.insert("kind".into(), Value::from("page"));
                        obj.insert("page_id".into(), Value::from(id));
                    }
                    r
                })
                .collect()
        })
        .unwrap_or_default()
}

/// A post or reel read whole from `/v1/facebook/post` (with its
/// transcript) and `/v1/facebook/post/comments`.
pub fn parse_post(url: &str, post: &Value, comments: Option<&Value>) -> Page {
    let text = api::text(post, "/description");
    let stats = Stats {
        author: api::text(post, "/author/name"),
        views: api::num(post, "/view_count"),
        likes: api::num(post, "/like_count"),
        comments: api::num(post, "/comment_count"),
        shares: api::num(post, "/share_count"),
        duration_s: api::float(post, "/video/length_in_second"),
        ..Stats::default()
    };
    let transcript = api::non_empty(api::text(post, "/transcript")).map(|t| api::squash(&t));
    Page {
        title: api::title_from(&text, &format!("{} on Facebook", stats.author)),
        url: api::non_empty(api::text(post, "/url")).unwrap_or_else(|| url.to_string()),
        published: api::non_empty(api::text(post, "/creation_time")),
        image: api::non_empty(api::text(post, "/video/thumbnail"))
            .or_else(|| api::non_empty(api::text(post, "/image_url"))),
        stats,
        caption: text,
        transcript,
        comments: comments
            .and_then(|c| c["comments"].as_array())
            .map(|list| {
                list.iter()
                    .map(|c| Remark {
                        author: api::text(c, "/author/name"),
                        likes: api::num(c, "/reaction_count"),
                        text: api::text(c, "/text"),
                    })
                    .filter(|r| !r.text.is_empty())
                    .collect()
            })
            .unwrap_or_default(),
        ..Page::default()
    }
}

/// A page read from `/v1/facebook/profile` and its latest posts from
/// `/v1/facebook/profile/posts`.
pub fn parse_page(url: &str, profile: &Value, posts: Option<&Value>) -> Page {
    let name =
        api::non_empty(api::text(profile, "/name")).unwrap_or_else(|| "Facebook page".into());
    let mut details = Vec::new();
    for (label, ptr) in [
        ("Category", "/category"),
        ("Website", "/website"),
        ("Address", "/address"),
        ("Phone", "/phone"),
        ("Email", "/email"),
        ("Rating", "/rating"),
        ("Created", "/creationDate"),
    ] {
        let v = api::text(profile, ptr);
        if !v.is_empty() {
            details.push(format!("{label}: {v}"));
        }
    }
    if let Some(n) = api::num(profile, "/likeCount") {
        details.push(format!("Page likes: {n}"));
    }
    let latest: Vec<String> = posts
        .and_then(|p| p["posts"].as_array())
        .map(|list| {
            list.iter()
                .map(|p| {
                    let when = api::text(p, "/creation_time");
                    let counts = [
                        api::num(p, "/reactionCount").map(|n| format!("{n} reactions")),
                        api::num(p, "/commentCount").map(|n| format!("{n} comments")),
                        api::num(p, "/share_count").map(|n| format!("{n} shares")),
                    ]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join(", ");
                    format!(
                        "- {when} ({counts}) {}\n  {}",
                        super::thread::clip(&api::squash(&api::text(p, "/text")), 400),
                        api::text(p, "/url")
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    let mut sections = Vec::new();
    if !details.is_empty() {
        sections.push(("Page".to_string(), details.join("\n")));
    }
    if !latest.is_empty() {
        sections.push(("Latest posts".to_string(), latest.join("\n")));
    }
    Page {
        title: name.clone(),
        url: api::non_empty(api::text(profile, "/url")).unwrap_or_else(|| url.to_string()),
        image: api::non_empty(api::text(profile, "/profilePicLarge")),
        stats: Stats {
            author: name,
            followers: api::num(profile, "/followerCount"),
            ..Stats::default()
        },
        caption: api::text(profile, "/pageIntro"),
        sections,
        ..Page::default()
    }
}

#[async_trait]
impl Adapter for Facebook {
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
                let r = api::get(
                    self,
                    &self.ctx,
                    key,
                    "/v1/facebook/adLibrary/search/companies",
                    &[("query", &query)],
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
                if is_post_url(url) {
                    let post = api::get(
                        self,
                        &self.ctx,
                        key,
                        "/v1/facebook/post",
                        &[("url", url), ("get_transcript", "true")],
                        call.timeout,
                    );
                    let comments = api::get(
                        self,
                        &self.ctx,
                        key,
                        "/v1/facebook/post/comments",
                        &[("url", url)],
                        call.timeout,
                    );
                    let (post, comments) = tokio::join!(post, comments);
                    let post = post?;
                    let comments = api::side("comments", comments);
                    let mut page =
                        parse_post(url, &post.body, comments.body.as_ref()).with(&comments);
                    page.credits += post.credits;
                    Ok(page.output("facebook"))
                } else {
                    let profile = api::get(
                        self,
                        &self.ctx,
                        key,
                        "/v1/facebook/profile",
                        &[("url", url)],
                        call.timeout,
                    );
                    let posts = api::get(
                        self,
                        &self.ctx,
                        key,
                        "/v1/facebook/profile/posts",
                        &[("url", url)],
                        call.timeout,
                    );
                    let (profile, posts) = tokio::join!(profile, posts);
                    let profile = profile?;
                    let posts = api::side("latest posts", posts);
                    let mut page = parse_page(url, &profile.body, posts.body.as_ref()).with(&posts);
                    page.credits += profile.credits;
                    Ok(page.output("facebook"))
                }
            }
            other => Err(super::direct::unsupported("facebook", other)),
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
    fn page_search_finds_pages_with_likes_and_instagram() {
        let hits = parse_search(&fixture("facebook_search"));
        assert!(!hits.is_empty());
        let h = &hits[0];
        assert_eq!(h.title, "Keychron");
        assert_eq!(h.url, "https://www.facebook.com/keychron");
        assert!(h.snippet.contains("Instagram @keychron"), "{}", h.snippet);
        assert_eq!(h.extra.as_ref().unwrap()["kind"], "page");
    }

    #[test]
    fn a_page_reads_with_details_and_latest_posts() {
        let page = parse_page(
            "https://www.facebook.com/keychron",
            &fixture("facebook_page"),
            Some(&fixture("facebook_page_posts")),
        );
        assert_eq!(page.title, "Keychron");
        assert_eq!(page.stats.followers, Some(66_000));
        let text = page.render();
        assert!(text.contains("## Latest posts"), "{text}");
        assert!(
            text.contains("Website: https://www.keychron.com/"),
            "{text}"
        );
    }

    #[test]
    fn post_and_page_urls_are_told_apart() {
        assert!(is_post_url(
            "https://www.facebook.com/reel/1535656380759655"
        ));
        assert!(is_post_url(
            "https://www.facebook.com/keychron/posts/pfbid02x6"
        ));
        assert!(is_post_url("https://fb.watch/abc123/"));
        assert!(!is_post_url("https://www.facebook.com/keychron"));
    }
}
