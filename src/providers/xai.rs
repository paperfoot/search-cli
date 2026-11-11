//! xAI: live X search through the Responses API and the `x_search` tool.
//! Grok searches X, then answers in a fixed JSON shape (posts + summary). A
//! post becomes a result only when its status id is among the response's
//! `url_citation` annotations, so every result is a post the search actually
//! fetched; the summary goes to `answers`. xAI bills tokens plus posts
//! fetched and reports the total in `usage.cost_in_usd_ticks`
//! (1 USD = 10^10 ticks), which is the cost reported here.
//! https://docs.x.ai/developers/tools/x-search

use super::spec::{FilterSupport, Price};
use super::{Adapter, Call, Output, ProviderSpec};
use crate::context::AppContext;
use crate::errors::SearchError;
use crate::types::{Answer, FailureCategory, Mode, SearchResult};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

const ENDPOINT: &str = "https://api.x.ai/v1/responses";
/// `grok-4-1-fast` was retired on 15 May 2026 and only redirects here; name
/// the model we pay for.
const MODEL: &str = "grok-4.3";
/// Bounds the search rounds, and with them the posts fetched (billed per post).
const MAX_TURNS: u8 = 2;
/// The API caps each handle list at 20.
const MAX_HANDLES: usize = 20;
const TICKS_PER_MICRO: u64 = 10_000;
const SNIPPET_MAX: usize = 2_000;

pub static SPEC: ProviderSpec = ProviderSpec {
    name: "xai",
    env_keys: &["XAI_API_KEY"],
    modes: &[Mode::Social, Mode::Deep],
    // In social, domains map to X handles (see registry::filter_support's
    // note). Deep sends no domain filter; a filter that rules x.com out skips
    // the call (see `deep_skip`).
    filters: FilterSupport {
        freshness: true,
        domains: true,
        country: false,
        lang: false,
    },
    max_count: 20,
    timeout: Duration::from_secs(60),
    billed_on_send: true,
    price: Price {
        per_call: 45_000,
        per_extra_result: 0,
        included_results: 0,
    },
};

pub struct Xai {
    ctx: Arc<AppContext>,
}

impl Xai {
    pub fn new(ctx: Arc<AppContext>) -> Self {
        Self { ctx }
    }
}

/// X handles from the domain filters, bare (no `@`). Deep mode's domains are
/// web domains, not handles, so deep sends none.
fn handles(call: &Call<'_>, list: &[String]) -> Vec<String> {
    let mut all = all_handles(call, list);
    all.truncate(MAX_HANDLES);
    all
}

/// Every handle, uncapped: the local deny filter must cover the entries past
/// the 20 xAI accepts.
fn all_handles(call: &Call<'_>, list: &[String]) -> Vec<String> {
    if call.mode != Mode::Social {
        return Vec::new();
    }
    list.iter()
        .map(|h| h.trim().trim_start_matches('@').to_string())
        .filter(|h| !h.is_empty())
        .collect()
}

/// A bare host from a -d / --exclude-domain value: no scheme, `www.`, path
/// or case.
fn bare_host(domain: &str) -> String {
    let d = domain.trim();
    let d = d.split_once("://").map_or(d, |(_, rest)| rest);
    let d = d.split(['/', '?', '#']).next().unwrap_or_default();
    let d = d.to_ascii_lowercase();
    d.strip_prefix("www.").map(str::to_string).unwrap_or(d)
}

fn is_x_domain(domain: &str) -> bool {
    matches!(bare_host(domain).as_str(), "x.com" | "twitter.com")
}

/// Deep has no domain filter to send (-d holds web domains, not handles) and
/// every result is an x.com post, so when the filters rule x.com out (an
/// allow list without it, or a deny list with it) the call would be billed
/// only to return rows the user excluded. Skip it: no rows, no answer, and a
/// cost of `Some(0)` (`None` would make the engine bill the estimate).
fn deep_skip(call: &Call<'_>) -> Option<Output> {
    let opts = call.opts;
    let off_x = (!opts.include_domains.is_empty()
        && !opts.include_domains.iter().any(|d| is_x_domain(d)))
        || opts.exclude_domains.iter().any(|d| is_x_domain(d));
    (call.mode == Mode::Deep && off_x).then(|| Output {
        results: Vec::new(),
        answers: Vec::new(),
        cost_micros: Some(0),
    })
}

fn body(call: &Call<'_>) -> Value {
    let opts = call.opts;
    let mut tool = json!({ "type": "x_search" });
    // to_date is exclusive: setting it to today drops today's posts, so it is
    // never sent.
    if let Some(days) = opts.freshness.as_deref().and_then(super::freshness_days) {
        tool["from_date"] = json!(super::date_days_ago(days));
    }
    // xAI refuses both lists in one request: the allow list wins, and the
    // deny list is applied to the posts in `parse`.
    let allow = handles(call, &opts.include_domains);
    let deny = handles(call, &opts.exclude_domains);
    if !allow.is_empty() {
        tool["allowed_x_handles"] = json!(allow);
    } else if !deny.is_empty() {
        tool["excluded_x_handles"] = json!(deny);
    }
    let prompt = format!(
        "Search X for posts about: {}\n\
         Return up to {} of the most relevant posts, each with the author's handle (without @), \
         the post text, its date (ISO 8601) and its URL, then a two-sentence summary of what the \
         posts say. Only list posts the search returned.",
        call.query, call.count
    );
    json!({
        "model": MODEL,
        "input": [{ "role": "user", "content": prompt }],
        "tools": [tool],
        "max_turns": MAX_TURNS,
        "store": false,
        "text": { "format": {
            "type": "json_schema",
            "name": "x_posts",
            "strict": true,
            "schema": {
                "type": "object",
                "additionalProperties": false,
                "required": ["posts", "summary"],
                "properties": {
                    "posts": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "additionalProperties": false,
                            "required": ["url", "author", "text", "date"],
                            "properties": {
                                "url": { "type": "string" },
                                "author": { "type": "string" },
                                "text": { "type": "string" },
                                "date": { "type": "string" }
                            }
                        }
                    },
                    "summary": { "type": "string" }
                }
            }
        } },
    })
}

/// The x.com search page for the query, properly encoded (`AT&T`, `#tag`).
pub(crate) fn x_search_url(query: &str) -> String {
    url::Url::parse_with_params("https://x.com/search", &[("q", query), ("f", "live")])
        .map(String::from)
        .unwrap_or_else(|_| "https://x.com/search".to_string())
}

#[derive(Deserialize, Default)]
struct XaiResponse {
    status: Option<String>,
    error: Option<XaiError>,
    #[serde(default)]
    output: Vec<XaiOutputItem>,
    usage: Option<XaiUsage>,
}

#[derive(Deserialize)]
struct XaiError {
    message: Option<String>,
}

#[derive(Deserialize)]
struct XaiOutputItem {
    #[serde(rename = "type")]
    item_type: Option<String>,
    #[serde(default)]
    content: Vec<XaiContent>,
}

#[derive(Deserialize)]
struct XaiContent {
    #[serde(rename = "type")]
    content_type: Option<String>,
    text: Option<String>,
    #[serde(default)]
    annotations: Vec<XaiAnnotation>,
}

#[derive(Deserialize)]
struct XaiAnnotation {
    #[serde(rename = "type")]
    annotation_type: Option<String>,
    url: Option<String>,
}

#[derive(Deserialize, Default)]
struct XaiUsage {
    cost_in_usd_ticks: Option<u64>,
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    input_tokens_details: Option<InputDetails>,
    server_side_tool_usage_details: Option<ToolUsage>,
}

#[derive(Deserialize, Default)]
struct InputDetails {
    #[serde(default)]
    cached_tokens: u64,
}

#[derive(Deserialize, Default)]
struct ToolUsage {
    #[serde(default)]
    x_posts_fetched: u64,
    #[serde(default)]
    x_users_fetched: u64,
}

/// What the call cost in micro-USD: the reported ticks, rounded up; else
/// grok-4.3's card ($1.25/M input, $0.20/M cached, $2.50/M output, $5 per
/// 1k posts, $10 per 1k profiles) on the reported usage.
fn cost_micros(usage: &XaiUsage) -> u64 {
    if let Some(ticks) = usage.cost_in_usd_ticks {
        return ticks.div_ceil(TICKS_PER_MICRO);
    }
    let cached = usage
        .input_tokens_details
        .as_ref()
        .map_or(0, |d| d.cached_tokens)
        .min(usage.input_tokens);
    let tools = usage
        .server_side_tool_usage_details
        .as_ref()
        .map_or((0, 0), |t| (t.x_posts_fetched, t.x_users_fetched));
    // In hundredths of a micro-USD per token, to stay in integers.
    let token_cents = (usage.input_tokens - cached) * 125 + cached * 20 + usage.output_tokens * 250;
    token_cents.div_ceil(100) + tools.0 * 5_000 + tools.1 * 10_000
}

#[derive(Deserialize)]
struct Reply {
    #[serde(default)]
    posts: Vec<Post>,
    #[serde(default)]
    summary: String,
}

#[derive(Deserialize)]
struct Post {
    #[serde(default)]
    url: String,
    #[serde(default)]
    author: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    date: String,
}

/// The numeric status id of an x.com / twitter.com post URL.
fn status_id(url: &str) -> Option<&str> {
    let u = url.split_once("://")?.1;
    let u = u.strip_prefix("www.").unwrap_or(u);
    let (host, path) = u.split_once('/')?;
    if !host.eq_ignore_ascii_case("x.com") && !host.eq_ignore_ascii_case("twitter.com") {
        return None;
    }
    let mut parts = path.split(['/', '?', '#']);
    let (_user, kind, id) = (parts.next()?, parts.next()?, parts.next()?);
    (kind == "status" && !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit())).then_some(id)
}

fn clip(s: &str) -> String {
    let s = s.trim();
    if s.chars().count() <= SNIPPET_MAX {
        return s.to_string();
    }
    let mut out: String = s.chars().take(SNIPPET_MAX - 1).collect();
    out.push('…');
    out
}

fn parse(data: XaiResponse, call: &Call<'_>) -> Result<Output, SearchError> {
    let cost = data.usage.as_ref().map(cost_micros);
    if data.error.is_some() || data.status.as_deref() == Some("failed") {
        let message = data
            .error
            .and_then(|e| e.message)
            .filter(|m| !m.trim().is_empty())
            .unwrap_or_else(|| "response failed".to_string());
        return Err(SearchError::Upstream {
            provider: "xai",
            status: 200,
            category: FailureCategory::Server,
            message,
        });
    }

    let mut text = String::new();
    // status id -> cited URL, in citation order.
    let mut cited: Vec<(String, String)> = Vec::new();
    for item in data
        .output
        .iter()
        .filter(|i| i.item_type.as_deref() == Some("message"))
    {
        for c in item
            .content
            .iter()
            .filter(|c| c.content_type.as_deref() == Some("output_text"))
        {
            text.push_str(c.text.as_deref().unwrap_or_default());
            for a in &c.annotations {
                if a.annotation_type.as_deref() != Some("url_citation") {
                    continue;
                }
                let Some(url) = a.url.as_deref() else {
                    continue;
                };
                if let Some(id) = status_id(url) {
                    if !cited.iter().any(|(i, _)| i == id) {
                        cited.push((id.to_string(), url.to_string()));
                    }
                }
            }
        }
    }

    let (posts, summary) = match serde_json::from_str::<Reply>(&text) {
        Ok(reply) => (reply.posts, reply.summary.trim().to_string()),
        // Not the JSON we asked for: the text is the answer, citations the results.
        Err(_) => (Vec::new(), text.trim().to_string()),
    };

    // The whole deny list is applied here too: upstream gets at most 20, and
    // none when the allow list took the request slot.
    let deny = all_handles(call, &call.opts.exclude_domains);
    let denied = |author: &str| deny.iter().any(|h| h.eq_ignore_ascii_case(author));

    let mut results = Vec::new();
    let mut used: HashSet<String> = HashSet::new();
    for p in posts {
        let Some(id) = status_id(p.url.trim()) else {
            continue;
        };
        if !cited.iter().any(|(i, _)| i == id) || used.contains(id) {
            continue;
        }
        used.insert(id.to_string());
        let author = p.author.trim().trim_start_matches('@');
        let url_user = p.url.trim().split('/').nth(3).unwrap_or_default();
        if denied(author) || denied(url_user) {
            continue;
        }
        let date = p.date.trim();
        results.push(SearchResult {
            title: if author.is_empty() {
                "Post on X".to_string()
            } else {
                format!("@{author}")
            },
            url: p.url.trim().to_string(),
            snippet: clip(&p.text),
            source: "xai".to_string(),
            published: (date.len() >= 10 && date.as_bytes()[4] == b'-').then(|| date.to_string()),
            image_url: None,
            extra: None,
        });
    }
    for (id, url) in &cited {
        if !used.contains(id) {
            results.push(SearchResult {
                title: "Post on X".to_string(),
                url: url.clone(),
                snippet: String::new(),
                source: "xai".to_string(),
                published: None,
                image_url: None,
                extra: None,
            });
        }
    }
    results.truncate(call.count as usize);

    let answers = if summary.is_empty() {
        Vec::new()
    } else {
        vec![Answer {
            provider: "xai".to_string(),
            text: format!("{summary}\n\nMore on X: {}", x_search_url(call.query)),
        }]
    };
    Ok(Output {
        results,
        answers,
        cost_micros: cost,
    })
}

#[async_trait]
impl Adapter for Xai {
    fn spec(&self) -> &'static ProviderSpec {
        &SPEC
    }

    fn keys(&self) -> Vec<String> {
        super::keys_for(&self.ctx.config, &SPEC)
    }

    fn is_configured(&self) -> bool {
        !self.keys().is_empty()
    }

    async fn call(&self, call: &Call<'_>) -> Result<Output, SearchError> {
        if !self.is_configured() {
            return Err(SearchError::AuthMissing { provider: "xai" });
        }
        if let Some(out) = deep_skip(call) {
            return Ok(out);
        }
        let body = body(call);
        let client = &self.ctx.client;
        let key = call.key;
        let resp = super::send(self, call.timeout, || {
            client.post(ENDPOINT).bearer_auth(key).json(&body)
        })
        .await?;
        let data: XaiResponse = resp.json().await?;
        parse(data, call)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::SearchOpts;

    fn call<'a>(mode: Mode, count: u16, opts: &'a SearchOpts) -> Call<'a> {
        Call {
            mode,
            query: "rust async runtime",
            count,
            opts,
            timeout: Duration::from_secs(5),
            key: "",
        }
    }

    fn fixture() -> XaiResponse {
        serde_json::from_str(include_str!(
            "../../tests/fixtures/xai/x_search_grok43.json"
        ))
        .unwrap()
    }

    #[test]
    fn request_pins_model_turns_and_no_to_date() {
        let opts = SearchOpts {
            freshness: Some("day".into()),
            ..SearchOpts::default()
        };
        let b = body(&call(Mode::Social, 10, &opts));
        assert_eq!(b["model"], "grok-4.3");
        assert_eq!(b["max_turns"], 2);
        assert_eq!(b["store"], false);
        assert_eq!(b["text"]["format"]["type"], "json_schema");
        let tool = &b["tools"][0];
        assert_eq!(tool["type"], "x_search");
        assert_eq!(tool["from_date"], json!(super::super::date_days_ago(1)));
        assert!(tool.get("to_date").is_none(), "to_date is exclusive");
    }

    #[test]
    fn request_sends_one_handle_list_and_deep_sends_none() {
        let opts = SearchOpts {
            include_domains: vec!["@rustlang".into(), "tokio_rs".into()],
            exclude_domains: vec!["spambot".into()],
            ..SearchOpts::default()
        };
        let tool = &body(&call(Mode::Social, 10, &opts))["tools"][0];
        assert_eq!(tool["allowed_x_handles"], json!(["rustlang", "tokio_rs"]));
        assert!(tool.get("excluded_x_handles").is_none());

        let deny_only = SearchOpts {
            exclude_domains: vec!["@spambot".into()],
            ..SearchOpts::default()
        };
        let tool = &body(&call(Mode::Social, 10, &deny_only))["tools"][0];
        assert_eq!(tool["excluded_x_handles"], json!(["spambot"]));
        assert!(tool.get("allowed_x_handles").is_none());

        // Deep: -d carries web domains, which are not handles.
        let tool = &body(&call(Mode::Deep, 10, &opts))["tools"][0];
        assert!(tool.get("allowed_x_handles").is_none());
        assert!(tool.get("excluded_x_handles").is_none());
    }

    #[test]
    fn parses_the_recorded_grok_4_3_reply() {
        let opts = SearchOpts::default();
        let out = parse(fixture(), &call(Mode::Social, 10, &opts)).unwrap();
        assert_eq!(out.results.len(), 5, "every cited post, once");
        let r = &out.results[0];
        assert_eq!(r.url, "https://x.com/mctursh/status/2114788678782365700");
        assert_eq!(r.title, "@mctursh");
        assert!(r.snippet.contains("async runtime"));
        assert_eq!(r.published.as_deref(), Some("2026-10-26T18:38:06Z"));
        assert!(out.results.iter().all(|r| r.source == "xai"));
        assert_eq!(
            out.answers.len(),
            1,
            "the summary is an answer, not a result"
        );
        assert!(out.answers[0]
            .text
            .contains("More on X: https://x.com/search?q="));
        // 352,600,000 ticks = $0.03526.
        assert_eq!(out.cost_micros, Some(35_260));
        // The count truncates.
        assert_eq!(
            parse(fixture(), &call(Mode::Social, 2, &opts))
                .unwrap()
                .results
                .len(),
            2
        );
    }

    #[test]
    fn uncited_posts_are_dropped_and_denied_handles_filtered() {
        let text = json!({
            "posts": [
                { "url": "https://x.com/a/status/1", "author": "a", "text": "cited", "date": "2026-11-01" },
                { "url": "https://x.com/b/status/2", "author": "b", "text": "invented", "date": "" },
                { "url": "https://x.com/spam/status/3", "author": "@spam", "text": "denied", "date": "" }
            ],
            "summary": "Two posts."
        })
        .to_string();
        let raw = json!({
            "status": "completed",
            "output": [{ "type": "message", "content": [{
                "type": "output_text",
                "text": text,
                "annotations": [
                    { "type": "url_citation", "url": "https://x.com/i/status/1" },
                    { "type": "url_citation", "url": "https://x.com/i/status/3" },
                    { "type": "url_citation", "url": "https://x.com/i/status/9" }
                ]
            }]}],
            "usage": { "cost_in_usd_ticks": 447_231_500u64 }
        });
        let data: XaiResponse = serde_json::from_value(raw.clone()).unwrap();
        // With no allow list, a deny entry past the 20 xAI accepts is still
        // filtered here.
        let mut deny: Vec<String> = (0..20).map(|i| format!("h{i}")).collect();
        deny.push("spam".into());
        let long = SearchOpts {
            exclude_domains: deny,
            ..SearchOpts::default()
        };
        let out = parse(data, &call(Mode::Social, 10, &long)).unwrap();
        assert!(out.results.iter().all(|r| !r.url.contains("/spam/")));
        let data: XaiResponse = serde_json::from_value(raw).unwrap();
        let opts = SearchOpts {
            include_domains: vec!["a".into(), "spam".into()],
            exclude_domains: vec!["spam".into()],
            ..SearchOpts::default()
        };
        let out = parse(data, &call(Mode::Social, 10, &opts)).unwrap();
        let urls: Vec<&str> = out.results.iter().map(|r| r.url.as_str()).collect();
        assert_eq!(
            urls,
            ["https://x.com/a/status/1", "https://x.com/i/status/9"],
            "post 2 was never fetched, post 3 is denied, 9 was cited without a row"
        );
        // 447,231,500 ticks -> 44,723.15 µ$, rounded up.
        assert_eq!(out.cost_micros, Some(44_724));
    }

    #[test]
    fn cost_falls_back_to_the_rate_card_without_ticks() {
        let usage: XaiUsage = serde_json::from_value(json!({
            "input_tokens": 7037,
            "input_tokens_details": { "cached_tokens": 3072 },
            "output_tokens": 1661,
            "server_side_tool_usage_details": { "x_posts_fetched": 7, "x_users_fetched": 0 }
        }))
        .unwrap();
        // 3965 × 1.25 + 3072 × 0.20 + 1661 × 2.50 = 9,723.15 → 9,724; + 7 posts × 5,000.
        // The measured call reported 44,723.15 µ$ for the same usage.
        assert_eq!(cost_micros(&usage), 44_724);
    }

    #[test]
    fn failed_status_is_an_error() {
        let data: XaiResponse = serde_json::from_value(json!({
            "status": "failed",
            "error": { "message": "tool error" },
            "output": []
        }))
        .unwrap();
        let err = parse(data, &call(Mode::Social, 10, &SearchOpts::default())).unwrap_err();
        assert_eq!(err.category(), FailureCategory::Server);
    }

    #[test]
    fn search_link_encodes_ampersand_hash_and_spaces() {
        let u = x_search_url("AT&T #tag");
        assert_eq!(u, "https://x.com/search?q=AT%26T+%23tag&f=live");
        let parsed = url::Url::parse(&u).unwrap();
        let q: Vec<(String, String)> = parsed.query_pairs().into_owned().collect();
        assert_eq!(q[0], ("q".to_string(), "AT&T #tag".to_string()));
    }

    /// Deep sends no domain filter (-d holds web domains, not handles), and
    /// every result is an x.com post. When the filters rule x.com out, the
    /// call must not be made (and billed) to return rows the user excluded.
    #[test]
    fn deep_skips_the_call_when_domain_filters_rule_out_x() {
        let skipped = |include: &[&str], exclude: &[&str]| {
            let opts = SearchOpts {
                include_domains: include.iter().map(|d| d.to_string()).collect(),
                exclude_domains: exclude.iter().map(|d| d.to_string()).collect(),
                ..SearchOpts::default()
            };
            deep_skip(&call(Mode::Deep, 10, &opts))
        };
        let out = skipped(&[], &["x.com"]).expect("excluding x.com skips the call");
        assert!(out.results.is_empty() && out.answers.is_empty());
        // Some(0), not None: None would bill the 45,000 µ$ estimate.
        assert_eq!(out.cost_micros, Some(0));
        assert!(skipped(&[], &["https://www.Twitter.com/"]).is_some());
        assert!(skipped(&["docs.rs"], &[]).is_some(), "x.com is outside -d");
        assert!(skipped(&["docs.rs", "x.com"], &[]).is_none());
        assert!(skipped(&["twitter.com"], &[]).is_none());
        assert!(skipped(&[], &["reddit.com"]).is_none());
        assert!(skipped(&[], &[]).is_none());
        // Social maps the same flags to handles: never skipped here.
        let opts = SearchOpts {
            exclude_domains: vec!["x.com".into()],
            ..SearchOpts::default()
        };
        assert!(deep_skip(&call(Mode::Social, 10, &opts)).is_none());
    }

    #[test]
    fn status_ids_come_from_x_and_twitter_post_urls_only() {
        assert_eq!(status_id("https://x.com/i/status/123"), Some("123"));
        assert_eq!(
            status_id("https://twitter.com/u/status/45?s=20"),
            Some("45")
        );
        assert_eq!(status_id("https://www.x.com/u/status/7#m"), Some("7"));
        assert_eq!(status_id("https://x.com/u"), None);
        assert_eq!(status_id("https://evil.com/u/status/1"), None);
        assert_eq!(status_id("https://x.com/u/status/abc"), None);
    }
}
