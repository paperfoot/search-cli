//! The direct, honest GET: markdown first, redirects followed by hand with
//! the SSRF guard on every hop, a 5 MB body cap, charset-aware decoding.

use super::{Page, PageError};
use encoding_rs::{Encoding, UTF_16BE, UTF_16LE, UTF_8};
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::header::{HeaderValue, ACCEPT, CONTENT_TYPE, LOCATION, USER_AGENT};
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use url::{Host, Url};

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

/// A DNS answer slower than this fails the fetch (it never passes the guard:
/// a guard that let a slow lookup through would be one an attacker's DNS
/// server can switch off). Generous, because a cold lookup can take seconds.
const DNS_TIMEOUT: Duration = Duration::from_secs(5);

/// The only client [`fetch_direct`] accepts. It never follows redirects on
/// its own (every hop goes through the guard here), and the guarded kind
/// resolves names itself, refusing any answer with a private, loopback or
/// link-local address. The check runs on the addresses the connection then
/// uses, so DNS rebinding and a slow first answer have nothing to slip
/// through. Build one with [`client`] or [`client_allow_private`].
#[derive(Clone)]
pub struct PageClient {
    http: reqwest::Client,
    guarded: bool,
    /// Tests only: one local server treated as public, so a redirect from a
    /// "public" page to a private one can be exercised on 127.0.0.1.
    #[cfg(test)]
    exempt: Option<SocketAddr>,
}

impl std::fmt::Debug for PageClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PageClient")
            .field("guarded", &self.guarded)
            .finish()
    }
}

/// The default client: private, loopback and link-local destinations are
/// refused on every hop, by name and by resolved address. System proxies are
/// ignored, because a proxy resolves the name where the guard cannot see it.
pub fn client() -> Result<PageClient, PageError> {
    guarded_client(Arc::new(PublicOnly {
        lookup: system_lookup,
    }))
}

/// For `--allow-private`: no address filtering, system proxies honoured.
pub fn client_allow_private() -> Result<PageClient, PageError> {
    Ok(PageClient {
        http: base_builder().build().map_err(build_error)?,
        guarded: false,
        #[cfg(test)]
        exempt: None,
    })
}

fn guarded_client(resolver: Arc<PublicOnly>) -> Result<PageClient, PageError> {
    Ok(PageClient {
        http: base_builder()
            .no_proxy()
            .dns_resolver(resolver)
            .build()
            .map_err(build_error)?,
        guarded: true,
        #[cfg(test)]
        exempt: None,
    })
}

fn base_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(UA)
        .connect_timeout(Duration::from_secs(5))
}

fn build_error(e: reqwest::Error) -> PageError {
    PageError::Network(format!("cannot build HTTP client: {e}"))
}

type LookupFuture = Pin<Box<dyn Future<Output = std::io::Result<Vec<SocketAddr>>> + Send>>;

fn system_lookup(host: String) -> LookupFuture {
    Box::pin(async move { Ok(tokio::net::lookup_host((host.as_str(), 0)).await?.collect()) })
}

/// The resolver of a guarded client: fails closed on an error, a timeout,
/// an empty answer or any non-public address in the answer.
struct PublicOnly {
    lookup: fn(String) -> LookupFuture,
}

impl Resolve for PublicOnly {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().trim_end_matches('.').to_ascii_lowercase();
        let lookup = self.lookup;
        Box::pin(async move {
            if is_internal_name(&host) {
                return Err(Blocked(blocked_name(&host)).into());
            }
            let addrs = tokio::time::timeout(DNS_TIMEOUT, lookup(host.clone()))
                .await
                .map_err(|_| format!("DNS lookup for {host} timed out after {DNS_TIMEOUT:?}"))??;
            if addrs.is_empty() {
                return Err(format!("DNS lookup for {host} returned no addresses").into());
            }
            for addr in &addrs {
                if let Err(e) = check_ip(addr.ip()).await {
                    return Err(Blocked(format!("{host} resolves to {e}")).into());
                }
            }
            Ok(Box::new(addrs.into_iter()) as Addrs)
        })
    }
}

/// A destination the guard refused, carried through reqwest's error chain
/// so the caller gets [`PageError::Blocked`], not a network error.
#[derive(Debug)]
struct Blocked(String);

impl std::fmt::Display for Blocked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Blocked {}

/// Names that never leave the machine or the LAN: the same list as
/// `guard::assert_public_url`.
fn is_internal_name(host: &str) -> bool {
    host == "localhost"
        || host.ends_with(".localhost")
        || host.ends_with(".local")
        || host.ends_with(".internal")
}

fn blocked_name(host: &str) -> String {
    format!(
        "'{host}' is a private/loopback/link-local address — blocked by default so agents can't be steered into internal endpoints. Pass --allow-private to override."
    )
}

/// The guard's address rules, applied to one IP (no DNS for an IP string).
async fn check_ip(ip: IpAddr) -> Result<(), String> {
    crate::guard::assert_public_host(&ip.to_string(), 0)
        .await
        .map_err(|e| match e {
            crate::errors::SearchError::InvalidInput { message } => message,
            other => other.to_string(),
        })
}

/// Fetch `url`, guarded on every hop when `client` came from [`client`].
/// `timeout` bounds the whole fetch, redirects and body included.
pub async fn fetch_direct(
    client: &PageClient,
    url: &str,
    timeout: Duration,
) -> Result<Page, PageError> {
    tokio::time::timeout(timeout, fetch_inner(client, url))
        .await
        .map_err(|_| PageError::Timeout(timeout))?
}

impl PageClient {
    /// IP-literal hosts never reach the resolver, so they are checked here.
    async fn check_url(&self, url: &Url) -> Result<(), PageError> {
        if !self.guarded {
            return Ok(());
        }
        let ip = match url.host() {
            Some(Host::Ipv4(ip)) => IpAddr::V4(ip),
            Some(Host::Ipv6(ip)) => IpAddr::V6(ip),
            Some(Host::Domain(_)) => return Ok(()), // the resolver checks it
            None => return Err(PageError::InvalidUrl(format!("{url}: no host"))),
        };
        #[cfg(test)]
        if self.exempt
            == Some(SocketAddr::new(
                ip,
                url.port_or_known_default().unwrap_or(0),
            ))
        {
            return Ok(());
        }
        check_ip(ip).await.map_err(PageError::Blocked)
    }
}

fn send_error(e: reqwest::Error) -> PageError {
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(&e);
    while let Some(err) = source {
        if let Some(blocked) = err.downcast_ref::<Blocked>() {
            return PageError::Blocked(blocked.0.clone());
        }
        source = err.source();
    }
    PageError::Network(e.without_url().to_string())
}

async fn fetch_inner(client: &PageClient, url: &str) -> Result<Page, PageError> {
    let mut current = parse_http_url(url)?;
    let mut hop = 0;
    loop {
        // A fragment is never sent; the HTTP stack drops it, and userinfo
        // too, from the URL it reports back.
        current.set_fragment(None);
        client.check_url(&current).await?;
        let mut resp = client
            .http
            .get(current.clone())
            .header(ACCEPT, HeaderValue::from_static(ACCEPT_VALUE))
            .header(USER_AGENT, HeaderValue::from_static(UA))
            .send()
            .await
            .map_err(send_error)?;

        let status = resp.status();
        if status.is_redirection() && status.as_u16() != 304 {
            hop += 1;
            if hop > MAX_REDIRECTS {
                return Err(PageError::TooManyRedirects);
            }
            // Raw UTF-8 in Location is common; `join` percent-encodes it.
            let location = resp
                .headers()
                .get(LOCATION)
                .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
                .ok_or(PageError::BadRedirect(status.as_u16()))?;
            let next = current
                .join(location.trim())
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
        while let Some(chunk) = resp.chunk().await.map_err(send_error)? {
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

/// Decode with the BOM, else the Content-Type charset, else a `<meta>`
/// charset in the first 4 KB, else UTF-8 (the WHATWG order, simplified).
fn decode(raw: &[u8], content_type: Option<&str>) -> String {
    let declared = content_type
        .and_then(charset_param)
        .and_then(|l| Encoding::for_label(l.as_bytes()));
    // A `<meta>` tag only declares the charset of HTML: markdown or JSON
    // that quotes one in its text must not be re-decoded by it.
    let may_be_html = content_type
        .map(super::mime_essence)
        .is_none_or(|e| e.is_empty() || e.contains("html"));
    let encoding = declared
        .or_else(|| {
            may_be_html
                .then(|| meta_charset(raw))
                .flatten()
                .and_then(|l| Encoding::for_label(l.as_bytes()))
        })
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

    /// GET through the `--allow-private` client: the test servers are local.
    async fn get(addr: SocketAddr, path: &str) -> Result<Page, PageError> {
        let client = client_allow_private().unwrap();
        fetch_direct(
            &client,
            &format!("http://{addr}{path}"),
            Duration::from_secs(5),
        )
        .await
    }

    /// A guarded client whose resolver answers from `lookup`, and which
    /// treats the one local server `exempt` as public.
    fn guarded_with(lookup: fn(String) -> LookupFuture, exempt: Option<SocketAddr>) -> PageClient {
        let mut c = guarded_client(Arc::new(PublicOnly { lookup })).unwrap();
        c.exempt = exempt;
        c
    }

    fn loopback_lookup(_: String) -> LookupFuture {
        Box::pin(async { Ok(vec![SocketAddr::from(([127, 0, 0, 1], 0))]) })
    }

    fn secrets(_: &str, _: &str, _: SocketAddr) -> Vec<u8> {
        response(
            "200 OK",
            &[("Content-Type", "text/plain")],
            b"internal secrets",
        )
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
        let page = get(addr, "/docs").await.unwrap();
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
            let page = get(addr, path).await.unwrap();
            assert!(
                page.body.contains("日本語のページです"),
                "{path}: {}",
                page.body
            );
        }
    }

    #[tokio::test]
    async fn redirect_to_a_private_address_is_refused() {
        let private = serve(secrets).await;
        // A "public" page (exempted local server) redirecting inward.
        static TARGET: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        TARGET.set(format!("http://{private}/admin")).unwrap();
        let public =
            serve(|_, _, _| response("302 Found", &[("Location", TARGET.get().unwrap())], b""))
                .await;
        let client = guarded_with(loopback_lookup, Some(public));
        let err = fetch_direct(
            &client,
            &format!("http://{public}/start"),
            Duration::from_secs(5),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, PageError::Blocked(_)), "{err:?}");
        // The first hop is guarded as well.
        let err = fetch_direct(
            &client,
            &format!("http://{private}/admin"),
            Duration::from_secs(5),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, PageError::Blocked(_)), "{err:?}");
        // So is the default client.
        let err = fetch_direct(
            &super::client().unwrap(),
            &format!("http://{private}/admin"),
            Duration::from_secs(5),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, PageError::Blocked(_)), "{err:?}");
    }

    /// The guard must judge the address the connection uses, not an earlier
    /// lookup of the same name: a rebinding name, or a first answer slower
    /// than the guard's timeout, used to reach 127.0.0.1 (review R5b).
    #[tokio::test]
    async fn a_name_that_resolves_to_loopback_is_refused_at_connect() {
        let private = serve(secrets).await;
        let client = guarded_with(loopback_lookup, None);
        let url = format!("http://rebind.attacker.example:{}/admin", private.port());
        let r = fetch_direct(&client, &url, Duration::from_secs(5)).await;
        assert!(matches!(r, Err(PageError::Blocked(_))), "{r:?}");
        // Also as a redirect target.
        static TARGET: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        TARGET
            .set(format!(
                "http://rebind.attacker.example:{}/admin",
                private.port()
            ))
            .unwrap();
        let public = serve(|_, _, _| {
            response(
                "307 Temporary Redirect",
                &[("Location", TARGET.get().unwrap())],
                b"",
            )
        })
        .await;
        let client = guarded_with(loopback_lookup, Some(public));
        let r = fetch_direct(
            &client,
            &format!("http://{public}/"),
            Duration::from_secs(5),
        )
        .await;
        assert!(matches!(r, Err(PageError::Blocked(_))), "{r:?}");
    }

    /// A failed or slow DNS answer fails the fetch; it used to count as a
    /// pass for the guard.
    #[tokio::test]
    async fn dns_failure_and_timeout_fail_closed() {
        fn failing(_: String) -> LookupFuture {
            Box::pin(async { Err(std::io::Error::other("SERVFAIL")) })
        }
        fn hanging(_: String) -> LookupFuture {
            Box::pin(std::future::pending())
        }
        fn empty(_: String) -> LookupFuture {
            Box::pin(async { Ok(vec![]) })
        }
        for lookup in [failing as fn(String) -> LookupFuture, hanging, empty] {
            let client = guarded_with(lookup, None);
            let r = fetch_direct(
                &client,
                "http://slow.attacker.example/",
                Duration::from_secs(10),
            )
            .await;
            assert!(matches!(r, Err(PageError::Network(_))), "{r:?}");
        }
        // Internal names are refused before any lookup.
        let r = fetch_direct(
            &guarded_with(failing, None),
            "http://metadata.google.internal/",
            Duration::from_secs(5),
        )
        .await;
        assert!(matches!(r, Err(PageError::Blocked(_))), "{r:?}");
    }

    /// Fragments and userinfo used to fail every fetch with "the HTTP client
    /// follows redirects itself" (review R3, R14): the HTTP stack reports a
    /// URL without them.
    #[tokio::test]
    async fn fragment_and_userinfo_urls_are_fetched() {
        let addr = serve(|path, _, _| match path {
            "/hop" => response("302 Found", &[("Location", "/docs#from-location")], b""),
            _ => response("200 OK", &[("Content-Type", "text/plain")], b"hello"),
        })
        .await;
        for path in ["/docs#section-2", "/hop"] {
            let page = get(addr, path).await.unwrap();
            assert_eq!(page.body, "hello");
            assert_eq!(page.final_url, format!("http://{addr}/docs"));
        }
        let client = client_allow_private().unwrap();
        let page = fetch_direct(
            &client,
            &format!("http://user:pw@{addr}/x"),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert_eq!(page.body, "hello");
    }

    /// Raw UTF-8 in a Location header is followed, as browsers do.
    #[tokio::test]
    async fn a_utf8_location_is_followed() {
        let addr = serve(|path, _, _| match path {
            "/start" => {
                let mut r = b"HTTP/1.1 302 Found\r\nContent-Length: 0\r\nConnection: close\r\nLocation: /caf".to_vec();
                r.extend_from_slice("\u{e9}\r\n\r\n".as_bytes());
                r
            }
            _ => response("200 OK", &[("Content-Type", "text/plain")], b"arrived"),
        })
        .await;
        let page = get(addr, "/start").await.unwrap();
        assert_eq!(page.body, "arrived");
        assert_eq!(page.final_url, format!("http://{addr}/caf%C3%A9"));
    }

    /// Markdown quoting a `<meta charset>` in its text stays UTF-8.
    #[tokio::test]
    async fn a_meta_tag_in_markdown_text_does_not_change_the_charset() {
        let addr = serve(|_, _, _| {
            response(
                "200 OK",
                &[("Content-Type", "text/markdown")],
                "Old pages declare `<meta charset=\"iso-8859-1\">`. \u{e9}t\u{e9}".as_bytes(),
            )
        })
        .await;
        let page = get(addr, "/enc.md").await.unwrap();
        assert!(page.body.contains("\u{e9}t\u{e9}"), "{}", page.body);
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
        let page = get(addr, "/a").await.unwrap();
        assert_eq!(page.body, "arrived");
        assert_eq!(page.final_url, format!("http://{addr}/c"));
        let err = get(addr, "/loop").await.unwrap_err();
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
        let page = get(addr, "/big").await.unwrap();
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
        let page = get(addr, "/paid").await.unwrap();
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
