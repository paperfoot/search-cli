//! Single source of truth for the mode map: what each mode does, what input
//! it takes, and which providers it routes to. The engine, `agent-info`, and
//! the filter-honesty warnings all read this table; SKILL.md and README
//! summarize it. A unit test pins the mode list so a new `Mode` variant
//! cannot ship without a registry entry.

use crate::errors::SearchError;
use crate::page::{self, Verdict};
use crate::providers::{self, Output};
use crate::types::{FailureCategory, Mode};
use std::sync::Mutex;
use std::time::Duration;

/// What the `-q` argument means for a mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputKind {
    /// Free-text search query.
    Query,
    /// A full http(s) URL.
    Url,
}

impl InputKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Query => "query",
            Self::Url => "url",
        }
    }
}

/// How a mode combines its providers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeKind {
    /// All providers queried in parallel, results deduped and ranked with
    /// reciprocal-rank fusion (cross-provider consensus ranks higher).
    Fused,
    /// Providers tried in listed order until one returns content.
    Chain,
    /// Exactly one provider serves the mode.
    Single,
}

impl MergeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fused => "fused",
            Self::Chain => "fallback_chain",
            Self::Single => "single_provider",
        }
    }
}

pub struct ModeSpec {
    pub mode: Mode,
    pub input: InputKind,
    pub merge: MergeKind,
    /// Providers the engine uses for this mode, in fan-out (Fused) or
    /// fallback (Chain) order.
    pub providers: &'static [&'static str],
    pub description: &'static str,
    pub when_to_use: &'static str,
    /// Whole-run budget: the server's deadline plus 5 s, so a hosted call is
    /// never abandoned while the server still works on it (PLAN.md §3.5).
    /// Calls already sent are never cancelled before it.
    pub deadline: Duration,
}

const fn secs(s: u64) -> Duration {
    Duration::from_secs(s)
}

/// The extract chain, free and honest first, paid renderers last: the
/// direct markdown-first GET, the local browser-fingerprint fetch (only when
/// direct was refused, see [`rung_gate`]), Jina Reader (renders JS, reads
/// PDFs), Firecrawl (PDFs capped at 20 pages), Browserless (anti-bot last
/// resort).
const EXTRACT_CHAIN: &[&str] = &[
    "direct",
    #[cfg(feature = "stealth")]
    "stealth",
    "jina",
    "firecrawl",
    "browserless",
];

pub const MODES: &[ModeSpec] = &[
    ModeSpec {
        mode: Mode::General,
        input: InputKind::Query,
        merge: MergeKind::Fused,
        providers: &[
            "serper",
            "exa",
            "jina",
            "brave",
            "linkup",
            "parallel",
            "tavily",
            "perplexity",
        ],
        description: "Broad web search fused across all general-capable providers",
        when_to_use: "Default for any web lookup that doesn't fit a specialist mode.",
        deadline: secs(25),
    },
    ModeSpec {
        mode: Mode::News,
        input: InputKind::Query,
        merge: MergeKind::Fused,
        providers: &["serper", "brave", "linkup", "parallel", "tavily", "perplexity"],
        description: "News-specific endpoints of the news-capable providers",
        when_to_use: "Current events and anything where recency dominates; combine with -f day or -f week.",
        deadline: secs(25),
    },
    ModeSpec {
        mode: Mode::Academic,
        input: InputKind::Query,
        merge: MergeKind::Fused,
        providers: &["exa", "serper", "tavily", "perplexity"],
        description: "Papers, preprints, and studies on the open web (semantic + web search)",
        when_to_use: "Finding research by topic. For Google Scholar records (citations, versions, PDFs) use scholar instead.",
        deadline: secs(30),
    },
    ModeSpec {
        mode: Mode::People,
        input: InputKind::Query,
        merge: MergeKind::Single,
        providers: &["exa"],
        description: "Person and LinkedIn-profile search (Exa category search)",
        when_to_use: "Finding a specific person, their role, or their LinkedIn profile.",
        deadline: secs(25),
    },
    ModeSpec {
        mode: Mode::Deep,
        input: InputKind::Query,
        merge: MergeKind::Fused,
        providers: &[
            "exa",
            "serper",
            "brave",
            "linkup",
            "parallel",
            "tavily",
            "perplexity",
            "xai",
        ],
        description: "Maximum-coverage fan-out (web + X/Twitter + Brave grounding)",
        when_to_use: "Research where recall matters more than latency. Raise -c (e.g. -c 30) to keep more of the fused pool.",
        deadline: secs(50),
    },
    ModeSpec {
        mode: Mode::Extract,
        input: InputKind::Url,
        merge: MergeKind::Chain,
        providers: EXTRACT_CHAIN,
        description: "Full page content as markdown: a direct fetch that asks for markdown first, then (when the site refuses it) a local browser-fingerprint fetch, then Jina Reader, Firecrawl and Browserless until one returns real content",
        when_to_use: "Reading one specific page, including JS-heavy or anti-bot pages. -q must be a full URL.",
        deadline: secs(65),
    },
    ModeSpec {
        mode: Mode::Similar,
        input: InputKind::Url,
        merge: MergeKind::Single,
        providers: &["exa"],
        description: "Pages semantically similar to the given URL (Exa findSimilar)",
        when_to_use: "\"More like this page\": competitors, alternatives, related coverage. -q must be a full URL.",
        deadline: secs(25),
    },
    ModeSpec {
        mode: Mode::Scrape,
        input: InputKind::Url,
        merge: MergeKind::Chain,
        providers: EXTRACT_CHAIN,
        description: "Alias of extract — identical provider chain and behavior",
        when_to_use: "Same as extract; the two names are interchangeable.",
        deadline: secs(65),
    },
    ModeSpec {
        mode: Mode::Scholar,
        input: InputKind::Query,
        merge: MergeKind::Fused,
        providers: &["serper", "serpapi"],
        description: "Google Scholar records (citations, versions, PDF links)",
        when_to_use: "Scholarly metadata for a known paper/author. For topic discovery use academic.",
        deadline: secs(25),
    },
    ModeSpec {
        mode: Mode::Patents,
        input: InputKind::Query,
        merge: MergeKind::Single,
        providers: &["serper"],
        description: "Google Patents search",
        when_to_use: "Prior art, patent families, inventor/assignee lookups.",
        deadline: secs(25),
    },
    ModeSpec {
        mode: Mode::Images,
        input: InputKind::Query,
        merge: MergeKind::Single,
        providers: &["serper"],
        description: "Google Images search (returns image_url per result)",
        when_to_use: "Finding images; check the image_url field of each result.",
        deadline: secs(25),
    },
    ModeSpec {
        mode: Mode::Places,
        input: InputKind::Query,
        merge: MergeKind::Single,
        providers: &["serper"],
        description: "Local businesses and places (Google Maps data)",
        when_to_use: "Businesses, opening hours, addresses near a location.",
        deadline: secs(25),
    },
    ModeSpec {
        mode: Mode::Social,
        input: InputKind::Query,
        merge: MergeKind::Single,
        providers: &["xai"],
        description: "Live X/Twitter search via xAI Grok — an X summary plus cited posts",
        when_to_use: "What's being said on X, trending topics, account activity. Here -d/--exclude-domain filter X handles, not domains.",
        deadline: secs(65),
    },
];

pub fn spec(mode: Mode) -> &'static ModeSpec {
    // The test below guarantees every Mode variant has an entry.
    MODES
        .iter()
        .find(|s| s.mode == mode)
        .expect("registry entry for every mode")
}

/// Which of the CLI's `-f`/`-d` filters a provider forwards to its API, read
/// from its spec. The engine warns when a provider that drops a requested
/// filter contributes to a filtered search.
pub struct FilterSupport {
    pub freshness: bool,
    pub domains: bool,
    /// Caveat surfaced verbatim in warnings, e.g. xai remapping domains to handles.
    pub note: Option<&'static str>,
}

pub fn filter_support(provider: &str) -> FilterSupport {
    let f = providers::find(provider).map(|s| s.filters);
    FilterSupport {
        freshness: f.is_some_and(|f| f.freshness),
        domains: f.is_some_and(|f| f.domains),
        note: (provider == "xai")
            .then_some("social: xai maps --domain/--exclude-domain to X handles. deep: xai skips the call (no charge) when the filters rule out x.com, and otherwise does not apply them"),
    }
}

/// Modes whose calls forward the caller's filters. The vertical endpoints
/// (scholar, patents, images, places), people, similar and extract send none.
pub fn mode_applies_filters(mode: Mode) -> bool {
    matches!(
        mode,
        Mode::General | Mode::News | Mode::Academic | Mode::Deep | Mode::Social
    )
}

/// Whether an output is acceptable for the mode. In a chain a rejected output
/// is recorded as a `rejected` failure (and still billed) and the next
/// provider runs, so junk never ends the chain as a success. Only extract
/// judges content.
///
/// A rung that saw the whole page marks its own verdict in
/// `extra.verdict` (thin is judged there, on the full extraction). Every
/// output is then judged on its markdown: binary bytes, a challenge page or
/// a paywall is rejected whoever returned it.
pub fn accept(mode: Mode, output: &Output) -> Result<(), &'static str> {
    if !matches!(mode, Mode::Extract | Mode::Scrape) {
        return Ok(());
    }
    let Some(r) = output.results.first() else {
        return Ok(());
    };
    let marked = r
        .extra
        .as_ref()
        .and_then(|e| e.get("verdict"))
        .and_then(|v| v.as_str());
    if let Some(code) = marked.filter(|c| *c != "ok") {
        return Err(rejection_reason(code));
    }
    match page::verdict(&r.snippet, Some("text/markdown"), 200, None) {
        Verdict::Ok | Verdict::Thin => Ok(()),
        v => Err(rejection_reason(v.code())),
    }
}

/// The failure reason for a verdict code ("<provider> returned <reason>").
fn rejection_reason(code: &str) -> &'static str {
    match code {
        "binary" => "binary or undecodable content (verdict: binary)",
        "challenge" => "an anti-bot challenge page instead of content (verdict: challenge)",
        "js_shell" => "a page that only renders in a browser (verdict: js_shell)",
        "thin" => "too little text to be the page (verdict: thin)",
        "payment_required" => "a paywall (verdict: payment_required)",
        "http_error" => "an HTTP error page (verdict: http_error)",
        _ => "content the extract mode does not accept",
    }
}

/// The chain's escalation rules, as each rung sees them.
///
/// The direct rung notes what it saw for a URL; the later rungs ask
/// [`rung_gate`] before they send anything. One CLI run is one search, so
/// the notes live for the process, keyed by the input URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DirectSaw {
    /// A response the direct rung did not accept.
    Rejected(Verdict),
    /// No response at all: refused connection, TLS failure or timeout.
    Unreachable,
    /// The SSRF guard refused the destination (a private address, by name,
    /// by DNS answer or on a redirect hop). No later rung is sent it.
    Blocked,
}

static DIRECT_NOTES: Mutex<Vec<(String, DirectSaw)>> = Mutex::new(Vec::new());

/// Record what the direct rung saw for `url`.
pub fn note_direct(url: &str, saw: DirectSaw) {
    let mut notes = DIRECT_NOTES.lock().unwrap_or_else(|e| e.into_inner());
    notes.retain(|(u, _)| u != url);
    notes.push((url.to_string(), saw));
}

fn direct_saw(url: &str) -> Option<DirectSaw> {
    let notes = DIRECT_NOTES.lock().unwrap_or_else(|e| e.into_inner());
    notes.iter().find(|(u, _)| u == url).map(|(_, s)| s.clone())
}

/// Whether `rung` may fetch `url`, given what the direct rung saw. Without a
/// note (direct not in the plan, `-p` chose the rungs) every rung runs.
///
/// - A 402 with `crawler-price` stops the chain: a paywall is never fetched
///   around, and no paid rung is called for it.
/// - A 404 or 410 stops it: the page does not exist, and a paid rung would
///   bill for the same error page.
/// - Stealth runs only when direct was refused (a challenge; 401, 403, 429
///   or 503; no connection). A browser fingerprint does nothing for a JS
///   shell, a PDF or a thin page; those go on to the rendering rungs.
pub fn rung_gate(rung: &'static str, url: &str) -> Result<(), SearchError> {
    let Some(saw) = direct_saw(url) else {
        return Ok(());
    };
    match &saw {
        DirectSaw::Rejected(Verdict::PaymentRequired { price }) => {
            return Err(SearchError::Upstream {
                provider: rung,
                status: 402,
                category: FailureCategory::Rejected,
                message: format!(
                    "payment_required: the site charges for this page (crawler-price: {}); the chain stops at a paywall, so {rung} was not called",
                    price.as_deref().unwrap_or("unstated")
                ),
            });
        }
        DirectSaw::Blocked => {
            return Err(SearchError::Upstream {
                provider: rung,
                status: 0,
                category: FailureCategory::BadRequest,
                message: format!(
                    "blocked: the guard refused this URL's destination (a private, loopback or link-local address); {rung} was not called. Pass --allow-private to override."
                ),
            });
        }
        DirectSaw::Rejected(Verdict::HttpError(status @ (404 | 410))) => {
            return Err(SearchError::Upstream {
                provider: rung,
                status: *status,
                category: FailureCategory::BadRequest,
                message: format!(
                    "not_found: the site answered HTTP {status}; {rung} was not called for a page that does not exist"
                ),
            });
        }
        _ => {}
    }
    if rung != "stealth" {
        return Ok(());
    }
    let refused = match &saw {
        DirectSaw::Unreachable => true,
        DirectSaw::Blocked => false,
        DirectSaw::Rejected(v) => matches!(
            v,
            Verdict::Challenge | Verdict::HttpError(401 | 403 | 429 | 503)
        ),
    };
    if refused {
        return Ok(());
    }
    let seen = match &saw {
        DirectSaw::Rejected(Verdict::HttpError(s)) => format!("HTTP {s}"),
        DirectSaw::Rejected(v) => v.code().to_string(),
        DirectSaw::Unreachable => "no answer".to_string(),
        DirectSaw::Blocked => "a blocked destination".to_string(),
    };
    Err(SearchError::Api {
        provider: rung,
        code: "skipped",
        message: format!(
            "skipped: direct got {seen}, not a refusal; a browser fingerprint does not change that"
        ),
        status: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::ValueEnum;

    #[test]
    fn every_mode_has_a_registry_entry() {
        for mode in Mode::value_variants() {
            assert!(
                MODES.iter().any(|s| s.mode == *mode),
                "mode {mode} missing from registry"
            );
        }
        assert_eq!(MODES.len(), Mode::value_variants().len());
    }

    #[test]
    fn registry_providers_serve_their_modes() {
        // Every provider a mode routes to has a spec that lists the mode
        // (stealth only exists with its feature).
        for spec in MODES {
            let mode = if spec.mode == Mode::Scrape {
                Mode::Extract
            } else {
                spec.mode
            };
            for p in spec.providers {
                match providers::find(p) {
                    Some(ps) => assert!(ps.modes.contains(&mode), "{p} does not serve {mode}"),
                    None => assert_eq!(*p, "stealth", "unknown provider {p} in registry"),
                }
            }
        }
    }

    #[test]
    fn deadlines_are_server_plus_five() {
        assert_eq!(spec(Mode::General).deadline, secs(25));
        assert_eq!(spec(Mode::Academic).deadline, secs(30));
        assert_eq!(spec(Mode::Deep).deadline, secs(50));
        assert_eq!(spec(Mode::Extract).deadline, secs(65));
        assert_eq!(spec(Mode::Social).deadline, secs(65));
        for m in MODES {
            assert!(m.deadline >= secs(25));
        }
    }

    fn page_output(text: &str, verdict: Option<&str>) -> Output {
        use crate::types::SearchResult;
        Output {
            results: vec![SearchResult {
                title: String::new(),
                url: "https://x.com".into(),
                snippet: text.to_string(),
                source: "t".into(),
                published: None,
                image_url: None,
                extra: verdict.map(|v| serde_json::json!({ "verdict": v })),
            }],
            ..Output::default()
        }
    }

    #[test]
    fn accept_rejects_binary_and_challenge_pages_in_extract() {
        let out = |text: &str| page_output(text, None);
        // Mojibake: mostly replacement characters.
        let binary: String = "\u{FFFD}x\u{FFFD}\u{FFFD}".repeat(100);
        assert!(accept(Mode::Extract, &out(&binary)).is_err());
        // Cloudflare interstitial served as 200.
        let challenge = "## Checking your browser before accessing\nexample.com";
        assert!(accept(Mode::Extract, &out(challenge)).is_err());
        assert!(accept(Mode::Scrape, &out(challenge)).is_err());
        // Real prose passes, even when long or mentioning Cloudflare.
        let article = format!(
            "How Cloudflare works. {}",
            "Real sentence with ordinary words. ".repeat(300)
        );
        assert!(accept(Mode::Extract, &out(&article)).is_ok());
        assert!(accept(Mode::Extract, &out("Short but real snippet about rust.")).is_ok());
        // Search modes don't judge content.
        assert!(accept(Mode::General, &out(challenge)).is_ok());
    }

    /// A rung that judged the whole page marks its verdict; accept honours
    /// it, so a thin or JS-shell page from a paid rung is rejected and billed.
    #[test]
    fn accept_honours_a_rung_verdict() {
        let article = "Real sentence with ordinary words. ".repeat(30);
        assert_eq!(
            accept(Mode::Extract, &page_output("short", Some("thin"))),
            Err(rejection_reason("thin"))
        );
        assert!(accept(Mode::Extract, &page_output(&article, Some("js_shell"))).is_err());
        assert!(accept(Mode::Extract, &page_output(&article, Some("ok"))).is_ok());
    }

    #[test]
    fn extract_chain_is_free_first_paid_last() {
        let chain = spec(Mode::Extract).providers;
        assert_eq!(chain.first(), Some(&"direct"));
        let pos = |p: &str| chain.iter().position(|x| *x == p).unwrap();
        assert!(
            pos("jina") < pos("firecrawl"),
            "render JS before static scrape"
        );
        assert_eq!(chain.last(), Some(&"browserless"));
        #[cfg(feature = "stealth")]
        assert_eq!(pos("stealth"), 1);
    }

    #[test]
    fn a_paywall_or_a_missing_page_stops_every_later_rung() {
        let url = "https://gate.example/paywalled";
        note_direct(
            url,
            DirectSaw::Rejected(Verdict::PaymentRequired {
                price: Some("USD 0.01".into()),
            }),
        );
        for rung in ["stealth", "jina", "firecrawl", "browserless"] {
            let err = rung_gate(rung, url).unwrap_err();
            assert_eq!(err.category(), FailureCategory::Rejected);
            assert!(err.to_string().contains("USD 0.01"), "{err}");
        }
        let url = "https://gate.example/gone";
        note_direct(url, DirectSaw::Rejected(Verdict::HttpError(404)));
        assert!(rung_gate("firecrawl", url).is_err());
        // No note: every rung runs.
        assert!(rung_gate("jina", "https://gate.example/unseen").is_ok());
    }

    #[test]
    fn stealth_runs_only_after_a_refusal() {
        let cases = [
            (DirectSaw::Rejected(Verdict::Challenge), true),
            (DirectSaw::Rejected(Verdict::HttpError(403)), true),
            (DirectSaw::Rejected(Verdict::HttpError(429)), true),
            (DirectSaw::Unreachable, true),
            (DirectSaw::Rejected(Verdict::JsShell), false),
            (DirectSaw::Rejected(Verdict::Binary), false),
            (DirectSaw::Rejected(Verdict::Thin), false),
            (DirectSaw::Rejected(Verdict::HttpError(500)), false),
        ];
        for (i, (saw, runs)) in cases.into_iter().enumerate() {
            let url = format!("https://gate.example/stealth-{i}");
            note_direct(&url, saw.clone());
            assert_eq!(rung_gate("stealth", &url).is_ok(), runs, "{saw:?}");
            // A JS shell or a PDF goes on to the rendering rungs.
            assert!(rung_gate("jina", &url).is_ok(), "{saw:?}");
        }
    }

    #[test]
    fn scrape_is_exact_alias_of_extract() {
        assert_eq!(spec(Mode::Scrape).providers, spec(Mode::Extract).providers);
    }

    #[test]
    fn url_modes_are_marked() {
        for m in [Mode::Extract, Mode::Scrape, Mode::Similar] {
            assert_eq!(spec(m).input, InputKind::Url);
        }
    }
}
