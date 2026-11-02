use super::spec::{FilterSupport, Price};
use super::{Adapter, Call, Output, ProviderSpec};
use crate::context::AppContext;
use crate::errors::SearchError;
use crate::types::{Mode, SearchOpts, SearchResult};
use async_trait::async_trait;
use std::sync::Arc;
use std::time::Duration;

pub static SPEC: ProviderSpec = ProviderSpec {
    name: "serpapi",
    env_keys: &["SERPAPI_API_KEY"],
    modes: &[Mode::Scholar],
    // Scholar sends no filters today (the engine warns).
    filters: FilterSupport {
        freshness: false,
        domains: false,
        country: false,
        lang: false,
    },
    max_count: 20,
    timeout: Duration::from_secs(15),
    billed_on_send: true,
    price: Price {
        per_call: 25_000,
        per_extra_result: 0,
        included_results: 0,
    },
};

pub struct SerpApi {
    ctx: Arc<AppContext>,
}

impl SerpApi {
    pub fn new(ctx: Arc<AppContext>) -> Self {
        Self { ctx }
    }

    fn api_key(&self) -> String {
        super::key_for(&self.ctx.config, &SPEC)
    }

    async fn query_endpoint(
        &self,
        engine: &str,
        call: &Call<'_>,
        opts: &SearchOpts,
    ) -> Result<serde_json::Value, SearchError> {
        let client = &self.ctx.client;
        let mut params = vec![
            ("engine", engine.to_string()),
            ("q", super::augment_query(call.query, opts)),
            ("num", call.count.to_string()),
            ("api_key", self.api_key()),
        ];
        if let Some(f) = &opts.freshness {
            let tbs = match f.as_str() {
                "day" => "qdr:d",
                "week" => "qdr:w",
                "month" => "qdr:m",
                "year" => "qdr:y",
                other => other,
            };
            params.push(("tbs", tbs.to_string()));
        }
        if let Some(country) = &opts.country {
            params.push(("gl", country.to_lowercase()));
        }
        if let Some(lang) = &opts.lang {
            params.push(("hl", lang.to_lowercase()));
        }

        let resp = super::send(self, call.timeout, || {
            client.get("https://serpapi.com/search").query(&params)
        })
        .await?;
        Ok(resp.json().await?)
    }
}

fn parse_results(body: &serde_json::Value, key: &str, source: &str) -> Vec<SearchResult> {
    let items = body.get(key).and_then(|v| v.as_array());
    items
        .map(|arr| {
            arr.iter()
                .map(|item| {
                    let str_of = |k: &str| item.get(k).and_then(|v| v.as_str()).map(String::from);
                    // Scholar results carry citation info in extra.
                    let mut map = serde_json::Map::new();
                    for k in ["publication_info", "cited_by"] {
                        if let Some(v) = item.get(k) {
                            map.insert(k.to_string(), v.clone());
                        }
                    }
                    SearchResult {
                        title: str_of("title").unwrap_or_default(),
                        url: str_of("link").unwrap_or_default(),
                        snippet: str_of("snippet").unwrap_or_default(),
                        source: source.to_string(),
                        published: str_of("date"),
                        image_url: None,
                        extra: (!map.is_empty()).then_some(serde_json::Value::Object(map)),
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

#[async_trait]
impl Adapter for SerpApi {
    fn spec(&self) -> &'static ProviderSpec {
        &SPEC
    }

    fn is_configured(&self) -> bool {
        !self.api_key().is_empty()
    }

    async fn call(&self, call: &Call<'_>) -> Result<Output, SearchError> {
        if !self.is_configured() {
            return Err(SearchError::AuthMissing {
                provider: "serpapi",
            });
        }
        let body = self
            .query_endpoint("google_scholar", call, &super::NO_FILTERS)
            .await?;
        Ok(Output {
            results: parse_results(&body, "organic_results", "serpapi_scholar"),
            ..Output::default()
        })
    }
}
