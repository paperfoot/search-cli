//! HTML to markdown: Readability-style article extraction when the page
//! reads as an article, the whole page minus its chrome otherwise (docs,
//! listings, tables, where article extraction throws content away).
//!
//! Both paths share one HTML parser (dom_query, which dom_smoothie is built
//! on), so a page is parsed once and the binary carries one HTML5 stack.

use dom_query::Document;
use dom_smoothie::{Config, Readability, TextMode};
use std::borrow::Cow;
use url::Url;

/// Element nesting deeper than this is flattened before parsing. The HTML
/// tree builder scans its stack of open elements on every start tag, so
/// parse time grows with depth squared: `<div>` repeated 32k times took 6 s,
/// and the 5 MB body cap allows a million levels. Browsers stop nesting
/// around 512 too.
pub const MAX_DEPTH: usize = 512;
/// The parser's stack work is the sum, over start tags, of the depth each
/// opens at. Honest pages stay far below this (a 2.2 MB docs page nesting
/// 46 deep: 90k); 5 MB of 511-deep blocks is about 120M. Past it, the
/// depth limit drops until the work fits.
const WORK_BUDGET: usize = 5_000_000;
/// Start tags handed to the parser; the rest of the page is cut. Parsing,
/// article scoring and markdown all scale with the element count, and 5 MB
/// of `<p>x</p>` is 650k elements (18 s in a debug build). The same 2.2 MB
/// docs page has 10k nesting tags.
const MAX_START_TAGS: usize = 100_000;
/// The depth limit never drops below this.
const MIN_DEPTH: usize = 32;

/// Elements dropped from a whole-page conversion: not content.
const CHROME: &[&str] = &[
    "head", "script", "style", "noscript", "template", "svg", "nav", "footer", "iframe",
];

/// `(title, markdown)` for `html` fetched from `url`. The title comes from
/// the article, else `<title>`, else the first `<h1>`, else the URL.
///
/// This is synchronous CPU work: 5-20 ms on a typical page, bounded on a
/// hostile one by flattening nesting past [`MAX_DEPTH`]. A blocking call
/// cannot be cancelled, so callers run it in `spawn_blocking` and still
/// hold the mode's deadline over the await (abandoning the result when it
/// fires).
pub fn to_markdown(html: &str, url: &str) -> (String, String) {
    let html = flatten_deep(html, MAX_DEPTH);
    let html = html.as_ref();
    let doc = Document::from(html);
    let page_title = first_text(&doc, "title").or_else(|| first_text(&doc, "h1"));

    if dom_smoothie::is_probably_readable(&doc, None, None) {
        let cfg = Config {
            text_mode: TextMode::Markdown,
            ..Config::default()
        };
        let article = Readability::with_document(doc, Some(url), Some(cfg))
            .and_then(|mut r| r.parse())
            .ok()
            .filter(|a| !a.text_content.trim().is_empty());
        if let Some(article) = article {
            let title = Some(article.title.trim().to_string())
                .filter(|t| !t.is_empty())
                .or(page_title)
                .unwrap_or_else(|| url.to_string());
            return (title, tidy(&article.text_content));
        }
        // Readability gave up: convert the whole page instead.
        return whole_page(&Document::from(html), url, page_title);
    }
    whole_page(&doc, url, page_title)
}

fn whole_page(doc: &Document, url: &str, page_title: Option<String>) -> (String, String) {
    absolutize(doc, url);
    let markdown = match doc.select("body").nodes().first() {
        Some(body) => body.md(Some(CHROME)),
        None => doc.md(Some(CHROME)),
    };
    (
        page_title.unwrap_or_else(|| url.to_string()),
        tidy(&markdown),
    )
}

/// Make relative links and images absolute, so the markdown still works
/// once it leaves the page.
fn absolutize(doc: &Document, url: &str) {
    let Ok(base) = Url::parse(url) else {
        return;
    };
    for (selector, name) in [("a[href]", "href"), ("img[src]", "src")] {
        for node in doc.select(selector).nodes() {
            let Some(value) = node.attr(name) else {
                continue;
            };
            if let Ok(abs) = base.join(value.trim()) {
                if matches!(abs.scheme(), "http" | "https") {
                    node.set_attr(name, abs.as_str());
                }
            }
        }
    }
}

fn first_text(doc: &Document, selector: &str) -> Option<String> {
    let node = *doc.select(selector).nodes().first()?;
    let text = node.text().split_whitespace().collect::<Vec<_>>().join(" ");
    (!text.is_empty()).then_some(text)
}

/// Elements that never hold children.
const VOID: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "keygen", "link", "meta", "param",
    "source", "track", "wbr",
];
/// Elements a parser closes on its own when a sibling starts (`<p><p>`,
/// `<li><li>`), or that exist once: they never nest by repetition, so
/// counting them would only overcount sloppy but honest pages.
const SELF_CLOSING_SIBLINGS: &[&str] = &[
    "p", "li", "dt", "dd", "option", "optgroup", "tr", "td", "th", "thead", "tbody", "tfoot",
    "colgroup", "caption", "rb", "rt", "rp", "rtc", "html", "head", "body",
];
/// Elements whose content is text, not tags.
const RAW: &[&str] = &[
    "script", "style", "textarea", "title", "xmp", "iframe", "noembed", "noframes",
];

/// `html` with every start and end tag past `max` levels of nesting
/// removed (their text stays), `max` lowered further when the page would
/// still cost more than [`WORK_BUDGET`]. Borrowed when the page is within
/// both, which is every honest page. One linear pass to measure, one to
/// rewrite.
fn flatten_deep(html: &str, max: usize) -> Cow<'_, str> {
    let n = nesting(html, max, None);
    let html = n.cut.map_or(html, |at| &html[..at]);
    if n.deepest <= max && n.work <= WORK_BUDGET {
        return Cow::Borrowed(html);
    }
    let limit = max.min((WORK_BUDGET / n.start_tags.max(1)).max(MIN_DEPTH));
    let mut out = String::with_capacity(html.len());
    nesting(html, limit, Some(&mut out));
    Cow::Owned(out)
}

/// What one walk over the tags found.
#[derive(Debug, Default)]
struct Nesting {
    /// The deepest level any start tag would open at.
    deepest: usize,
    /// Start tags that nest (not void, not self-closing siblings).
    start_tags: usize,
    /// Sum of the depth each kept start tag opens at.
    work: usize,
    /// Where the start tag past [`MAX_START_TAGS`] begins: the walk stops
    /// there, and the page is cut there.
    cut: Option<usize>,
}

/// Walk the tags of `html`, tracking nesting depth. With `out`, copies the
/// document there, dropping the tags that would nest past `max` and the
/// end tags that match them.
fn nesting(html: &str, max: usize, mut out: Option<&mut String>) -> Nesting {
    let lower = html.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut n = Nesting::default();
    let (mut depth, mut dropped, mut all_starts) = (0usize, 0usize, 0usize);
    // Inside <svg> or <math>, `/>` closes an element, as in XML.
    let mut foreign = 0usize;
    let mut copied = 0;
    let mut i = 0;
    while let Some(lt) = lower[i..].find('<').map(|n| n + i) {
        let rest = &lower[lt..];
        let end = if rest.starts_with("<!--") {
            rest.find("-->").map_or(html.len(), |n| lt + n + 3)
        } else {
            rest.find('>').map_or(html.len(), |n| lt + n + 1)
        };
        let closing = rest.starts_with("</");
        let name_start = lt + 1 + usize::from(closing);
        let name_len = bytes[name_start.min(bytes.len())..]
            .iter()
            .take_while(|b| b.is_ascii_alphanumeric() || **b == b'-' || **b == b':')
            .count();
        let name = &lower[name_start.min(lower.len())..name_start.min(lower.len()) + name_len];
        i = end.max(lt + 1);
        if name.is_empty() || rest.starts_with("<!") {
            continue;
        }
        if !closing {
            all_starts += 1;
            if all_starts > MAX_START_TAGS {
                n.cut = Some(lt);
                break;
            }
        }
        if !closing && RAW.contains(&name) {
            // Skip the raw text to its end tag: it holds no tags.
            i = lower[i..]
                .find(&format!("</{name}"))
                .map_or(html.len(), |n| i + n);
            continue;
        }
        if VOID.contains(&name) || SELF_CLOSING_SIBLINGS.contains(&name) {
            continue;
        }
        let self_closed = foreign > 0 && lower[lt..end].trim_end_matches('>').ends_with('/');
        let mut drop = false;
        if closing {
            if dropped > 0 {
                dropped -= 1;
                drop = true;
            } else {
                depth = depth.saturating_sub(1);
                if matches!(name, "svg" | "math") {
                    foreign = foreign.saturating_sub(1);
                }
            }
        } else if !self_closed {
            n.start_tags += 1;
            if depth >= max {
                dropped += 1;
                drop = true;
            } else {
                depth += 1;
                n.work += depth;
                if matches!(name, "svg" | "math") {
                    foreign += 1;
                }
            }
            n.deepest = n.deepest.max(depth + dropped);
        }
        if let Some(out) = out.as_deref_mut() {
            if drop {
                out.push_str(&html[copied..lt]);
                copied = end;
            }
        }
    }
    if let Some(out) = out {
        out.push_str(&html[copied..]);
    }
    n
}

/// Trim trailing spaces and collapse runs of blank lines.
fn tidy(markdown: &str) -> String {
    let mut out = String::with_capacity(markdown.len());
    let mut blank = 0;
    // Inside a fenced code block every line is kept as is (review P10).
    let mut fence: Option<&str> = None;
    for raw in markdown.lines() {
        let marker = raw.trim_start();
        let marker = ["```", "~~~"].into_iter().find(|m| marker.starts_with(m));
        if let Some(open) = fence {
            out.push_str(raw);
            out.push('\n');
            if marker == Some(open) {
                fence = None;
            }
            continue;
        }
        if marker.is_some() {
            fence = marker;
        }
        let line = raw.trim_end();
        if line.is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        out.push_str(line);
        out.push('\n');
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ARTICLE: &str = include_str!("fixtures/article.html");
    const DOCS_TABLE: &str = include_str!("fixtures/docs-table.html");

    #[test]
    fn tidy_leaves_fenced_code_alone() {
        let md = "# T\n\n\n\ntext  \n\n```rust\nfn a() {}  \n\n\n\nfn b() {}\n```\n\n\nafter";
        assert_eq!(
            tidy(md),
            "# T\n\ntext\n\n```rust\nfn a() {}  \n\n\n\nfn b() {}\n```\n\nafter"
        );
    }

    #[test]
    fn an_article_keeps_headings_and_links() {
        let (title, md) = to_markdown(ARTICLE, "https://blog.example.com/posts/rust-allocators");
        assert_eq!(title, "Choosing an allocator for a Rust CLI");
        assert!(md.contains("## Why the default allocator"), "{md}");
        assert!(
            md.contains("[mimalloc](https://github.com/microsoft/mimalloc)"),
            "{md}"
        );
        // Navigation, footer and scripts are gone.
        assert!(!md.contains("Subscribe to our newsletter"), "{md}");
        assert!(!md.contains("window.analytics"), "{md}");
    }

    #[test]
    fn a_docs_page_keeps_its_table_and_absolute_links() {
        let (title, md) = to_markdown(DOCS_TABLE, "https://docs.example.com/api/limits");
        assert_eq!(title, "Rate limits");
        assert!(md.contains("| Plan"), "{md}");
        assert!(md.contains("https://docs.example.com/api/keys"), "{md}");
        assert!(!md.contains("All rights reserved"), "{md}");
    }

    #[test]
    fn a_page_without_a_title_falls_back_to_the_url() {
        let (title, md) = to_markdown("<p>hi</p>", "https://example.com/x");
        assert_eq!(title, "https://example.com/x");
        assert_eq!(md, "hi");
    }

    /// A nesting bomb used to take minutes (quadratic in depth: 32k levels
    /// took 6 s in release, 100k did not finish in 20 min in debug), and a
    /// 5 MB body allows a million levels.
    #[test]
    fn a_nesting_bomb_converts_in_bounded_time() {
        let text = "Plain words that should survive the flattening. ".repeat(10);
        let html = format!(
            "<html><head><title>t</title></head><body>{}<p>{text}</p></body></html>",
            "<div>".repeat(90_000)
        );
        let started = std::time::Instant::now();
        let (_, md) = to_markdown(&html, "https://example.com/");
        assert!(md.contains("Plain words that should survive"), "{md}");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(20),
            "{:?}",
            started.elapsed()
        );
    }

    /// Many blocks each just under the depth limit: the work budget, not
    /// the depth limit, has to stop this one.
    #[test]
    fn repeated_deep_blocks_fit_the_work_budget() {
        let block = format!("{}word{}", "<div>".repeat(511), "</div>".repeat(511));
        let html = block.repeat(2 * 1024 * 1024 / block.len());
        let flat = flatten_deep(&html, MAX_DEPTH);
        assert!(nesting(&flat, MAX_DEPTH, None).work <= WORK_BUDGET);
        let started = std::time::Instant::now();
        let (_, md) = to_markdown(&html, "https://example.com/");
        assert!(md.contains("word"), "{md}");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(30),
            "{:?}",
            started.elapsed()
        );
    }

    /// Hundreds of thousands of flat elements: cut at the start-tag cap.
    #[test]
    fn a_page_of_countless_elements_is_cut() {
        let html = "<p>x</p>".repeat(300_000);
        let flat = flatten_deep(&html, MAX_DEPTH);
        assert_eq!(flat.len(), MAX_START_TAGS * "<p>x</p>".len());
        assert!(matches!(flat, Cow::Borrowed(_)));
    }

    #[test]
    fn honest_pages_are_not_rewritten() {
        for page in [
            ARTICLE,
            DOCS_TABLE,
            include_str!("fixtures/cloudflare-jsd-page.html"),
        ] {
            assert!(matches!(flatten_deep(page, MAX_DEPTH), Cow::Borrowed(_)));
        }
        // Unclosed paragraphs and list items do not nest.
        let sloppy = "<ul>".to_string() + &"<li><p>item".repeat(5000) + "</ul>";
        assert!(matches!(flatten_deep(&sloppy, MAX_DEPTH), Cow::Borrowed(_)));
        // Self-closed SVG children do not nest; `<div/>` does (HTML ignores
        // the slash).
        let svg = "<svg>".to_string() + &"<path d=\"M0\"/>".repeat(5000) + "</svg>";
        assert!(matches!(flatten_deep(&svg, MAX_DEPTH), Cow::Borrowed(_)));
        assert!(nesting(&"<div/>".repeat(600), MAX_DEPTH, None).deepest > MAX_DEPTH);
    }

    #[test]
    fn flattening_keeps_text_and_balance() {
        let html = format!("{}x{}", "<div>".repeat(5), "</div>".repeat(5));
        assert_eq!(
            flatten_deep(&html, 3),
            format!("{}x{}", "<div>".repeat(3), "</div>".repeat(3))
        );
        // Script content is not scanned for tags.
        let script = "<script>var s = '<div><div><div><div>';</script><b>x</b>";
        assert!(matches!(flatten_deep(script, 3), Cow::Borrowed(_)));
    }
}
