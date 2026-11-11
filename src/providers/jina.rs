use super::spec::{FilterSupport, Price};
use super::{Adapter, Call, Output, ProviderSpec};
use crate::context::AppContext;
use crate::errors::SearchError;
use crate::page::Page;
use crate::types::{Mode, SearchResult};
use async_trait::async_trait;
use serde::Deserialize;
use std::sync::Arc;
use std::time::Duration;

pub static SPEC: ProviderSpec = ProviderSpec {
    name: "jina",
    env_keys: &["JINA_API_KEY"],
    modes: &[Mode::General, Mode::Extract],
    filters: FilterSupport {
        freshness: false,
        domains: true,
        country: false,
        lang: false,
    },
    max_count: 20,
    timeout: Duration::from_secs(20),
    billed_on_send: true,
    price: Price {
        per_call: 500,
        per_extra_result: 0,
        included_results: 0,
    },
};

/// Reader (extract) estimate when Jina reports no token count: 250 µ$,
/// the server's quote (metasearch `providers/jina.ts`). Reader has no
/// 10k-token minimum, so it costs less than a search (`SPEC.price`).
const READER: Price = Price {
    per_call: 250,
    per_extra_result: 0,
    included_results: 0,
};

pub struct Jina {
    ctx: Arc<AppContext>,
}

impl Jina {
    pub fn new(ctx: Arc<AppContext>) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct JinaSearchResponse {
    data: Option<Vec<JinaResult>>,
}

#[derive(Deserialize)]
struct JinaResult {
    title: Option<String>,
    url: Option<String>,
    description: Option<String>,
    content: Option<String>,
}

#[async_trait]
impl Adapter for Jina {
    fn spec(&self) -> &'static ProviderSpec {
        &SPEC
    }

    fn keys(&self) -> Vec<String> {
        super::keys_for(&self.ctx.config, &SPEC)
    }

    fn is_configured(&self) -> bool {
        !self.keys().is_empty()
    }

    fn sent_cost(&self, mode: Mode) -> u64 {
        match mode {
            Mode::Extract | Mode::Scrape => READER.estimate(0),
            _ => SPEC.price.estimate(0),
        }
    }

    async fn call(&self, call: &Call<'_>) -> Result<Output, SearchError> {
        if !self.is_configured() {
            return Err(SearchError::AuthMissing { provider: "jina" });
        }
        match call.mode {
            Mode::Extract | Mode::Scrape => self.read_url(call).await,
            Mode::General => Ok(Output {
                results: self.search(call).await?,
                ..Output::default()
            }),
            other => Err(super::direct::unsupported("jina", other)),
        }
    }
}

impl Jina {
    async fn search(&self, call: &Call<'_>) -> Result<Vec<SearchResult>, SearchError> {
        let client = &self.ctx.client;
        let auth = format!("Bearer {}", call.key);
        let count = call.count.to_string();
        // No native domain filters: rewrite the query with site: operators.
        let q = super::augment_query(call.query, call.opts);

        let resp = super::send(self, call.timeout, || {
            client
                .get("https://s.jina.ai/")
                .header("Authorization", &auth)
                .header("Accept", "application/json")
                .header("X-Retain-Images", "none")
                .query(&[("q", q.as_str()), ("count", count.as_str())])
        })
        .await?;

        let body: JinaSearchResponse = resp.json().await?;
        Ok(body
            .data
            .unwrap_or_default()
            .into_iter()
            .map(|r| SearchResult {
                title: r.title.unwrap_or_default(),
                url: r.url.unwrap_or_default(),
                snippet: r.description.or(r.content).unwrap_or_default(),
                source: "jina".to_string(),
                published: None,
                image_url: None,
                extra: None,
            })
            .collect())
    }

    /// Jina Reader: renders the page (JS included) or reads the PDF and
    /// returns markdown. Sent as a POST with the URL in the body, because a
    /// GET to `r.jina.ai/<url>` loses the fragment and mangles query strings.
    async fn read_url(&self, call: &Call<'_>) -> Result<Output, SearchError> {
        crate::registry::rung_gate("jina", call.query)?;
        let url = call.query;
        let client = &self.ctx.client;
        let auth = format!("Bearer {}", call.key);

        let resp = super::send(self, call.timeout, || reader_request(client, &auth, url)).await?;
        let body: serde_json::Value = resp.json().await?;
        let field = |k: &str| {
            body.pointer(&format!("/data/{k}"))
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string()
        };
        let final_url = Some(field("url"))
            .filter(|u| !u.is_empty())
            .unwrap_or_else(|| url.to_string());
        let title = field("title");
        let mut content = field("content");
        if !title.is_empty() && !content.trim_start().starts_with('#') {
            content = format!("# {title}\n\n{content}");
        }
        let page = Page {
            final_url,
            status: 200,
            content_type: Some("text/markdown".to_string()),
            body: content,
            headers: reqwest::header::HeaderMap::new(),
        };
        let mut out = super::direct::marked_output("jina_reader", page).await;
        if let Some(r) = out.results.first_mut().filter(|_| !title.is_empty()) {
            r.title = title;
        }
        out.cost_micros = Some(reader_cost(&body).unwrap_or(READER.estimate(0)));
        Ok(out)
    }
}

/// Reader bills by the tokens it reports: $0.05 per million, so
/// ceil(tokens × 0.05) µ$ (PLAN D4, provider-reported cost wins). `None`
/// when the field is missing, and the reader estimate applies.
fn reader_cost(body: &serde_json::Value) -> Option<u64> {
    let tokens = body.pointer("/data/usage/tokens")?.as_u64()?;
    Some(tokens.div_ceil(20))
}

fn reader_request(client: &reqwest::Client, auth: &str, url: &str) -> reqwest::RequestBuilder {
    client
        .post("https://r.jina.ai/")
        .header("Authorization", auth)
        .header("Accept", "application/json")
        .header("X-Retain-Images", "none")
        .json(&serde_json::json!({ "url": url }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_reader_is_estimated_below_a_search() {
        let jina = Jina::new(Arc::new(AppContext::new(
            crate::config::AppConfig::default(),
        )));
        assert_eq!(jina.sent_cost(Mode::Extract), 250);
        assert_eq!(jina.sent_cost(Mode::General), 500);
    }

    #[test]
    fn reader_cost_comes_from_reported_tokens() {
        let body = |t: u64| serde_json::json!({"data": {"content": "x", "usage": {"tokens": t}}});
        assert_eq!(reader_cost(&body(100_000)), Some(5_000));
        assert_eq!(reader_cost(&body(2_000)), Some(100));
        assert_eq!(reader_cost(&body(1)), Some(1));
        assert_eq!(
            reader_cost(&serde_json::json!({"data": {"content": "x"}})),
            None
        );
    }
    use crate::config::AppConfig;
    use crate::types::SearchOpts;

    #[test]
    fn the_reader_is_a_post_with_the_url_in_the_body() {
        let client = reqwest::Client::new();
        let url = "https://example.com/a?b=1&c=2#section";
        let req = reader_request(&client, "Bearer k", url).build().unwrap();
        assert_eq!(req.method(), reqwest::Method::POST);
        assert_eq!(req.url().as_str(), "https://r.jina.ai/");
        let body: serde_json::Value =
            serde_json::from_slice(req.body().unwrap().as_bytes().unwrap()).unwrap();
        assert_eq!(body["url"], url, "fragment and query survive");
    }

    #[tokio::test]
    async fn modes_it_does_not_serve_are_errors_not_empty() {
        let mut config = AppConfig::default();
        config.keys.jina = crate::config::KeyList(vec!["k".into()]);
        let jina = Jina::new(Arc::new(AppContext::new(config)));
        let opts = SearchOpts::default();
        let err = jina
            .call(&Call {
                mode: Mode::News,
                query: "x",
                count: 1,
                opts: &opts,
                timeout: Duration::from_secs(1),
                key: "k",
            })
            .await
            .unwrap_err();
        assert_eq!(err.error_code(), "unsupported_mode");
    }
}
