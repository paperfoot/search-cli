//! Page fetching, verdicts and HTML to markdown: the ground the extract
//! chain stands on.
//!
//! - [`fetch_direct`]: one honest GET that asks for markdown first, follows
//!   redirects by hand with the SSRF guard on every hop, caps the body at
//!   5 MB and decodes it with the page's own charset.
//! - [`verdict`]: is this content, or junk (binary, a challenge, a JS shell,
//!   too thin, a paywall)? Junk must never be returned or billed as success.
//! - [`to_markdown`]: article extraction to markdown, whole-page markdown when
//!   the page is not an article.
//! - [`extract`]: all three in order for one fetched page: the raw verdict,
//!   the conversion, then the verdict on the markdown (a page of navigation
//!   and footer passes the first and extracts to nothing).
//!
//! The verdict rules mirror the hosted service's `page.ts` (PLAN.md §3.8).

// Callers arrive with the extract chain (cli-extract); until then only the
// tests use this module.
#![allow(dead_code, unused_imports)]

mod fetch;
mod markdown;
mod verdict;

pub use fetch::{
    client, client_allow_private, fetch_direct, PageClient, MAX_BODY_BYTES, MAX_REDIRECTS,
};
pub use markdown::{to_markdown, MAX_DEPTH};
pub use verdict::{verdict, Verdict, MIN_TEXT_CHARS};

use reqwest::header::HeaderMap;
use std::time::Duration;

/// One fetched page, after redirects and charset decoding.
#[derive(Debug, Clone)]
pub struct Page {
    /// The URL of the last hop (after redirects).
    pub final_url: String,
    /// HTTP status of the last hop. Non-2xx pages are returned, not errors:
    /// a 402 or a 403 challenge body is evidence the caller needs.
    pub status: u16,
    /// The `Content-Type` header as sent, if any.
    pub content_type: Option<String>,
    /// The decoded body, at most [`MAX_BODY_BYTES`] of the raw response.
    pub body: String,
    /// Response headers of the last hop (`crawler-price`, `content-signal`).
    pub headers: HeaderMap,
}

impl Page {
    /// The verdict on this page's raw body.
    pub fn verdict(&self) -> Verdict {
        verdict(
            &self.body,
            self.content_type.as_deref(),
            self.status,
            Some(&self.headers),
        )
    }

    /// True when the server answered with markdown, so the body needs no
    /// conversion (Cloudflare "Markdown for Agents", Mintlify, Vercel, ...).
    pub fn is_markdown(&self) -> bool {
        matches!(
            self.content_type.as_deref().map(mime_essence).as_deref(),
            Some("text/markdown" | "text/x-markdown")
        )
    }
}

/// A page's content as markdown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Extracted {
    pub title: String,
    pub markdown: String,
}

/// The page as markdown, or the verdict that rejects it. Judges the raw
/// body first (status, binary, challenge, JS shell), converts it (markdown
/// passes through; HTML goes through [`to_markdown`]; other text is kept),
/// then judges the markdown as the server's `resultVerdict` does, so "thin"
/// means thin after extraction (PLAN.md §3.8). Synchronous CPU work: run it
/// like [`to_markdown`].
pub fn extract(page: &Page) -> Result<Extracted, Verdict> {
    match page.verdict() {
        Verdict::Ok => {}
        rejected => return Err(rejected),
    }
    let is_html = match page.content_type.as_deref().map(mime_essence) {
        Some(m) if !m.is_empty() => m.contains("html"),
        _ => verdict::sniff_html(&page.body),
    };
    let (title, markdown) = if page.is_markdown() || !is_html {
        let title = markdown_title(&page.body).unwrap_or_else(|| page.final_url.clone());
        (title, page.body.trim().to_string())
    } else {
        to_markdown(&page.body, &page.final_url)
    };
    match verdict(&markdown, Some("text/markdown"), 200, None) {
        Verdict::Ok => Ok(Extracted { title, markdown }),
        rejected => Err(rejected),
    }
}

/// The first `# Heading` in the opening lines of a markdown document.
fn markdown_title(markdown: &str) -> Option<String> {
    markdown
        .lines()
        .take(40)
        .find_map(|l| l.trim().strip_prefix("# "))
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
}

/// Why a fetch produced no page at all.
#[derive(Debug, thiserror::Error)]
pub enum PageError {
    /// The URL (or a redirect target) is private, loopback or link-local.
    #[error("{0}")]
    Blocked(String),
    #[error("not a fetchable URL: {0}")]
    InvalidUrl(String),
    #[error("more than {MAX_REDIRECTS} redirects")]
    TooManyRedirects,
    #[error("redirect (HTTP {0}) without a usable Location header")]
    BadRedirect(u16),
    #[error("timed out after {0:?}")]
    Timeout(Duration),
    #[error("network error: {0}")]
    Network(String),
}

/// `text/html; charset=utf-8` → `text/html`.
pub(crate) fn mime_essence(content_type: &str) -> String {
    content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(body: &str, content_type: &str) -> Page {
        Page {
            final_url: "https://example.com/page".into(),
            status: 200,
            content_type: Some(content_type.into()),
            body: body.into(),
            headers: HeaderMap::new(),
        }
    }

    /// Navigation and a footer pass the raw-HTML count and extract to
    /// nothing (review R9): thin is judged on the markdown.
    #[test]
    fn a_chrome_only_page_is_thin_after_extraction() {
        let nav: String = (0..20)
            .map(|i| format!("<li><a href=\"/section-{i}\">Section number {i}</a></li>"))
            .collect();
        let body = format!(
            "<html><head><title>Example</title></head><body><nav><ul>{nav}</ul></nav><main></main><footer><p>Copyright 2026 Example Corporation. All rights reserved. Privacy policy. Terms of service.</p></footer></body></html>"
        );
        let p = page(&body, "text/html");
        assert_eq!(p.verdict(), Verdict::Ok, "raw HTML has enough text");
        assert_eq!(extract(&p), Err(Verdict::Thin));
    }

    #[test]
    fn an_article_extracts_and_markdown_passes_through() {
        let article = page(include_str!("fixtures/article.html"), "text/html");
        let got = extract(&article).unwrap();
        assert_eq!(got.title, "Choosing an allocator for a Rust CLI");
        assert!(got.markdown.contains("## Why the default allocator"));

        let md = format!("# Docs\n\n{}", "A real paragraph of markdown. ".repeat(10));
        let got = extract(&page(&md, "text/markdown; charset=utf-8")).unwrap();
        assert_eq!(got.title, "Docs");
        assert_eq!(got.markdown, md.trim());
    }

    #[test]
    fn raw_rejections_come_first() {
        let shell = page(include_str!("fixtures/next-shell.html"), "text/html");
        assert_eq!(extract(&shell), Err(Verdict::JsShell));
        let mut gone = page(include_str!("fixtures/article.html"), "text/html");
        gone.status = 404;
        assert_eq!(extract(&gone), Err(Verdict::HttpError(404)));
    }
}
