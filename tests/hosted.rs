//! Hosted transport against a mock of the metasearch.sh v1 contract
//! (briefs/contracts/server/api-v1.ts): a tokio TcpListener speaking just
//! enough HTTP/1.1, and a temp HOME holding nothing but a token.

use assert_cmd::Command;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const TOKEN: &str = "ms_live_mocktoken000001";

#[derive(Clone, Debug)]
struct Req {
    method: String,
    path: String,
    headers: HashMap<String, String>,
    body: Value,
}

enum Reply {
    Json(u16, Value),
    /// Drop the connection without answering.
    Close,
}

type Handler = Arc<dyn Fn(&Req, usize) -> Reply + Send + Sync>;

struct Mock {
    base: String,
    log: Arc<Mutex<Vec<Req>>>,
    home: PathBuf,
}

impl Mock {
    fn requests(&self, path: &str) -> Vec<Req> {
        self.log
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.path == path)
            .cloned()
            .collect()
    }

    /// The CLI with only this mock and a token: no provider keys, no logs.
    fn cmd(&self) -> Command {
        let mut c = Command::cargo_bin("search").unwrap();
        c.env_clear()
            .env("HOME", &self.home)
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("METASEARCH_URL", &self.base)
            .env("METASEARCH_TOKEN", TOKEN)
            .env("METASEARCH_ALLOW_INSECURE_HOST", "1")
            .env("SEARCH_LOG", "off");
        c
    }
}

impl Drop for Mock {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

fn start(name: &str, handler: Handler) -> Mock {
    let log: Arc<Mutex<Vec<Req>>> = Arc::new(Mutex::new(Vec::new()));
    let (tx, rx) = std::sync::mpsc::channel();
    let server_log = log.clone();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            tx.send(listener.local_addr().unwrap()).unwrap();
            loop {
                let Ok((sock, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(serve(sock, handler.clone(), server_log.clone()));
            }
        });
    });
    let addr = rx.recv().unwrap();
    let home = std::env::temp_dir().join(format!("search-hosted-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    Mock {
        base: format!("http://{addr}"),
        log,
        home,
    }
}

async fn serve(mut sock: tokio::net::TcpStream, handler: Handler, log: Arc<Mutex<Vec<Req>>>) {
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 16384];
    let head_end = loop {
        match sock.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i;
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let mut lines = head.split("\r\n");
    let mut first = lines.next().unwrap_or("").split(' ');
    let method = first.next().unwrap_or("").to_string();
    let path = first.next().unwrap_or("").to_string();
    let headers: HashMap<String, String> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    let len: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    while buf.len() < head_end + 4 + len {
        match sock.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
    let body = serde_json::from_slice(&buf[head_end + 4..]).unwrap_or(Value::Null);
    let req = Req {
        method,
        path,
        headers,
        body,
    };
    let index = {
        let mut l = log.lock().unwrap();
        l.push(req.clone());
        l.iter().filter(|r| r.path == req.path).count() - 1
    };
    match handler(&req, index) {
        Reply::Close => {}
        Reply::Json(status, v) => {
            let body = v.to_string();
            let head = format!(
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = sock.write_all(head.as_bytes()).await;
            let _ = sock.write_all(body.as_bytes()).await;
            let _ = sock.shutdown().await;
        }
    }
}

const GENERAL: &[&str] = &[
    "serper",
    "exa",
    "jina",
    "brave",
    "linkup",
    "parallel",
    "tavily",
    "perplexity",
];

fn manifest() -> Value {
    let entry = |name: &str| {
        let filters = if name == "exa" {
            json!(["include_domains", "exclude_domains"])
        } else {
            json!([
                "freshness",
                "include_domains",
                "exclude_domains",
                "country",
                "language"
            ])
        };
        json!({ "name": name, "available": true, "filters": filters, "max_count": 20,
                "price_usd": 0.0015, "max_usd": 0.01, "usage_priced": false })
    };
    json!({
        "version": "1",
        "limits": { "max_count": 20 },
        "modes": [
            { "mode": "general", "deadline_ms": 20000, "providers": GENERAL.iter().map(|n| entry(n)).collect::<Vec<_>>() },
            { "mode": "academic", "deadline_ms": 20000, "providers": [entry("exa"), entry("serper")] },
            { "mode": "social", "deadline_ms": 65000, "providers": [entry("xai"), entry("tiktok"), entry("instagram"), entry("youtube")] },
            { "mode": "places", "deadline_ms": 20000, "providers": [entry("serper")] },
            { "mode": "images", "deadline_ms": 20000, "providers": [entry("serper")] },
        ],
    })
}

/// A 200 the way the server answers: one pinned provider gives three
/// results unique to it; several are fused, each charged; none is routed to
/// the cheapest (serper, a chain) and, unless `rerank: false`, reranked.
fn search_ok(req: &Req) -> Reply {
    let pinned: Vec<String> = req.body["providers"]
        .as_array()
        .map(|a| a.iter().map(|p| p.as_str().unwrap().to_string()).collect())
        .unwrap_or_default();
    let routed = pinned.is_empty();
    let engines = if routed {
        vec!["serper".to_string()]
    } else {
        pinned.clone()
    };
    let mut results: Vec<Value> = Vec::new();
    for p in &engines {
        for i in 0..3 {
            results.push(json!({ "title": format!("{p} result {i}"), "url": format!("https://{p}.example/{i}"),
                "snippet": "about rust", "imageUrl": if i == 0 { json!("https://img.example/a.png") } else { Value::Null },
                "sources": [{ "provider": p, "rank": i + 1 }] }));
        }
    }
    let attempts: Vec<Value> = engines
        .iter()
        .map(|p| json!({ "provider": p, "outcome": "ok", "latency_ms": 100, "charged_micros": 1500 }))
        .collect();
    let charged = 1500 * engines.len() as u64;
    let mut routing = json!({ "merge": if engines.len() > 1 { "fused" } else if routed { "chain" } else { "single" },
        "served_by": engines[0], "providers": engines, "fell_back": false, "attempts": attempts });
    if (routed || engines.len() > 1) && req.body["rerank"] != json!(false) {
        routing["reranked"] = json!(true);
    }
    Reply::Json(
        200,
        json!({
            "request_id": "r", "mode": req.body["mode"], "provider": engines[0], "results": results,
            "routing": routing,
            "billing": { "reserved_micros": 3000, "charged_micros": charged, "balance_micros": 998500,
                         "charged_usd": charged as f64 / 1e6, "balance_usd": 0.9985 },
        }),
    )
}

/// Manifest, a model route to general, a reversing rerank, and `search` from `on_search`.
fn handler(on_search: impl Fn(&Req, usize) -> Reply + Send + Sync + 'static) -> Handler {
    Arc::new(move |req, i| match req.path.as_str() {
        "/api/v1/manifest" => Reply::Json(200, manifest()),
        "/api/v1/route" => Reply::Json(
            200,
            json!({ "auto": { "mode": "general", "source": "model", "reason": "Anything else (p 0.81); used general.",
                              "probabilities": { "general": 0.81 }, "latency_ms": 300 },
                    "providers": GENERAL }),
        ),
        "/api/v1/rerank" => {
            let ids: Vec<String> = req.body["results"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| r["id"].as_str().unwrap().to_string())
                .collect();
            let relevance: serde_json::Map<String, Value> = ids
                .iter()
                .enumerate()
                .map(|(n, id)| (id.clone(), json!(0.1 + n as f64 * 0.05)))
                .collect();
            let order: Vec<String> = ids.into_iter().rev().collect();
            Reply::Json(
                200,
                json!({ "order": order, "relevance": relevance, "latency_ms": 200 }),
            )
        }
        "/api/v1/search" => on_search(req, i),
        "/api/v1/me" if req.method == "GET" => Reply::Json(
            200,
            json!({ "email": "agent@example.com", "balance_usd": 4.2, "balance_micros": 4200000,
                    "key": { "id": "k", "name": "laptop", "prefix": "ms_live_mock", "created_at": "x",
                             "last_used_at": null, "daily_cap_usd": null, "spent_today_usd": 0.0 } }),
        ),
        "/api/v1/me" => Reply::Json(200, json!({ "revoked": true })),
        _ => Reply::Json(404, json!({ "error": "not_found" })),
    })
}

fn run_json(cmd: &mut Command) -> (i32, Value) {
    let out = cmd.output().unwrap();
    let v: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|_| {
        panic!(
            "stdout is not one JSON document: {}\nstderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    });
    (out.status.code().unwrap_or(-1), v)
}

#[test]
fn request_shape_headers_and_listed_filters_only() {
    let mock = start("shape", handler(|r, _| search_ok(r)));
    let (code, v) = run_json(mock.cmd().args([
        "search",
        "-q",
        "rust async",
        "-m",
        "general",
        "-p",
        "exa",
        "-f",
        "week",
        "-d",
        "a.com",
        "--country",
        "gb",
        "--json",
    ]));
    assert_eq!(code, 0, "{v}");
    let reqs = mock.requests("/api/v1/search");
    assert_eq!(reqs.len(), 1);
    let r = &reqs[0];
    assert_eq!(r.method, "POST");
    assert_eq!(r.body["query"], "rust async");
    assert_eq!(r.body["mode"], "general");
    assert_eq!(r.body["providers"], json!(["exa"]));
    assert_eq!(r.body["count"], 10);
    // exa lists only domain filters: freshness and country are not sent, and
    // the envelope says so.
    assert_eq!(r.body["filters"], json!({ "include_domains": ["a.com"] }));
    assert_eq!(
        v["metadata"]["warnings"],
        json!(["exa does not apply -f freshness or --country; its results are unfiltered"])
    );
    assert_eq!(r.headers["authorization"], format!("Bearer {TOKEN}"));
    assert!(r.headers["user-agent"].starts_with("search-cli/"));
    let key = &r.headers["idempotency-key"];
    assert!(uuid_like(key), "idempotency key {key:?}");
    // Imageurl maps to image_url; transport and charged cost are reported.
    assert_eq!(v["results"][0]["image_url"], "https://img.example/a.png");
    assert_eq!(v["metadata"]["transport"]["exa"], "hosted");
    assert_eq!(v["metadata"]["cost"]["basis"], "charged");
    assert_eq!(v["metadata"]["cost"]["by_provider"]["exa"], 1500);
    // An explicit -m never asks /route.
    assert!(mock.requests("/api/v1/route").is_empty());
}

fn uuid_like(s: &str) -> bool {
    s.len() == 36 && s.chars().filter(|c| *c == '-').count() == 4
}

#[test]
fn idempotency_key_reused_on_retry_and_fresh_per_call() {
    // The first attempt dies before any answer; the retry must carry the
    // same key so the server can replay instead of charging twice.
    let mock = start(
        "idem",
        handler(|r, i| if i == 0 { Reply::Close } else { search_ok(r) }),
    );
    let args = [
        "search",
        "-q",
        "rust",
        "-m",
        "general",
        "-p",
        "serper",
        "--json",
        "--no-cache",
    ];
    let (code, v) = run_json(mock.cmd().args(args));
    assert_eq!(code, 0, "{v}");
    let reqs = mock.requests("/api/v1/search");
    assert_eq!(reqs.len(), 2, "one retry");
    assert_eq!(
        reqs[0].headers["idempotency-key"],
        reqs[1].headers["idempotency-key"]
    );
    let (code, _) = run_json(mock.cmd().args(args));
    assert_eq!(code, 0);
    let reqs = mock.requests("/api/v1/search");
    assert_eq!(reqs.len(), 3);
    assert_ne!(
        reqs[2].headers["idempotency-key"],
        reqs[0].headers["idempotency-key"]
    );
}

fn failing(status: u16, body: Value) -> Mock {
    start(
        &format!("err{status}"),
        handler(move |_, _| Reply::Json(status, body.clone())),
    )
}

fn pinned(mock: &Mock) -> (i32, Value) {
    run_json(mock.cmd().args([
        "search", "-q", "rust", "-m", "general", "-p", "serper", "--json",
    ]))
}

#[test]
fn errors_map_to_categories_and_exit_codes() {
    let m = failing(
        401,
        json!({ "error": "invalid_api_key", "message": "unknown key", "retryable": false }),
    );
    let (code, v) = pinned(&m);
    assert_eq!(code, 2, "{v}");
    let f = &v["error"]["provider_failures"][0];
    assert_eq!(f["category"], "auth");
    assert!(f["reason"].as_str().unwrap().contains("search login"));
    assert!(!v.to_string().contains(TOKEN), "token leaked: {v}");

    let m = failing(
        402,
        json!({ "error": "insufficient_credits", "message": "top up", "retryable": false,
                "required_usd": 0.003, "balance_usd": 0.001, "top_up_url": "https://metasearch.sh/dashboard/billing" }),
    );
    let (code, v) = pinned(&m);
    assert_eq!(code, 2, "402 is exit 2: the user must top up");
    let f = &v["error"]["provider_failures"][0];
    assert_eq!(f["category"], "billing_quota");
    assert!(f["reason"]
        .as_str()
        .unwrap()
        .contains("https://metasearch.sh/dashboard/billing"));
    assert!(f["reason"].as_str().unwrap().contains("$0.0010"));

    let m = failing(
        429,
        json!({ "error": "rate_limited", "message": "slow", "retryable": true, "reason": "requests", "limit": 300 }),
    );
    let (code, v) = pinned(&m);
    assert_eq!(code, 4, "{v}");
    assert_eq!(v["error"]["provider_failures"][0]["category"], "rate_limit");

    let m = failing(
        502,
        json!({ "error": "provider_error", "message": "upstream down", "retryable": true,
                "attempts": [{ "provider": "serper", "outcome": "failed", "category": "timeout", "latency_ms": 9000, "charged_micros": 0 }] }),
    );
    let (code, v) = pinned(&m);
    assert_eq!(code, 1, "{v}");
    assert_eq!(v["error"]["provider_failures"][0]["category"], "timeout");
}

#[test]
fn provider_not_available_is_server_and_drops_the_manifest_cache() {
    let m = failing(
        503,
        json!({ "error": "provider_not_available", "message": "off", "retryable": false, "provider": "serper", "reason": "disabled" }),
    );
    let (code, v) = pinned(&m);
    assert_eq!(code, 1, "{v}");
    assert_eq!(v["error"]["provider_failures"][0]["category"], "server");
    pinned(&m);
    // Cached for ten minutes otherwise: the second run had to fetch it again.
    assert_eq!(m.requests("/api/v1/manifest").len(), 2);
}

/// The cached manifest under the mock's HOME, wherever the platform keeps caches.
fn manifest_cache(dir: &std::path::Path) -> Option<PathBuf> {
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let p = entry.path();
        if p.is_dir() {
            if let Some(found) = manifest_cache(&p) {
                return Some(found);
            }
        } else if p.file_name().is_some_and(|n| n == "manifest.json") {
            return Some(p);
        }
    }
    None
}

/// A failed manifest refresh keeps the stale copy and backs off for a
/// minute, so a metasearch.sh outage costs one wait, not one per search.
#[test]
fn failed_manifest_refresh_backs_off_instead_of_blocking_every_search() {
    let served = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let s = served.clone();
    let mock = start(
        "manifestbackoff",
        Arc::new(move |req: &Req, i| match req.path.as_str() {
            "/api/v1/manifest" => {
                if s.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                    Reply::Json(200, manifest())
                } else {
                    Reply::Json(500, json!({ "error": "internal_error" }))
                }
            }
            _ => handler(|r, _| search_ok(r))(req, i),
        }),
    );
    let (code, v) = pinned(&mock);
    assert_eq!(code, 0, "{v}");
    let path = manifest_cache(&mock.home).expect("manifest cached");
    let mut cached: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    cached["fetched_at"] = json!(0); // stale
    std::fs::write(&path, serde_json::to_vec(&cached).unwrap()).unwrap();
    let (code, v) = pinned(&mock);
    assert_eq!(code, 0, "the stale copy still serves: {v}");
    assert_eq!(mock.requests("/api/v1/manifest").len(), 2);
    pinned(&mock);
    assert_eq!(
        mock.requests("/api/v1/manifest").len(),
        2,
        "no refetch within the minute"
    );
}

/// No keys and no -p: ONE hosted call that names no engine, so the server
/// routes it (here to serper), reranks it, and the CLI does not rerank again.
#[test]
fn zero_keys_auto_general_is_one_routed_call() {
    let mock = start("zero", handler(|r, _| search_ok(r)));
    let (code, v) =
        run_json(
            mock.cmd()
                .args(["search", "-q", "rust async runtime", "-c", "5", "--json"]),
        );
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["status"], "success");
    assert_eq!(v["mode"], "general");
    let m = &v["metadata"];
    assert_eq!(m["cost"]["basis"], "charged");
    assert_eq!(m["cost"]["micros"], 1500);
    assert_eq!(m["cost"]["by_provider"], json!({ "serper": 1500 }));
    assert_eq!(m["providers_queried"], json!(["serper"]));
    assert_eq!(m["transport"], json!({ "serper": "hosted" }));
    let sent = mock.requests("/api/v1/search");
    assert_eq!(sent.len(), 1, "one hosted call per search");
    assert!(sent[0].body.get("providers").is_none(), "{}", sent[0].body);
    assert!(sent[0].body.get("rerank").is_none() && sent[0].body.get("fresh").is_none());
    assert!(m["warnings"].is_null(), "{}", m["warnings"]);
    // Model routing reported, with no model id.
    assert_eq!(m["routing"]["auto"]["source"], "model");
    assert_eq!(m["routing"]["auto"]["mode"], "general");
    assert!(m["routing"]["auto"].get("model").is_none());
    assert_eq!(mock.requests("/api/v1/route").len(), 1);
    // The server reranked; the CLI did not ask again.
    assert_eq!(m["routing"]["reranked"], true);
    assert!(mock.requests("/api/v1/rerank").is_empty());
    assert_eq!(v["results"][0]["source"], "serper");
    assert!(m["notes"].is_null(), "no login note when logged in");
}

/// `--no-cache` asks for fresh results; a cache hit and an escalation are
/// read back: cached in routing, the set-aside engine charged nothing.
#[test]
fn no_cache_sends_fresh_and_cached_and_escalated_answers_are_read() {
    let mock = start(
        "cachehit",
        handler(|r, _| {
            if r.body["fresh"] == json!(true) {
                return Reply::Json(
                    200,
                    json!({
                    "request_id": "r", "mode": "academic", "provider": "exa",
                    "results": [{ "title": "t", "url": "https://paper.example/1", "snippet": "s" }],
                    "routing": { "merge": "chain", "served_by": "exa", "providers": ["exa"], "fell_back": true,
                        "attempts": [
                            { "provider": "serper", "outcome": "ok", "latency_ms": 300, "charged_micros": 0, "escalated": true },
                            { "provider": "exa", "outcome": "ok", "latency_ms": 900, "charged_micros": 10500 }] },
                    "billing": { "reserved_micros": 10500, "charged_micros": 10500, "balance_micros": 1, "charged_usd": 0.0105, "balance_usd": 0.0 } }),
                );
            }
            Reply::Json(
                200,
                json!({
                "request_id": "r", "mode": "academic", "provider": "serper",
                "results": [{ "title": "t", "url": "https://paper.example/1", "snippet": "s" }],
                "routing": { "merge": "chain", "served_by": "serper", "providers": ["serper"], "fell_back": false,
                    "attempts": [], "cached": true, "cached_at": "2026-11-04T02:00:00.000Z" },
                "billing": { "reserved_micros": 1500, "charged_micros": 1500, "balance_micros": 1, "charged_usd": 0.0015, "balance_usd": 0.0 } }),
            )
        }),
    );
    let args = [
        "search",
        "-q",
        "crispr off-target",
        "-m",
        "academic",
        "--json",
    ];
    let (code, v) = run_json(mock.cmd().args(args));
    assert_eq!(code, 0, "{v}");
    let m = &v["metadata"];
    assert_eq!(m["routing"]["cached"], true, "{m}");
    assert_eq!(m["routing"]["cached_at"], "2026-11-04T02:00:00.000Z");
    assert!(m["routing"]["cache_age_secs"].is_number());
    assert_eq!(m["cost"]["by_provider"], json!({ "serper": 1500 }));
    // Not the local replay: that one is free and drops the cost.
    assert!(m["cached"].is_null());

    let (code, v) = run_json(mock.cmd().args(args).arg("--no-cache"));
    assert_eq!(code, 0, "{v}");
    let body = mock.requests("/api/v1/search").last().unwrap().body.clone();
    assert_eq!(body["fresh"], true, "{body}");
    assert!(body.get("providers").is_none());
    let m = &v["metadata"];
    assert!(m["routing"]["cached"].is_null());
    assert_eq!(m["routing"]["escalated"], json!(["serper"]));
    assert_eq!(
        m["cost"]["by_provider"],
        json!({ "exa": 10500, "serper": 0 })
    );
    assert_eq!(m["cost"]["micros"], 10500);
    assert_eq!(m["providers_queried"], json!(["serper", "exa"]));
    assert_eq!(m["provider_results"], json!({ "exa": 1 }));
}

/// A server that predates `fresh` refuses it with a 400; --no-cache asks
/// again without it rather than failing the search.
#[test]
fn no_cache_against_an_older_server_retries_without_fresh() {
    let mock = start(
        "oldfresh",
        handler(|r, _| {
            if r.body.get("fresh").is_some() {
                return Reply::Json(
                    400,
                    json!({ "error": "invalid_request",
                    "message": "body: Unrecognized key: \"fresh\"" }),
                );
            }
            search_ok(r)
        }),
    );
    let (code, v) = run_json(mock.cmd().args([
        "search",
        "-q",
        "rust",
        "-m",
        "general",
        "--no-cache",
        "--json",
    ]));
    assert_eq!(code, 0, "{v}");
    let sent = mock.requests("/api/v1/search");
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0].body["fresh"], true);
    assert!(sent[1].body.get("fresh").is_none());
}

/// Social with no cue: one routed call, the server picks the platforms.
#[test]
fn social_without_a_cue_is_left_to_the_server() {
    let mock = start("social", handler(|r, _| search_ok(r)));
    let (code, v) = run_json(mock.cmd().args([
        "search",
        "-q",
        "mechanical keyboard",
        "-m",
        "social",
        "--json",
    ]));
    assert_eq!(code, 0, "{v}");
    let sent = mock.requests("/api/v1/search");
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].body["mode"], "social");
    assert!(sent[0].body.get("providers").is_none(), "{}", sent[0].body);
}

#[test]
fn rerank_failure_keeps_the_fused_order() {
    let mock = start(
        "rerankfail",
        Arc::new(|req: &Req, i| match req.path.as_str() {
            "/api/v1/rerank" => Reply::Json(500, json!({ "error": "internal_error" })),
            _ => handler(|r, _| search_ok(r))(req, i),
        }),
    );
    // The CLI reranks only what it fused itself: an own key beside the hosted call.
    let (code, v) = run_json(mixed(&mock).args(["--json"]));
    assert_eq!(code, 0, "{v}");
    assert_eq!(mock.requests("/api/v1/rerank").len(), 1);
    assert_eq!(v["metadata"]["routing"]["reranked"], false);
    assert!(v["results"][0]["extra"]["relevance"].is_null());
    // --no-rerank never asks, and tells the server not to either.
    let before = mock.requests("/api/v1/rerank").len();
    let (_, v) = run_json(mixed(&mock).args(["--json", "--no-cache", "--no-rerank"]));
    assert_eq!(mock.requests("/api/v1/rerank").len(), before);
    assert!(v["metadata"]["routing"].is_null());
    let body = mock.requests("/api/v1/search").last().unwrap().body.clone();
    assert_eq!(body["rerank"], false, "{body}");
}

/// A general search with an own key for brave (unreachable here) beside the
/// hosted call, so the CLI fuses and reranks.
fn mixed(mock: &Mock) -> Command {
    let mut c = mock.cmd();
    c.env("BRAVE_API_KEY", "not-a-real-key-for-tests")
        .args(["search", "-q", "rust", "-m", "general", "-c", "20"]);
    c
}

/// The server's own fail-open answers 200 with the input order and no
/// scores: that is not a rerank.
#[test]
fn rerank_without_scores_is_not_a_rerank() {
    let mock = start(
        "rerankopen",
        Arc::new(|req: &Req, i| match req.path.as_str() {
            "/api/v1/rerank" => {
                let ids: Vec<Value> = req.body["results"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|r| r["id"].clone())
                    .rev()
                    .collect();
                Reply::Json(200, json!({ "order": ids, "relevance": {}, "model": null }))
            }
            _ => handler(|r, _| search_ok(r))(req, i),
        }),
    );
    let (code, v) = run_json(mixed(&mock).args(["--json"]));
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["metadata"]["routing"]["reranked"], false);
    assert!(v["results"][0]["extra"]["relevance"].is_null());
}

/// An older server named its routing model in `source` and sent a model id:
/// the envelope says "model" and carries no id.
#[test]
fn an_older_servers_route_answer_reads_as_model_without_an_id() {
    let mock = start(
        "routeold",
        Arc::new(|req: &Req, i| match req.path.as_str() {
            "/api/v1/route" => Reply::Json(
                200,
                json!({ "auto": { "mode": "general", "source": "vendor-x", "reason": "Anything else; used general.",
                                  "model": "vendor-x-1.13.0", "latency_ms": 300 } }),
            ),
            _ => handler(|r, _| search_ok(r))(req, i),
        }),
    );
    let out = mock
        .cmd()
        .args([
            "search",
            "-q",
            "rust async runtime",
            "--json",
            "--no-rerank",
        ])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    let v: Value = serde_json::from_str(&text).unwrap();
    let auto = &v["metadata"]["routing"]["auto"];
    assert_eq!(auto["source"], "model", "{auto}");
    assert!(auto.get("model").is_none(), "{auto}");
    assert!(!text.contains("vendor-x"), "{text}");
}

#[test]
fn route_failure_falls_back_to_local_signal_rules() {
    let mock = start(
        "routefail",
        Arc::new(|req: &Req, i| match req.path.as_str() {
            "/api/v1/route" => Reply::Json(500, json!({ "error": "internal_error" })),
            _ => handler(|r, _| search_ok(r))(req, i),
        }),
    );
    let (code, v) = run_json(
        mock.cmd()
            .args(["search", "-q", "coffee near me", "--json"]),
    );
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["mode"], "places");
    assert_eq!(v["metadata"]["routing"]["auto"]["source"], "heuristic");
    assert_eq!(mock.requests("/api/v1/route").len(), 1);
    // A decisive rule never asks /route.
    let (_, v) = run_json(
        mock.cmd()
            .args(["search", "-q", "images of cats", "--json"]),
    );
    assert_eq!(v["metadata"]["routing"]["auto"]["source"], "rule");
    assert_eq!(mock.requests("/api/v1/route").len(), 1);
}

/// routing.auto reports only the filter the CLI sent: a freshness /route
/// suggested is dropped (the caller's -f, or a mode that takes no freshness
/// word, wins), and the reason no longer names it.
#[test]
fn routing_auto_reports_only_the_freshness_that_was_sent() {
    let mock = start(
        "routefresh",
        Arc::new(|req: &Req, i| match req.path.as_str() {
            "/api/v1/route" => Reply::Json(
                200,
                json!({ "auto": { "mode": "general", "source": "model",
                                  "reason": "Anything else (p 0.9); used general with freshness week.",
                                  "filters_applied": { "freshness": "week" } } }),
            ),
            _ => handler(|r, _| search_ok(r))(req, i),
        }),
    );
    for (extra, sent) in [(None, Value::Null), (Some("month"), json!("month"))] {
        let mut cmd = mock.cmd();
        cmd.args([
            "search",
            "-q",
            "latest rust release",
            "-p",
            "serper",
            "--json",
            "--no-rerank",
        ]);
        if let Some(f) = extra {
            cmd.args(["-f", f]);
        }
        let (code, v) = run_json(&mut cmd);
        assert_eq!(code, 0, "{v}");
        let auto = &v["metadata"]["routing"]["auto"];
        assert!(
            auto.get("filters_applied").is_none_or(Value::is_null),
            "{auto}"
        );
        assert!(
            !auto["reason"].as_str().unwrap().contains("freshness"),
            "{auto}"
        );
        let body = mock.requests("/api/v1/search").last().unwrap().body.clone();
        assert_eq!(body["filters"]["freshness"], sent, "{body}");
    }
}

#[test]
fn auto_never_picks_a_mode_the_pinned_provider_cannot_serve() {
    // /route says news; exa serves no news, so auto falls back to general
    // instead of failing a search that ran before auto existed.
    let mock = start(
        "pinnedauto",
        Arc::new(|req: &Req, i| match req.path.as_str() {
            "/api/v1/route" => Reply::Json(
                200,
                json!({ "auto": { "mode": "news", "source": "model", "reason": "News (p 0.9); used news." } }),
            ),
            _ => handler(|r, _| search_ok(r))(req, i),
        }),
    );
    let (code, v) = run_json(mock.cmd().args([
        "search",
        "-q",
        "latest x",
        "-p",
        "exa",
        "--json",
        "--no-rerank",
    ]));
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["mode"], "general");
    let auto = &v["metadata"]["routing"]["auto"];
    assert_eq!(auto["source"], "fallback", "{auto}");
    assert_eq!(auto["mode"], "general");
    assert!(
        auto["reason"]
            .as_str()
            .unwrap()
            .starts_with("exa cannot serve news"),
        "{auto}"
    );
    let reqs = mock.requests("/api/v1/search");
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].body["mode"], "general");
    assert_eq!(reqs[0].body["providers"], json!(["exa"]));
}

/// -p goes as given, in one call (the server fans out and fuses).
#[test]
fn p_is_sent_as_given_in_one_call() {
    let mock = start("plift", handler(|r, _| search_ok(r)));
    let (code, v) = run_json(mock.cmd().args([
        "search",
        "-q",
        "rust",
        "-m",
        "general",
        "-p",
        "brave,serper,exa",
        "--json",
    ]));
    assert_eq!(code, 0, "{v}");
    let sent = mock.requests("/api/v1/search");
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].body["providers"], json!(["brave", "serper", "exa"]));
    let m = &v["metadata"];
    assert_eq!(m["cost"]["micros"], 4500);
    assert_eq!(m["providers_queried"], json!(["brave", "serper", "exa"]));
    assert!(m["warnings"].is_null(), "{}", m["warnings"]);
}

#[test]
fn own_key_goes_direct_and_basis_is_mixed() {
    let mock = start("mixed", handler(|r, _| search_ok(r)));
    // A key that is not real: serper answers 4xx (or the network is absent);
    // either way the call went direct, and the hosted remainder is one call.
    let (code, v) = run_json(
        mock.cmd()
            .env("SERPER_API_KEY", "not-a-real-key-for-tests")
            .args([
                "search",
                "-q",
                "rust",
                "-m",
                "general",
                "-c",
                "3",
                "--json",
                "--no-rerank",
            ]),
    );
    assert_eq!(code, 0, "{v}");
    let t = &v["metadata"]["transport"];
    assert_eq!(t["serper"], "direct");
    assert_eq!(t["exa"], "hosted");
    let sent = mock.requests("/api/v1/search");
    assert_eq!(sent.len(), 1, "the hosted remainder is one call");
    // Pinned to the first engine without a key, so serper never runs twice.
    assert_eq!(sent[0].body["providers"], json!(["exa"]));
    assert_eq!(sent[0].body["rerank"], false);
    assert_eq!(v["metadata"]["cost"]["basis"], "mixed");
}

#[test]
fn untrusted_host_is_refused_for_every_account_call() {
    let mock = start("guard", handler(|r, _| search_ok(r)));
    let refuse = |args: &[&str]| {
        let (code, v) = run_json(
            mock.cmd()
                .env_remove("METASEARCH_ALLOW_INSECURE_HOST")
                .args(args),
        );
        assert_eq!(code, 2, "{args:?}: {v}");
        assert!(
            v["error"]["message"].as_str().unwrap().contains("refusing"),
            "{v}"
        );
    };
    refuse(&["whoami", "--json"]);
    refuse(&["logout", "--revoke", "--json"]);
    refuse(&["search", "-q", "rust", "--json"]);
    assert!(
        mock.log.lock().unwrap().is_empty(),
        "the token reached the host"
    );
}

#[test]
fn whoami_and_revoke_go_through_the_client() {
    let mock = start("whoami", handler(|r, _| search_ok(r)));
    let (code, v) = run_json(mock.cmd().args(["whoami", "--json"]));
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["account"]["email"], "agent@example.com");
    assert_eq!(v["account"]["balance_usd"], 4.2);
    let me = &mock.requests("/api/v1/me")[0];
    assert_eq!(me.headers["authorization"], format!("Bearer {TOKEN}"));
    let (code, v) = run_json(mock.cmd().args(["logout", "--revoke", "--json"]));
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["revoked"], true);
}

#[test]
fn usage_shows_the_account_and_each_transport() {
    let mock = start("usage", handler(|r, _| search_ok(r)));
    let (code, v) = run_json(mock.cmd().args(["usage", "--json"]));
    assert_eq!(code, 0, "{v}");
    let rows = v["providers"].as_array().unwrap();
    let row = |name: &str| rows.iter().find(|r| r["provider"] == name).unwrap().clone();
    assert_eq!(row("metasearch")["data"]["credits_remaining"], 4.2);
    assert_eq!(row("serper")["transport"], "hosted");
    assert_eq!(row("exa")["transport"], "hosted");
}

#[test]
fn without_login_or_keys_the_error_points_at_login() {
    let mock = start("nologin", handler(|r, _| search_ok(r)));
    let (code, v) = run_json(mock.cmd().env_remove("METASEARCH_TOKEN").args([
        "search",
        "-q",
        "rust async",
        "--json",
    ]));
    assert_eq!(code, 2, "{v}");
    assert!(v["error"]["suggestion"]
        .as_str()
        .unwrap()
        .contains("search login"));
    assert!(mock.log.lock().unwrap().is_empty(), "no token, no calls");
}

#[test]
fn metasearch_api_key_is_accepted_and_metasearch_token_wins() {
    let mock = start("apikey", handler(|r, _| search_ok(r)));
    let args = [
        "search",
        "-q",
        "rust async",
        "-m",
        "general",
        "-p",
        "exa",
        "--json",
    ];
    let (code, v) = run_json(
        mock.cmd()
            .env_remove("METASEARCH_TOKEN")
            .env("METASEARCH_API_KEY", "ms_live_alias")
            .args(args),
    );
    assert_eq!(code, 0, "{v}");
    let (code, v) = run_json(
        mock.cmd()
            .env("METASEARCH_API_KEY", "ms_live_alias")
            .args(args),
    );
    assert_eq!(code, 0, "{v}");
    let reqs = mock.requests("/api/v1/search");
    assert_eq!(reqs[0].headers["authorization"], "Bearer ms_live_alias");
    assert_eq!(reqs[1].headers["authorization"], format!("Bearer {TOKEN}"));
}

#[test]
fn piped_json_is_one_compact_line_and_pretty_indents_it() {
    let mock = start("compact", handler(|r, _| search_ok(r)));
    let args = ["search", "-q", "rust async", "-m", "general", "-p", "exa"];
    let out = mock.cmd().args(args).output().unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(text.trim_end().lines().count(), 1, "{text}");
    serde_json::from_str::<Value>(&text).unwrap();
    let out = mock.cmd().args(args).arg("--pretty").output().unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.lines().count() > 5, "{text}");
    serde_json::from_str::<Value>(&text).unwrap();
}
