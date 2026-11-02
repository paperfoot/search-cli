use super::spec::{FilterSupport, Price};
use super::{Adapter, Call, Output, ProviderSpec};
use crate::context::AppContext;
use crate::errors::SearchError;
use crate::types::{Mode, SearchResult};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

pub static SPEC: ProviderSpec = ProviderSpec {
    name: "linkup",
    env_keys: &["LINKUP_API_KEY"],
    modes: &[Mode::General, Mode::News, Mode::Deep],
    filters: FilterSupport {
        freshness: true,
        domains: true,
        country: false,
        lang: false,
    },
    max_count: 20,
    timeout: Duration::from_secs(15),
    billed_on_send: true,
    price: Price {
        per_call: 5_000,
        per_extra_result: 0,
        included_results: 0,
    },
};

pub struct Linkup {
    ctx: Arc<AppContext>,
}

impl Linkup {
    pub fn new(ctx: Arc<AppContext>) -> Self {
        Self { ctx }
    }

    fn api_key(&self) -> String {
        super::key_for(&self.ctx.config, &SPEC)
    }
}

#[derive(Deserialize)]
struct LinkupResponse {
    results: Option<Vec<LinkupResult>>,
}

#[derive(Deserialize)]
struct LinkupResult {
    #[serde(rename = "type")]
    result_type: Option<String>,
    name: Option<String>,
    url: Option<String>,
    content: Option<String>,
}

#[async_trait]
impl Adapter for Linkup {
    fn spec(&self) -> &'static ProviderSpec {
        &SPEC
    }

    fn is_configured(&self) -> bool {
        !self.api_key().is_empty()
    }

    // Every mode sends `depth: "standard"` today, so deep costs the base
    // price. A lane that switches deep to `depth: "deep"` must set
    // `cost_micros` to 50,000.
    async fn call(&self, call: &Call<'_>) -> Result<Output, SearchError> {
        if !self.is_configured() {
            return Err(SearchError::AuthMissing { provider: "linkup" });
        }
        let opts = call.opts;
        let mut body = json!({
            "q": call.query,
            "depth": "standard",
            "outputType": "searchResults",
            "maxResults": call.count,
        });
        if !opts.include_domains.is_empty() {
            body["includeDomains"] = json!(opts.include_domains);
        }
        if !opts.exclude_domains.is_empty() {
            body["excludeDomains"] = json!(opts.exclude_domains);
        }
        // Freshness maps to fromDate (YYYY-MM-DD). News search without an
        // explicit -f defaults to the last week, since Linkup has no news
        // vertical of its own.
        let default_days = (call.mode == Mode::News).then_some(7);
        let days_back = opts
            .freshness
            .as_deref()
            .and_then(super::freshness_days)
            .or(default_days);
        if let Some(days) = days_back {
            body["fromDate"] = json!(super::date_days_ago(days));
        }

        let client = &self.ctx.client;
        let key = self.api_key();
        let resp = super::send(self, call.timeout, || {
            client
                .post("https://api.linkup.so/v1/search")
                .bearer_auth(&key)
                .json(&body)
        })
        .await?;
        let resp: LinkupResponse = resp.json().await?;

        let results = resp
            .results
            .unwrap_or_default()
            .into_iter()
            .filter(|r| r.result_type.as_deref() != Some("image"))
            .map(|r| SearchResult {
                title: r.name.unwrap_or_default(),
                url: r.url.unwrap_or_default(),
                snippet: r.content.unwrap_or_default(),
                source: "linkup".to_string(),
                published: None,
                image_url: None,
                extra: None,
            })
            .filter(|r| !r.url.is_empty())
            .collect();
        Ok(Output {
            results,
            ..Output::default()
        })
    }
}
