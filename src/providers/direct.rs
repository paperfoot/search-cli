//! `direct`: rung 0 of the extract chain (PLAN.md D11). One honest GET that
//! asks for markdown first, follows redirects with the SSRF guard on every
//! hop, and returns the page as markdown when it is real content. Free and
//! keyless; works on every platform.

use super::spec::{FilterSupport, Price};
use super::{Adapter, Call, Output, ProviderSpec};
use crate::context::AppContext;
use crate::errors::SearchError;
use crate::page::{self, Page, PageError, Verdict};
use crate::registry::{self, DirectSaw};
use crate::types::{FailureCategory, Mode, SearchResult};
use async_trait::async_trait;
use std::sync::Arc;
use std::time::Duration;

pub static SPEC: ProviderSpec = ProviderSpec {
    name: "direct",
    env_keys: &[],
    modes: &[Mode::Extract],
    filters: FilterSupport {
        freshness: false,
        domains: false,
        country: false,
        lang: false,
    },
    max_count: 1,
    timeout: Duration::from_secs(8),
    billed_on_send: false,
    price: Price {
        per_call: 0,
        per_extra_result: 0,
        included_results: 0,
    },
};

pub struct Direct {
    _ctx: Arc<AppContext>,
}

impl Direct {
    pub fn new(ctx: Arc<AppContext>) -> Self {
        Self { _ctx: ctx }
    }
}

#[async_trait]
impl Adapter for Direct {
    fn spec(&self) -> &'static ProviderSpec {
        &SPEC
    }

    fn is_configured(&self) -> bool {
        true // keyless, runs locally
    }

    async fn call(&self, call: &Call<'_>) -> Result<Output, SearchError> {
        unsupported_unless_extract("direct", call.mode)?;
        let client = if crate::guard::allow_private() {
            page::client_allow_private()
        } else {
            page::client()
        }
        .map_err(|e| page_error("direct", e))?;
        fetch_and_note(&client, call.query, call.timeout).await
    }
}

/// Fetch and judge `url`, noting for the later rungs what direct saw: a
/// rejected page, no answer, or a destination the guard refused (then no
/// rung, local or paid, is sent the URL).
async fn fetch_and_note(
    client: &page::PageClient,
    url: &str,
    timeout: Duration,
) -> Result<Output, SearchError> {
    let page = match page::fetch_direct(client, url, timeout).await {
        Ok(page) => page,
        Err(e) => {
            match e {
                PageError::Network(_) | PageError::Timeout(_) => {
                    registry::note_direct(url, DirectSaw::Unreachable)
                }
                PageError::Blocked(_) => registry::note_direct(url, DirectSaw::Blocked),
                _ => {}
            }
            return Err(page_error("direct", e));
        }
    };
    match extract_page("direct", page).await {
        Ok(out) => Ok(out),
        Err((verdict, err)) => {
            registry::note_direct(url, DirectSaw::Rejected(verdict));
            Err(err)
        }
    }
}

/// The page as one extract result, or its rejecting verdict as a failure
/// (`rejected`; a paywall keeps its price, an error page its status). Used
/// by the local rungs, which pay nothing for a rejected page.
pub(crate) async fn extract_page(
    provider: &'static str,
    page: Page,
) -> Result<Output, (Verdict, SearchError)> {
    let (page, extracted) = tokio::task::spawn_blocking(move || {
        let extracted = page::extract(&page);
        (page, extracted)
    })
    .await
    .map_err(|e| {
        (
            Verdict::Binary,
            SearchError::Config(format!("{provider}: page extraction panicked: {e}")),
        )
    })?;
    match extracted {
        Ok(ex) => Ok(Output {
            results: vec![result(provider, &page, ex)],
            ..Output::default()
        }),
        Err(verdict) => {
            let err = verdict_error(provider, &page, &verdict);
            Err((verdict, err))
        }
    }
}

/// A paid rung's page as one result, its verdict marked in
/// `extra.verdict` when the page is junk: the chain then records a billed
/// rejection instead of returning it.
pub(crate) async fn marked_output(provider: &'static str, page: Page) -> Output {
    let (page, extracted) = tokio::task::spawn_blocking(move || {
        let extracted = crate::page::extract(&page);
        (page, extracted)
    })
    .await
    .unwrap_or_else(|e| std::panic::resume_unwind(e.into_panic()));
    let (title, snippet, verdict) = match extracted {
        Ok(ex) => (ex.title, ex.markdown, None),
        Err(v) => (page.final_url.clone(), String::new(), Some(v.code())),
    };
    Output {
        results: vec![SearchResult {
            title,
            url: page.final_url.clone(),
            snippet,
            source: provider.to_string(),
            published: None,
            image_url: None,
            extra: Some(match verdict {
                Some(v) => serde_json::json!({ "verdict": v }),
                None => serde_json::json!({ "content_type": page.content_type }),
            }),
        }],
        ..Output::default()
    }
}

fn result(provider: &'static str, page: &Page, ex: page::Extracted) -> SearchResult {
    SearchResult {
        title: ex.title,
        url: page.final_url.clone(),
        snippet: ex.markdown,
        source: provider.to_string(),
        published: None,
        image_url: None,
        extra: Some(serde_json::json!({
            "content_origin": "untrusted_web_content",
            "content_type": page.content_type,
        })),
    }
}

fn verdict_error(provider: &'static str, page: &Page, verdict: &Verdict) -> SearchError {
    let detail = match verdict {
        Verdict::PaymentRequired { price } => format!(
            "the site charges for this page (crawler-price: {})",
            price.as_deref().unwrap_or("unstated")
        ),
        Verdict::HttpError(s) => format!("HTTP {s} from {}", page.final_url),
        Verdict::Binary => format!(
            "not text ({})",
            page.content_type.as_deref().unwrap_or("no content type")
        ),
        Verdict::Challenge => "an anti-bot challenge page".to_string(),
        Verdict::JsShell => "a page that only renders in a browser".to_string(),
        Verdict::Thin => "too little text after extraction".to_string(),
        Verdict::Ok => "content".to_string(),
    };
    SearchError::Upstream {
        provider,
        status: page.status,
        category: FailureCategory::Rejected,
        message: format!("{}: {detail}", verdict.code()),
    }
}

/// A fetch that produced no page, typed for the envelope: the guard's
/// refusal is invalid input, everything else a network failure or timeout.
pub(crate) fn page_error(provider: &'static str, e: PageError) -> SearchError {
    match e {
        PageError::Blocked(message) | PageError::InvalidUrl(message) => {
            SearchError::InvalidInput { message }
        }
        PageError::Timeout(_) => super::timeout_error(provider),
        other => SearchError::Upstream {
            provider,
            status: 0,
            category: FailureCategory::Network,
            message: other.to_string(),
        },
    }
}

/// The URL rungs serve extract (and its scrape alias) only; any other mode
/// is a routing mistake, never an empty success.
pub(crate) fn unsupported_unless_extract(
    provider: &'static str,
    mode: Mode,
) -> Result<(), SearchError> {
    if matches!(mode, Mode::Extract | Mode::Scrape) {
        return Ok(());
    }
    Err(unsupported(provider, mode))
}

pub(crate) fn unsupported(provider: &'static str, mode: Mode) -> SearchError {
    SearchError::Api {
        provider,
        code: "unsupported_mode",
        message: format!("{provider} does not serve mode {mode}"),
        status: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;
    use crate::types::SearchOpts;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// Serve one canned response per connection on 127.0.0.1.
    pub(crate) async fn serve(responses: Vec<(&'static str, String)>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let responses = responses.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]).to_string();
                    let path = req.split_whitespace().nth(1).unwrap_or("/").to_string();
                    let (head, body) = responses
                        .iter()
                        .find(|(p, _)| *p == path)
                        .map(|(_, r)| r.split_once("\r\n\r\n").unwrap())
                        .unwrap_or(("HTTP/1.1 404 Not Found", ""));
                    let msg = format!(
                        "{head}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = sock.write_all(msg.as_bytes()).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        format!("http://{addr}")
    }

    async fn run(url: &str) -> Result<Output, SearchError> {
        crate::guard::set_allow_private(true); // local test servers
        let d = Direct::new(Arc::new(AppContext::new(AppConfig::default())));
        let opts = SearchOpts::default();
        d.call(&Call {
            mode: Mode::Extract,
            query: url,
            count: 1,
            opts: &opts,
            timeout: Duration::from_secs(5),
            key: "",
        })
        .await
    }

    #[tokio::test]
    async fn markdown_comes_back_as_is_with_the_final_url() {
        let md = format!("# Guide\n\n{}", "Plain markdown paragraph. ".repeat(20));
        let base = serve(vec![
            (
                "/old",
                "HTTP/1.1 301 Moved\r\nLocation: /new\r\n\r\n".to_string(),
            ),
            (
                "/new",
                format!("HTTP/1.1 200 OK\r\nContent-Type: text/markdown\r\n\r\n{md}"),
            ),
        ])
        .await;
        let out = run(&format!("{base}/old")).await.unwrap();
        let r = &out.results[0];
        assert_eq!(r.url, format!("{base}/new"));
        assert_eq!(r.title, "Guide");
        assert_eq!(r.snippet, md.trim());
        let extra = r.extra.as_ref().unwrap();
        assert_eq!(extra["content_origin"], "untrusted_web_content");
        assert_eq!(extra["content_type"], "text/markdown");
    }

    #[tokio::test]
    async fn a_challenge_is_rejected_and_noted_for_the_next_rung() {
        let page = "<html><head><title>Just a moment...</title></head><body>Checking your browser before accessing the site.</body></html>";
        let base = serve(vec![(
            "/cf",
            format!("HTTP/1.1 403 Forbidden\r\nContent-Type: text/html\r\n\r\n{page}"),
        )])
        .await;
        let url = format!("{base}/cf");
        let err = run(&url).await.unwrap_err();
        assert_eq!(err.category(), FailureCategory::Rejected);
        assert!(err.to_string().contains("challenge"), "{err}");
        // Refused: stealth may try; the chain goes on.
        assert!(registry::rung_gate("stealth", &url).is_ok());
        assert!(registry::rung_gate("jina", &url).is_ok());
    }

    #[tokio::test]
    async fn a_paywall_keeps_its_price_and_stops_the_chain() {
        let base = serve(vec![(
            "/paid",
            "HTTP/1.1 402 Payment Required\r\ncrawler-price: USD 0.002\r\n\r\npay".to_string(),
        )])
        .await;
        let url = format!("{base}/paid");
        let err = run(&url).await.unwrap_err();
        assert_eq!(err.category(), FailureCategory::Rejected);
        assert!(err.to_string().contains("USD 0.002"), "{err}");
        assert!(registry::rung_gate("firecrawl", &url).is_err());
    }

    #[tokio::test]
    async fn a_pdf_skips_stealth_and_goes_to_the_readers() {
        let base = serve(vec![(
            "/doc.pdf",
            "HTTP/1.1 200 OK\r\nContent-Type: application/pdf\r\n\r\n%PDF-1.7 binary".to_string(),
        )])
        .await;
        let url = format!("{base}/doc.pdf");
        let err = run(&url).await.unwrap_err();
        assert!(err.to_string().contains("binary"), "{err}");
        assert!(registry::rung_gate("stealth", &url).is_err());
        assert!(registry::rung_gate("jina", &url).is_ok());
        assert!(registry::rung_gate("firecrawl", &url).is_ok());
    }

    /// Review (wave 2): a URL direct's guard refused is not retried by
    /// stealth (whose resolver may get a different answer) nor sent to a
    /// paid rung.
    #[tokio::test]
    async fn a_guard_refusal_stops_every_later_rung() {
        let url = "http://127.0.0.1:9/admin";
        let client = page::client().unwrap();
        let err = fetch_and_note(&client, url, Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(matches!(err, SearchError::InvalidInput { .. }), "{err}");
        for rung in ["stealth", "jina", "firecrawl", "browserless"] {
            assert!(registry::rung_gate(rung, url).is_err(), "{rung} ran");
        }
    }

    #[tokio::test]
    async fn other_modes_are_unsupported_not_empty() {
        let d = Direct::new(Arc::new(AppContext::new(AppConfig::default())));
        let opts = SearchOpts::default();
        let err = d
            .call(&Call {
                mode: Mode::News,
                query: "x",
                count: 1,
                opts: &opts,
                timeout: Duration::from_secs(1),
                key: "",
            })
            .await
            .unwrap_err();
        assert_eq!(err.error_code(), "unsupported_mode");
    }
}
