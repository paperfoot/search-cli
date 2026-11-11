use super::spec::{FilterSupport, Price};
use super::{Adapter, Call, Output, ProviderSpec};
use crate::context::AppContext;
use crate::errors::SearchError;
#[cfg(test)]
use crate::types::SearchOpts;
use crate::types::{Mode, SearchResult};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

pub static SPEC: ProviderSpec = ProviderSpec {
    name: "exa",
    env_keys: &["EXA_API_KEY"],
    modes: &[
        Mode::General,
        Mode::Academic,
        Mode::People,
        Mode::Deep,
        Mode::Similar,
    ],
    filters: FilterSupport {
        freshness: true,
        domains: true,
        country: false,
        lang: false,
    },
    max_count: 20,
    // Deep documents 4-15 s before queueing, and a call cut off at 15 s was
    // already billed. Exa runs only in fused and single modes, which wait for
    // the mode deadline anyway, so the longer cap delays nothing else.
    timeout: Duration::from_secs(30),
    billed_on_send: true,
    price: Price {
        per_call: 7_000,
        per_extra_result: 1_000,
        included_results: 10,
    },
};

/// `type: "deep"` costs $12/1k requests (10 results), plus $1/1k per extra result.
const DEEP: Price = Price {
    per_call: 12_000,
    per_extra_result: 1_000,
    included_results: 10,
};
/// People and similar ask Exa for at most this much page text, and the
/// snippet is clipped to it, so a whole page never lands in an agent's context.
const SNIPPET_MAX: usize = 2_000;

pub struct Exa {
    ctx: Arc<AppContext>,
}

impl Exa {
    pub fn new(ctx: Arc<AppContext>) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct ExaResponse {
    results: Option<Vec<ExaResult>>,
    #[serde(rename = "costDollars")]
    cost_dollars: Option<CostDollars>,
}

#[derive(Deserialize)]
struct CostDollars {
    total: Option<f64>,
}

#[derive(Deserialize)]
struct ExaResult {
    title: Option<String>,
    url: Option<String>,
    text: Option<String>,
    #[serde(rename = "publishedDate")]
    published_date: Option<String>,
    highlights: Option<Vec<String>>,
}

fn source(mode: Mode) -> &'static str {
    match mode {
        Mode::People => "exa_people",
        Mode::Similar => "exa_similar",
        _ => "exa",
    }
}

fn is_http(u: &str) -> bool {
    let l = u.get(..8).unwrap_or(u).to_ascii_lowercase();
    l.starts_with("https://") || l.starts_with("http://")
}

fn clip(s: &str) -> String {
    let s = s.trim();
    if s.chars().count() <= SNIPPET_MAX {
        return s.to_string();
    }
    let mut out: String = s.chars().take(SNIPPET_MAX - 1).collect();
    out.push('…');
    out
}

/// Results plus the cost: Exa's own `costDollars.total` when present, else
/// the list price for the type that was sent (only deep differs from SPEC).
fn parse(mode: Mode, exa: ExaResponse) -> Output {
    let results: Vec<SearchResult> = exa
        .results
        .unwrap_or_default()
        .into_iter()
        .filter_map(|r| {
            let url = r.url.filter(|u| is_http(u))?;
            // Highlights are the relevant passages; text is the fallback.
            let snippet = match r.highlights {
                Some(hl) if !hl.is_empty() => hl.join(" … "),
                _ => r.text.unwrap_or_default(),
            };
            Some(SearchResult {
                title: r.title.unwrap_or_default(),
                url,
                snippet: clip(&snippet),
                source: source(mode).to_string(),
                published: r.published_date,
                image_url: None,
                extra: None,
            })
        })
        .collect();
    let reported = exa
        .cost_dollars
        .and_then(|c| c.total)
        .filter(|t| t.is_finite() && *t >= 0.0)
        // USD to µ$, rounded so 0.009 is 9,000 and not 8,999.
        .map(|t| (t * 1e6).round() as u64);
    let cost_micros = reported.or_else(|| {
        (mode == Mode::Deep).then(|| DEEP.estimate(results.len().min(u16::MAX as usize) as u16))
    });
    Output {
        results,
        answers: Vec::new(),
        cost_micros,
    }
}

/// The path and body for one call. Search modes buy highlights only (full
/// text was billed and thrown away); people and similar buy clipped text.
/// People sends no domain or date filters (Exa answers 400 to them with
/// category "people"), and similar takes none: the engine warns for both.
fn body(call: &Call<'_>) -> (&'static str, serde_json::Value) {
    let text = json!({ "text": { "maxCharacters": SNIPPET_MAX } });
    let highlights = json!({ "highlights": true });
    match call.mode {
        Mode::People => (
            "search",
            json!({
                "query": call.query, "numResults": call.count, "type": "auto",
                "category": "people", "contents": text
            }),
        ),
        Mode::Similar => (
            "findSimilar",
            json!({ "url": call.query, "numResults": call.count, "contents": text }),
        ),
        mode => {
            let mut body = json!({
                "query": call.query,
                "numResults": call.count,
                "type": if mode == Mode::Deep { "deep" } else { "auto" },
                "contents": highlights,
            });
            // "publication" is Exa's scholarly category; "research paper" is
            // no longer in its enum and only a weak hint.
            if mode == Mode::Academic {
                body["category"] = json!("publication");
            }
            let opts = call.opts;
            if !opts.include_domains.is_empty() {
                body["includeDomains"] = json!(opts.include_domains);
            }
            if !opts.exclude_domains.is_empty() {
                body["excludeDomains"] = json!(opts.exclude_domains);
            }
            if let Some(days) = opts.freshness.as_deref().and_then(super::freshness_days) {
                body["startPublishedDate"] =
                    json!(format!("{}T00:00:00Z", super::date_days_ago(days)));
            }
            ("search", body)
        }
    }
}

fn request(client: &reqwest::Client, key: &str, call: &Call<'_>) -> reqwest::RequestBuilder {
    let (path, body) = body(call);
    client
        .post(format!("https://api.exa.ai/{path}"))
        .header("x-api-key", key)
        .header("Content-Type", "application/json")
        .json(&body)
}

#[async_trait]
impl Adapter for Exa {
    fn spec(&self) -> &'static ProviderSpec {
        &SPEC
    }

    fn keys(&self) -> Vec<String> {
        super::keys_for(&self.ctx.config, &SPEC)
    }

    fn is_configured(&self) -> bool {
        !self.keys().is_empty()
    }

    /// Deep sends `type: "deep"`, which costs more than the base call.
    fn sent_cost(&self, mode: Mode) -> u64 {
        if mode == Mode::Deep {
            DEEP.estimate(0)
        } else {
            SPEC.price.estimate(0)
        }
    }

    async fn call(&self, call: &Call<'_>) -> Result<Output, SearchError> {
        if !self.is_configured() {
            return Err(SearchError::AuthMissing { provider: "exa" });
        }
        let key = call.key;
        let client = &self.ctx.client;
        let resp = super::send(self, call.timeout, || request(client, key, call)).await?;
        let body: ExaResponse = resp.json().await?;
        Ok(parse(call.mode, body))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;

    fn sent(mode: Mode, opts: &SearchOpts) -> (reqwest::Request, serde_json::Value) {
        let call = Call {
            mode,
            query: "rust async",
            count: 12,
            opts,
            timeout: Duration::from_secs(1),
            key: "",
        };
        let req = request(&reqwest::Client::new(), "test-key", &call)
            .build()
            .unwrap();
        let body = serde_json::from_slice(req.body().unwrap().as_bytes().unwrap()).unwrap();
        (req, body)
    }

    fn filtered() -> SearchOpts {
        SearchOpts {
            include_domains: vec!["a.com".into()],
            exclude_domains: vec!["b.com".into()],
            freshness: Some("week".into()),
            ..SearchOpts::default()
        }
    }

    #[test]
    fn search_modes_buy_highlights_only() {
        let none = SearchOpts::default();
        let (req, general) = sent(Mode::General, &none);
        assert_eq!(req.url().as_str(), "https://api.exa.ai/search");
        assert!(req.headers().contains_key("x-api-key"));
        assert_eq!(
            general,
            json!({"query": "rust async", "numResults": 12, "type": "auto", "contents": {"highlights": true}})
        );
        let (_, academic) = sent(Mode::Academic, &none);
        assert_eq!(academic["category"], "publication");
        assert_eq!(academic["contents"], json!({"highlights": true}));
        let (_, deep) = sent(Mode::Deep, &filtered());
        assert_eq!(deep["type"], "deep");
        assert_eq!(deep["contents"], json!({"highlights": true}));
        assert_eq!(deep["includeDomains"], json!(["a.com"]));
        assert_eq!(deep["excludeDomains"], json!(["b.com"]));
        assert!(deep["startPublishedDate"]
            .as_str()
            .unwrap()
            .ends_with("T00:00:00Z"));
    }

    #[test]
    fn people_and_similar_buy_clipped_text_and_send_no_filters() {
        let (req, people) = sent(Mode::People, &filtered());
        assert_eq!(req.url().path(), "/search");
        assert_eq!(
            people,
            json!({"query": "rust async", "numResults": 12, "type": "auto", "category": "people",
                   "contents": {"text": {"maxCharacters": 2000}}})
        );
        let (req, similar) = sent(Mode::Similar, &filtered());
        assert_eq!(req.url().path(), "/findSimilar");
        assert_eq!(
            similar,
            json!({"url": "rust async", "numResults": 12, "contents": {"text": {"maxCharacters": 2000}}})
        );
    }

    fn fixture(raw: &str) -> ExaResponse {
        serde_json::from_str(raw).unwrap()
    }

    #[test]
    fn reported_cost_wins_and_rounds() {
        let general = parse(
            Mode::General,
            fixture(include_str!("../../tests/fixtures/exa/general.json")),
        );
        assert_eq!(general.cost_micros, Some(7_000));
        assert_eq!(general.results.len(), 3);
        assert!(general.results.iter().all(|r| !r.snippet.is_empty()));
        let deep = parse(
            Mode::Deep,
            fixture(include_str!("../../tests/fixtures/exa/deep.json")),
        );
        assert_eq!(deep.cost_micros, Some(12_000));
        // The measured $0.009 for 12 results is 9,000 µ$, not 8,999.
        let twelve = parse(
            Mode::General,
            fixture(r#"{"results": [], "costDollars": {"total": 0.009}}"#),
        );
        assert_eq!(twelve.cost_micros, Some(9_000));
    }

    #[test]
    fn deep_without_a_reported_cost_is_priced_as_deep() {
        let exa = Exa::new(Arc::new(AppContext::new(AppConfig::default())));
        assert_eq!(exa.sent_cost(Mode::Deep), 12_000);
        assert_eq!(exa.sent_cost(Mode::General), 7_000);
        let rows: Vec<serde_json::Value> = (0..12)
            .map(|i| json!({"url": format!("https://x.com/{i}"), "highlights": ["h"]}))
            .collect();
        let deep = parse(Mode::Deep, fixture(&json!({ "results": rows }).to_string()));
        assert_eq!(deep.cost_micros, Some(14_000));
        let general = parse(Mode::General, fixture(r#"{"results": []}"#));
        assert_eq!(general.cost_micros, None, "the engine's estimate applies");
    }

    #[test]
    fn page_text_is_clipped_and_bad_urls_dropped() {
        let long = "x".repeat(10_000);
        let body = json!({"results": [
            {"url": "https://p.com/a", "title": "A", "text": long},
            {"url": "ftp://p.com/b", "text": "dropped"}
        ]});
        let out = parse(Mode::People, fixture(&body.to_string()));
        assert_eq!(out.results.len(), 1);
        assert_eq!(out.results[0].snippet.chars().count(), SNIPPET_MAX);
        assert_eq!(out.results[0].source, "exa_people");
    }
}
