//! HTML to markdown: Readability-style article extraction when the page
//! reads as an article, the whole page minus its chrome otherwise (docs,
//! listings, tables, where article extraction throws content away).
//!
//! Both paths share one HTML parser (dom_query, which dom_smoothie is built
//! on), so a page is parsed once and the binary carries one HTML5 stack.

use dom_query::Document;
use dom_smoothie::{Config, Readability, TextMode};
use url::Url;

/// Elements dropped from a whole-page conversion: not content.
const CHROME: &[&str] = &[
    "head", "script", "style", "noscript", "template", "svg", "nav", "footer", "iframe",
];

/// `(title, markdown)` for `html` fetched from `url`. The title comes from
/// the article, else `<title>`, else the first `<h1>`, else the URL. This is
/// CPU work (5-20 ms on a large page); call it outside a latency-critical
/// async section, e.g. in `spawn_blocking`, when pages are big.
pub fn to_markdown(html: &str, url: &str) -> (String, String) {
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

/// Trim trailing spaces and collapse runs of blank lines.
fn tidy(markdown: &str) -> String {
    let mut out = String::with_capacity(markdown.len());
    let mut blank = 0;
    for line in markdown.lines() {
        let line = line.trim_end();
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
}
