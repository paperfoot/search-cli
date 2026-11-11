use super::direct::{marked_output, unsupported_unless_extract};
use super::spec::{FilterSupport, Price};
use super::{Adapter, Call, Output, ProviderSpec};
use crate::context::AppContext;
use crate::errors::SearchError;
use crate::page::Page;
use crate::types::Mode;
use async_trait::async_trait;
use std::sync::Arc;
use std::time::Duration;

pub static SPEC: ProviderSpec = ProviderSpec {
    name: "browserless",
    env_keys: &["BROWSERLESS_API_KEY"],
    modes: &[Mode::Extract],
    filters: FilterSupport {
        freshness: false,
        domains: false,
        country: false,
        lang: false,
    },
    max_count: 1,
    timeout: Duration::from_secs(60),
    billed_on_send: true,
    price: Price {
        per_call: 4_000,
        per_extra_result: 0,
        included_results: 0,
    },
};

pub struct Browserless {
    ctx: Arc<AppContext>,
}

impl Browserless {
    pub fn new(ctx: Arc<AppContext>) -> Self {
        Self { ctx }
    }
}

#[async_trait]
impl Adapter for Browserless {
    fn spec(&self) -> &'static ProviderSpec {
        &SPEC
    }

    fn keys(&self) -> Vec<String> {
        super::keys_for(&self.ctx.config, &SPEC)
    }

    fn is_configured(&self) -> bool {
        !self.keys().is_empty()
    }

    /// Render the page in Browserless's cloud browser (the anti-bot last
    /// resort), then convert the rendered HTML to markdown locally. The
    /// verdict on the rendered page is marked on the result, so the chain
    /// rejects a challenge or an empty render and still counts the cost.
    async fn call(&self, call: &Call<'_>) -> Result<Output, SearchError> {
        unsupported_unless_extract("browserless", call.mode)?;
        if !self.is_configured() {
            return Err(SearchError::AuthMissing {
                provider: "browserless",
            });
        }
        crate::registry::rung_gate("browserless", call.query)?;
        let url = call.query;
        let body = serde_json::json!({
            "url": url,
            "waitForSelector": { "selector": "body", "timeout": 10000 }
        });
        let client = &self.ctx.client;
        let token = call.key;

        // Browserless authenticates with `?token=`; `send` strips the URL
        // from transport errors, and `redact` keeps it out of the rest.
        let resp = super::send(self, call.timeout, || {
            client
                .post(ENDPOINT)
                .query(&[("token", token)])
                .header("Content-Type", "application/json")
                .json(&body)
        })
        .await
        .map_err(|e| redact(e, token))?;
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
            .or_else(|| Some("text/html".to_string()));
        let html = resp.text().await.map_err(|e| redact(e.into(), token))?;
        let page = Page {
            final_url: url.to_string(),
            status: 200,
            content_type,
            body: html,
            headers: reqwest::header::HeaderMap::new(),
        };
        Ok(marked_output("browserless", page).await)
    }
}

const ENDPOINT: &str = "https://production-sfo.browserless.io/content";

/// Remove the token from an error's text.
fn redact(e: SearchError, token: &str) -> SearchError {
    if token.is_empty() || !e.to_string().contains(token) {
        return e;
    }
    SearchError::Upstream {
        provider: "browserless",
        status: e.http_status().unwrap_or(0),
        category: e.category(),
        message: e.to_string().replace(token, "[redacted]"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_token_never_reaches_an_error_message() {
        let e = SearchError::Upstream {
            provider: "browserless",
            status: 401,
            category: crate::types::FailureCategory::Auth,
            message: "HTTP 401: bad token sk-secret-123".into(),
        };
        let shown = redact(e, "sk-secret-123").to_string();
        assert!(!shown.contains("sk-secret-123"), "{shown}");
        assert!(shown.contains("[redacted]"));
    }

    #[tokio::test]
    async fn a_challenge_render_is_marked_for_rejection() {
        let page = Page {
            final_url: "https://x.example/".into(),
            status: 200,
            content_type: Some("text/html".into()),
            body: "<html><head><title>Just a moment...</title></head><body>Checking your browser before accessing x.example</body></html>".into(),
            headers: reqwest::header::HeaderMap::new(),
        };
        let out = marked_output("browserless", page).await;
        assert!(crate::registry::accept(Mode::Extract, &out).is_err());
        assert_eq!(
            out.results[0].extra.as_ref().unwrap()["verdict"],
            "challenge"
        );
    }
}
