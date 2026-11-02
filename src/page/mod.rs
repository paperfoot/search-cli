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
pub use markdown::to_markdown;
pub use verdict::{verdict, Verdict};

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
