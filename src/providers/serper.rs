use super::spec::{FilterSupport, Price};
use super::{Adapter, Call, Output, ProviderSpec};
use crate::context::AppContext;
use crate::errors::SearchError;
use crate::types::{Mode, SearchOpts, SearchResult};
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

pub static SPEC: ProviderSpec = ProviderSpec {
    name: "serper",
    env_keys: &["SERPER_API_KEY"],
    modes: &[
        Mode::General,
        Mode::News,
        Mode::Academic,
        Mode::Deep,
        Mode::Scholar,
        Mode::Patents,
        Mode::Images,
        Mode::Places,
    ],
    filters: FilterSupport {
        freshness: true,
        domains: true,
        country: true,
        lang: true,
    },
    // Google returns at most 10 per page since Sept 2025; more would be a
    // second billed page.
    max_count: 10,
    timeout: Duration::from_secs(15),
    billed_on_send: true,
    price: Price {
        per_call: 1_000,
        per_extra_result: 0,
        included_results: 0,
    },
};

pub struct Serper {
    ctx: Arc<AppContext>,
}

impl Serper {
    pub fn new(ctx: Arc<AppContext>) -> Self {
        Self { ctx }
    }

    fn api_key(&self) -> String {
        super::key_for(&self.ctx.config, &SPEC)
    }

    async fn query_endpoint(
        &self,
        endpoint: &str,
        call: &Call<'_>,
        opts: &SearchOpts,
    ) -> Result<serde_json::Value, SearchError> {
        // Serper uses site: in query for domain filtering
        let q = super::augment_query(call.query, opts);
        let url = format!("https://google.serper.dev/{endpoint}");
        let client = &self.ctx.client;
        let api_key = self.api_key();

        let mut body = json!({ "q": q, "num": call.count });
        // Serper freshness via tbs param: qdr:d, qdr:w, qdr:m, qdr:y
        if let Some(f) = &opts.freshness {
            let tbs = match f.as_str() {
                "day" => "qdr:d",
                "week" => "qdr:w",
                "month" => "qdr:m",
                "year" => "qdr:y",
                other => other,
            };
            body["tbs"] = json!(tbs);
        }
        if let Some(country) = &opts.country {
            body["gl"] = json!(country.to_lowercase());
        }
        if let Some(lang) = &opts.lang {
            body["hl"] = json!(lang.to_lowercase());
        }

        let resp = super::send(self, call.timeout, || {
            client
                .post(&url)
                .header("X-API-KEY", api_key.as_str())
                .header("Content-Type", "application/json")
                .json(&body)
        })
        .await?;
        Ok(resp.json().await?)
    }
}

fn parse_organic(body: &serde_json::Value, source: &str) -> Vec<SearchResult> {
    let key = match source {
        "serper_news" => "news",
        "serper_images" => "images",
        "serper_places" => "places",
        _ => "organic",
    };

    let items = body.get(key).and_then(|v| v.as_array());
    items
        .map(|arr| {
            arr.iter()
                .map(|item| {
                    let str_of = |k: &str| item.get(k).and_then(|v| v.as_str()).map(String::from);
                    SearchResult {
                        title: str_of("title").unwrap_or_default(),
                        url: str_of("link").unwrap_or_default(),
                        snippet: str_of("snippet").unwrap_or_default(),
                        source: source.to_string(),
                        published: str_of("date"),
                        image_url: str_of("imageUrl"),
                        extra: None,
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

#[async_trait]
impl Adapter for Serper {
    fn spec(&self) -> &'static ProviderSpec {
        &SPEC
    }

    fn is_configured(&self) -> bool {
        !self.api_key().is_empty()
    }

    async fn call(&self, call: &Call<'_>) -> Result<Output, SearchError> {
        if !self.is_configured() {
            return Err(SearchError::AuthMissing { provider: "serper" });
        }
        // The vertical endpoints take no filters (the engine warns).
        let (endpoint, source, opts) = match call.mode {
            Mode::News => ("news", "serper_news", call.opts),
            Mode::Scholar => ("scholar", "serper_scholar", &super::NO_FILTERS),
            Mode::Patents => ("patents", "serper_patents", &super::NO_FILTERS),
            Mode::Images => ("images", "serper_images", &super::NO_FILTERS),
            Mode::Places => ("places", "serper_places", &super::NO_FILTERS),
            _ => ("search", "serper", call.opts),
        };
        let body = self.query_endpoint(endpoint, call, opts).await?;
        Ok(Output {
            results: parse_organic(&body, source),
            ..Output::default()
        })
    }
}
