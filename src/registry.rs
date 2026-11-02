//! Single source of truth for the mode map: what each mode does, what input
//! it takes, and which providers it routes to. The engine, `agent-info`, and
//! the filter-honesty warnings all read this table; SKILL.md and README
//! summarize it. A unit test pins the mode list so a new `Mode` variant
//! cannot ship without a registry entry.

use crate::providers::{self, Output};
use crate::types::Mode;
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
        providers: &["stealth", "jina", "firecrawl", "browserless"],
        description: "Full page content as markdown; tries the local stealth scraper, then Jina Reader, Firecrawl, and Browserless until one succeeds",
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
        providers: &["stealth", "jina", "firecrawl", "browserless"],
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
            .then_some("xai maps --domain/--exclude-domain to X handles, not web domains"),
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
/// is recorded as a `rejected` failure and the next provider runs, so junk
/// never ends the chain as a success. Only extract judges content today.
pub fn accept(mode: Mode, output: &Output) -> Result<(), &'static str> {
    match mode {
        Mode::Extract | Mode::Scrape => match output.results.first() {
            Some(r) => match scrape_rejection(&r.snippet) {
                Some(reason) => Err(reason),
                None => Ok(()),
            },
            None => Ok(()),
        },
        _ => Ok(()),
    }
}

/// Why scraped content is unusable, or None if it looks like real page text.
/// Two failure classes seen in the wild: undecompressed/binary bytes served
/// as text (mojibake), and anti-bot challenge pages returned with HTTP 200.
fn scrape_rejection(content: &str) -> Option<&'static str> {
    let sample: Vec<char> = content.chars().take(4000).collect();
    if sample.len() > 200 {
        let garbage = sample
            .iter()
            .filter(|c| **c == '\u{FFFD}' || (c.is_control() && !c.is_whitespace()))
            .count();
        if garbage * 20 > sample.len() {
            return Some("binary or undecodable content");
        }
    }
    // Challenge pages are short; a real article mentioning Cloudflare isn't.
    if content.chars().count() < 4000 {
        const CHALLENGE_MARKERS: &[&str] = &[
            "Checking your browser before accessing",
            "Just a moment...",
            "Verifying you are human",
            "Enable JavaScript and cookies to continue",
            "cf-browser-verification",
            "Attention Required! | Cloudflare",
        ];
        if CHALLENGE_MARKERS.iter().any(|m| content.contains(m)) {
            return Some("anti-bot challenge page instead of content");
        }
    }
    None
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

    #[test]
    fn accept_rejects_binary_and_challenge_pages_in_extract() {
        use crate::types::SearchResult;
        let out = |text: &str| Output {
            results: vec![SearchResult {
                title: String::new(),
                url: "https://x.com".into(),
                snippet: text.to_string(),
                source: "t".into(),
                published: None,
                image_url: None,
                extra: None,
            }],
            ..Output::default()
        };
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
