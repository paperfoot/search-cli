//! Serper (Google results). One POST per call to /search, /news, /scholar,
//! /patents, /images or /places; academic and deep run on /search. The
//! response's `credits` is what Serper billed, so it is the call's cost.

use super::spec::{FilterSupport, Price};
use super::{Adapter, Call, Output, ProviderSpec};
use crate::context::AppContext;
use crate::errors::SearchError;
use crate::types::{Answer, FailureCategory, Mode, SearchResult};
use async_trait::async_trait;
use serde_json::{json, Map, Value};
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

/// Starter pack: $50 for 50,000 credits.
const MICROS_PER_CREDIT: u64 = 1_000;

pub struct Serper {
    ctx: Arc<AppContext>,
}

impl Serper {
    pub fn new(ctx: Arc<AppContext>) -> Self {
        Self { ctx }
    }
}

fn endpoint(mode: Mode) -> &'static str {
    match mode {
        Mode::News => "news",
        Mode::Scholar => "scholar",
        Mode::Patents => "patents",
        Mode::Images => "images",
        Mode::Places => "places",
        _ => "search",
    }
}

/// Only /search and /news take filters; the vertical endpoints get the bare
/// query (the engine warns that the mode applies no filters).
fn takes_filters(mode: Mode) -> bool {
    matches!(endpoint(mode), "search" | "news")
}

fn request(client: &reqwest::Client, key: &str, call: &Call<'_>) -> reqwest::RequestBuilder {
    let opts = if takes_filters(call.mode) {
        call.opts
    } else {
        &super::NO_FILTERS
    };
    let q = super::augment_query(call.query, opts);
    let mut body = json!({ "q": q, "num": call.count.min(SPEC.max_count) });
    if let Some(tbs) = opts.freshness.as_deref().and_then(tbs) {
        body["tbs"] = json!(tbs);
    }
    if let Some(country) = &opts.country {
        body["gl"] = json!(country.to_lowercase());
    }
    if let Some(lang) = &opts.lang {
        body["hl"] = json!(lang.to_lowercase());
    }
    client
        .post(format!("https://google.serper.dev/{}", endpoint(call.mode)))
        .header("X-API-KEY", key)
        .header("Content-Type", "application/json")
        .json(&body)
}

fn tbs(freshness: &str) -> Option<&'static str> {
    match freshness {
        "day" => Some("qdr:d"),
        "week" => Some("qdr:w"),
        "month" => Some("qdr:m"),
        "year" => Some("qdr:y"),
        _ => None,
    }
}

fn text<'a>(item: &'a Value, key: &str) -> Option<&'a str> {
    item.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

fn http(url: Option<&str>) -> Option<&str> {
    url.filter(|u| {
        let l = u.get(..8).unwrap_or(u).to_ascii_lowercase();
        l.starts_with("https://") || l.starts_with("http://")
    })
}

/// The listed fields that are present, renamed where `rename` says so.
fn pick(item: &Value, keys: &[&str], rename: &[(&str, &str)]) -> Option<Value> {
    let mut out = Map::new();
    for k in keys {
        match item.get(*k) {
            None | Some(Value::Null) => {}
            Some(Value::String(s)) if s.is_empty() => {}
            Some(v) => {
                let name = rename
                    .iter()
                    .find(|(from, _)| from == k)
                    .map_or(*k, |(_, to)| to);
                out.insert(name.to_string(), v.clone());
            }
        }
    }
    (!out.is_empty()).then_some(Value::Object(out))
}

fn parse_item(mode: Mode, item: &Value) -> Option<SearchResult> {
    let row =
        |url: &str, snippet: String, published: Option<&str>, extra: Option<Value>| SearchResult {
            title: text(item, "title").unwrap_or_default().to_string(),
            url: url.to_string(),
            snippet,
            source: source(mode).to_string(),
            published: published.map(String::from),
            image_url: None,
            extra,
        };
    let snippet = text(item, "snippet").unwrap_or_default().to_string();
    match mode {
        Mode::Places => {
            let maps;
            let url = match (http(text(item, "website")), text(item, "cid")) {
                (Some(site), _) => site,
                (None, Some(cid)) => {
                    maps = format!("https://maps.google.com/?cid={cid}");
                    &maps
                }
                (None, None) => return None,
            };
            let snippet = [text(item, "address"), text(item, "category")]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" · ");
            let extra = pick(
                item,
                &[
                    "address",
                    "rating",
                    "ratingCount",
                    "category",
                    "phoneNumber",
                    "website",
                    "cid",
                    "latitude",
                    "longitude",
                ],
                &[],
            );
            Some(row(url, snippet, None, extra))
        }
        Mode::Images => {
            let link = http(text(item, "link"))?;
            let extra = pick(
                item,
                &["thumbnailUrl", "imageWidth", "imageHeight", "source"],
                &[],
            );
            let mut r = row(link, String::new(), None, extra);
            r.image_url = http(text(item, "imageUrl")).map(String::from);
            Some(r)
        }
        Mode::Scholar => {
            let link = http(text(item, "link"))?;
            let extra = pick(item, &["publicationInfo", "citedBy", "pdfUrl", "year"], &[]);
            Some(row(link, snippet, None, extra))
        }
        Mode::Patents => {
            let link = http(text(item, "link"))?;
            let extra = pick(
                item,
                &[
                    "priorityDate",
                    "filingDate",
                    "inventor",
                    "assignee",
                    "publicationNumber",
                    "pdfUrl",
                ],
                &[("pdfUrl", "pdf")],
            );
            Some(row(link, snippet, text(item, "publicationDate"), extra))
        }
        Mode::News => {
            let link = http(text(item, "link"))?;
            let extra = pick(
                item,
                &["source", "imageUrl"],
                &[("imageUrl", "thumbnailUrl")],
            );
            Some(row(link, snippet, text(item, "date"), extra))
        }
        _ => {
            let link = http(text(item, "link"))?;
            Some(row(link, snippet, text(item, "date"), None))
        }
    }
}

fn source(mode: Mode) -> &'static str {
    match mode {
        Mode::News => "serper_news",
        Mode::Scholar => "serper_scholar",
        Mode::Patents => "serper_patents",
        Mode::Images => "serper_images",
        Mode::Places => "serper_places",
        _ => "serper",
    }
}

fn parse(mode: Mode, body: &Value) -> Output {
    let key = match mode {
        Mode::News => "news",
        Mode::Images => "images",
        Mode::Places => "places",
        _ => "organic",
    };
    let results = body
        .get(key)
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(|i| parse_item(mode, i)).collect())
        .unwrap_or_default();
    // Google's answer box, when /search shows one.
    let answers = body
        .get("answerBox")
        .filter(|_| endpoint(mode) == "search")
        .and_then(|b| text(b, "answer").or_else(|| text(b, "snippet")))
        .map(|t| Answer {
            provider: "serper".to_string(),
            text: t.to_string(),
        })
        .into_iter()
        .collect();
    let cost_micros = body
        .get("credits")
        .and_then(Value::as_u64)
        .map(|c| c * MICROS_PER_CREDIT);
    Output {
        results,
        answers,
        cost_micros,
    }
}

#[async_trait]
impl Adapter for Serper {
    fn spec(&self) -> &'static ProviderSpec {
        &SPEC
    }

    fn keys(&self) -> Vec<String> {
        super::keys_for(&self.ctx.config, &SPEC)
    }

    fn is_configured(&self) -> bool {
        !self.keys().is_empty()
    }

    /// Serper answers an empty balance with a 400 "Not enough credits".
    fn classify(&self, _status: u16, body: &str) -> Option<FailureCategory> {
        let b = body.to_ascii_lowercase();
        if b.contains("not enough credits") {
            Some(FailureCategory::BillingQuota)
        } else if b.contains("unauthorized") || b.contains("invalid api key") {
            Some(FailureCategory::Auth)
        } else {
            None
        }
    }

    async fn call(&self, call: &Call<'_>) -> Result<Output, SearchError> {
        if !self.is_configured() {
            return Err(SearchError::AuthMissing { provider: "serper" });
        }
        let key = call.key;
        let client = &self.ctx.client;
        let resp = super::send(self, call.timeout, || request(client, key, call)).await?;
        let body: Value = resp.json().await?;
        Ok(parse(call.mode, &body))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;
    use crate::types::SearchOpts;

    fn built(mode: Mode, count: u16, opts: &SearchOpts) -> (reqwest::Request, Value) {
        let call = Call {
            mode,
            query: "coffee",
            count,
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

    fn filters() -> SearchOpts {
        SearchOpts {
            include_domains: vec!["example.com".into()],
            freshness: Some("week".into()),
            country: Some("GB".into()),
            lang: Some("EN".into()),
            ..SearchOpts::default()
        }
    }

    #[test]
    fn web_request_carries_filters_and_caps_num_at_ten() {
        for mode in [Mode::General, Mode::Academic, Mode::Deep] {
            let (req, body) = built(mode, 50, &filters());
            assert_eq!(req.url().as_str(), "https://google.serper.dev/search");
            assert!(req.headers().contains_key("x-api-key"));
            assert_eq!(
                body,
                json!({"q": "coffee site:example.com", "num": 10, "tbs": "qdr:w", "gl": "gb", "hl": "en"})
            );
        }
        let (req, body) = built(Mode::News, 5, &filters());
        assert_eq!(req.url().path(), "/news");
        assert_eq!(body["tbs"], "qdr:w");
        assert_eq!(body["num"], 5);
    }

    /// Google ANDs bare `site:` operators, so two `-d` domains must be ORed
    /// or Serper returns nothing and still bills the credit.
    #[test]
    fn two_include_domains_are_ored_and_excludes_stay_negated() {
        let opts = SearchOpts {
            include_domains: vec!["wikipedia.org".into(), "britannica.com".into()],
            exclude_domains: vec!["pinterest.com".into()],
            ..SearchOpts::default()
        };
        let (_, body) = built(Mode::General, 10, &opts);
        assert_eq!(
            body["q"],
            "coffee site:wikipedia.org OR site:britannica.com -site:pinterest.com"
        );
    }

    #[test]
    fn vertical_requests_send_only_the_query() {
        for (mode, path) in [
            (Mode::Places, "/places"),
            (Mode::Scholar, "/scholar"),
            (Mode::Patents, "/patents"),
            (Mode::Images, "/images"),
        ] {
            let (req, body) = built(mode, 10, &filters());
            assert_eq!(req.url().path(), path);
            assert_eq!(body, json!({"q": "coffee", "num": 10}));
        }
    }

    fn fixture(mode: Mode, raw: &str) -> Output {
        parse(mode, &serde_json::from_str(raw).unwrap())
    }

    #[test]
    fn places_keep_address_website_and_coordinates() {
        let out = fixture(
            Mode::Places,
            include_str!("../../tests/fixtures/serper/places.json"),
        );
        assert_eq!(out.cost_micros, Some(1_000));
        assert_eq!(out.results.len(), 3);
        let r = &out.results[1];
        assert_eq!(r.title, "Kiss the Hippo Coffee Soho");
        assert_eq!(r.url, "https://www.kissthehippo.com/");
        assert_eq!(r.snippet, "30 Golden Square · Coffee shop");
        assert_eq!(r.source, "serper_places");
        let extra = r.extra.as_ref().unwrap();
        assert_eq!(extra["address"], "30 Golden Square");
        assert_eq!(extra["rating"], 4.6);
        assert_eq!(extra["ratingCount"], 716);
        assert_eq!(extra["cid"], "5686153484176968158");
        assert_eq!(extra["latitude"], 51.511703);
    }

    #[test]
    fn a_place_without_a_website_links_to_maps() {
        let body = json!({"places": [
            {"title": "Kiosk", "address": "1 Lane", "cid": "42"},
            {"title": "Nowhere"}
        ]});
        let out = parse(Mode::Places, &body);
        assert_eq!(out.results.len(), 1, "a place with no link is dropped");
        assert_eq!(out.results[0].url, "https://maps.google.com/?cid=42");
        assert_eq!(out.results[0].snippet, "1 Lane");
        assert_eq!(out.cost_micros, None);
    }

    #[test]
    fn scholar_keeps_citations_and_pdf() {
        let out = fixture(
            Mode::Scholar,
            include_str!("../../tests/fixtures/serper/scholar.json"),
        );
        let extra = out.results[0].extra.as_ref().unwrap();
        assert_eq!(extra["citedBy"], 2201);
        assert_eq!(extra["year"], 2009);
        assert_eq!(
            extra["pdfUrl"],
            "https://dl.acm.org/doi/pdf/10.1145/1571941.1572114"
        );
        assert!(extra["publicationInfo"]
            .as_str()
            .unwrap()
            .starts_with("GV Cormack"));
    }

    #[test]
    fn patents_keep_publication_number_and_dates() {
        let out = fixture(
            Mode::Patents,
            include_str!("../../tests/fixtures/serper/patents.json"),
        );
        let r = &out.results[0];
        assert_eq!(r.published.as_deref(), Some("2025-03-25"));
        let extra = r.extra.as_ref().unwrap();
        assert_eq!(extra["publicationNumber"], "US12259913B1");
        assert_eq!(extra["priorityDate"], "2024-02-14");
        assert_eq!(extra["inventor"], "Brien H Muschett");
        assert_eq!(extra["assignee"], "Inventus Holdings, Llc");
        assert!(extra["pdf"].as_str().unwrap().ends_with("US12259913.pdf"));
    }

    #[test]
    fn images_news_and_web_parse_with_their_cost() {
        let img = fixture(
            Mode::Images,
            include_str!("../../tests/fixtures/serper/images.json"),
        );
        let r = &img.results[0];
        assert_eq!(r.url, "https://en.wikipedia.org/wiki/Golden_Gate_Bridge");
        assert!(r
            .image_url
            .as_deref()
            .unwrap()
            .contains("Golden_Gate_Bridge"));
        assert_eq!(r.extra.as_ref().unwrap()["imageWidth"], 5388);

        let news = fixture(
            Mode::News,
            include_str!("../../tests/fixtures/serper/news.json"),
        );
        assert_eq!(news.results[0].published.as_deref(), Some("2 days ago"));
        assert_eq!(news.results[0].extra.as_ref().unwrap()["source"], "OpenAI");

        let web = fixture(
            Mode::General,
            include_str!("../../tests/fixtures/serper/search.json"),
        );
        assert!(!web.results.is_empty());
        assert!(web.results.iter().all(|r| r.url.starts_with("https://")));
        assert_eq!(web.cost_micros, Some(1_000));
    }

    #[test]
    fn empty_balance_is_billing_not_bad_request() {
        let s = Serper::new(Arc::new(AppContext::new(AppConfig::default())));
        assert_eq!(
            s.classify(400, r#"{"message":"Not enough credits","statusCode":400}"#),
            Some(FailureCategory::BillingQuota)
        );
        assert_eq!(
            s.classify(403, r#"{"message":"Unauthorized.","statusCode":403}"#),
            Some(FailureCategory::Auth)
        );
        assert_eq!(s.classify(400, "bad"), None);
    }
}
