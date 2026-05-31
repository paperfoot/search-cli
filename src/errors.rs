use crate::types::{ErrorDetail, ErrorResponse, FailureCategory, ProviderFailure, ENVELOPE_VERSION};
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

    #[error("Authentication missing for {provider}")]
    AuthMissing { provider: &'static str },

    #[error("Rate limited by {provider}")]
    RateLimited { provider: &'static str },

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

    #[error(transparent)]
    Rquest(#[from] rquest::Error),

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
            Self::RateLimited { .. } => 4,
            Self::AllProvidersFailed { failed } => exit_code_for_failures(failed),
            Self::Api { .. } | Self::Http(_) | Self::Rquest(_) | Self::Resolver(_) => 1,
            Self::Json(_) | Self::Io(_) => 1,
        }
    }

    pub fn error_code(&self) -> &'static str {
        match self {
            Self::Api { code, .. } => code,
            Self::AuthMissing { .. } => "auth_missing",
            Self::RateLimited { .. } => "rate_limited",
            Self::Config(_) => "config_error",
            Self::NoProviders(_) => "no_providers",
            Self::InvalidInput { .. } => "invalid_input",
            Self::AllProvidersFailed { .. } => "all_providers_failed",
            Self::Resolver(_) => "resolver_error",
            Self::Http(_) | Self::Rquest(_) => "http_error",
            Self::Json(_) => "json_error",
            Self::Io(_) => "io_error",
        }
    }

    /// Coarse cause, for both the structured envelope and retry decisions.
    pub fn category(&self) -> FailureCategory {
        use FailureCategory as C;
        match self {
            Self::AuthMissing { .. } => C::Auth,
            Self::RateLimited { .. } => C::RateLimit,
            Self::Api { status, code, .. } => {
                if *code == "json_error" {
                    return C::Parse;
                }
                match status {
                    Some(401) | Some(403) => C::Auth,
                    Some(402) => C::BillingQuota,
                    Some(429) => C::RateLimit,
                    Some(408) => C::Timeout,
                    Some(s) if *s >= 500 => C::Server,
                    Some(s) if *s >= 400 => C::BadRequest,
                    _ => C::Other,
                }
            }
            Self::Http(e) => {
                if e.is_timeout() {
                    C::Timeout
                } else {
                    C::Network
                }
            }
            Self::Rquest(_) | Self::Resolver(_) => C::Network,
            Self::Json(_) => C::Parse,
            Self::Config(_) | Self::NoProviders(_) => C::Config,
            Self::InvalidInput { .. } => C::BadRequest,
            Self::AllProvidersFailed { .. } | Self::Io(_) => C::Other,
        }
    }

    pub fn http_status(&self) -> Option<u16> {
        match self {
            Self::Api { status, .. } => *status,
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
            reason: self.to_string(),
            retryable: self.is_retryable(),
        }
    }

    pub fn suggestion(&self) -> Option<String> {
        match self {
            Self::AuthMissing { provider } => Some(format!(
                "Set {}_API_KEY env var or run: search config set keys.{} YOUR_KEY",
                provider.to_uppercase(),
                provider
            )),
            Self::NoProviders(mode) => Some(format!(
                "No providers configured for mode '{}'. Run: search config check",
                mode
            )),
            Self::RateLimited { provider } => Some(format!(
                "Rate limited by {}. Wait and retry, or use a different provider: search -p <other>",
                provider
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
                message: self.to_string(),
                suggestion: self.suggestion(),
                provider_failures,
            },
        }
    }
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
