use serde::{Deserialize, Serialize};
use std::fmt;

/// JSON envelope schema version. Bump only on breaking schema changes.
pub const ENVELOPE_VERSION: &str = "1";

/// Provider lists and when-to-use guidance live in `registry::MODES` (the
/// single source of truth, surfaced via `search agent-info`); these doc
/// comments stay short so they cannot drift from the routing table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// Broad multi-provider web search, rank-fused
    General,
    /// News and current events (pair with -f day/week)
    News,
    /// Papers and studies on the open web; see also `scholar`
    Academic,
    /// Person / LinkedIn profile search
    People,
    /// Maximum-coverage fan-out; waits for all providers, never cancels early
    Deep,
    /// Full page content as markdown. -q must be a URL
    Extract,
    /// Pages similar to a given page. -q must be a URL
    Similar,
    /// Alias of `extract` (identical behavior). -q must be a URL
    Scrape,
    /// Google Scholar records (citations, PDFs); see also `academic`
    Scholar,
    /// Google Patents search
    Patents,
    /// Google Images search
    Images,
    /// Local businesses and places
    Places,
    /// Live X/Twitter search (X summary + cited posts)
    Social,
    /// What real people say: Reddit and Hacker News threads, fused
    Community,
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Mode::General => "general",
            Mode::News => "news",
            Mode::Academic => "academic",
            Mode::People => "people",
            Mode::Deep => "deep",
            Mode::Extract => "extract",
            Mode::Similar => "similar",
            Mode::Scrape => "scrape",
            Mode::Scholar => "scholar",
            Mode::Patents => "patents",
            Mode::Images => "images",
            Mode::Places => "places",
            Mode::Social => "social",
            Mode::Community => "community",
        };
        write!(f, "{s}")
    }
}

/// Outcome of a search that returned `Ok`. Total failure is modeled as
/// `SearchError::AllProvidersFailed` instead, so agents can branch on the
/// error envelope rather than a magic status string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseStatus {
    /// Results returned and no provider failed.
    Success,
    /// Results returned, but at least one provider failed.
    PartialSuccess,
    /// Every queried provider succeeded but none returned results.
    NoResults,
}

impl ResponseStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::PartialSuccess => "partial_success",
            Self::NoResults => "no_results",
        }
    }

    /// Single source of truth for the Ok-path status ladder.
    pub fn classify(results_empty: bool, any_failed: bool) -> Self {
        match (results_empty, any_failed) {
            (false, false) => Self::Success,
            (false, true) => Self::PartialSuccess,
            (true, _) => Self::NoResults,
        }
    }
}

/// Why a provider failed, so agents can branch on cause without parsing prose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureCategory {
    /// Missing/invalid credentials (401/403, or no key configured).
    Auth,
    /// Out of credits / quota exceeded / insufficient balance (402).
    BillingQuota,
    /// Throttled (429).
    RateLimit,
    /// Request/connect timed out.
    Timeout,
    /// Transport, DNS, or TLS failure.
    Network,
    /// Client error other than auth/billing/rate-limit (4xx).
    BadRequest,
    /// Upstream server error (5xx).
    Server,
    /// Response body could not be parsed.
    Parse,
    /// Local configuration or usage error.
    Config,
    /// The provider answered, but with content the mode does not accept
    /// (a challenge page, binary bytes); the chain moved on.
    Rejected,
    /// Anything else.
    Other,
}

impl FailureCategory {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auth => "auth",
            Self::BillingQuota => "billing_quota",
            Self::RateLimit => "rate_limit",
            Self::Timeout => "timeout",
            Self::Network => "network",
            Self::BadRequest => "bad_request",
            Self::Server => "server",
            Self::Parse => "parse",
            Self::Config => "config",
            Self::Rejected => "rejected",
            Self::Other => "other",
        }
    }
}

/// Structured detail for a single failed provider, surfaced in the envelope so
/// agents (and humans) can tell auth from billing from a transient network blip.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderFailure {
    pub provider: String,
    pub category: FailureCategory,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub http_status: Option<u16>,
    pub code: String,
    pub reason: String,
    pub retryable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub published: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra: Option<serde_json::Value>,
}

/// An AI-synthesized answer a provider returned alongside (not as) web
/// results — e.g. Perplexity's or Tavily's answer text. Kept out of
/// `results` so `.results[].url` is always a fetchable web URL and answers
/// never consume `-c` result slots.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Answer {
    pub provider: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResponse {
    pub version: String,
    pub status: String,
    pub query: String,
    pub mode: String,
    pub results: Vec<SearchResult>,
    /// AI-synthesized answers (Perplexity/Tavily), separate from web results.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub answers: Vec<Answer>,
    pub metadata: ResponseMetadata,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseMetadata {
    pub elapsed_ms: u128,
    pub result_count: usize,
    pub providers_queried: Vec<String>,
    /// Names of providers that failed. Kept for backward compatibility; see
    /// `provider_failures` for the reason behind each.
    pub providers_failed: Vec<String>,
    /// Always empty since 0.10: a call that was sent is paid for, so the
    /// engine never cancels one before the mode deadline. Kept so old
    /// consumers and log readers keep parsing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub providers_cancelled: Vec<String>,
    /// Providers still running when the mode deadline passed. Each also has a
    /// `timeout` entry in `provider_failures`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub providers_timed_out: Vec<String>,
    /// How each queried provider was reached: "direct" (own key), "local"
    /// (keyless, runs on this machine) or "hosted" (through metasearch.sh,
    /// charged to the account balance).
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub transport: std::collections::BTreeMap<String, String>,
    /// What this search cost upstream, per provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<CostSummary>,
    /// Results each provider actually contributed (pre-dedup), so the fused
    /// list is auditable: absent name = returned nothing or was cancelled.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub provider_results: std::collections::BTreeMap<String, usize>,
    /// Which of a provider's own keys answered (1 = the first), for each
    /// provider given more than one. Never the key itself.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub key_slots: std::collections::BTreeMap<String, usize>,
    /// Keys a provider answered as out of credit (failure code
    /// `credits_exhausted`): tried after the provider's other keys until
    /// `until`. Each names the key's place in the list, never the key.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub out_of_credit_keys: Vec<crate::spent::SpentKey>,
    /// Honesty notes, e.g. providers that don't apply a requested -f/-d filter.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    /// True when this response was replayed from the local cache.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cached: bool,
    /// Age of the cached response in seconds (only set when `cached`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_age_secs: Option<u64>,
    /// Per-provider failure detail (category, HTTP status, reason, retryable).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provider_failures: Vec<ProviderFailure>,
    /// Automatic decisions: the mode `auto` resolved to, and whether results
    /// were reranked. Absent when nothing was decided for the caller.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routing: Option<Routing>,
    /// Plain one-line notes for the caller (not problems; see `warnings`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

/// What the CLI and metasearch.sh decided on the caller's behalf (ROUTING.md).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Routing {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto: Option<AutoDecision>,
    /// True when the relevance rerank reordered the results (each then has
    /// `extra.relevance`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reranked: Option<bool>,
    /// metasearch.sh answered from its result cache: no engine was asked,
    /// and the charge is the stored call's (unlike `metadata.cached`, a free
    /// replay from this machine's cache).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached: Option<bool>,
    /// When metasearch.sh fetched the cached answer (RFC 3339).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_age_secs: Option<u64>,
    /// Engines metasearch.sh asked first whose list it set aside for a
    /// better engine's: never charged (0 in `cost.by_provider`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub escalated: Vec<String>,
}

/// How `auto` resolved. `source`: "rule" (a decisive pattern), "model"
/// (metasearch.sh's routing model), "heuristic" (a local signal pattern)
/// or "fallback" (nothing matched, or a `-p` that cannot serve auto's pick:
/// general). No model id is carried: the server no longer sends one, and an
/// older server's is ignored.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AutoDecision {
    pub mode: String,
    #[serde(deserialize_with = "de_source")]
    pub source: String,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probabilities: Option<serde_json::Map<String, serde_json::Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filters_applied: Option<serde_json::Map<String, serde_json::Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u64>,
}

/// The sources an envelope may show. Anything else (an older server named
/// its routing model, and envelopes cached by 0.10.0 repeat it) reads as
/// "model", the only source that comes from the server's model.
const SOURCES: [&str; 4] = ["rule", "model", "heuristic", "fallback"];

fn de_source<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    let s = String::deserialize(d)?;
    Ok(if SOURCES.contains(&s.as_str()) {
        s
    } else {
        "model".to_string()
    })
}

/// Cost of one search in micro-USD (1 USD = 1,000,000).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CostSummary {
    pub micros: u64,
    pub usd: f64,
    /// "estimate" (list price, or the upstream's own figure, for calls made
    /// with your keys), "charged" (billed by metasearch.sh), or "mixed".
    pub basis: String,
    pub by_provider: std::collections::BTreeMap<String, u64>,
}

impl CostSummary {
    pub fn with_basis(by_provider: std::collections::BTreeMap<String, u64>, basis: &str) -> Self {
        let micros = by_provider.values().sum();
        Self {
            micros,
            usd: micros as f64 / 1_000_000.0,
            basis: basis.to_string(),
            by_provider,
        }
    }
}

/// How a provider call reaches its upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// Your own key, straight to the provider.
    Direct,
    /// Keyless, runs on this machine.
    Local,
    /// Through metasearch.sh, charged to your balance.
    Hosted,
}

impl Transport {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Local => "local",
            Self::Hosted => "hosted",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct SearchOpts {
    pub include_domains: Vec<String>,
    pub exclude_domains: Vec<String>,
    /// day, week, month, year
    pub freshness: Option<String>,
    /// ISO country code (e.g. "gb") for region-biased results; applied by
    /// providers that support it (serper/serpapi gl, brave country, parallel location).
    pub country: Option<String>,
    /// Language code (e.g. "de"); applied by serper/serpapi (hl) and brave.
    pub lang: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ErrorResponse {
    pub version: String,
    pub status: String,
    pub error: ErrorDetail,
}

#[derive(Debug, Serialize)]
pub struct ErrorDetail {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggestion: Option<String>,
    /// Populated for `all_providers_failed` so the error envelope carries the
    /// same per-provider detail the success envelope would.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provider_failures: Vec<ProviderFailure>,
    /// What the failed run still cost (billed timeouts, rejected pages).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<CostSummary>,
    /// Keys marked out of credit (see `metadata.out_of_credit_keys`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub out_of_credit_keys: Vec<crate::spent::SpentKey>,
}

#[cfg(test)]
mod tests {
    use super::{AutoDecision, FailureCategory, ResponseStatus};

    #[test]
    fn status_ladder() {
        assert_eq!(
            ResponseStatus::classify(false, false),
            ResponseStatus::Success
        );
        assert_eq!(
            ResponseStatus::classify(false, true),
            ResponseStatus::PartialSuccess
        );
        assert_eq!(
            ResponseStatus::classify(true, true),
            ResponseStatus::NoResults
        );
        assert_eq!(
            ResponseStatus::classify(true, false),
            ResponseStatus::NoResults
        );
    }

    #[test]
    fn category_serializes_snake_case() {
        let j = serde_json::to_string(&FailureCategory::BillingQuota).unwrap();
        assert_eq!(j, "\"billing_quota\"");
    }

    #[test]
    fn an_unknown_routing_source_reads_as_model_and_no_model_id_is_kept() {
        let d: AutoDecision = serde_json::from_value(serde_json::json!({
            "mode": "general", "source": "vendor-x", "reason": "r",
            "model": "vendor-x-1.13.0", "latency_ms": 300
        }))
        .unwrap();
        assert_eq!(d.source, "model");
        assert_eq!(d.latency_ms, Some(300));
        let out = serde_json::to_string(&d).unwrap();
        assert!(!out.contains("vendor"), "{out}");
    }

    #[test]
    fn known_sources_pass_through() {
        for source in super::SOURCES {
            let d: AutoDecision = serde_json::from_value(
                serde_json::json!({ "mode": "news", "source": source, "reason": "r" }),
            )
            .unwrap();
            assert_eq!(d.source, source);
        }
    }
}
