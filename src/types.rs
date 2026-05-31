use serde::{Deserialize, Serialize};
use std::fmt;

/// JSON envelope schema version. Bump only on breaking schema changes.
pub const ENVELOPE_VERSION: &str = "1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// General multi-provider web search (Brave + Serper + Exa + Jina + Tavily
    /// + Perplexity). Default when no `-m` is given.
    General,
    /// Breaking news and current events (Brave + Serper + Tavily + Perplexity)
    News,
    /// Research papers and studies (Exa + Serper + Tavily + Perplexity)
    Academic,
    /// Find people, LinkedIn profiles (Exa)
    People,
    /// Maximum coverage (Brave LLM Context + Exa + Serper + Tavily + Perplexity + xAI)
    Deep,
    /// Extract full text content from a URL (Jina Reader -> Firecrawl)
    Extract,
    /// Find pages similar to a URL (Exa findSimilar)
    Similar,
    /// Scrape page content (Jina Reader -> Firecrawl)
    Scrape,
    /// Google Scholar search (Serper)
    Scholar,
    /// Patent search (Serper)
    Patents,
    /// Image search (Serper)
    Images,
    /// Local businesses and places (Serper)
    Places,
    /// X/Twitter social search (xAI Grok)
    Social,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResponse {
    pub version: String,
    pub status: String,
    pub query: String,
    pub mode: String,
    pub results: Vec<SearchResult>,
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
    /// Per-provider failure detail (category, HTTP status, reason, retryable).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provider_failures: Vec<ProviderFailure>,
}

#[derive(Debug, Clone, Default)]
pub struct SearchOpts {
    pub include_domains: Vec<String>,
    pub exclude_domains: Vec<String>,
    /// day, week, month, year
    pub freshness: Option<String>,
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
}

#[cfg(test)]
mod tests {
    use super::{FailureCategory, ResponseStatus};

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
}
