//! Is a response content, or junk? A port of the hosted service's
//! `page.ts` (PLAN.md §3.8), rule for rule, so a page rejected by one is
//! rejected by both and junk is never billed as success. Both repos test
//! against the same vectors (`fixtures/verdict-vectors.json`).
//!
//! One addition the server does not have yet: a non-2xx status without a
//! challenge body is [`Verdict::HttpError`], never content.

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
    /// HTTP 402 with Cloudflare's `crawler-price` (always `Some`). Never
    /// escalate around a paywall.
    PaymentRequired { price: Option<String> },
    /// Any other status outside 2xx: an error page, not content.
    HttpError(u16),
}

impl Verdict {
    /// The rejection code shared with the server (`page.ts` `Rejection`),
    /// `ok` for content. `http_error` is the CLI's addition.
    pub fn code(&self) -> &'static str {
        match self {
            Verdict::Ok => "ok",
            Verdict::Binary => "binary",
            Verdict::Challenge => "challenge",
            Verdict::JsShell => "js_shell",
            Verdict::Thin => "thin",
            Verdict::PaymentRequired { .. } => "payment_required",
            Verdict::HttpError(_) => "http_error",
        }
    }
}

/// Under this many characters of text, a page has nothing worth returning.
pub const MIN_TEXT_CHARS: usize = 200;
/// Challenge markers only count on short pages: an article about Cloudflare
/// is not a challenge.
const CHALLENGE_MAX_TEXT: usize = 4000;
/// An empty mount node or a noscript plea only means "shell" when the page
/// has little else.
const SHELL_MAX_TEXT: usize = 1000;
/// A captcha widget only counts this close after a `<form`.
const CAPTCHA_FORM_SPAN: usize = 4000;
/// Marker checks only read the head of a page: interstitials are small,
/// attackers are not.
const INSPECT_BYTES: usize = 256 * 1024;
/// How much of the body the binary sniff reads.
const SNIFF_CHARS: usize = 2048;
/// Elements whose content is never visible text.
const RAW_TEXT: &[&str] = &["script", "style", "noscript", "template", "svg", "head"];

/// Judge `body` (raw HTML, markdown or text). `content_type` is the
/// response's media type; `headers` supply `crawler-price` and
/// `cf-mitigated`. Every scan here is linear in the body.
pub fn verdict(
    body: &str,
    content_type: Option<&str>,
    status: u16,
    headers: Option<&HeaderMap>,
) -> Verdict {
    let header = |name: &str| {
        headers
            .and_then(|h| h.get(name))
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|v| !v.is_empty())
    };

    if status == 402 {
        if let Some(price) = header("crawler-price") {
            return Verdict::PaymentRequired {
                price: Some(price.to_string()),
            };
        }
    }

    let media = content_type.map(mime_essence).filter(|e| !e.is_empty());
    if media.as_deref().is_some_and(|m| !is_textual(m)) || looks_binary(body) {
        return Verdict::Binary;
    }

    let html = match media.as_deref() {
        Some(m) => m.contains("html"),
        None => sniff_html(body),
    };
    let text = if html {
        visible_text(body)
    } else {
        collapse_whitespace(body)
    };
    let text_len = text.chars().count();
    let text_chars = text.chars().filter(|c| !c.is_whitespace()).count();
    let head = prefix(body, INSPECT_BYTES).to_ascii_lowercase();

    if header("cf-mitigated").is_some_and(|v| v.eq_ignore_ascii_case("challenge")) {
        return Verdict::Challenge;
    }
    if text_len < CHALLENGE_MAX_TEXT
        && (has_challenge_marker(&head)
            || (html && text_chars < SHELL_MAX_TEXT && has_captcha_form(&head)))
    {
        return Verdict::Challenge;
    }

    if !(200..300).contains(&status) {
        return Verdict::HttpError(status);
    }

    if html && text_chars < SHELL_MAX_TEXT {
        if text_chars < MIN_TEXT_CHARS && has_open_tag(&body.to_ascii_lowercase(), "script") {
            return Verdict::JsShell;
        }
        if has_empty_mount(&head) || noscript_wants_js(&head) {
            return Verdict::JsShell;
        }
    }

    if text_len < MIN_TEXT_CHARS {
        return Verdict::Thin;
    }
    Verdict::Ok
}

/// The server's `TEXTUAL` regex, `^(text/(markdown|x-markdown|html|plain|xml)
/// |application/(json|xml|xhtml+xml|[\w.+-]+\+(json|xml)))\b`: a prefix
/// followed by a word boundary, not a whole-string match.
fn is_textual(media: &str) -> bool {
    let bytes = media.as_bytes();
    let bounded = |end: usize| bytes.get(end).is_none_or(|b| !is_word_byte(*b));
    let fixed = [
        "text/markdown",
        "text/x-markdown",
        "text/html",
        "text/plain",
        "text/xml",
        "application/json",
        "application/xml",
        "application/xhtml+xml",
    ];
    if fixed
        .iter()
        .any(|p| media.starts_with(p) && bounded(p.len()))
    {
        return true;
    }
    let Some(sub) = media.strip_prefix("application/") else {
        return false;
    };
    let base = "application/".len();
    let class = |b: u8| is_word_byte(b) || matches!(b, b'.' | b'+' | b'-');
    for suffix in ["+json", "+xml"] {
        for (k, _) in sub.match_indices(suffix) {
            if k > 0
                && sub.as_bytes()[..k].iter().all(|b| class(*b))
                && bounded(base + k + suffix.len())
            {
                return true;
            }
        }
    }
    false
}

/// `%PDF-`, a NUL, or more than 10% control characters (C0 other than tab,
/// LF and CR; DEL; U+FFFD from bytes that failed to decode) in the first
/// 2048 characters.
fn looks_binary(body: &str) -> bool {
    if body.starts_with("%PDF-") {
        return true;
    }
    let mut total = 0usize;
    let mut control = 0usize;
    for c in body.chars().take(SNIFF_CHARS) {
        if c == '\0' {
            return true;
        }
        total += 1;
        let n = c as u32;
        if (n < 32 && !matches!(c, '\t' | '\n' | '\r')) || n == 127 || c == '\u{FFFD}' {
            control += 1;
        }
    }
    total > 0 && control * 10 > total
}

pub(crate) fn sniff_html(body: &str) -> bool {
    let start = prefix(body.trim_start(), 64).to_ascii_lowercase();
    ["<!doctype html", "<html", "<head", "<body"]
        .iter()
        .any(|p| start.starts_with(p))
}

/// The longest prefix of `s` within `max` bytes that ends on a char boundary.
fn prefix(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn collapse_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// `s[at..]` starts with `word` followed by a word boundary (regex `\b`).
fn word_at(s: &[u8], at: usize, word: &str) -> bool {
    s[at..].starts_with(word.as_bytes()) && s.get(at + word.len()).is_none_or(|b| !is_word_byte(*b))
}

/// `<name\b` anywhere in lowercased `s`.
fn has_open_tag(s: &str, name: &str) -> bool {
    let bytes = s.as_bytes();
    let open = format!("<{name}");
    let mut from = 0;
    while let Some(i) = s[from..].find(&open).map(|i| i + from) {
        if word_at(bytes, i + 1, name) {
            return true;
        }
        from = i + 1;
    }
    false
}

/// Visible text of an HTML document: comments and raw-text elements
/// removed (an unterminated one hides the rest, as in a browser), tags
/// replaced by spaces, entities counted as one character, whitespace
/// collapsed. Linear: forward searches only.
pub(crate) fn visible_text(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut stripped = String::with_capacity(html.len() / 2);
    let mut from = 0;
    let mut i = 0;
    'scan: while let Some(lt) = lower[i..].find('<').map(|n| n + i) {
        let end = if lower[lt..].starts_with("<!--") {
            lower[lt + 4..].find("-->").map(|n| lt + 4 + n + 3)
        } else if let Some(tag) = RAW_TEXT.iter().find(|t| word_at(bytes, lt + 1, t)) {
            find_closer(&lower, lt + 1 + tag.len(), tag)
        } else {
            i = lt + 1;
            continue 'scan;
        };
        stripped.push_str(&html[from..lt]);
        stripped.push(' ');
        match end {
            Some(e) => {
                from = e;
                i = e;
            }
            None => {
                from = html.len();
                break 'scan;
            }
        }
    }
    stripped.push_str(&html[from..]);

    // Tags (`<` then no `<` or `>` up to a `>`) become spaces.
    let mut text = String::with_capacity(stripped.len());
    let mut rest = stripped.as_str();
    while let Some(lt) = rest.find('<') {
        text.push_str(&rest[..lt]);
        let after = &rest[lt + 1..];
        match after.find(['<', '>']) {
            Some(n) if after.as_bytes()[n] == b'>' => {
                text.push(' ');
                rest = &after[n + 1..];
            }
            _ => {
                text.push('<');
                rest = after;
            }
        }
    }
    text.push_str(rest);
    collapse_whitespace(&replace_entities(&text))
}

/// The end of the first `</tag\s*>` at or after `from` in lowercased `s`.
fn find_closer(s: &str, from: usize, tag: &str) -> Option<usize> {
    let close = format!("</{tag}");
    let mut at = from;
    while let Some(i) = s[at..].find(&close).map(|i| i + at) {
        let after = &s[i + close.len()..];
        let trimmed = after.trim_start_matches(|c: char| c.is_ascii_whitespace());
        if trimmed.starts_with('>') {
            return Some(s.len() - trimmed.len() + 1);
        }
        at = i + 1;
    }
    None
}

/// `&nbsp;` and `&#160;` become spaces; any other `&name;` or `&#123;`
/// counts as one character.
fn replace_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp + 1..];
        let (numeric, body) = match after.strip_prefix('#') {
            Some(b) => (true, b),
            None => (false, after),
        };
        let len = body
            .bytes()
            .take_while(|b| {
                if numeric {
                    b.is_ascii_digit()
                } else {
                    b.is_ascii_alphabetic()
                }
            })
            .count();
        if len > 0 && body.as_bytes().get(len) == Some(&b';') {
            let name = &body[..len];
            let nbsp = if numeric {
                name == "160"
            } else {
                name.eq_ignore_ascii_case("nbsp")
            };
            out.push(if nbsp { ' ' } else { 'x' });
            rest = &body[len + 1..];
        } else {
            out.push('&');
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

/// Challenge markers, matched on the lowercased head of the body (HTML,
/// markdown or text alike).
fn has_challenge_marker(lower: &str) -> bool {
    const PLAIN: &[&str] = &[
        "just a moment...", // Cloudflare interstitial title
        "cf-chl",           // Cloudflare challenge form and options
        "cf_chl",
        "checking your browser before accessing",
        "enable javascript and cookies to continue",
        "captcha-delivery.com", // DataDome
        "px-captcha",           // PerimeterX / HUMAN
    ];
    if PLAIN.iter().any(|m| lower.contains(m)) {
        return true;
    }
    // verify(ing)? (that )?you are (a )?human
    for verb in ["verify ", "verifying "] {
        for that in ["", "that "] {
            for a in ["", "a "] {
                if lower.contains(&format!("{verb}{that}you are {a}human")) {
                    return true;
                }
            }
        }
    }
    // attention required!?\s*\|\s*cloudflare
    for (i, _) in lower.match_indices("attention required") {
        let rest = &lower[i + "attention required".len()..];
        let rest = rest.strip_prefix('!').unwrap_or(rest).trim_start();
        if let Some(rest) = rest.strip_prefix('|') {
            if rest.trim_start().starts_with("cloudflare") {
                return true;
            }
        }
    }
    // Akamai: <title>\s*access denied\s*</title>, or "access denied" with
    // "you don't have permission to access" or "reference #" within 400.
    for (i, _) in lower.match_indices("access denied") {
        let before = lower[..i].trim_end();
        let after = lower[i + "access denied".len()..].trim_start();
        if before.ends_with("<title>") && after.starts_with("</title>") {
            return true;
        }
        let window = prefix(&lower[i + "access denied".len()..], 400 + 40);
        let near = |needle: &str| window.find(needle).is_some_and(|n| n <= 400);
        if near("you don't have permission to access") || near("you dont have permission to access")
        {
            return true;
        }
        for (r, _) in window.match_indices("reference") {
            if r <= 400
                && window[r + "reference".len()..]
                    .trim_start()
                    .starts_with('#')
            {
                return true;
            }
        }
    }
    false
}

/// A `captcha` or `cf-turnstile` within 4000 bytes after a `<form`.
fn has_captcha_form(lower: &str) -> bool {
    let bytes = lower.as_bytes();
    let mut form: Option<usize> = None;
    let mut i = 0;
    while i < bytes.len() {
        if word_at(bytes, i, "<form") {
            form = Some(i);
            i += 5;
        } else if bytes[i..].starts_with(b"captcha") || bytes[i..].starts_with(b"cf-turnstile") {
            if form.is_some_and(|f| i - f <= CAPTCHA_FORM_SPAN) {
                return true;
            }
            i += 7;
        } else {
            i += 1;
        }
    }
    false
}

/// `<div …id=root|app|__next|__nuxt…>` followed by nothing but whitespace
/// and `</div>`.
fn has_empty_mount(lower: &str) -> bool {
    let bytes = lower.as_bytes();
    let mut from = 0;
    while let Some(i) = lower[from..].find("<div").map(|n| n + from) {
        from = i + 1;
        if !word_at(bytes, i + 1, "div") {
            continue;
        }
        let attrs_start = i + 4;
        let Some(len) = lower[attrs_start..].find(['<', '>']) else {
            return false;
        };
        if len > 400 || bytes[attrs_start + len] != b'>' {
            continue;
        }
        let after = lower[attrs_start + len + 1..].trim_start();
        if after.starts_with("</div>") && mount_id(&lower[attrs_start..attrs_start + len]) {
            return true;
        }
    }
    false
}

/// `\bid=["']?(root|app|__next|__nuxt)(["'\s/]|$)` in an attribute run.
fn mount_id(attrs: &str) -> bool {
    let bytes = attrs.as_bytes();
    for (i, _) in attrs.match_indices("id=") {
        if i > 0 && is_word_byte(bytes[i - 1]) {
            continue;
        }
        let value = &attrs[i + 3..];
        let value = value.strip_prefix(['"', '\'']).unwrap_or(value);
        for id in ["root", "app", "__next", "__nuxt"] {
            if let Some(rest) = value.strip_prefix(id) {
                if rest
                    .chars()
                    .next()
                    .is_none_or(|c| matches!(c, '"' | '\'' | '/') || c.is_whitespace())
                {
                    return true;
                }
            }
        }
    }
    false
}

/// `<noscript …>` with "enable javascript" within 500 bytes.
fn noscript_wants_js(lower: &str) -> bool {
    let bytes = lower.as_bytes();
    let mut from = 0;
    while let Some(i) = lower[from..].find("<noscript").map(|n| n + from) {
        from = i + 1;
        if !word_at(bytes, i + 1, "noscript") {
            continue;
        }
        let attrs_start = i + 9;
        let Some(len) = lower[attrs_start..].find(['<', '>']) else {
            return false;
        };
        if len > 400 || bytes[attrs_start + len] != b'>' {
            continue;
        }
        let inner = prefix(
            &lower[attrs_start + len + 1..],
            500 + "enable javascript".len(),
        );
        if inner.contains("enable javascript") {
            return true;
        }
    }
    false
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
        // Without a price a 402 is an error page like any other (as on the
        // server, which does not treat it as a paywall either).
        assert_eq!(verdict("", None, 402, None), Verdict::HttpError(402));
    }

    /// Error pages are not content (review R2): a 503 page with ordinary
    /// text used to come back Ok, and would have been extracted and billed.
    #[test]
    fn http_error_pages_are_not_ok() {
        let body = format!(
            "<html><head><title>503 Service Unavailable</title></head><body><h1>Service Unavailable</h1><p>{}</p></body></html>",
            "The server is temporarily unable to service your request. ".repeat(6)
        );
        for status in [400u16, 403, 404, 410, 429, 500, 502, 503, 304] {
            assert_eq!(
                verdict(&body, Some("text/html"), status, None),
                Verdict::HttpError(status)
            );
        }
        assert_eq!(verdict(&body, Some("text/html"), 203, None), Verdict::Ok);
    }

    /// Cloudflare injects `/cdn-cgi/challenge-platform/scripts/jsd/main.js`
    /// into ordinary pages; that path used to mark them Challenge, so a free
    /// page escalated to paid renderers (live on discord.com, 2 Nov 2026).
    #[test]
    fn cloudflare_javascript_detections_is_not_a_challenge() {
        let page = include_str!("fixtures/cloudflare-jsd-page.html");
        assert!(page.contains("/cdn-cgi/challenge-platform/scripts/jsd/main.js"));
        assert_eq!(html(page), Verdict::Ok);
    }

    /// A DataDome tag and an ordinary form used to read as a captcha form.
    #[test]
    fn captcha_forms_follow_the_server_rule() {
        assert_eq!(
            html(include_str!("fixtures/datadome-tag-page.html")),
            Verdict::Ok
        );
        let short = "<html><body><h1>One more step</h1><form action=\"/verify\"><div class=\"h-captcha\"></div></form></body></html>";
        assert_eq!(html(short), Verdict::Challenge);
        // The widget must sit within 4000 characters after the form.
        let far = format!(
            "<html><body><form action=\"/q\"></form><!--{}--><div class=\"g-recaptcha\"></div><p>{}</p></body></html>",
            " ".repeat(4100),
            "word ".repeat(60)
        );
        assert_eq!(html(&far), Verdict::Ok);
    }

    #[test]
    fn cf_mitigated_header_is_a_challenge() {
        let mut headers = HeaderMap::new();
        headers.insert("cf-mitigated", HeaderValue::from_static("challenge"));
        assert_eq!(
            verdict(ARTICLE, Some("text/html"), 403, Some(&headers)),
            Verdict::Challenge
        );
    }

    #[test]
    fn head_and_svg_text_are_not_visible() {
        assert_eq!(
            visible_text("<html><head><title>T</title></head><body><svg><text>x</text></svg><p>Hi &amp; bye&nbsp;now</p><!-- c --></body></html>"),
            "Hi x bye now"
        );
        // An unterminated raw-text element hides the rest.
        assert_eq!(visible_text("<p>a</p><script>var x = 1;"), "a");
        assert_eq!(visible_text("1 < 2 and <b>bold</b>"), "1 < 2 and bold");
        // `<header>` is not `<head>`.
        assert_eq!(visible_text("<header>Top</header>"), "Top");
    }

    #[test]
    fn media_types_follow_the_server_regex() {
        for t in [
            "text/html",
            "text/markdown",
            "application/ld+json",
            "application/vnd.api+json",
            "application/xhtml+xml",
            "text/plain-x",
        ] {
            assert!(is_textual(t), "{t}");
        }
        for t in [
            "application/pdf",
            "image/png",
            "application/jsonl",
            "text/csv",
            "application/+json",
            "text/htmlx",
        ] {
            assert!(!is_textual(t), "{t}");
        }
    }

    /// The vectors shared with the server's `page.ts`: same input, same code.
    #[test]
    fn shared_vectors_match_the_server() {
        let raw = include_str!("fixtures/verdict-vectors.json");
        let doc: serde_json::Value = serde_json::from_str(raw).unwrap();
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/page/fixtures");
        let mut failures = vec![];
        let vectors = doc["vectors"].as_array().unwrap();
        for v in vectors {
            let body = match v["body_file"].as_str() {
                Some(f) => std::fs::read_to_string(dir.join(f)).unwrap(),
                None => v["body"].as_str().unwrap().to_string(),
            };
            let mut headers = HeaderMap::new();
            for (k, val) in v["headers"].as_object().unwrap() {
                headers.insert(
                    reqwest::header::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                    HeaderValue::from_str(val.as_str().unwrap()).unwrap(),
                );
            }
            let got = verdict(
                &body,
                v["content_type"].as_str(),
                v["status"].as_u64().unwrap() as u16,
                Some(&headers),
            );
            if got.code() != v["expect"].as_str().unwrap() {
                failures.push(format!(
                    "{}: expected {}, got {got:?}",
                    v["name"], v["expect"]
                ));
            }
        }
        assert!(vectors.len() >= 20);
        assert!(failures.is_empty(), "{failures:#?}");
    }
}
