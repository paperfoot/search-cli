use crate::types::{
    ErrorDetail, ErrorResponse, FailureCategory, ProviderFailure, ENVELOPE_VERSION,
};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum SearchError {
    #[error("API error from {provider}: {message}")]
    Api {
        provider: &'static str,
        code: &'static str,
        message: String,
        /// HTTP status, when the failure came from a non-2xx response.
        status: Option<u16>,
    },

    /// A non-2xx answer from a provider, classified from its body as well as
    /// its status (see [`classify_http`]).
    #[error("API error from {provider}: {message}")]
    Upstream {
        provider: &'static str,
        status: u16,
        category: FailureCategory,
        message: String,
    },

    #[error("Authentication missing for {provider}")]
    AuthMissing { provider: &'static str },

    #[error("Configuration error: {0}")]
    Config(String),

    #[error("No providers configured for mode '{0}'")]
    NoProviders(String),

    #[error("Invalid input: {message}")]
    InvalidInput { message: String },

    #[error("all {} provider(s) failed", .failed.len())]
    AllProvidersFailed { failed: Vec<ProviderFailure> },

    #[error("DNS resolver error: {0}")]
    Resolver(String),

    #[error(transparent)]
    Http(#[from] reqwest::Error),

    #[cfg(feature = "stealth")]
    #[error(transparent)]
    Wreq(#[from] wreq::Error),

    #[error(transparent)]
    Json(#[from] serde_json::Error),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl SearchError {
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Config(_) | Self::NoProviders(_) | Self::AuthMissing { .. } => 2,
            Self::InvalidInput { .. } => 3,
            Self::AllProvidersFailed { failed } => exit_code_for_failures(failed),
            #[cfg(feature = "stealth")]
            Self::Wreq(_) => 1,
            Self::Api { .. } | Self::Upstream { .. } | Self::Http(_) | Self::Resolver(_) => 1,
            Self::Json(_) | Self::Io(_) => 1,
        }
    }

    pub fn error_code(&self) -> &'static str {
        match self {
            Self::Api { code, .. } => code,
            Self::Upstream { .. } => "api_error",
            Self::AuthMissing { .. } => "auth_missing",
            Self::Config(_) => "config_error",
            Self::NoProviders(_) => "no_providers",
            Self::InvalidInput { .. } => "invalid_input",
            Self::AllProvidersFailed { .. } => "all_providers_failed",
            Self::Resolver(_) => "resolver_error",
            #[cfg(feature = "stealth")]
            Self::Wreq(_) => "http_error",
            Self::Http(_) => "http_error",
            Self::Json(_) => "json_error",
            Self::Io(_) => "io_error",
        }
    }

    /// Coarse cause, for both the structured envelope and retry decisions.
    pub fn category(&self) -> FailureCategory {
        use FailureCategory as C;
        match self {
            Self::AuthMissing { .. } => C::Auth,
            Self::Upstream { category, .. } => *category,
            Self::Api { status, code, .. } => {
                if *code == "json_error" {
                    return C::Parse;
                }
                match status {
                    Some(s) => category_for_status(*s),
                    None => C::Other,
                }
            }
            Self::Http(e) => {
                if e.is_timeout() {
                    C::Timeout
                } else if e.is_decode() {
                    C::Parse
                } else {
                    C::Network
                }
            }
            #[cfg(feature = "stealth")]
            Self::Wreq(_) => C::Network,
            Self::Resolver(_) => C::Network,
            Self::Json(_) => C::Parse,
            Self::Config(_) | Self::NoProviders(_) => C::Config,
            Self::InvalidInput { .. } => C::BadRequest,
            Self::AllProvidersFailed { .. } | Self::Io(_) => C::Other,
        }
    }

    pub fn http_status(&self) -> Option<u16> {
        match self {
            Self::Api { status, .. } => *status,
            Self::Upstream { status, .. } => Some(*status),
            Self::Http(e) => e.status().map(|s| s.as_u16()),
            _ => None,
        }
    }

    /// True when retrying might plausibly succeed (transient causes only).
    pub fn is_retryable(&self) -> bool {
        use FailureCategory::*;
        matches!(self.category(), RateLimit | Timeout | Network | Server)
    }

    /// Build the structured per-provider failure record for the envelope.
    pub fn to_provider_failure(&self, provider: &str) -> ProviderFailure {
        ProviderFailure {
            provider: provider.to_string(),
            category: self.category(),
            http_status: self.http_status(),
            code: self.error_code().to_string(),
            reason: redact_secrets(&self.to_string()),
            retryable: self.is_retryable(),
        }
    }

    pub fn suggestion(&self) -> Option<String> {
        match self {
            Self::AuthMissing { provider } => Some(format!(
                "Set {}_API_KEY env var, or: echo YOUR_KEY | search config set keys.{} -",
                provider.to_uppercase(),
                provider
            )),
            Self::NoProviders(mode) => Some(format!(
                "No providers configured for mode '{}'. Run: search config check",
                mode
            )),
            Self::InvalidInput { .. } => Some("Check arguments with: search --help".to_string()),
            Self::AllProvidersFailed { failed } => Some(suggestion_for_failures(failed)),
            Self::Resolver(_) => Some(
                "DNS resolver could not be initialized. Check /etc/resolv.conf or network config."
                    .to_string(),
            ),
            _ => None,
        }
    }

    pub fn to_error_response(&self) -> ErrorResponse {
        let provider_failures = match self {
            Self::AllProvidersFailed { failed } => failed.clone(),
            _ => Vec::new(),
        };
        ErrorResponse {
            version: ENVELOPE_VERSION.to_string(),
            status: "error".to_string(),
            error: ErrorDetail {
                code: self.error_code().to_string(),
                message: redact_secrets(&self.to_string()),
                suggestion: self.suggestion(),
                provider_failures,
            },
        }
    }
}

/// Category of a non-2xx response: the adapter's own reading of the body
/// first, then generic body rules, then the status alone. Status alone gets
/// live providers wrong: Linkup reports an empty balance as 429, Perplexity
/// an exhausted quota as 401, Brave a dead key as 422.
pub fn classify_http(
    adapter_says: Option<FailureCategory>,
    status: u16,
    body: &str,
) -> FailureCategory {
    adapter_says
        .or_else(|| classify_body(status, body))
        .unwrap_or_else(|| category_for_status(status))
}

/// Generic body rules for 4xx answers (a 5xx body describes the provider's
/// trouble, not the caller's account). Only message text counts: in a JSON
/// body that is its string values, never its key names, so Brave's per-second
/// 429 (`"meta":{"quota_limit":2000,…}`) stays a rate limit.
pub fn classify_body(status: u16, body: &str) -> Option<FailureCategory> {
    if !(400..500).contains(&status) {
        return None;
    }
    let b = message_text(body).to_ascii_lowercase();
    const BILLING: &[&str] = &["quota", "credits", "funds", "insufficient", "balance"];
    if BILLING.iter().any(|w| b.contains(w)) {
        return Some(FailureCategory::BillingQuota);
    }
    if b.contains("unauthorized") || b.contains("unauthenticated") || invalid_credential(&b) {
        return Some(FailureCategory::Auth);
    }
    None
}

/// The human-readable part of an error body: every string value of a JSON
/// body joined by newlines, or the raw text when it is not JSON.
fn message_text(body: &str) -> String {
    fn walk(v: &serde_json::Value, out: &mut String) {
        match v {
            serde_json::Value::String(s) => {
                out.push_str(s);
                out.push('\n');
            }
            serde_json::Value::Array(items) => items.iter().for_each(|i| walk(i, out)),
            serde_json::Value::Object(map) => map.values().for_each(|i| walk(i, out)),
            _ => {}
        }
    }
    match serde_json::from_str::<serde_json::Value>(body) {
        Ok(v) => {
            let mut out = String::new();
            walk(&v, &mut out);
            out
        }
        Err(_) => body.to_string(),
    }
}

/// "invalid … key/token" or "key/token … invalid", within a short window so
/// an unrelated "invalid" elsewhere in a long body doesn't count.
fn invalid_credential(b: &str) -> bool {
    const WINDOW: usize = 40;
    let near = |from: usize, to: usize| -> &str {
        let lo = (0..=from)
            .rev()
            .find(|i| b.is_char_boundary(*i))
            .unwrap_or(0);
        let hi = (to.min(b.len())..=b.len())
            .find(|i| b.is_char_boundary(*i))
            .unwrap_or(b.len());
        &b[lo..hi]
    };
    b.match_indices("invalid").any(|(i, _)| {
        let after = near(i, i + 7 + WINDOW);
        let before = near(i.saturating_sub(WINDOW), i);
        ["key", "token"]
            .iter()
            .any(|w| after.contains(w) || before.contains(w))
    })
}

/// Status-only fallback.
pub fn category_for_status(status: u16) -> FailureCategory {
    use FailureCategory as C;
    match status {
        401 | 403 => C::Auth,
        402 => C::BillingQuota,
        429 => C::RateLimit,
        408 => C::Timeout,
        s if s >= 500 => C::Server,
        s if s >= 400 => C::BadRequest,
        _ => C::Other,
    }
}

/// Scrub credential values from user-visible strings. Transport errors can
/// embed full request URLs (SerpApi authenticates via `?api_key=` in the
/// query string), and provider error bodies sometimes echo the caller's key.
pub fn redact_secrets(s: &str) -> String {
    const MARKERS: &[&str] = &["api_key=", "apikey=", "apiKey=", "token=", "key="];
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    // Earliest marker wins each pass; ties resolve to the longer marker so
    // "api_key=" isn't half-matched as "key=".
    while let Some((pos, mlen)) = MARKERS
        .iter()
        .filter_map(|m| rest.find(m).map(|p| (p, m.len())))
        .min_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)))
    {
        out.push_str(&rest[..pos + mlen]);
        let value = &rest[pos + mlen..];
        let end = value
            .find(['&', ' ', '"', '\'', '\n'])
            .unwrap_or(value.len());
        out.push_str("REDACTED");
        rest = &value[end..];
    }
    out.push_str(rest);
    out
}

/// Exit code for total failure, derived from the underlying causes:
/// every provider blocked on auth/billing/config -> 2 (user must act);
/// every provider rate-limited -> 4; otherwise -> 1 (mixed/transient, retry may help).
fn exit_code_for_failures(failed: &[ProviderFailure]) -> i32 {
    use FailureCategory::*;
    if failed.is_empty() {
        return 1;
    }
    if failed
        .iter()
        .all(|f| matches!(f.category, Auth | BillingQuota | Config))
    {
        2
    } else if failed.iter().all(|f| matches!(f.category, RateLimit)) {
        4
    } else {
        1
    }
}

fn suggestion_for_failures(failed: &[ProviderFailure]) -> String {
    use FailureCategory::*;
    if failed
        .iter()
        .all(|f| matches!(f.category, Auth | BillingQuota | Config))
    {
        "Every provider failed on credentials/billing. Run `search config check` and verify your API keys have credit.".to_string()
    } else if failed.iter().any(|f| f.retryable) {
        "Some failures look transient. Retry, or narrow to a healthy provider with `search -p <name>`.".to_string()
    } else {
        "All providers failed. Run `search config check` to verify configuration.".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::FailureCategory;

    #[test]
    fn redacts_keys_in_urls_and_bodies() {
        assert_eq!(
            redact_secrets("GET https://serpapi.com/account.json?api_key=sk-123&x=1 failed"),
            "GET https://serpapi.com/account.json?api_key=REDACTED&x=1 failed"
        );
        assert_eq!(
            redact_secrets("body: {\"token=abc def\"}"),
            "body: {\"token=REDACTED def\"}"
        );
        assert_eq!(redact_secrets("no secrets here"), "no secrets here");
    }

    fn api(status: u16) -> SearchError {
        SearchError::Api {
            provider: "x",
            code: "api_error",
            message: "m".into(),
            status: Some(status),
        }
    }

    #[test]
    fn category_derived_from_http_status() {
        assert_eq!(api(401).category(), FailureCategory::Auth);
        assert_eq!(api(403).category(), FailureCategory::Auth);
        assert_eq!(api(402).category(), FailureCategory::BillingQuota);
        assert_eq!(api(429).category(), FailureCategory::RateLimit);
        assert_eq!(api(500).category(), FailureCategory::Server);
        assert_eq!(api(404).category(), FailureCategory::BadRequest);
    }

    #[test]
    fn only_transient_categories_retry() {
        assert!(api(503).is_retryable());
        assert!(api(429).is_retryable());
        assert!(!api(401).is_retryable());
        assert!(!api(402).is_retryable());
        assert!(!api(400).is_retryable());
    }

    #[test]
    fn all_providers_failed_exit_code_reflects_cause() {
        let auth = vec![api(401).to_provider_failure("a")];
        assert_eq!(
            SearchError::AllProvidersFailed { failed: auth }.exit_code(),
            2
        );
        let rate = vec![api(429).to_provider_failure("a")];
        assert_eq!(
            SearchError::AllProvidersFailed { failed: rate }.exit_code(),
            4
        );
        let mixed = vec![
            api(503).to_provider_failure("a"),
            api(401).to_provider_failure("b"),
        ];
        assert_eq!(
            SearchError::AllProvidersFailed { failed: mixed }.exit_code(),
            1
        );
    }

    #[test]
    fn body_rules_fix_status_only_categories() {
        use FailureCategory as C;
        // Linkup: an empty balance comes back as 429.
        let linkup =
            r#"{"error":{"code":"INSUFFICIENT_FUNDS_CREDITS","message":"Remaining funds: 0"}}"#;
        assert_eq!(classify_http(None, 429, linkup), C::BillingQuota);
        // Perplexity: an exhausted quota comes back as 401.
        let pplx = "You exceeded your current quota, please add credits";
        assert_eq!(classify_http(None, 401, pplx), C::BillingQuota);
        // Brave: a dead key comes back as 422.
        let brave = r#"{"error":{"detail":"The provided API key is invalid."}}"#;
        assert_eq!(classify_http(None, 422, brave), C::Auth);
        assert_eq!(classify_http(None, 400, "token invalid"), C::Auth);
        // An unrelated "invalid" stays a bad request.
        assert_eq!(
            classify_http(None, 400, "invalid count parameter"),
            C::BadRequest
        );
        // The adapter's own reading wins; 5xx bodies don't move the category.
        assert_eq!(classify_http(Some(C::Auth), 429, linkup), C::Auth);
        assert_eq!(classify_http(None, 503, "balance service down"), C::Server);
        assert_eq!(classify_http(None, 429, "slow down"), C::RateLimit);
    }

    /// Brave's free-plan per-second 429 carries `quota_limit` in its meta: a
    /// JSON key, not a message, so it must stay a rate limit (exit 4), not a
    /// billing failure (exit 2, "add credit").
    #[test]
    fn billing_words_in_json_keys_do_not_count() {
        use FailureCategory as C;
        let brave = r#"{"type":"ErrorResponse","error":{"status":429,"detail":"Request rate limit exceeded for plan","meta":{"plan":"Free","rate_limit":1,"rate_current":1,"quota_limit":2000,"quota_current":342},"code":"RATE_LIMITED"}}"#;
        assert_eq!(classify_http(None, 429, brave), C::RateLimit);
        let failed = vec![SearchError::Upstream {
            provider: "brave",
            status: 429,
            category: classify_http(None, 429, brave),
            message: "m".into(),
        }
        .to_provider_failure("brave")];
        assert_eq!(SearchError::AllProvidersFailed { failed }.exit_code(), 4);
        // A key named like a credential is not an auth message either.
        assert_eq!(
            classify_http(None, 400, r#"{"invalid_token_count":0,"detail":"bad q"}"#),
            C::BadRequest
        );
        // Billing words in values still count, JSON or plain text.
        assert_eq!(
            classify_http(None, 429, r#"{"error":{"code":"QUOTA_LIMITED"}}"#),
            C::BillingQuota
        );
        assert_eq!(
            classify_http(None, 403, "insufficient balance"),
            C::BillingQuota
        );
    }

    #[test]
    fn invalid_input_is_exit_3() {
        assert_eq!(
            SearchError::InvalidInput {
                message: "x".into()
            }
            .exit_code(),
            3
        );
    }
}
