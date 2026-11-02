//! Is a response content, or junk? The same rules as the hosted service's
//! `page.ts` (PLAN.md §3.8), so a page rejected by one is rejected by both
//! and junk is never billed as success.

use super::mime_essence;
use reqwest::header::HeaderMap;

/// The judgement on one response body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Real content.
    Ok,
    /// Not text: a PDF, an image, or bytes that do not decode.
    Binary,
    /// An anti-bot interstitial (Cloudflare, DataDome, PerimeterX, Akamai).
    Challenge,
    /// A page that renders only in a browser: escalate to a rendering rung.
    JsShell,
    /// Under 200 characters of text.
    Thin,
    /// HTTP 402, with Cloudflare's `crawler-price` when sent. Never escalate
    /// around a paywall.
    PaymentRequired { price: Option<String> },
}

/// Under this many visible non-space characters a page is thin.
const THIN_CHARS: usize = 200;
/// An empty mount node or a noscript plea only means "JS shell" when the
/// page around it is short too: server-rendered sites keep both.
const SHELL_CONTEXT_CHARS: usize = 1000;
/// Challenge pages are short; an article about Cloudflare is not.
const CHALLENGE_MAX_CHARS: usize = 4000;
/// How much of the body the binary sniff reads.
const SNIFF_BYTES: usize = 2048;

/// Judge `body` (raw HTML, markdown or text). `headers` matter only for a
/// 402's `crawler-price`. Other non-2xx statuses are judged on their body;
/// treating them as failures is the caller's call.
pub fn verdict(
    body: &str,
    content_type: Option<&str>,
    status: u16,
    headers: Option<&HeaderMap>,
) -> Verdict {
    if status == 402 {
        let price = headers
            .and_then(|h| h.get("crawler-price"))
            .and_then(|v| v.to_str().ok())
            .map(|v| v.trim().to_string());
        return Verdict::PaymentRequired { price };
    }

    let essence = content_type.map(mime_essence).filter(|e| !e.is_empty());
    if essence.as_deref().is_some_and(|e| !is_textual(e)) || looks_binary(body) {
        return Verdict::Binary;
    }

    let is_html = match essence.as_deref() {
        Some(e) => e == "text/html" || e == "application/xhtml+xml",
        None => sniff_html(body),
    };

    if !is_html {
        let chars = non_space_chars(body);
        if chars < CHALLENGE_MAX_CHARS && is_challenge(&body.to_ascii_lowercase(), false) {
            return Verdict::Challenge;
        }
        return if chars < THIN_CHARS {
            Verdict::Thin
        } else {
            Verdict::Ok
        };
    }

    let scan = scan_html(body);
    if scan.visible_chars < CHALLENGE_MAX_CHARS && is_challenge(&body.to_ascii_lowercase(), true) {
        return Verdict::Challenge;
    }
    let shell_signal = scan.empty_mount || scan.noscript_wants_js;
    if (scan.has_script && scan.visible_chars < THIN_CHARS)
        || (shell_signal && scan.visible_chars < SHELL_CONTEXT_CHARS)
    {
        return Verdict::JsShell;
    }
    if scan.visible_chars < THIN_CHARS {
        return Verdict::Thin;
    }
    Verdict::Ok
}

fn is_textual(essence: &str) -> bool {
    matches!(
        essence,
        "text/markdown"
            | "text/x-markdown"
            | "text/html"
            | "text/plain"
            | "text/xml"
            | "application/xhtml+xml"
            | "application/json"
            | "application/xml"
    ) || essence.ends_with("+json")
        || essence.ends_with("+xml")
}

/// NUL bytes, or more than 10% control or replacement characters in the
/// first 2 KB (a binary decoded as text turns into U+FFFD).
fn looks_binary(body: &str) -> bool {
    let mut total = 0usize;
    let mut bad = 0usize;
    for (i, c) in body.char_indices() {
        if i >= SNIFF_BYTES {
            break;
        }
        if c == '\0' {
            return true;
        }
        total += 1;
        if c == '\u{FFFD}' || (c.is_control() && !matches!(c, '\t' | '\n' | '\r' | '\x0c')) {
            bad += 1;
        }
    }
    bad * 10 > total
}

fn sniff_html(body: &str) -> bool {
    let head: String = body.trim_start().chars().take(512).collect();
    let lower = head.to_ascii_lowercase();
    lower.starts_with("<!doctype html")
        || lower.starts_with("<html")
        || lower.contains("<head")
        || lower.contains("<body")
}

fn non_space_chars(text: &str) -> usize {
    text.chars().filter(|c| !c.is_whitespace()).count()
}

/// Markers of anti-bot interstitials, matched on the lowercased body. Raw
/// HTML carries structural markers (script URLs, class names) that a
/// markdown rendering of the same page does not.
fn is_challenge(lower: &str, html: bool) -> bool {
    const TEXT: &[&str] = &[
        "just a moment...",                       // Cloudflare
        "checking your browser before accessing", // Cloudflare (old)
        "attention required! | cloudflare",
        "enable javascript and cookies to continue",
        "verify you are human",
        "verifying you are human",
        "please verify you are a human",       // PerimeterX
        "access to this page has been denied", // PerimeterX
    ];
    const HTML: &[&str] = &[
        "cf-chl", // Cloudflare challenge scripts and forms
        "cf_chl_opt",
        "/cdn-cgi/challenge-platform/",
        "captcha-delivery.com", // DataDome
        "px-captcha",           // PerimeterX
        "<title>attention required!",
    ];
    if TEXT.iter().any(|m| lower.contains(m)) {
        return true;
    }
    // Akamai: "Access Denied ... Reference #18.xxxx".
    if lower.contains("access denied")
        && (lower.contains("reference #") || lower.contains("you don't have permission to access"))
    {
        return true;
    }
    if !html {
        return false;
    }
    if HTML.iter().any(|m| lower.contains(m)) {
        return true;
    }
    // A short page whose form is a captcha widget.
    lower.contains("<form")
        && ["g-recaptcha", "h-captcha", "cf-turnstile", "datadome"]
            .iter()
            .any(|m| lower.contains(m))
}

#[derive(Debug, Default)]
struct HtmlScan {
    /// Non-space characters outside tags, scripts, styles, templates and
    /// noscript.
    visible_chars: usize,
    has_script: bool,
    /// `<div id="root|app|__next|__nuxt">` with nothing inside.
    empty_mount: bool,
    /// `<noscript>` asking the reader to enable JavaScript.
    noscript_wants_js: bool,
}

/// One pass over raw HTML, cheap enough to run on every page (no DOM).
fn scan_html(html: &str) -> HtmlScan {
    const MOUNT_IDS: &[&str] = &["root", "app", "__next", "__nuxt"];
    let lower = html.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut scan = HtmlScan::default();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'<' {
            let next = lower[i..].find('<').map_or(lower.len(), |n| i + n);
            scan.visible_chars += non_space_chars(&html[i..next]);
            i = next;
            continue;
        }
        let rest = &lower[i..];
        if rest.starts_with("<!--") {
            i = rest.find("-->").map_or(lower.len(), |n| i + n + 3);
            continue;
        }
        let tag_end = tag_end(rest).map_or(lower.len(), |n| i + n + 1);
        let tag = &lower[i..tag_end];
        let name: String = tag[1..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
            .collect();
        match name.as_str() {
            "script" | "style" | "template" | "noscript" => {
                let close = format!("</{name}");
                let body_end = lower[tag_end..]
                    .find(&close)
                    .map_or(lower.len(), |n| tag_end + n);
                match name.as_str() {
                    "script" => scan.has_script = true,
                    "noscript" if lower[tag_end..body_end].contains("enable javascript") => {
                        scan.noscript_wants_js = true;
                    }
                    _ => {}
                }
                i = lower[body_end..]
                    .find('>')
                    .map_or(lower.len(), |n| body_end + n + 1);
                continue;
            }
            _ => {}
        }
        if !scan.empty_mount
            && attr(tag, "id").is_some_and(|id| MOUNT_IDS.contains(&id))
            && lower[tag_end..].trim_start().starts_with("</")
        {
            scan.empty_mount = true;
        }
        i = tag_end;
    }
    scan
}

/// Index of the `>` closing the tag at the start of `tag`, skipping quoted
/// attribute values.
fn tag_end(tag: &str) -> Option<usize> {
    let mut quote: Option<u8> = None;
    for (i, b) in tag.bytes().enumerate() {
        match (quote, b) {
            (Some(q), _) if b == q => quote = None,
            (Some(_), _) => {}
            (None, b'"' | b'\'') => quote = Some(b),
            (None, b'>') => return Some(i),
            _ => {}
        }
    }
    None
}

/// The value of attribute `name` in a lowercased start tag.
fn attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let mut from = 0;
    while let Some(pos) = tag[from..].find(name).map(|p| p + from) {
        let before = tag[..pos].chars().next_back();
        let after = tag[pos + name.len()..].trim_start();
        if before.is_some_and(char::is_whitespace) && after.starts_with('=') {
            let value = after[1..].trim_start();
            return Some(match value.chars().next() {
                Some(q @ ('"' | '\'')) => value[1..].split(q).next().unwrap_or(""),
                _ => value
                    .split(|c: char| c.is_whitespace() || c == '>' || c == '/')
                    .next()
                    .unwrap_or(""),
            });
        }
        from = pos + name.len();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::HeaderValue;

    const ARTICLE: &str = include_str!("fixtures/article.html");
    const CLOUDFLARE: &str = include_str!("fixtures/cloudflare-challenge.html");
    const NEXT_SHELL: &str = include_str!("fixtures/next-shell.html");
    const NOSCRIPT_SHELL: &str = include_str!("fixtures/noscript-shell.html");

    fn html(body: &str) -> Verdict {
        verdict(body, Some("text/html; charset=utf-8"), 200, None)
    }

    #[test]
    fn an_article_is_ok() {
        assert_eq!(html(ARTICLE), Verdict::Ok);
    }

    #[test]
    fn cloudflare_challenge_is_rejected() {
        assert_eq!(html(CLOUDFLARE), Verdict::Challenge);
        // Also when a 403 carries it, and as a renderer's markdown.
        assert_eq!(
            verdict(CLOUDFLARE, Some("text/html"), 403, None),
            Verdict::Challenge
        );
        assert_eq!(
            verdict(
                "# Just a moment...\n\nexample.com",
                Some("text/markdown"),
                200,
                None
            ),
            Verdict::Challenge
        );
    }

    #[test]
    fn other_vendors_challenges_are_rejected() {
        let datadome = "<html><head><title>example.com</title></head><body><script src=\"https://ct.captcha-delivery.com/c.js\"></script></body></html>";
        assert_eq!(html(datadome), Verdict::Challenge);
        let px = "<html><body><div id=\"px-captcha\"></div><p>Press and hold</p></body></html>";
        assert_eq!(html(px), Verdict::Challenge);
        let akamai = "<HTML><HEAD><TITLE>Access Denied</TITLE></HEAD><BODY><H1>Access Denied</H1>You don't have permission to access \"http://www.example.com/\" on this server.<P>Reference #18.6f2d3e17.1730550000.1a2b3c4d</BODY></HTML>";
        assert_eq!(html(akamai), Verdict::Challenge);
    }

    #[test]
    fn an_article_mentioning_cloudflare_is_not_a_challenge() {
        let long = format!(
            "<html><body><h1>How the cf-chl challenge works</h1><p>Just a moment... is the title Cloudflare shows. {}</p></body></html>",
            "Real sentences with ordinary words about networks. ".repeat(120)
        );
        assert_eq!(html(&long), Verdict::Ok);
    }

    #[test]
    fn js_shells_are_rejected() {
        assert_eq!(html(NEXT_SHELL), Verdict::JsShell);
        assert_eq!(html(NOSCRIPT_SHELL), Verdict::JsShell);
        // A server-rendered page keeps its mount node full: fine.
        let ssr = format!(
            "<html><body><div id=\"__next\"><main><p>{}</p></main></div><script src=\"/app.js\"></script></body></html>",
            "Rendered on the server, readable without JavaScript. ".repeat(20)
        );
        assert_eq!(html(&ssr), Verdict::Ok);
    }

    #[test]
    fn pdf_and_binary_bytes_are_binary() {
        assert_eq!(
            verdict("%PDF-1.7 ...", Some("application/pdf"), 200, None),
            Verdict::Binary
        );
        assert_eq!(verdict("", Some("image/png"), 200, None), Verdict::Binary);
        // A PDF served without a content type, decoded lossily.
        let bytes = format!("%PDF-1.4\n{}", "\u{FFFD}\0\u{FFFD}\u{1}".repeat(200));
        assert_eq!(verdict(&bytes, None, 200, None), Verdict::Binary);
        // JSON and feeds are text.
        let feed = format!("<rss>{}</rss>", "<item>news</item>".repeat(40));
        assert_eq!(
            verdict(&feed, Some("application/rss+xml"), 200, None),
            Verdict::Ok
        );
    }

    #[test]
    fn thin_pages_are_thin() {
        assert_eq!(
            html("<html><body><p>Coming soon.</p></body></html>"),
            Verdict::Thin
        );
        assert_eq!(
            verdict("Short but real.", Some("text/plain"), 200, None),
            Verdict::Thin
        );
        let md = "# Title\n\n".to_string() + &"A real paragraph of markdown. ".repeat(10);
        assert_eq!(verdict(&md, Some("text/markdown"), 200, None), Verdict::Ok);
    }

    #[test]
    fn payment_required_carries_the_crawler_price() {
        let mut headers = HeaderMap::new();
        headers.insert("crawler-price", HeaderValue::from_static("USD 0.01"));
        assert_eq!(
            verdict("<p>pay</p>", Some("text/html"), 402, Some(&headers)),
            Verdict::PaymentRequired {
                price: Some("USD 0.01".into())
            }
        );
        assert_eq!(
            verdict("", None, 402, None),
            Verdict::PaymentRequired { price: None }
        );
    }

    #[test]
    fn attributes_are_read_from_lowercased_tags() {
        assert_eq!(attr("<div id=\"root\" class=\"x\">", "id"), Some("root"));
        assert_eq!(attr("<div data-id='x' id=app>", "id"), Some("app"));
        assert_eq!(attr("<div grid=\"1\">", "id"), None);
    }
}
