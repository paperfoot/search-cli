//! The direct, honest GET: markdown first, redirects followed by hand with
//! the SSRF guard on every hop, a 5 MB body cap, charset-aware decoding.

use super::{Page, PageError};
use encoding_rs::{Encoding, UTF_16BE, UTF_16LE, UTF_8};
use reqwest::header::{HeaderValue, ACCEPT, CONTENT_TYPE, LOCATION, USER_AGENT};
use std::time::Duration;
use url::Url;

/// Markdown first with an implicit q=1: listed after HTML it loses, and
/// developers.cloudflare.com answers HTML (measured 2 Nov 2026).
const ACCEPT_VALUE: &str = "text/markdown, text/html;q=0.9, */*;q=0.8";

/// Who is asking, honestly: no browser costume, no fake referer.
const UA: &str = concat!(
    "search-cli/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/paperfoot/search-cli)"
);

/// Body cap, the same as the hosted service: a page bigger than this is not
/// an article, and the cap keeps a binary URL from filling memory.
pub const MAX_BODY_BYTES: usize = 5 * 1024 * 1024;

/// Redirect hops followed before giving up.
pub const MAX_REDIRECTS: usize = 5;

/// Which hops go through `guard::assert_public_url`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HopGuard {
    /// Every hop, the first included. The default.
    Every,
    /// Only redirect targets: the caller already checked the first URL.
    RedirectsOnly,
    /// None: the user passed `--allow-private`.
    Off,
}

/// A client fit for [`fetch_direct`]: it must not follow redirects itself,
/// or a redirect to a private address would skip the guard.
pub fn client() -> Result<reqwest::Client, PageError> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(UA)
        .connect_timeout(Duration::from_secs(5))
        .build()
        .map_err(|e| PageError::Network(format!("cannot build HTTP client: {e}")))
}

/// Fetch `url` with every hop guarded. `client` must come from [`client`]
/// (or otherwise have redirects disabled); `timeout` bounds the whole fetch,
/// redirects and body included.
pub async fn fetch_direct(
    client: &reqwest::Client,
    url: &str,
    timeout: Duration,
) -> Result<Page, PageError> {
    fetch_guarded(client, url, timeout, HopGuard::Every).await
}

/// [`fetch_direct`] with an explicit guard policy.
pub async fn fetch_guarded(
    client: &reqwest::Client,
    url: &str,
    timeout: Duration,
    guard: HopGuard,
) -> Result<Page, PageError> {
    tokio::time::timeout(timeout, fetch_inner(client, url, guard))
        .await
        .map_err(|_| PageError::Timeout(timeout))?
}

async fn fetch_inner(
    client: &reqwest::Client,
    url: &str,
    guard: HopGuard,
) -> Result<Page, PageError> {
    let mut current = parse_http_url(url)?;
    let mut hop = 0;
    loop {
        if guard == HopGuard::Every || (guard == HopGuard::RedirectsOnly && hop > 0) {
            check_public(current.as_str()).await?;
        }
        let mut resp = client
            .get(current.clone())
            .header(ACCEPT, HeaderValue::from_static(ACCEPT_VALUE))
            .header(USER_AGENT, HeaderValue::from_static(UA))
            .send()
            .await
            .map_err(|e| PageError::Network(e.without_url().to_string()))?;

        // A client that followed a redirect on its own skipped the guard.
        if resp.url() != &current {
            return Err(PageError::Network(
                "the HTTP client follows redirects itself; build it with page::client()".into(),
            ));
        }

        let status = resp.status();
        if status.is_redirection() && status.as_u16() != 304 {
            hop += 1;
            if hop > MAX_REDIRECTS {
                return Err(PageError::TooManyRedirects);
            }
            let location = resp
                .headers()
                .get(LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or(PageError::BadRedirect(status.as_u16()))?;
            let next = current
                .join(location)
                .map_err(|_| PageError::BadRedirect(status.as_u16()))?;
            if !matches!(next.scheme(), "http" | "https") {
                return Err(PageError::BadRedirect(status.as_u16()));
            }
            current = next;
            continue;
        }

        let headers = resp.headers().clone();
        let content_type = headers
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string);

        let mut raw: Vec<u8> = Vec::new();
        while let Some(chunk) = resp
            .chunk()
            .await
            .map_err(|e| PageError::Network(e.without_url().to_string()))?
        {
            let room = MAX_BODY_BYTES - raw.len();
            if chunk.len() >= room {
                raw.extend_from_slice(&chunk[..room]);
                break;
            }
            raw.extend_from_slice(&chunk);
        }

        return Ok(Page {
            final_url: current.to_string(),
            status: status.as_u16(),
            body: decode(&raw, content_type.as_deref()),
            content_type,
            headers,
        });
    }
}

fn parse_http_url(raw: &str) -> Result<Url, PageError> {
    let url = Url::parse(raw).map_err(|e| PageError::InvalidUrl(format!("{raw}: {e}")))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(PageError::InvalidUrl(format!(
            "{raw}: only http and https are fetched"
        )));
    }
    Ok(url)
}

async fn check_public(url: &str) -> Result<(), PageError> {
    crate::guard::assert_public_url(url)
        .await
        .map_err(|e| match e {
            crate::errors::SearchError::InvalidInput { message } => PageError::Blocked(message),
            other => PageError::Blocked(other.to_string()),
        })
}

/// Decode with the BOM, else the Content-Type charset, else a `<meta>`
/// charset in the first 4 KB, else UTF-8 (the WHATWG order, simplified).
fn decode(raw: &[u8], content_type: Option<&str>) -> String {
    let declared = content_type
        .and_then(charset_param)
        .and_then(|l| Encoding::for_label(l.as_bytes()));
    let encoding = declared
        .or_else(|| meta_charset(raw).and_then(|l| Encoding::for_label(l.as_bytes())))
        // A UTF-16 label without a BOM is a lie the spec says to read as UTF-8.
        .map(|e| {
            if e == UTF_16LE || e == UTF_16BE {
                UTF_8
            } else {
                e
            }
        })
        .unwrap_or(UTF_8);
    // `decode` sniffs a BOM first, which overrides the label.
    let (text, _, _) = encoding.decode(raw);
    text.into_owned()
}

/// `text/html; charset="Shift_JIS"` → `Shift_JIS`.
fn charset_param(content_type: &str) -> Option<&str> {
    content_type.split(';').skip(1).find_map(|param| {
        let (key, value) = param.split_once('=')?;
        key.trim()
            .eq_ignore_ascii_case("charset")
            .then(|| value.trim().trim_matches(|c| c == '"' || c == '\''))
            .filter(|v| !v.is_empty())
    })
}

/// The charset of the first `<meta charset=…>` or
/// `<meta http-equiv="Content-Type" content="…; charset=…">` in the head.
fn meta_charset(raw: &[u8]) -> Option<String> {
    let head = &raw[..raw.len().min(4096)];
    let lower = head.to_ascii_lowercase();
    let mut from = 0;
    while let Some(start) = find(&lower[from..], b"<meta").map(|i| i + from) {
        let end = find(&lower[start..], b">").map_or(lower.len(), |i| i + start);
        let tag = &lower[start..end];
        if let Some(i) = find(tag, b"charset=") {
            let value: Vec<u8> = tag[i + 8..]
                .iter()
                .copied()
                .skip_while(|b| *b == b'"' || *b == b'\'' || *b == b' ')
                .take_while(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b':' | b'.'))
                .collect();
            if !value.is_empty() {
                return String::from_utf8(value).ok();
            }
        }
        from = end;
    }
    None
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// A one-file HTTP server on 127.0.0.1: `respond(path, request)` returns
    /// the raw response bytes. Serves until the test ends.
    async fn serve(respond: fn(&str, &str, SocketAddr) -> Vec<u8>) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        match sock.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        }
                    }
                    let request = String::from_utf8_lossy(&buf).to_string();
                    let path = request.split_whitespace().nth(1).unwrap_or("/").to_string();
                    let _ = sock.write_all(&respond(&path, &request, addr)).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        addr
    }

    fn response(status: &str, headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
        let mut out = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n",
            body.len()
        );
        for (k, v) in headers {
            out.push_str(&format!("{k}: {v}\r\n"));
        }
        out.push_str("\r\n");
        let mut bytes = out.into_bytes();
        bytes.extend_from_slice(body);
        bytes
    }

    async fn get(addr: SocketAddr, path: &str, guard: HopGuard) -> Result<Page, PageError> {
        let client = client().unwrap();
        fetch_guarded(
            &client,
            &format!("http://{addr}{path}"),
            Duration::from_secs(5),
            guard,
        )
        .await
    }

    #[tokio::test]
    async fn markdown_is_asked_for_first_and_passed_through() {
        let addr = serve(|_, req, _| {
            let lower = req.to_ascii_lowercase();
            let wants_md = lower.contains("\r\naccept: text/markdown,");
            let honest_ua = lower.contains("\r\nuser-agent: search-cli/");
            if wants_md && honest_ua {
                response(
                    "200 OK",
                    &[("Content-Type", "text/markdown; charset=utf-8")],
                    b"# Docs\n\nHello.",
                )
            } else {
                response("200 OK", &[("Content-Type", "text/html")], b"<p>html</p>")
            }
        })
        .await;
        let page = get(addr, "/docs", HopGuard::RedirectsOnly).await.unwrap();
        assert_eq!(page.status, 200);
        assert!(page.is_markdown(), "content type {:?}", page.content_type);
        assert_eq!(page.body, "# Docs\n\nHello.");
        assert_eq!(page.final_url, format!("http://{addr}/docs"));
    }

    #[tokio::test]
    async fn shift_jis_is_decoded_from_meta_and_from_the_header() {
        let addr = serve(|path, _, _| {
            let html = "<html><head><meta http-equiv=\"Content-Type\" content=\"text/html; charset=Shift_JIS\"></head><body><p>日本語のページです</p></body></html>";
            let (bytes, _, _) = encoding_rs::SHIFT_JIS.encode(html);
            let ct = if path == "/header" { "text/html; charset=shift_jis" } else { "text/html" };
            response("200 OK", &[("Content-Type", ct)], &bytes)
        })
        .await;
        for path in ["/meta", "/header"] {
            let page = get(addr, path, HopGuard::Off).await.unwrap();
            assert!(
                page.body.contains("日本語のページです"),
                "{path}: {}",
                page.body
            );
        }
    }

    #[tokio::test]
    async fn redirect_to_a_private_address_is_refused() {
        let addr = serve(|path, _, addr| match path {
            "/start" => response(
                "302 Found",
                &[(
                    "Location",
                    &format!("http://127.0.0.1:{}/admin", addr.port()),
                )],
                b"",
            ),
            _ => response(
                "200 OK",
                &[("Content-Type", "text/plain")],
                b"internal secrets",
            ),
        })
        .await;
        let err = get(addr, "/start", HopGuard::RedirectsOnly)
            .await
            .unwrap_err();
        assert!(matches!(err, PageError::Blocked(_)), "{err:?}");
        // The default guards the first hop as well.
        let err = get(addr, "/admin", HopGuard::Every).await.unwrap_err();
        assert!(matches!(err, PageError::Blocked(_)), "{err:?}");
    }

    #[tokio::test]
    async fn redirects_are_followed_to_the_final_url_and_capped() {
        let addr = serve(|path, _, _| match path {
            "/a" => response("301 Moved Permanently", &[("Location", "/b")], b""),
            "/b" => response("308 Permanent Redirect", &[("Location", "c")], b""),
            "/c" => response("200 OK", &[("Content-Type", "text/plain")], b"arrived"),
            _ => response("302 Found", &[("Location", "/loop")], b""),
        })
        .await;
        let page = get(addr, "/a", HopGuard::Off).await.unwrap();
        assert_eq!(page.body, "arrived");
        assert_eq!(page.final_url, format!("http://{addr}/c"));
        let err = get(addr, "/loop", HopGuard::Off).await.unwrap_err();
        assert!(matches!(err, PageError::TooManyRedirects), "{err:?}");
    }

    #[tokio::test]
    async fn body_is_capped_at_five_megabytes() {
        let addr = serve(|_, _, _| {
            response(
                "200 OK",
                &[("Content-Type", "text/plain")],
                &vec![b'a'; MAX_BODY_BYTES + 1024 * 1024],
            )
        })
        .await;
        let page = get(addr, "/big", HopGuard::Off).await.unwrap();
        assert_eq!(page.body.len(), MAX_BODY_BYTES);
    }

    #[tokio::test]
    async fn payment_required_keeps_status_and_price_header() {
        let addr = serve(|_, _, _| {
            response(
                "402 Payment Required",
                &[("crawler-price", "USD 0.01"), ("Content-Type", "text/html")],
                b"<p>pay</p>",
            )
        })
        .await;
        let page = get(addr, "/paid", HopGuard::Off).await.unwrap();
        assert_eq!(page.status, 402);
        assert_eq!(
            page.verdict(),
            super::super::Verdict::PaymentRequired {
                price: Some("USD 0.01".into())
            }
        );
    }

    #[tokio::test]
    async fn non_http_schemes_are_refused() {
        let client = client().unwrap();
        let err = fetch_direct(&client, "file:///etc/passwd", Duration::from_secs(1))
            .await
            .unwrap_err();
        assert!(matches!(err, PageError::InvalidUrl(_)), "{err:?}");
    }

    #[test]
    fn charset_labels_are_parsed() {
        assert_eq!(
            charset_param("text/html; charset=\"Shift_JIS\""),
            Some("Shift_JIS")
        );
        assert_eq!(charset_param("text/html;CHARSET=utf-8"), Some("utf-8"));
        assert_eq!(charset_param("text/html"), None);
        assert_eq!(
            meta_charset(b"<html><head><meta charset='windows-1251'>").as_deref(),
            Some("windows-1251")
        );
        assert_eq!(meta_charset(b"<p>no meta</p>"), None);
    }

    /// Cloudflare's own docs answer `Accept: text/markdown` with markdown.
    /// Live and free; run with `cargo test page:: -- --ignored`.
    #[tokio::test]
    #[ignore = "live network"]
    async fn live_cloudflare_docs_answer_markdown() {
        let client = client().unwrap();
        let page = fetch_direct(
            &client,
            "https://developers.cloudflare.com/",
            Duration::from_secs(15),
        )
        .await
        .unwrap();
        assert_eq!(page.status, 200);
        assert!(page.is_markdown(), "content type {:?}", page.content_type);
    }
}
