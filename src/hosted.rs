//! metasearch.sh as a transport, not a provider (PLAN.md D10, §4.6).
//!
//! An engine with your own key goes direct; an engine without one goes
//! through your metasearch.sh balance, one hosted call per (provider, mode),
//! so failures, timeouts, cost and `-p` behave the same either way and fusion
//! stays local. Every account call (login, whoami, logout, search, route,
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
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The manifest changes when the operator turns a provider on or off; ten
/// minutes keeps that fresh without a round trip per search.
const MANIFEST_TTL_SECS: u64 = 600;
const MANIFEST_TIMEOUT: Duration = Duration::from_secs(5);
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

#[derive(Serialize, Deserialize)]
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
    #[serde(default)]
    pub model: Option<String>,
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
            None => cached.map(|c| c.manifest),
        }
    }

    /// Forget the cached manifest (the server said a provider it listed is
    /// not available after all).
    pub fn drop_manifest_cache(&self) {
        let _ = std::fs::remove_file(manifest_cache_path());
    }

    /// One hosted call: `providers: [provider]`, pinned, no server fallback.
    /// Only the filters in `allowed` are sent; the rest keep producing the
    /// CLI's local "not applied" warnings. Retries: transport errors before
    /// any answer (same idempotency key, so the server replays rather than
    /// charges twice), and a 409 once after 1 s.
    #[allow(clippy::too_many_arguments)]
    pub async fn search(
        &self,
        provider: &'static str,
        mode: Mode,
        query: &str,
        count: u16,
        opts: &SearchOpts,
        allowed: &[String],
        idempotency_key: &str,
        timeout: Duration,
    ) -> Result<Output, SearchError> {
        let body = search_body(provider, mode, query, count, opts, allowed);
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
                .json(&body)
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
    /// 1.5 s, or when the server answers without a model (its own fail-open).
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
        answer.model.is_some().then_some(answer)
    }
}

fn account_error(message: String) -> SearchError {
    SearchError::Api {
        provider: "metasearch",
        code: "api_error",
        message: redact_secrets(&message),
        status: None,
    }
}

/// The wire body for one pinned call (contract `SearchBodySchema`).
fn search_body(
    provider: &str,
    mode: Mode,
    query: &str,
    count: u16,
    opts: &SearchOpts,
    allowed: &[String],
) -> serde_json::Value {
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
        "providers": [provider],
        "count": count.clamp(1, 20),
    });
    if !filters.is_empty() {
        body["filters"] = serde_json::Value::Object(filters);
    }
    body
}

fn to_output(provider: &str, v: V1Response) -> Output {
    Output {
        results: v
            .results
            .into_iter()
            .map(|r| SearchResult {
                title: r.title,
                url: r.url,
                snippet: r.snippet,
                source: provider.to_string(),
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
                    provider.to_string()
                } else {
                    a.provider
                },
                text: a.text,
            })
            .collect(),
        // What the account was charged, not an estimate.
        cost_micros: Some(v.billing.map_or(0, |b| b.charged_micros)),
    }
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

/// A provider served through metasearch.sh: the same `Adapter` the engine
/// dispatches for a direct call (same deadline, same fusion), sent to the
/// hosted API instead of the upstream.
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
    /// Exactly what `search_body` sends: the filters the manifest lists for
    /// this provider in this mode.
    fn applied_filters(&self, opts: &SearchOpts) -> FilterSupport {
        let listed = |f: &str| self.filters.iter().any(|a| a == f);
        FilterSupport {
            freshness: listed("freshness"),
            domains: (opts.include_domains.is_empty() || listed("include_domains"))
                && (opts.exclude_domains.is_empty() || listed("exclude_domains")),
            country: listed("country"),
            lang: listed("language"),
        }
    }
    async fn call(&self, call: &Call<'_>) -> Result<Output, SearchError> {
        // Fresh per call, reused by that call's own retries.
        let key = uuid::Uuid::new_v4().to_string();
        self.client
            .search(
                self.spec.name,
                call.mode,
                call.query,
                call.count.min(self.max_count),
                call.opts,
                &self.filters,
                &key,
                call.timeout,
            )
            .await
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
        let b = search_body(
            "exa",
            Mode::General,
            "q",
            50,
            &opts,
            &["include_domains".into()],
        );
        assert_eq!(b["providers"], serde_json::json!(["exa"]));
        assert_eq!(b["count"], 20);
        assert_eq!(
            b["filters"],
            serde_json::json!({"include_domains": ["a.com"]})
        );
        let none = search_body("exa", Mode::Scrape, "https://x.y", 3, &opts, &[]);
        assert!(none.get("filters").is_none());
        assert_eq!(none["mode"], "extract");
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
