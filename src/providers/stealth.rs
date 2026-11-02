//! `stealth`: rung 1 of the extract chain, reached only when the direct
//! fetch was refused (a challenge, 401/403/429/503, no connection; see
//! `registry::rung_gate`). A local GET with a current Chrome TLS and HTTP/2
//! fingerprint (wreq). The emulation owns every header: no hand-set header
//! map and no invented referer. Redirects are followed by hand, at most 5,
//! with the SSRF guard on every hop; the body is capped at 5 MB, decoded
//! with the page's charset and judged like any other page.

use super::direct::{extract_page, unsupported_unless_extract};
use super::spec::{FilterSupport, Price};
use super::{Adapter, Call, Output, ProviderSpec};
use crate::context::AppContext;
use crate::errors::SearchError;
use crate::page::{self, Page, MAX_BODY_BYTES, MAX_REDIRECTS};
use crate::types::{FailureCategory, Mode};
use async_trait::async_trait;
use http_body_util::BodyExt;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use url::Url;
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

fn build_client(guarded: bool) -> Result<wreq::Client, SearchError> {
    let builder = wreq::Client::builder()
        .emulation(PROFILE)
        .redirect(wreq::redirect::Policy::none())
        .cookie_store(true)
        .connect_timeout(Duration::from_secs(5));
    // A proxy resolves the name where the per-hop guard cannot see it.
    let builder = if guarded { builder.no_proxy() } else { builder };
    builder.build().map_err(|e| network(&e))
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

/// GET `url`, following redirects by hand. `check` vets every hop before it
/// is sent (the SSRF guard, or nothing with `--allow-private`).
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
            .map_err(|e| network(&e.without_uri()))?;
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

async fn guard_hop(url: Url) -> Result<(), SearchError> {
    crate::guard::assert_public_url(url.as_str()).await
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
        let client = build_client(!allow_private)?;
        let fetched = if allow_private {
            tokio::time::timeout(
                call.timeout,
                fetch(&client, call.query, |_| async { Ok(()) }),
            )
            .await
        } else {
            tokio::time::timeout(call.timeout, fetch(&client, call.query, guard_hop)).await
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
                    crate::guard::assert_public_url(url.as_str()).await
                }
            })
        }
    }

    #[tokio::test]
    async fn follows_a_redirect_and_records_the_final_url() {
        let base = serve().await;
        let client = build_client(true).unwrap();
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
        let client = build_client(true).unwrap();
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
        let client = build_client(true).unwrap();
        // Port 9 on loopback: nothing listens there.
        let err = fetch(&client, "http://127.0.0.1:9/", |_| async { Ok(()) })
            .await
            .unwrap_err();
        assert_eq!(err.category(), FailureCategory::Network, "{err}");
    }
}
