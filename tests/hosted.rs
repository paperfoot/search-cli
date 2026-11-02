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
            { "mode": "places", "deadline_ms": 20000, "providers": [entry("serper")] },
            { "mode": "images", "deadline_ms": 20000, "providers": [entry("serper")] },
        ],
    })
}

/// A 200 for one pinned provider: three results unique to it.
fn search_ok(req: &Req) -> Reply {
    let p = req.body["providers"][0].as_str().unwrap_or("?").to_string();
    let results: Vec<Value> = (0..3)
        .map(|i| json!({ "title": format!("{p} result {i}"), "url": format!("https://{p}.example/{i}"),
                         "snippet": "about rust", "imageUrl": if i == 0 { json!("https://img.example/a.png") } else { Value::Null } }))
        .collect();
    Reply::Json(
        200,
        json!({
            "request_id": "r", "mode": req.body["mode"], "provider": p, "results": results,
            "routing": { "merge": "single", "served_by": p, "providers": [p], "fell_back": false, "attempts": [] },
            "billing": { "reserved_micros": 3000, "charged_micros": 1500, "balance_micros": 998500,
                         "charged_usd": 0.0015, "balance_usd": 0.9985 },
        }),
    )
}

/// Manifest, a JEV route to general, a reversing rerank, and `search` from `on_search`.
fn handler(on_search: impl Fn(&Req, usize) -> Reply + Send + Sync + 'static) -> Handler {
    Arc::new(move |req, i| match req.path.as_str() {
        "/api/v1/manifest" => Reply::Json(200, manifest()),
        "/api/v1/route" => Reply::Json(
            200,
            json!({ "auto": { "mode": "general", "source": "jev", "reason": "Anything else (p 0.81); used general.",
                              "probabilities": { "general": 0.81 }, "model": "jev-1.13.0", "latency_ms": 300 },
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
                json!({ "order": order, "relevance": relevance, "model": "jev-1.13.0", "latency_ms": 200 }),
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
    // exa lists only domain filters: freshness and country stay local warnings.
    assert_eq!(r.body["filters"], json!({ "include_domains": ["a.com"] }));
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

#[test]
fn zero_keys_auto_general_is_charged_capped_routed_and_reranked() {
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
    assert_eq!(m["cost"]["micros"], 4500);
    let by: Vec<&String> = m["cost"]["by_provider"]
        .as_object()
        .unwrap()
        .keys()
        .collect();
    assert_eq!(by, ["exa", "jina", "serper"]);
    // The cap of 3, in registry order, with the rest named.
    let sent: Vec<String> = mock
        .requests("/api/v1/search")
        .iter()
        .map(|r| r.body["providers"][0].as_str().unwrap().to_string())
        .collect();
    assert_eq!(sent.len(), 3);
    let warning = m["warnings"][0].as_str().unwrap();
    assert!(
        warning.starts_with("hosted: queried serper, exa, jina; add -p to choose others"),
        "{warning}"
    );
    assert!(warning.contains("brave"));
    // JEV routing reported.
    assert_eq!(m["routing"]["auto"]["source"], "jev");
    assert_eq!(m["routing"]["auto"]["mode"], "general");
    assert_eq!(m["routing"]["auto"]["model"], "jev-1.13.0");
    assert_eq!(mock.requests("/api/v1/route").len(), 1);
    // Reranked: the mock reverses the order it was sent.
    assert_eq!(m["routing"]["reranked"], true);
    let sent = &mock.requests("/api/v1/rerank")[0].body["results"];
    let last = sent.as_array().unwrap().last().unwrap();
    assert_eq!(v["results"][0]["url"], last["url"]);
    assert!(v["results"][0]["extra"]["relevance"].is_number());
    assert!(m["notes"].is_null(), "no login note when logged in");
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
    let args = ["search", "-q", "rust", "-m", "general", "--json"];
    let (code, v) = run_json(mock.cmd().args(args));
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["metadata"]["routing"]["reranked"], false);
    assert!(v["results"][0]["extra"]["relevance"].is_null());
    // --no-rerank never asks.
    let before = mock.requests("/api/v1/rerank").len();
    let (_, v) = run_json(mock.cmd().args([
        "search",
        "-q",
        "rust",
        "-m",
        "general",
        "--json",
        "--no-cache",
        "--no-rerank",
    ]));
    assert_eq!(mock.requests("/api/v1/rerank").len(), before);
    assert!(v["metadata"]["routing"].is_null());
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

#[test]
fn auto_never_picks_a_mode_the_pinned_provider_cannot_serve() {
    // /route says news; exa serves no news, so auto falls back to general
    // instead of failing a search that ran before auto existed.
    let mock = start(
        "pinnedauto",
        Arc::new(|req: &Req, i| match req.path.as_str() {
            "/api/v1/route" => Reply::Json(
                200,
                json!({ "auto": { "mode": "news", "source": "jev", "reason": "News (p 0.9); used news." } }),
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

#[test]
fn p_lifts_the_cap() {
    let mock = start("plift", handler(|r, _| search_ok(r)));
    let (code, v) = run_json(mock.cmd().args([
        "search",
        "-q",
        "rust",
        "-m",
        "general",
        "-p",
        "serper,exa,jina,brave,linkup",
        "--json",
    ]));
    assert_eq!(code, 0, "{v}");
    assert_eq!(mock.requests("/api/v1/search").len(), 5);
    assert!(
        v["metadata"]["warnings"].is_null(),
        "{}",
        v["metadata"]["warnings"]
    );
}

#[test]
fn own_key_goes_direct_and_basis_is_mixed() {
    let mock = start("mixed", handler(|r, _| search_ok(r)));
    // A key that is not real: serper answers 4xx (or the network is absent);
    // either way the call went direct, and nothing hosted was sent for it.
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
    assert_eq!(v["metadata"]["cost"]["basis"], "mixed");
    let hosted: Vec<String> = mock
        .requests("/api/v1/search")
        .iter()
        .map(|r| r.body["providers"][0].as_str().unwrap().to_string())
        .collect();
    assert!(!hosted.contains(&"serper".to_string()));
    assert_eq!(hosted.len(), 3, "the cap counts hosted calls only");
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
