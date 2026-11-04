//! metasearch.sh as a transport, not a provider (PLAN.md D10, §4.6).
//!
//! An engine with your own key goes direct; the engines without one go
//! through your metasearch.sh balance as ONE hosted call per search
//! ([`HostedGroup`]). Without `-p` that call names no engine, so the server
//! routes it: cheapest engine first, a pricier one only when the cheap list
//! is thin, its result cache, and at most two social platforms. `-p` is sent
//! as given. Every account call (login, whoami, logout, search, route,
//! rerank) goes through [`MetasearchClient`], whose constructor runs the host
//! guard once: the bearer token never reaches a host the user did not choose.

use crate::auth;
use crate::context::AppContext;
use crate::errors::{redact_secrets, SearchError};
use crate::providers::{self, Adapter, Call, FilterSupport, Output, ProviderSpec};
use crate::types::{
    Answer, AutoDecision, FailureCategory, Mode, SearchOpts, SearchResult, Transport,
};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The manifest changes when the operator turns a provider on or off; ten
/// minutes keeps that fresh without a round trip per search.
const MANIFEST_TTL_SECS: u64 = 600;
const MANIFEST_TIMEOUT: Duration = Duration::from_secs(5);
/// After a failed fetch, the stale copy is used this long before retrying.
const MANIFEST_RETRY_SECS: u64 = 60;
/// Account calls (`/me`, revoke, device flow) must fail in seconds.
pub const ACCOUNT_TIMEOUT: Duration = Duration::from_secs(15);
pub const ROUTE_TIMEOUT: Duration = Duration::from_millis(1200);
pub const RERANK_TIMEOUT: Duration = Duration::from_millis(1500);
/// At most this many results go to /rerank (the server's limit).
pub const RERANK_MAX: usize = 30;
const CONNECT_RETRIES: u32 = 2;

pub struct MetasearchClient {
    http: reqwest::Client,
    base: String,
    token: String,
}

/// `GET /api/v1/manifest`, only the fields the CLI reads.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct V1Manifest {
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub modes: Vec<ManifestMode>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ManifestMode {
    pub mode: String,
    #[serde(default)]
    pub providers: Vec<ManifestProvider>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ManifestProvider {
    pub name: String,
    #[serde(default)]
    pub available: bool,
    /// Filter names this provider applies in this mode, on the wire's
    /// spelling: freshness, include_domains, exclude_domains, country, language.
    #[serde(default)]
    pub filters: Vec<String>,
    #[serde(default)]
    pub max_count: Option<u16>,
    /// What one call costs the account, in USD.
    #[serde(default)]
    pub price_usd: Option<f64>,
}

impl V1Manifest {
    /// The provider's entry for `mode`, when the server offers it.
    pub fn offer(&self, provider: &str, mode: Mode) -> Option<&ManifestProvider> {
        let mode = match mode {
            Mode::Scrape => Mode::Extract,
            m => m,
        }
        .to_string();
        self.modes
            .iter()
            .find(|m| m.mode == mode)?
            .providers
            .iter()
            .find(|p| p.name.eq_ignore_ascii_case(provider) && p.available)
    }
}

impl V1Manifest {
    /// The lowest price the provider is offered at, in USD per call.
    pub fn price_from(&self, provider: &str) -> Option<f64> {
        self.modes
            .iter()
            .flat_map(|m| m.providers.iter())
            .filter(|p| p.name.eq_ignore_ascii_case(provider) && p.available)
            .filter_map(|p| p.price_usd)
            .min_by(|a, b| a.total_cmp(b))
    }
}

/// The manifest last fetched from `base`, however old (no network).
pub fn cached_manifest(base: &str) -> Option<V1Manifest> {
    read_manifest_cache()
        .filter(|c| c.base == base.trim_end_matches('/'))
        .map(|c| c.manifest)
}

#[derive(Clone, Serialize, Deserialize)]
struct CachedManifest {
    fetched_at: u64,
    base: String,
    manifest: V1Manifest,
}

/// `GET /api/v1/me`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Me {
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub balance_usd: f64,
    #[serde(default)]
    pub key: MeKey,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct MeKey {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub prefix: Option<String>,
    #[serde(default)]
    pub last_used_at: Option<String>,
    #[serde(default)]
    pub daily_cap_usd: Option<f64>,
    #[serde(default)]
    pub spent_today_usd: Option<f64>,
}

#[derive(Deserialize)]
struct V1Response {
    #[serde(default)]
    results: Vec<V1Result>,
    #[serde(default)]
    answers: Vec<V1Answer>,
    #[serde(default)]
    billing: Option<V1Billing>,
    #[serde(default)]
    routing: Option<V1Routing>,
}

/// `routing` in a v1 answer: who the server asked and what each cost.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct V1Routing {
    #[serde(default)]
    pub served_by: Option<String>,
    #[serde(default)]
    pub attempts: Vec<V1Attempt>,
    /// Answered from the server's result cache: no engine was asked.
    #[serde(default)]
    pub cached: bool,
    #[serde(default)]
    pub cached_at: Option<String>,
    #[serde(default)]
    pub reranked: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct V1Attempt {
    pub provider: String,
    /// ok | empty | rejected | failed | timeout | skipped
    #[serde(default)]
    pub outcome: String,
    #[serde(default)]
    pub category: Option<FailureCategory>,
    #[serde(default)]
    pub http_status: Option<u16>,
    #[serde(default)]
    pub charged_micros: u64,
    /// Its list was too thin, so the server asked a better engine; never charged.
    #[serde(default)]
    pub escalated: bool,
}

/// What one hosted call reported beyond its results: the engine reads it to
/// name the engines the server asked, their charges, and a cache hit.
#[derive(Debug, Clone, Default)]
pub struct HostedReport {
    pub routing: V1Routing,
    /// The account's charge for the whole call.
    pub charged_micros: u64,
}

impl HostedReport {
    /// The engines the server sent the query to, in order (skipped ones left out).
    pub fn asked(&self) -> Vec<&str> {
        let mut names: Vec<&str> = Vec::new();
        for a in &self.routing.attempts {
            if a.outcome != "skipped" && !names.contains(&a.provider.as_str()) {
                names.push(a.provider.as_str());
            }
        }
        if names.is_empty() {
            if let Some(s) = self.routing.served_by.as_deref() {
                names.push(s);
            }
        }
        names
    }

    /// The charge per engine. An escalated attempt (its list set aside for a
    /// better engine's) costs the account nothing. When the attempts do not
    /// add up to the charge (a cache hit has none), the charge goes to the
    /// engine that served the answer.
    pub fn charges(&self) -> std::collections::BTreeMap<String, u64> {
        let mut by: std::collections::BTreeMap<String, u64> = std::collections::BTreeMap::new();
        for a in &self.routing.attempts {
            if a.outcome == "skipped" {
                continue;
            }
            let c = if a.escalated { 0 } else { a.charged_micros };
            *by.entry(a.provider.clone()).or_default() += c;
        }
        let sum: u64 = by.values().sum();
        if sum != self.charged_micros {
            by.values_mut().for_each(|v| *v = 0);
            let who = self
                .routing
                .served_by
                .clone()
                .unwrap_or_else(|| HOSTED_SPEC.name.to_string());
            by.insert(who, self.charged_micros);
        }
        by
    }

    /// Attempts that failed while the server moved on to another engine.
    pub fn failures(&self) -> Vec<crate::types::ProviderFailure> {
        self.routing
            .attempts
            .iter()
            .filter(|a| matches!(a.outcome.as_str(), "failed" | "timeout" | "rejected"))
            .map(|a| {
                let category = a.category.unwrap_or(match a.outcome.as_str() {
                    "timeout" => FailureCategory::Timeout,
                    "rejected" => FailureCategory::Rejected,
                    _ => FailureCategory::Server,
                });
                crate::types::ProviderFailure {
                    provider: a.provider.clone(),
                    category,
                    http_status: a.http_status,
                    code: a.outcome.clone(),
                    reason: format!(
                        "{} {} on metasearch.sh; the server answered from another engine",
                        a.provider, a.outcome
                    ),
                    retryable: matches!(
                        category,
                        FailureCategory::Timeout
                            | FailureCategory::RateLimit
                            | FailureCategory::Server
                    ),
                }
            })
            .collect()
    }

    /// Seconds since the server cached the answer, when it came from its cache.
    pub fn cache_age_secs(&self) -> Option<u64> {
        let at = self.routing.cached_at.as_deref()?;
        let then = parse_rfc3339_secs(at)?;
        Some(now_secs().saturating_sub(then))
    }
}

/// Seconds since the epoch for an RFC 3339 UTC time ("2026-11-04T02:45:33.123Z").
fn parse_rfc3339_secs(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' {
        return None;
    }
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, m, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hh, mm, ss) = (num(11..13)?, num(14..16)?, num(17..19)?);
    // Days from civil (Howard Hinnant's algorithm).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(days * 86_400 + hh * 3_600 + mm * 60 + ss).ok()
}

#[derive(Deserialize)]
struct V1Result {
    #[serde(default)]
    title: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    snippet: String,
    #[serde(default)]
    published: Option<String>,
    #[serde(default, rename = "imageUrl")]
    image_url: Option<String>,
    #[serde(default)]
    extra: Option<serde_json::Value>,
    /// Fused answers: every engine that returned this URL.
    #[serde(default)]
    sources: Vec<V1Source>,
}

#[derive(Deserialize)]
struct V1Source {
    provider: String,
}

#[derive(Deserialize)]
struct V1Answer {
    #[serde(default)]
    text: String,
    #[serde(default)]
    provider: String,
}

#[derive(Deserialize)]
struct V1Billing {
    #[serde(default)]
    charged_micros: u64,
}

#[derive(Deserialize)]
struct RouteAnswer {
    auto: AutoDecision,
}

/// One result as /rerank sees it.
#[derive(Serialize)]
pub struct RerankItem {
    pub id: String,
    pub title: String,
    pub snippet: String,
    pub url: String,
}

#[derive(Deserialize)]
pub struct RerankAnswer {
    #[serde(default)]
    pub order: Vec<String>,
    #[serde(default)]
    pub relevance: std::collections::HashMap<String, f64>,
}

impl MetasearchClient {
    /// The logged-in client, or `None` when no token is stored. Refuses a
    /// host other than metasearch.sh unless the user consented
    /// (`METASEARCH_ALLOW_INSECURE_HOST=1`), whatever the call.
    pub fn new(ctx: &AppContext) -> Result<Option<Self>, SearchError> {
        let token = auth::resolve_token(&ctx.config);
        if token.is_empty() {
            return Ok(None);
        }
        let base = auth::resolve_base_url(&ctx.config);
        Self::with_base(ctx.client.clone(), base, token).map(Some)
    }

    /// A client for `base` (login has no token yet). Runs the host guard.
    pub fn with_base(
        http: reqwest::Client,
        base: String,
        token: String,
    ) -> Result<Self, SearchError> {
        let base = base.trim_end_matches('/').to_string();
        auth::assert_login_host_allowed(&base)?;
        Ok(Self { http, base, token })
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    /// POST without a bearer (the device flow).
    pub fn post_anonymous(&self, path: &str) -> reqwest::RequestBuilder {
        self.http.post(self.url(path)).timeout(ACCOUNT_TIMEOUT)
    }

    /// The manifest, from a 10-minute disk cache when fresh. A failed fetch
    /// falls back to a stale copy; `None` when neither exists.
    pub async fn manifest(&self) -> Option<V1Manifest> {
        let cached = read_manifest_cache().filter(|c| c.base == self.base);
        if let Some(c) = &cached {
            if now_secs().saturating_sub(c.fetched_at) < MANIFEST_TTL_SECS {
                return Some(c.manifest.clone());
            }
        }
        let fetched = async {
            let resp = self
                .http
                .get(self.url("/api/v1/manifest"))
                .timeout(MANIFEST_TIMEOUT)
                .send()
                .await
                .ok()?;
            if !resp.status().is_success() {
                return None;
            }
            resp.json::<V1Manifest>().await.ok()
        }
        .await;
        match fetched {
            Some(manifest) => {
                write_manifest_cache(&CachedManifest {
                    fetched_at: now_secs(),
                    base: self.base.clone(),
                    manifest: manifest.clone(),
                });
                Some(manifest)
            }
            // A failed or hung fetch keeps the stale copy and is not retried
            // for a minute, so an outage costs one 5 s wait, not one per search.
            None => cached.map(|c| {
                write_manifest_cache(&CachedManifest {
                    fetched_at: now_secs().saturating_sub(MANIFEST_TTL_SECS - MANIFEST_RETRY_SECS),
                    ..c.clone()
                });
                c.manifest
            }),
        }
    }

    /// Forget the cached manifest (the server said a provider it listed is
    /// not available after all).
    pub fn drop_manifest_cache(&self) {
        let _ = std::fs::remove_file(manifest_cache_path());
    }

    /// One hosted call with `body` (see [`search_body`]). Failures are
    /// reported under `label` ("metasearch" for a routed call, else the
    /// pinned engine). Retries: transport errors before any answer (same
    /// idempotency key, so the server replays rather than charges twice),
    /// and a 409 once after 1 s.
    pub async fn search(
        &self,
        label: &'static str,
        body: &serde_json::Value,
        idempotency_key: &str,
        timeout: Duration,
    ) -> Result<(Output, HostedReport), SearchError> {
        let provider = label;
        let mut request = body.clone();
        let deadline = Instant::now() + timeout;
        let mut connect_retries = 0u32;
        let mut conflict_retried = false;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(providers::timeout_error(provider));
            }
            let sent = self
                .http
                .post(self.url("/api/v1/search"))
                .bearer_auth(&self.token)
                .header("Idempotency-Key", idempotency_key)
                .header(
                    reqwest::header::USER_AGENT,
                    concat!("search-cli/", env!("CARGO_PKG_VERSION")),
                )
                .json(&request)
                .timeout(left)
                .send()
                .await;
            let resp = match sent {
                Ok(r) => r,
                Err(e) if e.is_timeout() => return Err(providers::timeout_error(provider)),
                Err(e) => {
                    if connect_retries < CONNECT_RETRIES {
                        let pause = Duration::from_millis(200 * (1 << connect_retries));
                        if pause < deadline.saturating_duration_since(Instant::now()) {
                            connect_retries += 1;
                            tokio::time::sleep(pause).await;
                            continue;
                        }
                    }
                    return Err(SearchError::Http(e.without_url()));
                }
            };
            let status = resp.status().as_u16();
            if resp.status().is_success() {
                let v: V1Response = resp.json().await.map_err(|e| SearchError::Upstream {
                    provider,
                    status,
                    category: FailureCategory::Parse,
                    message: format!("metasearch.sh sent an unreadable answer: {e}"),
                })?;
                return Ok(to_output(provider, v));
            }
            let body: serde_json::Value = resp.json().await.unwrap_or_default();
            // A server older than 0.12.1's contract refuses the `fresh` field
            // (a 400 before anything is reserved): ask again without it.
            if status == 400 && refuses_fresh(&request, &body) {
                if let Some(obj) = request.as_object_mut() {
                    obj.remove("fresh");
                }
                continue;
            }
            if status == 409 && !conflict_retried {
                conflict_retried = true;
                if Duration::from_secs(2) < deadline.saturating_duration_since(Instant::now()) {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
            }
            let failure = map_error(status, &body);
            if status == 503 && body["error"] == "provider_not_available" {
                self.drop_manifest_cache();
            }
            return Err(SearchError::Upstream {
                provider,
                status,
                category: failure.0,
                message: redact_secrets(&failure.1),
            });
        }
    }

    /// `GET /api/v1/me`: who the key belongs to, and the balance.
    pub async fn me(&self) -> Result<Me, SearchError> {
        let resp = self
            .http
            .get(self.url("/api/v1/me"))
            .bearer_auth(&self.token)
            .timeout(ACCOUNT_TIMEOUT)
            .send()
            .await
            .map_err(|e| {
                account_error(format!(
                    "could not reach {}: {}",
                    self.base,
                    e.without_url()
                ))
            })?;
        match resp.status().as_u16() {
            200..=299 => resp
                .json()
                .await
                .map_err(|e| account_error(format!("malformed response: {e}"))),
            401 => Err(SearchError::Config(
                "stored key is not valid — run `search login` again".to_string(),
            )),
            s => Err(account_error(format!("HTTP {s}"))),
        }
    }

    /// `DELETE /api/v1/me`: revoke the calling key. True when the server confirmed.
    pub async fn revoke(&self) -> bool {
        let resp = self
            .http
            .delete(self.url("/api/v1/me"))
            .bearer_auth(&self.token)
            .timeout(ACCOUNT_TIMEOUT)
            .send()
            .await;
        matches!(resp, Ok(r) if r.status().is_success())
    }

    /// `POST /api/v1/route` (free): the mode metasearch.sh would pick. Fail-open:
    /// `None` on any error or after 1.2 s, and the caller uses local rules.
    pub async fn route(&self, query: &str) -> Option<AutoDecision> {
        let resp = self
            .http
            .post(self.url("/api/v1/route"))
            .bearer_auth(&self.token)
            .json(&serde_json::json!({ "query": query }))
            .timeout(ROUTE_TIMEOUT)
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let answer: RouteAnswer = resp.json().await.ok()?;
        Some(answer.auto)
    }

    /// `POST /api/v1/rerank` (free). Fail-open: `None` on any error, after
    /// 1.5 s, or when the server answers without relevance scores (its own
    /// fail-open returns the input order and no scores).
    pub async fn rerank(&self, query: &str, items: &[RerankItem]) -> Option<RerankAnswer> {
        let resp = self
            .http
            .post(self.url("/api/v1/rerank"))
            .bearer_auth(&self.token)
            .json(&serde_json::json!({ "query": query, "results": items }))
            .timeout(RERANK_TIMEOUT)
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let answer: RerankAnswer = resp.json().await.ok()?;
        (!answer.relevance.is_empty()).then_some(answer)
    }
}

/// True when the server refused the request for carrying `fresh`.
fn refuses_fresh(request: &serde_json::Value, answer: &serde_json::Value) -> bool {
    request.get("fresh").is_some()
        && answer["error"] == "invalid_request"
        && answer["message"]
            .as_str()
            .is_some_and(|m| m.contains("fresh"))
}

fn account_error(message: String) -> SearchError {
    SearchError::Api {
        provider: "metasearch",
        code: "api_error",
        message: redact_secrets(&message),
        status: None,
    }
}

/// What a hosted call asks for beyond the query (contract `SearchBodySchema`).
pub struct BodyOpts<'a> {
    /// `None`: no provider list, the server routes. `Some`: sent as given.
    pub providers: Option<&'a [&'a str]>,
    /// Filters the call may carry; the rest stay local "not applied" warnings.
    pub allowed: &'a [String],
    /// `Some(false)` turns the server's rerank off (the CLI reranks itself,
    /// or `--no-rerank`); `None` leaves its default.
    pub rerank: Option<bool>,
    /// `--no-cache`: skip the server's result cache.
    pub fresh: bool,
}

/// The wire body for one hosted call.
pub fn search_body(
    mode: Mode,
    query: &str,
    count: u16,
    opts: &SearchOpts,
    b: &BodyOpts<'_>,
) -> serde_json::Value {
    let allowed = b.allowed;
    let allows = |f: &str| allowed.iter().any(|a| a == f);
    let mut filters = serde_json::Map::new();
    if let Some(f) = opts.freshness.as_ref().filter(|_| allows("freshness")) {
        filters.insert("freshness".into(), f.clone().into());
    }
    if !opts.include_domains.is_empty() && allows("include_domains") {
        filters.insert(
            "include_domains".into(),
            opts.include_domains.clone().into(),
        );
    }
    if !opts.exclude_domains.is_empty() && allows("exclude_domains") {
        filters.insert(
            "exclude_domains".into(),
            opts.exclude_domains.clone().into(),
        );
    }
    if let Some(c) = opts.country.as_ref().filter(|_| allows("country")) {
        filters.insert("country".into(), c.clone().into());
    }
    if let Some(l) = opts.lang.as_ref().filter(|_| allows("language")) {
        filters.insert("language".into(), l.clone().into());
    }
    let mode = match mode {
        Mode::Scrape => Mode::Extract,
        m => m,
    };
    let mut body = serde_json::json!({
        "query": query,
        "mode": mode.to_string(),
        "count": count.clamp(1, 20),
    });
    if let Some(list) = b.providers {
        body["providers"] = serde_json::json!(list);
    }
    if !filters.is_empty() {
        body["filters"] = serde_json::Value::Object(filters);
    }
    if let Some(r) = b.rerank {
        body["rerank"] = r.into();
    }
    if b.fresh {
        body["fresh"] = true.into();
    }
    body
}

fn to_output(label: &str, v: V1Response) -> (Output, HostedReport) {
    let routing = v.routing.unwrap_or_default();
    let charged_micros = v.billing.map_or(0, |b| b.charged_micros);
    // A routed call names its engines per row; a pinned one is its engine.
    let fallback = routing
        .served_by
        .clone()
        .filter(|_| label == HOSTED_SPEC.name)
        .unwrap_or_else(|| label.to_string());
    let output = Output {
        results: v
            .results
            .into_iter()
            .map(|r| SearchResult {
                title: r.title,
                url: r.url,
                snippet: r.snippet,
                source: r
                    .sources
                    .into_iter()
                    .next()
                    .map(|s| s.provider)
                    .unwrap_or_else(|| fallback.clone()),
                published: r.published,
                image_url: r.image_url,
                extra: r.extra,
            })
            .collect(),
        answers: v
            .answers
            .into_iter()
            .filter(|a| !a.text.is_empty())
            .map(|a| Answer {
                provider: if a.provider.is_empty() {
                    fallback.clone()
                } else {
                    a.provider
                },
                text: a.text,
            })
            .collect(),
        // What the account was charged, not an estimate.
        cost_micros: Some(charged_micros),
    };
    (
        output,
        HostedReport {
            routing,
            charged_micros,
        },
    )
}

/// A v1 error answer as (category, message). The message carries what the
/// caller needs to act: `search login`, the balance and top-up link, the wait.
fn map_error(status: u16, body: &serde_json::Value) -> (FailureCategory, String) {
    let code = body["error"].as_str().unwrap_or("");
    let msg = body["message"].as_str().unwrap_or("");
    let usd = |k: &str| body[k].as_f64().map(|v| format!("${v:.4}"));
    match status {
        401 => (
            FailureCategory::Auth,
            "metasearch.sh rejected the stored key; run `search login`".to_string(),
        ),
        402 if code == "spend_cap_reached" => (
            FailureCategory::BillingQuota,
            format!(
                "metasearch.sh: this key's daily spend cap ({}) is reached; it resets at {}. Raise it at https://metasearch.sh/dashboard",
                usd("cap_usd").unwrap_or_else(|| "?".into()),
                body["resets_at"].as_str().unwrap_or("midnight UTC")
            ),
        ),
        402 => (
            FailureCategory::BillingQuota,
            format!(
                "metasearch.sh: insufficient credits (balance {}, this search needs {}); top up at {}",
                usd("balance_usd").unwrap_or_else(|| "?".into()),
                usd("required_usd").unwrap_or_else(|| "?".into()),
                body["top_up_url"]
                    .as_str()
                    .unwrap_or("https://metasearch.sh/dashboard/billing")
            ),
        ),
        429 => (
            FailureCategory::RateLimit,
            format!(
                "metasearch.sh rate limit ({}); wait and retry",
                body["reason"].as_str().unwrap_or("requests")
            ),
        ),
        502 => {
            let category = body["attempts"][0]["category"]
                .as_str()
                .and_then(|c| serde_json::from_value(serde_json::json!(c)).ok())
                .unwrap_or(FailureCategory::Server);
            (category, format!("upstream failed via metasearch.sh: {msg}"))
        }
        503 => (
            FailureCategory::Server,
            format!("metasearch.sh: {} ({msg})", if code.is_empty() { "unavailable" } else { code }),
        ),
        400 | 422 => (
            FailureCategory::BadRequest,
            format!("metasearch.sh refused the request ({code}): {msg}"),
        ),
        s => (
            crate::errors::category_for_status(s),
            format!("metasearch.sh HTTP {s}: {msg}"),
        ),
    }
}

fn manifest_cache_path() -> std::path::PathBuf {
    crate::cache::cache_dir().join("manifest.json")
}

fn read_manifest_cache() -> Option<CachedManifest> {
    let content = std::fs::read_to_string(manifest_cache_path()).ok()?;
    serde_json::from_str(&content).ok()
}

fn write_manifest_cache(c: &CachedManifest) {
    if let Ok(bytes) = serde_json::to_vec(c) {
        let _ = crate::cache::write_atomic(&manifest_cache_path(), &bytes);
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// A provider you have no key for and metasearch.sh offers in this mode.
/// The engine plans with these exactly like direct adapters (so `-p`, cues
/// and the registry order behave the same either way), then folds every one
/// of a search into a single [`HostedGroup`] call.
pub struct HostedAdapter {
    pub spec: &'static ProviderSpec,
    pub client: Arc<MetasearchClient>,
    /// Filters the manifest lists for this provider in this mode.
    pub filters: Vec<String>,
    pub max_count: u16,
}

#[async_trait::async_trait]
impl Adapter for HostedAdapter {
    fn spec(&self) -> &'static ProviderSpec {
        self.spec
    }
    fn is_configured(&self) -> bool {
        true
    }
    fn transport(&self) -> Transport {
        Transport::Hosted
    }
    /// metasearch.sh charges only for accepted results; a call that failed or
    /// timed out costs the account nothing.
    fn sent_cost(&self, _mode: Mode) -> u64 {
        0
    }
    fn applied_filters(&self, opts: &SearchOpts) -> FilterSupport {
        filter_support(&self.filters, opts)
    }
    fn as_hosted(&self) -> Option<&HostedAdapter> {
        Some(self)
    }
    /// Only reached when the engine did not fold it into a group: one call
    /// pinned to this provider.
    async fn call(&self, call: &Call<'_>) -> Result<Output, SearchError> {
        let key = uuid::Uuid::new_v4().to_string();
        let names = [self.spec.name];
        let body = search_body(
            call.mode,
            call.query,
            call.count.min(self.max_count),
            call.opts,
            &BodyOpts {
                providers: Some(&names),
                allowed: &self.filters,
                rerank: None,
                fresh: false,
            },
        );
        self.client
            .search(self.spec.name, &body, &key, call.timeout)
            .await
            .map(|(out, _)| out)
    }
}

/// Which of the caller's filters a hosted call sends, given the filters it may carry.
pub fn filter_support(listed: &[String], opts: &SearchOpts) -> FilterSupport {
    let listed = |f: &str| listed.iter().any(|a| a == f);
    FilterSupport {
        freshness: listed("freshness"),
        domains: (opts.include_domains.is_empty() || listed("include_domains"))
            && (opts.exclude_domains.is_empty() || listed("exclude_domains")),
        country: listed("country"),
        lang: listed("language"),
    }
}

/// The record a [`HostedGroup`] runs under. Not an engine: the engine
/// expands its result into the engines the server asked.
pub static HOSTED_SPEC: ProviderSpec = ProviderSpec {
    name: "metasearch",
    env_keys: &[],
    modes: &[],
    filters: FilterSupport {
        freshness: false,
        domains: false,
        country: false,
        lang: false,
    },
    max_count: 20,
    timeout: Duration::from_secs(60),
    billed_on_send: false,
    price: crate::providers::spec::Price {
        per_call: 0,
        per_extra_result: 0,
        included_results: 0,
    },
};

/// Every hosted provider of one search as ONE metasearch.sh call. Without
/// `providers` the server routes (cheapest first, its result cache, its
/// social pick); with them, they are sent as given.
pub struct HostedGroup {
    pub client: Arc<MetasearchClient>,
    /// `None`: the server picks. `Some`: these engines, in this order.
    pub providers: Option<Vec<&'static str>>,
    /// The filters this call may carry.
    pub filters: Vec<String>,
    pub max_count: u16,
    pub rerank: Option<bool>,
    pub fresh: bool,
    /// Filled by `call`: the attempts, charges and cache hit.
    pub report: Arc<Mutex<Option<HostedReport>>>,
}

impl HostedGroup {
    pub fn body(&self, call: &Call<'_>) -> serde_json::Value {
        search_body(
            call.mode,
            call.query,
            call.count.min(self.max_count),
            call.opts,
            &BodyOpts {
                providers: self.providers.as_deref(),
                allowed: &self.filters,
                rerank: self.rerank,
                fresh: self.fresh,
            },
        )
    }
}

#[async_trait::async_trait]
impl Adapter for HostedGroup {
    fn spec(&self) -> &'static ProviderSpec {
        &HOSTED_SPEC
    }
    fn is_configured(&self) -> bool {
        true
    }
    fn transport(&self) -> Transport {
        Transport::Hosted
    }
    fn sent_cost(&self, _mode: Mode) -> u64 {
        0
    }
    fn applied_filters(&self, opts: &SearchOpts) -> FilterSupport {
        filter_support(&self.filters, opts)
    }
    async fn call(&self, call: &Call<'_>) -> Result<Output, SearchError> {
        // Fresh per call, reused by that call's own retries.
        let key = uuid::Uuid::new_v4().to_string();
        let label = match self.providers.as_deref() {
            Some([one]) => one,
            _ => HOSTED_SPEC.name,
        };
        let (out, report) = self
            .client
            .search(label, &self.body(call), &key, call.timeout)
            .await?;
        if let Ok(mut slot) = self.report.lock() {
            *slot = Some(report);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_sends_only_listed_filters() {
        let opts = SearchOpts {
            include_domains: vec!["a.com".into()],
            exclude_domains: vec![],
            freshness: Some("week".into()),
            country: Some("gb".into()),
            lang: Some("de".into()),
        };
        let pinned = ["exa"];
        let only_inc = ["include_domains".to_string()];
        let b = search_body(
            Mode::General,
            "q",
            50,
            &opts,
            &BodyOpts {
                providers: Some(&pinned),
                allowed: &only_inc,
                rerank: None,
                fresh: false,
            },
        );
        assert_eq!(b["providers"], serde_json::json!(["exa"]));
        assert_eq!(b["count"], 20);
        assert_eq!(
            b["filters"],
            serde_json::json!({"include_domains": ["a.com"]})
        );
        assert!(b.get("fresh").is_none() && b.get("rerank").is_none());
        let none = search_body(
            Mode::Scrape,
            "https://x.y",
            3,
            &opts,
            &BodyOpts {
                providers: None,
                allowed: &[],
                rerank: Some(false),
                fresh: true,
            },
        );
        assert!(none.get("filters").is_none());
        assert!(none.get("providers").is_none(), "routed: no provider list");
        assert_eq!(none["mode"], "extract");
        assert_eq!(none["fresh"], true);
        assert_eq!(none["rerank"], false);
    }

    fn answer(v: serde_json::Value) -> (Output, HostedReport) {
        to_output(HOSTED_SPEC.name, serde_json::from_value(v).unwrap())
    }

    /// An escalated attempt is free; the engine that answered is charged.
    #[test]
    fn escalated_attempts_are_uncharged() {
        let (out, rep) = answer(serde_json::json!({
            "results": [{"title": "t", "url": "https://a.org/1", "snippet": "s"}],
            "routing": {"merge": "chain", "served_by": "exa", "providers": ["exa"], "fell_back": true,
                "attempts": [
                    {"provider": "serper", "outcome": "ok", "latency_ms": 300, "charged_micros": 0, "escalated": true},
                    {"provider": "exa", "outcome": "ok", "latency_ms": 900, "charged_micros": 10500}
                ]},
            "billing": {"reserved_micros": 10500, "charged_micros": 10500, "balance_micros": 1, "charged_usd": 0.0105, "balance_usd": 0.0}
        }));
        assert_eq!(out.results[0].source, "exa");
        assert_eq!(out.cost_micros, Some(10_500));
        assert_eq!(rep.asked(), ["serper", "exa"]);
        let by = rep.charges();
        assert_eq!(by["serper"], 0);
        assert_eq!(by["exa"], 10_500);
        assert!(rep.failures().is_empty());
        assert!(!rep.routing.cached);
    }

    /// A cache hit has no attempts: its charge goes to the engine that served it.
    #[test]
    fn cached_answer_is_read() {
        let (out, rep) = answer(serde_json::json!({
            "results": [{"title": "t", "url": "https://a.org/1", "snippet": "s",
                "sources": [{"provider": "serper", "rank": 1}]}],
            "routing": {"merge": "chain", "served_by": "serper", "providers": ["serper"], "fell_back": false,
                "attempts": [], "cached": true, "cached_at": "2026-11-04T02:00:00.000Z"},
            "billing": {"reserved_micros": 1500, "charged_micros": 1500, "balance_micros": 1, "charged_usd": 0.0015, "balance_usd": 0.0}
        }));
        assert_eq!(out.results[0].source, "serper");
        assert!(rep.routing.cached);
        assert_eq!(rep.asked(), ["serper"]);
        assert_eq!(rep.charges()["serper"], 1_500);
        assert!(rep.cache_age_secs().is_some());
    }

    #[test]
    fn an_older_server_refusing_fresh_is_recognised() {
        let sent = serde_json::json!({"query": "q", "fresh": true});
        let refused = serde_json::json!({"error": "invalid_request",
            "message": "body: Unrecognized key: \"fresh\""});
        assert!(refuses_fresh(&sent, &refused));
        assert!(!refuses_fresh(&serde_json::json!({"query": "q"}), &refused));
        let other = serde_json::json!({"error": "invalid_request", "message": "count: too big"});
        assert!(!refuses_fresh(&sent, &other));
    }

    #[test]
    fn rfc3339_parses_to_epoch_seconds() {
        assert_eq!(parse_rfc3339_secs("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_rfc3339_secs("2026-11-04T02:45:33.123Z"),
            Some(1_793_760_333)
        );
        assert_eq!(parse_rfc3339_secs("nope"), None);
    }

    #[test]
    fn errors_map_to_categories() {
        assert_eq!(
            map_error(401, &serde_json::json!({})).0,
            FailureCategory::Auth
        );
        let (c, m) = map_error(
            402,
            &serde_json::json!({"error":"insufficient_credits","balance_usd":0.01,"required_usd":0.02,"top_up_url":"https://metasearch.sh/dashboard/billing"}),
        );
        assert_eq!(c, FailureCategory::BillingQuota);
        assert!(m.contains("https://metasearch.sh/dashboard/billing") && m.contains("$0.0100"));
        assert_eq!(
            map_error(429, &serde_json::json!({})).0,
            FailureCategory::RateLimit
        );
        assert_eq!(
            map_error(
                502,
                &serde_json::json!({"attempts":[{"category":"billing_quota"}]})
            )
            .0,
            FailureCategory::BillingQuota
        );
        assert_eq!(
            map_error(503, &serde_json::json!({"error":"provider_not_available"})).0,
            FailureCategory::Server
        );
        assert_eq!(
            map_error(400, &serde_json::json!({})).0,
            FailureCategory::BadRequest
        );
    }
}
