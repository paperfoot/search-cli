use super::spec::{FilterSupport, Price};
use super::{Adapter, Call, Output, ProviderSpec};
use crate::context::AppContext;
use crate::errors::SearchError;
use crate::types::{Answer, FailureCategory, Mode, SearchResult};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

pub static SPEC: ProviderSpec = ProviderSpec {
    name: "tavily",
    env_keys: &["TAVILY_API_KEY"],
    modes: &[Mode::General, Mode::News, Mode::Academic, Mode::Deep],
    filters: FilterSupport {
        freshness: true,
        domains: true,
        country: false,
        lang: false,
    },
    max_count: 20,
    timeout: Duration::from_secs(20),
    billed_on_send: true,
    price: Price {
        per_call: 8_000,
        per_extra_result: 0,
        included_results: 0,
    },
};

/// PAYG: one credit is $0.008. Basic costs one credit, advanced two.
const MICROS_PER_CREDIT: u64 = 8_000;

pub struct Tavily {
    ctx: Arc<AppContext>,
}

impl Tavily {
    pub fn new(ctx: Arc<AppContext>) -> Self {
        Self { ctx }
    }
}

/// Academic and deep buy the advanced depth; general and news the basic one.
fn advanced(mode: Mode) -> bool {
    matches!(mode, Mode::Academic | Mode::Deep)
}

/// What a call at this mode's depth costs: 1 credit basic, 2 advanced.
fn depth_micros(mode: Mode) -> u64 {
    MICROS_PER_CREDIT * if advanced(mode) { 2 } else { 1 }
}

#[derive(Deserialize)]
struct TavilyResponse {
    #[serde(default)]
    results: Vec<TavilyResult>,
    answer: Option<String>,
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct Usage {
    credits: Option<f64>,
}

#[derive(Deserialize)]
struct TavilyResult {
    title: Option<String>,
    url: Option<String>,
    content: Option<String>,
    score: Option<f64>,
    published_date: Option<String>,
}

fn request(client: &reqwest::Client, key: &str, call: &Call<'_>) -> reqwest::RequestBuilder {
    let opts = call.opts;
    let mut body = json!({
        "query": call.query,
        "search_depth": if advanced(call.mode) { "advanced" } else { "basic" },
        "topic": if call.mode == Mode::News { "news" } else { "general" },
        "max_results": call.count.min(SPEC.max_count),
        "include_answer": "basic",
        "include_raw_content": false,
        "include_usage": true,
        // Without this Tavily returns dates only for topic news.
        "include_published_date": true,
    });
    if !opts.include_domains.is_empty() {
        body["include_domains"] = json!(opts.include_domains);
    }
    if !opts.exclude_domains.is_empty() {
        body["exclude_domains"] = json!(opts.exclude_domains);
    }
    // Tavily time_range: day, week, month, year
    if let Some(f) = &opts.freshness {
        body["time_range"] = json!(f);
    }
    client
        .post("https://api.tavily.com/search")
        .bearer_auth(key)
        .json(&body)
}

fn is_http(u: &str) -> bool {
    let l = u.get(..8).unwrap_or(u).to_ascii_lowercase();
    l.starts_with("https://") || l.starts_with("http://")
}

/// Results, the answer, and the cost: the credits Tavily reports, else the
/// price of the depth that was sent.
fn parse(mode: Mode, resp: TavilyResponse) -> Output {
    let source = if mode == Mode::News {
        "tavily_news"
    } else {
        "tavily"
    };
    let answers = resp
        .answer
        .filter(|a| !a.trim().is_empty())
        .map(|text| Answer {
            provider: "tavily".to_string(),
            text,
        })
        .into_iter()
        .collect();
    let results = resp
        .results
        .into_iter()
        .filter_map(|r| {
            let url = r.url.filter(|u| is_http(u))?;
            Some(SearchResult {
                title: r.title.unwrap_or_default(),
                url,
                snippet: r.content.unwrap_or_default(),
                source: source.to_string(),
                published: r.published_date.filter(|d| !d.is_empty()),
                image_url: None,
                extra: r.score.map(|s| json!({"score": s})),
            })
        })
        .collect();
    let reported = resp
        .usage
        .and_then(|u| u.credits)
        .filter(|c| c.is_finite() && *c >= 0.0)
        .map(|c| (c * MICROS_PER_CREDIT as f64).round() as u64);
    Output {
        results,
        answers,
        cost_micros: Some(reported.unwrap_or_else(|| depth_micros(mode))),
    }
}

#[async_trait]
impl Adapter for Tavily {
    fn spec(&self) -> &'static ProviderSpec {
        &SPEC
    }

    fn keys(&self) -> Vec<String> {
        super::keys_for(&self.ctx.config, &SPEC)
    }

    fn is_configured(&self) -> bool {
        !self.keys().is_empty()
    }

    /// 432 is the plan limit and 433 the pay-as-you-go limit: billing, not a
    /// malformed request.
    fn classify(&self, status: u16, body: &str) -> Option<FailureCategory> {
        let b = body.to_ascii_lowercase();
        (matches!(status, 432 | 433)
            || b.contains("usage limit")
            || b.contains("pay-as-you-go limit"))
        .then_some(FailureCategory::BillingQuota)
    }

    /// A timed-out call cost the depth it asked for.
    fn sent_cost(&self, mode: Mode) -> u64 {
        depth_micros(mode)
    }

    async fn call(&self, call: &Call<'_>) -> Result<Output, SearchError> {
        if !self.is_configured() {
            return Err(SearchError::AuthMissing { provider: "tavily" });
        }
        let key = call.key;
        let client = &self.ctx.client;
        let resp = super::send(self, call.timeout, || request(client, key, call)).await?;
        let resp: TavilyResponse = resp.json().await?;
        Ok(parse(call.mode, resp))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;
    use crate::types::SearchOpts;

    fn tavily() -> Tavily {
        Tavily::new(Arc::new(AppContext::new(AppConfig::default())))
    }

    fn sent(mode: Mode, opts: &SearchOpts) -> (reqwest::Request, serde_json::Value) {
        let call = Call {
            mode,
            query: "world cup",
            count: 50,
            opts,
            timeout: Duration::from_secs(1),
            key: "",
        };
        let req = request(&reqwest::Client::new(), "tvly-test", &call)
            .build()
            .unwrap();
        let body = serde_json::from_slice(req.body().unwrap().as_bytes().unwrap()).unwrap();
        (req, body)
    }

    #[test]
    fn key_rides_in_bearer_and_depth_follows_mode() {
        let opts = SearchOpts {
            include_domains: vec!["fifa.com".into()],
            freshness: Some("week".into()),
            ..SearchOpts::default()
        };
        let (req, general) = sent(Mode::General, &opts);
        assert_eq!(req.url().as_str(), "https://api.tavily.com/search");
        assert!(req
            .headers()
            .get("authorization")
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("Bearer "));
        assert!(general.get("api_key").is_none(), "no key in the body");
        assert_eq!(general["search_depth"], "basic");
        assert_eq!(general["topic"], "general");
        assert_eq!(general["max_results"], 20);
        assert_eq!(general["include_usage"], true);
        assert_eq!(general["include_published_date"], true);
        assert_eq!(general["include_domains"], json!(["fifa.com"]));
        assert_eq!(general["time_range"], "week");
        let (_, news) = sent(Mode::News, &SearchOpts::default());
        assert_eq!(
            (news["search_depth"].as_str(), news["topic"].as_str()),
            (Some("basic"), Some("news"))
        );
        for mode in [Mode::Academic, Mode::Deep] {
            let (_, body) = sent(mode, &SearchOpts::default());
            assert_eq!(body["search_depth"], "advanced");
        }
    }

    #[test]
    fn parses_published_date_answer_and_reported_credits() {
        let resp: TavilyResponse =
            serde_json::from_str(include_str!("../../tests/fixtures/tavily/search.json")).unwrap();
        let out = parse(Mode::General, resp);
        assert_eq!(out.results.len(), 2);
        assert_eq!(
            out.results[0].published.as_deref(),
            Some("Sun, 19 Jul 2026 22:10:00 GMT")
        );
        assert_eq!(out.results[1].published, None);
        assert_eq!(out.results[0].extra.as_ref().unwrap()["score"], 0.91);
        assert_eq!(out.answers.len(), 1);
        assert_eq!(out.answers[0].text, "Spain won the 2026 FIFA World Cup.");
        assert_eq!(out.cost_micros, Some(8_000));
    }

    #[test]
    fn cost_follows_the_depth_sent_when_unreported() {
        let t = tavily();
        assert_eq!(t.sent_cost(Mode::General), 8_000);
        assert_eq!(t.sent_cost(Mode::News), 8_000);
        assert_eq!(t.sent_cost(Mode::Academic), 16_000);
        assert_eq!(t.sent_cost(Mode::Deep), 16_000);
        let bare =
            |mode| parse(mode, serde_json::from_str(r#"{"results": []}"#).unwrap()).cost_micros;
        assert_eq!(bare(Mode::General), Some(8_000));
        assert_eq!(bare(Mode::Deep), Some(16_000));
        let reported: TavilyResponse =
            serde_json::from_str(r#"{"results": [], "usage": {"credits": 2}}"#).unwrap();
        assert_eq!(parse(Mode::General, reported).cost_micros, Some(16_000));
    }

    #[test]
    fn plan_limit_is_billing() {
        let body = include_str!("../../tests/fixtures/tavily/plan-limit-432.json");
        assert_eq!(
            tavily().classify(432, body),
            Some(FailureCategory::BillingQuota)
        );
        assert_eq!(tavily().classify(400, "bad query"), None);
    }
}
