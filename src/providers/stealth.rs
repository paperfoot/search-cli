//! `stealth`: rung 1 of the extract chain, reached only when the direct
//! fetch was refused (a challenge, 401/403/429/503, no connection; see
//! `registry::rung_gate`). A local GET with a current Chrome TLS and HTTP/2
//! fingerprint (wreq). The emulation owns every header: no hand-set header
//! map and no invented referer. Redirects are followed by hand, at most 5.
//! The SSRF guard is wreq's own resolver (the one direct uses: it fails
//! closed and checks the very addresses the connection uses), plus a check
//! of IP-literal hosts on every hop. The body is capped at 5 MB, decoded
//! with the page's charset and judged like any other page.

use super::direct::{extract_page, unsupported_unless_extract};
use super::spec::{FilterSupport, Price};
use super::{Adapter, Call, Output, ProviderSpec};
use crate::context::AppContext;
use crate::errors::SearchError;
use crate::page::{self, LookupFuture, Page, MAX_BODY_BYTES, MAX_REDIRECTS};
use crate::types::{FailureCategory, Mode};
use async_trait::async_trait;
use http_body_util::BodyExt;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use url::Url;
use wreq::dns::{Addrs, Name, Resolve, Resolving};
use wreq::header::{CONTENT_TYPE, LOCATION};
use wreq_util::Profile;

/// The newest Chrome profile wreq-util 3.0.0-rc.14 ships.
const PROFILE: Profile = Profile::Chrome149;

pub static SPEC: ProviderSpec = ProviderSpec {
    name: "stealth",
    env_keys: &[],
    modes: &[Mode::Extract],
    filters: FilterSupport {
        freshness: false,
        domains: false,
        country: false,
        lang: false,
    },
    max_count: 1,
    timeout: Duration::from_secs(15),
    billed_on_send: false,
    price: Price {
        per_call: 0,
        per_extra_result: 0,
        included_results: 0,
    },
};

pub struct Stealth {
    _ctx: Arc<AppContext>,
}

impl Stealth {
    pub fn new(ctx: Arc<AppContext>) -> Self {
        Self { _ctx: ctx }
    }
}

/// A wreq client. With a `lookup`, names resolve through the guard
/// (`page::resolve_public`): wreq connects only to addresses that passed.
/// Without one (`--allow-private`), the system resolver and proxies apply.
fn build_client(lookup: Option<fn(String) -> LookupFuture>) -> Result<wreq::Client, SearchError> {
    let builder = wreq::Client::builder()
        .emulation(PROFILE)
        .redirect(wreq::redirect::Policy::none())
        .cookie_store(true)
        .connect_timeout(Duration::from_secs(5));
    let builder = match lookup {
        // A proxy resolves the name where the guard cannot see it.
        Some(lookup) => builder
            .no_proxy()
            .dns_resolver(Arc::new(PublicOnly { lookup })),
        None => builder,
    };
    builder.build().map_err(|e| network(&e))
}

/// wreq's side of the guarded resolver.
struct PublicOnly {
    lookup: fn(String) -> LookupFuture,
}

impl Resolve for PublicOnly {
    fn resolve(&self, name: Name) -> Resolving {
        let lookup = self.lookup;
        let host = name.as_str().to_string();
        Box::pin(async move {
            let addrs = page::resolve_public(&host, lookup).await?;
            Ok(Box::new(addrs.into_iter()) as Addrs)
        })
    }
}

fn network(e: &wreq::Error) -> SearchError {
    SearchError::Upstream {
        provider: "stealth",
        status: 0,
        category: if e.is_timeout() {
            FailureCategory::Timeout
        } else {
            FailureCategory::Network
        },
        message: page::error_chain(e),
    }
}

/// A failed send: the guard's refusal is invalid input (as in direct),
/// everything else a network failure or timeout.
fn send_error(e: wreq::Error) -> SearchError {
    match page::blocked_in(&e) {
        Some(message) => SearchError::InvalidInput { message },
        None => network(&e.without_uri()),
    }
}

/// GET `url`, following redirects by hand. `check` vets every hop's URL
/// before it is sent (IP-literal hosts; names are the resolver's job), or
/// nothing with `--allow-private`.
async fn fetch<F, Fut>(client: &wreq::Client, url: &str, check: F) -> Result<Page, SearchError>
where
    F: Fn(Url) -> Fut,
    Fut: Future<Output = Result<(), SearchError>>,
{
    let mut current = Url::parse(url).map_err(|e| SearchError::InvalidInput {
        message: format!("not a valid URL: {e}"),
    })?;
    let mut hops = 0;
    loop {
        current.set_fragment(None);
        if !matches!(current.scheme(), "http" | "https") {
            return Err(SearchError::InvalidInput {
                message: format!("{current}: only http and https are fetched"),
            });
        }
        check(current.clone()).await?;
        let resp = client
            .get(current.as_str())
            .send()
            .await
            .map_err(send_error)?;
        let status = resp.status();
        if status.is_redirection() && status.as_u16() != 304 {
            hops += 1;
            if hops > MAX_REDIRECTS {
                return Err(redirect_error(format!(
                    "more than {MAX_REDIRECTS} redirects"
                )));
            }
            let location = resp
                .headers()
                .get(LOCATION)
                .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
                .ok_or_else(|| redirect_error(format!("HTTP {status} without a Location")))?;
            current = current
                .join(location.trim())
                .map_err(|_| redirect_error(format!("HTTP {status} to an unusable Location")))?;
            continue;
        }

        let headers = resp.headers().clone();
        let content_type = headers
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string);
        let mut body = wreq::Body::from(resp);
        let mut raw: Vec<u8> = Vec::new();
        while let Some(frame) = body.frame().await {
            let frame = frame.map_err(|e| network(&e))?;
            let Ok(chunk) = frame.into_data() else {
                continue; // trailers
            };
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
            body: page::decode(&raw, content_type.as_deref()),
            content_type,
            headers,
        });
    }
}

fn redirect_error(message: String) -> SearchError {
    SearchError::Upstream {
        provider: "stealth",
        status: 0,
        category: FailureCategory::Network,
        message,
    }
}

/// The per-hop URL check of a guarded fetch: an IP-literal host is checked
/// here, because it never reaches the resolver. No separate DNS lookup: a
/// second answer could differ from the one wreq connects to.
async fn literal_hop(url: Url) -> Result<(), SearchError> {
    page::check_literal_host(&url)
        .await
        .map_err(|message| SearchError::InvalidInput { message })
}

#[async_trait]
impl Adapter for Stealth {
    fn spec(&self) -> &'static ProviderSpec {
        &SPEC
    }

    fn is_configured(&self) -> bool {
        true // keyless, runs locally
    }

    async fn call(&self, call: &Call<'_>) -> Result<Output, SearchError> {
        unsupported_unless_extract("stealth", call.mode)?;
        crate::registry::rung_gate("stealth", call.query)?;
        let allow_private = crate::guard::allow_private();
        let client = build_client((!allow_private).then_some(page::system_lookup as _))?;
        let fetched = if allow_private {
            tokio::time::timeout(
                call.timeout,
                fetch(&client, call.query, |_| async { Ok(()) }),
            )
            .await
        } else {
            tokio::time::timeout(call.timeout, fetch(&client, call.query, literal_hop)).await
        };
        let page = fetched.map_err(|_| super::timeout_error("stealth"))??;
        extract_page("stealth", page).await.map_err(|(_, e)| e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// A local server; `/hop` redirects to `/page`, `/evil` to a private
    /// address, `/page` serves an article.
    async fn serve() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]).to_string();
                    let path = req.split_whitespace().nth(1).unwrap_or("/").to_string();
                    let article = format!(
                        "<html><head><title>Hop target</title><meta charset=\"windows-1252\"></head><body><article><h1>Hop target</h1><p>{}</p><p>Caf\u{e9}</p></article></body></html>",
                        "A sentence of real article text for the reader. ".repeat(12)
                    );
                    let (head, body): (&str, Vec<u8>) = match path.as_str() {
                        "/hop" => ("HTTP/1.1 302 Found\r\nLocation: /page", Vec::new()),
                        "/evil" => (
                            "HTTP/1.1 301 Moved\r\nLocation: http://169.254.169.254/latest/meta-data/",
                            Vec::new(),
                        ),
                        "/page" => (
                            "HTTP/1.1 200 OK\r\nContent-Type: text/html",
                            encoding_rs::WINDOWS_1252.encode(&article).0.into_owned(),
                        ),
                        _ => ("HTTP/1.1 404 Not Found", Vec::new()),
                    };
                    let mut msg = format!(
                        "{head}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .into_bytes();
                    msg.extend_from_slice(&body);
                    let _ = sock.write_all(&msg).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        format!("http://{addr}")
    }

    /// The guard, with the one local test server treated as public.
    fn guard_except(
        base: &str,
    ) -> impl Fn(Url) -> std::pin::Pin<Box<dyn Future<Output = Result<(), SearchError>> + Send>>
    {
        let base = base.to_string();
        move |url: Url| {
            let local = url.as_str().starts_with(&base);
            Box::pin(async move {
                if local {
                    Ok(())
                } else {
                    literal_hop(url).await
                }
            })
        }
    }

    #[tokio::test]
    async fn follows_a_redirect_and_records_the_final_url() {
        let base = serve().await;
        let client = build_client(Some(page::system_lookup)).unwrap();
        let page = fetch(&client, &format!("{base}/hop#frag"), guard_except(&base))
            .await
            .unwrap();
        assert_eq!(page.final_url, format!("{base}/page"));
        assert!(page.body.contains("Café"), "charset decoded from <meta>");
        let out = extract_page("stealth", page)
            .await
            .map_err(|(_, e)| e)
            .unwrap();
        assert_eq!(out.results[0].title, "Hop target");
        assert_eq!(out.results[0].url, format!("{base}/page"));
    }

    #[tokio::test]
    async fn refuses_a_redirect_to_a_private_address() {
        let base = serve().await;
        let client = build_client(Some(page::system_lookup)).unwrap();
        let err = fetch(&client, &format!("{base}/evil"), guard_except(&base))
            .await
            .unwrap_err();
        assert!(
            matches!(err, SearchError::InvalidInput { .. }),
            "metadata hop must be blocked: {err}"
        );
    }

    #[tokio::test]
    async fn a_failed_connection_is_a_network_error_not_config() {
        let client = build_client(Some(page::system_lookup)).unwrap();
        // Port 9 on loopback: nothing listens there.
        let err = fetch(&client, "http://127.0.0.1:9/", |_| async { Ok(()) })
            .await
            .unwrap_err();
        assert_eq!(err.category(), FailureCategory::Network, "{err}");
    }

    /// A local server that counts connections and serves "internal secrets".
    async fn secret_server() -> (u16, Arc<std::sync::atomic::AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = hits.clone();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let _ = sock.read(&mut buf).await;
                    let body = "internal secrets ".repeat(40);
                    let msg = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = sock.write_all(msg.as_bytes()).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        (port, hits)
    }

    fn loopback(_: String) -> LookupFuture {
        Box::pin(async { Ok(vec![std::net::SocketAddr::from(([127, 0, 0, 1], 0))]) })
    }

    /// Review (wave 2): the guard must judge the address wreq connects to.
    /// A rebinding name answers public to the first lookup and loopback to
    /// every later one; the old pre-flight lookup took the first answer and
    /// wreq connected to the second. Now the one lookup is wreq's own.
    #[tokio::test]
    async fn a_rebinding_name_never_reaches_the_private_address() {
        static LOOKUPS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        fn rebinding(_: String) -> LookupFuture {
            let n = LOOKUPS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async move {
                let ip = if n == 0 {
                    [192, 0, 2, 1]
                } else {
                    [127, 0, 0, 1]
                };
                Ok(vec![std::net::SocketAddr::from((ip, 0))])
            })
        }
        let (port, hits) = secret_server().await;
        let client = build_client(Some(rebinding)).unwrap();
        let url = format!("http://rebind.test:{port}/latest/meta-data/");
        let got =
            tokio::time::timeout(Duration::from_secs(2), fetch(&client, &url, literal_hop)).await;
        if let Ok(Ok(page)) = &got {
            panic!("rebinding reached a private address: {}", page.body);
        }
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_name_resolving_to_loopback_is_blocked_at_connect() {
        let (port, hits) = secret_server().await;
        let client = build_client(Some(loopback)).unwrap();
        let err = fetch(
            &client,
            &format!("http://admin.example:{port}/"),
            literal_hop,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, SearchError::InvalidInput { .. }), "{err}");
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn one_private_address_in_the_answer_blocks_the_name() {
        fn mixed(_: String) -> LookupFuture {
            Box::pin(async {
                Ok(vec![
                    std::net::SocketAddr::from(([93, 184, 215, 14], 0)),
                    std::net::SocketAddr::from(([169, 254, 169, 254], 0)),
                ])
            })
        }
        let client = build_client(Some(mixed)).unwrap();
        let err = fetch(&client, "http://mixed.example/", literal_hop)
            .await
            .unwrap_err();
        assert!(matches!(err, SearchError::InvalidInput { .. }), "{err}");
    }

    /// A lookup that errors, hangs or answers nothing is never let through
    /// (the old guard passed any lookup failure).
    #[tokio::test]
    async fn lookup_failures_fail_closed() {
        fn failing(_: String) -> LookupFuture {
            Box::pin(async { Err(std::io::Error::other("SERVFAIL")) })
        }
        fn empty(_: String) -> LookupFuture {
            Box::pin(async { Ok(vec![]) })
        }
        for lookup in [failing as fn(String) -> LookupFuture, empty] {
            let client = build_client(Some(lookup)).unwrap();
            let err = fetch(&client, "http://review-nx-20261102.invalid/", literal_hop)
                .await
                .unwrap_err();
            assert_eq!(err.category(), FailureCategory::Network, "{err}");
        }
    }

    #[tokio::test]
    async fn an_ip_literal_hop_is_checked_without_dns() {
        let client = build_client(Some(page::system_lookup)).unwrap();
        let err = fetch(&client, "http://127.0.0.1:9/admin", literal_hop)
            .await
            .unwrap_err();
        assert!(matches!(err, SearchError::InvalidInput { .. }), "{err}");
    }
}
