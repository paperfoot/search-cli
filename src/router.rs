//! `auto` mode: the deterministic rules of ROUTING.md, as a pure function.
//!
//! Rules 1-5 are decisive (the first match decides, JEV is skipped). Rules
//! 6-10 are signals: without JEV (not logged in) the first one in table order
//! decides; with JEV, metasearch.sh decides. Auto never picks `deep`, and
//! picks `social` only on an explicit X/Twitter cue. The server runs the same
//! patterns; the table tests below mirror its vectors.

use crate::types::{AutoDecision, Mode};

/// One rule that matched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub rule: u8,
    pub mode: Mode,
    /// The text that matched, for the reason sentence.
    pub cue: String,
    /// Rule 3 searches the captured URL, not the whole sentence.
    pub query: Option<String>,
}

/// An explicit `-p` is obeyed exactly (ROUTING.md principle 1), so auto
/// never resolves to a mode the pinned providers cannot serve: when one of
/// them cannot serve `mode` and all of them serve general, general it is,
/// reported as a fallback (the server does the same). `None` keeps `mode`.
pub fn fit_pinned(mode: Mode, pinned: &[String]) -> Option<AutoDecision> {
    let specs: Vec<_> = pinned
        .iter()
        .filter_map(|p| crate::providers::find(p))
        .collect();
    let cannot: Vec<&str> = specs
        .iter()
        .filter(|s| !s.modes.contains(&mode))
        .map(|s| s.name)
        .collect();
    if cannot.is_empty() || !specs.iter().all(|s| s.modes.contains(&Mode::General)) {
        return None;
    }
    Some(AutoDecision {
        mode: Mode::General.to_string(),
        source: "fallback".to_string(),
        reason: format!(
            "{} cannot serve {mode}, so general was used.",
            cannot.join(", ")
        ),
        probabilities: None,
        filters_applied: None,
        model: None,
        latency_ms: None,
    })
}

/// Rules 1-5. The first match decides.
pub fn decisive(query: &str) -> Option<Hit> {
    let q = query.trim();
    let lower = q.to_lowercase();
    if is_absolute_url(q) {
        // Fetch the trimmed URL, so " HTTPS://x " reaches extract as a URL.
        let mut h = hit(1, Mode::Extract, "a URL");
        h.query = Some(q.to_string());
        return Some(h);
    }
    if let Some(cue) = x_cue(q) {
        return Some(hit(2, Mode::Social, &cue));
    }
    if let Some(url) = similar_url(q) {
        let mut h = hit(3, Mode::Similar, "a request for similar sites");
        h.query = Some(url);
        return Some(h);
    }
    if let Some(cue) =
        first_phrase(&lower, &["patent", "patents", "prior art"]).or_else(|| patent_number(&lower))
    {
        return Some(hit(4, Mode::Patents, &cue));
    }
    if let Some(cue) = image_cue(&lower) {
        return Some(hit(5, Mode::Images, &cue));
    }
    None
}

const SIGNALS: &[(u8, Mode, &[&str])] = &[
    (
        6,
        Mode::Places,
        &[
            "near me",
            "nearby",
            "restaurant in",
            "restaurants in",
            "cafe in",
            "cafes in",
            "café in",
            "cafés in",
            "hotel in",
            "hotels in",
            "opening hours",
            "directions to",
            "address of",
        ],
    ),
    (
        7,
        Mode::Scholar,
        &[
            "cited by",
            "citation",
            "citations",
            "h-index",
            "google scholar",
        ],
    ),
    (
        8,
        Mode::Academic,
        &[
            "paper",
            "papers",
            "study",
            "studies",
            "journal",
            "arxiv",
            "doi",
            "preprint",
            "meta-analysis",
            "systematic review",
            "clinical trial",
            "peer-reviewed",
        ],
    ),
    (
        9,
        Mode::People,
        &[
            "linkedin",
            "who is",
            "ceo of",
            "founder of",
            "cto of",
            "cfo of",
        ],
    ),
    (
        10,
        Mode::News,
        &[
            "today",
            "yesterday",
            "this week",
            "latest",
            "breaking",
            "news",
        ],
    ),
];

/// Rules 6-10 that fire, in table order (names go to JEV as hints).
pub fn signals(query: &str) -> Vec<Hit> {
    let lower = query.trim().to_lowercase();
    SIGNALS
        .iter()
        .filter_map(|(rule, mode, phrases)| {
            first_phrase(&lower, phrases).map(|cue| hit(*rule, *mode, &cue))
        })
        .collect()
}

/// Modes where a freshness word becomes a filter (ROUTING.md, amended 2 Nov):
/// only those whose point is recency. In general or academic, "today",
/// "latest" and "breaking" are usually names or versions.
pub fn freshness_applies(mode: Mode) -> bool {
    matches!(mode, Mode::News | Mode::Social)
}

/// The freshness auto applies for `mode` (server `freshnessFor`): the
/// explicit word's value in news or social, else none.
pub fn freshness_for(query: &str, mode: Mode) -> Option<(&'static str, String)> {
    freshness_applies(mode).then(|| freshness(query)).flatten()
}

/// Freshness from explicit words only (ROUTING.md), as (value, word).
pub fn freshness(query: &str) -> Option<(&'static str, String)> {
    let lower = query.to_lowercase();
    const WORDS: &[(&str, &[&str])] = &[
        ("day", &["today", "yesterday"]),
        ("week", &["this week", "latest", "breaking"]),
        ("month", &["this month"]),
        ("year", &["this year"]),
    ];
    WORDS
        .iter()
        .find_map(|(value, words)| first_phrase(&lower, words).map(|w| (*value, w)))
}

/// The decision without JEV: decisive rules, then the first signal, then general.
pub fn resolve_local(query: &str) -> (AutoDecision, Option<String>) {
    if let Some(h) = decisive(query) {
        return (decision_for(&h, "rule"), h.query);
    }
    if let Some(h) = signals(query).into_iter().next() {
        return (decision_for(&h, "heuristic"), None);
    }
    (
        AutoDecision {
            mode: Mode::General.to_string(),
            source: "fallback".to_string(),
            reason: "No routing rule matched; used general.".to_string(),
            probabilities: None,
            filters_applied: None,
            model: None,
            latency_ms: None,
        },
        None,
    )
}

pub fn decision_for(h: &Hit, source: &str) -> AutoDecision {
    let why = match h.rule {
        1 => "The query is a URL".to_string(),
        2 => format!("Mentions X/Twitter ('{}')", h.cue),
        3 => "Asked for sites similar to a URL".to_string(),
        _ => format!("Matched '{}'", h.cue),
    };
    AutoDecision {
        mode: h.mode.to_string(),
        source: source.to_string(),
        reason: format!("{why} (rule {}); used {}.", h.rule, h.mode),
        probabilities: None,
        filters_applied: None,
        model: None,
        latency_ms: None,
    }
}

/// Modes `auto` may hand to the engine; anything else from the server
/// (deep, an unknown name) falls back to general.
pub fn auto_mode(name: &str) -> Option<Mode> {
    use clap::ValueEnum;
    let m = Mode::from_str(name, true).ok()?;
    matches!(
        m,
        Mode::General
            | Mode::News
            | Mode::Academic
            | Mode::Scholar
            | Mode::People
            | Mode::Images
            | Mode::Places
            | Mode::Patents
            | Mode::Social
            | Mode::Extract
            | Mode::Similar
    )
    .then_some(m)
}

fn hit(rule: u8, mode: Mode, cue: &str) -> Hit {
    Hit {
        rule,
        mode,
        cue: cue.to_string(),
        query: None,
    }
}

/// A regex word character (`\w`), Unicode-aware.
fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn boundary_before(s: &str, at: usize) -> bool {
    s[..at].chars().next_back().is_none_or(|c| !is_word(c))
}

fn boundary_after(s: &str, at: usize) -> bool {
    s[at..].chars().next().is_none_or(|c| !is_word(c))
}

/// `\b(phrase)\b` for the first phrase that occurs, in list order.
fn first_phrase(lower: &str, phrases: &[&str]) -> Option<String> {
    phrases
        .iter()
        .find(|p| {
            lower
                .match_indices(*p)
                .any(|(i, m)| boundary_before(lower, i) && boundary_after(lower, i + m.len()))
        })
        .map(|p| p.to_string())
}

/// Rule 1: the whole trimmed query is one absolute http(s) URL.
fn is_absolute_url(q: &str) -> bool {
    let l = q.to_ascii_lowercase();
    (l.starts_with("http://") || l.starts_with("https://"))
        && !q.chars().any(char::is_whitespace)
        && url::Url::parse(q).is_ok_and(|u| u.host_str().is_some_and(|h| !h.is_empty()))
}

/// At-words that are code, not X handles: decorators, annotations, CSS
/// at-rules, doc tags, chat mentions and bare npm scopes. Copied from the
/// server's `CODE_AT_WORDS` (metasearch `src/lib/engine/judge.ts`); a wrong
/// social route costs about 45x a general search and returns posts, not docs.
const CODE_AT_WORDS: &[&str] = &[
    // Python, Java, Kotlin, Swift, TypeScript decorators and annotations
    "property",
    "staticmethod",
    "classmethod",
    "abstractmethod",
    "cached_property",
    "dataclass",
    "wraps",
    "override",
    "deprecated",
    "functionalinterface",
    "suppresswarnings",
    "transactional",
    "autowired",
    "bean",
    "component",
    "service",
    "controller",
    "restcontroller",
    "configuration",
    "entity",
    "table",
    "column",
    "id",
    "value",
    "getmapping",
    "postmapping",
    "requestmapping",
    "nullable",
    "nonnull",
    "notnull",
    "data",
    "builder",
    "getter",
    "setter",
    "slf4j",
    "test",
    "before",
    "after",
    "beforeeach",
    "aftereach",
    "beforeall",
    "afterall",
    "fixture",
    "pytest",
    "mark",
    "inject",
    "injectable",
    "input",
    "output",
    "published",
    "state",
    "binding",
    "observable",
    "observedobject",
    "stateobject",
    "environmentobject",
    "main",
    "mainactor",
    "objc",
    "escaping",
    "available",
    "discardableresult",
    "testable",
    "jsonproperty",
    "jsonignore",
    "async",
    "app",
    "route",
    "get",
    "post",
    "put",
    "patch",
    "delete",
    // CSS, Sass, Tailwind at-rules
    "media",
    "import",
    "keyframes",
    "font",
    "supports",
    "layer",
    "container",
    "apply",
    "tailwind",
    "extend",
    "mixin",
    "include",
    "use",
    "forward",
    "page",
    "charset",
    "namespace",
    "each",
    "if",
    "else",
    "for",
    "function",
    // JSDoc, Javadoc, Objective-C, Blade
    "param",
    "params",
    "returns",
    "return",
    "throws",
    "type",
    "types",
    "typedef",
    "template",
    "see",
    "link",
    "since",
    "author",
    "version",
    "example",
    "interface",
    "implementation",
    "protocol",
    "end",
    "synthesize",
    "section",
    "yield",
    "csrf",
    "extends",
    "foreach",
    // chat mentions
    "here",
    "channel",
    "everyone",
    "mention",
    // bare npm scopes that read as words in developer questions
    "angular",
    "nestjs",
    "tanstack",
    "babel",
    "emotion",
    "mui",
    "storybook",
    "prisma",
    "sveltejs",
    "reduxjs",
    "trpc",
    "nuxt",
    "nuxtjs",
    "octokit",
    "fortawesome",
    "fontsource",
    "heroicons",
    "headlessui",
    "vitejs",
    "rollup",
    "swc",
];

/// Rule 2, as amended in ROUTING.md (source of truth: the server's
/// `socialCue()`): (a) `tweet(s)`/`twitter`, except as a web-dev word;
/// (b) `on X`, case-sensitive, never before `axis`; (c) `x.com/`; (d) a lone
/// `@handle` that is not a code at-word. Returns the cue for the reason.
fn x_cue(q: &str) -> Option<String> {
    let lower = q.to_lowercase();
    if let Some(w) = x_word(&lower) {
        return Some(w);
    }
    if on_x(q) {
        return Some("on X".to_string());
    }
    if lower
        .match_indices("x.com/")
        .any(|(i, _)| boundary_before(&lower, i))
    {
        return Some("x.com/".to_string());
    }
    lone_handle(q)
}

/// (a) `(?<!\bembed(?:ded|ding)?\s+(?:an?\s+)?)\b(tweets?|twitter)\b(?!\s+(cards?|bootstrap|pixel|api|oauth|embeds?|widgets?|clone|share button)\b)`
fn x_word(lower: &str) -> Option<String> {
    const WEB_DEV: &[&str] = &[
        "cards",
        "card",
        "bootstrap",
        "pixel",
        "api",
        "oauth",
        "embeds",
        "embed",
        "widgets",
        "widget",
        "clone",
        "share button",
    ];
    for word in ["tweets", "tweet", "twitter"] {
        for (i, _) in lower.match_indices(word) {
            let j = i + word.len();
            if !boundary_before(lower, i) || !boundary_after(lower, j) {
                continue;
            }
            let after = &lower[j..];
            let next = after.trim_start();
            if next.len() < after.len()
                && WEB_DEV
                    .iter()
                    .any(|w| next.starts_with(w) && boundary_after(next, w.len()))
            {
                continue;
            }
            if after_embed(&lower[..i]) {
                continue;
            }
            return Some(word.to_string());
        }
    }
    None
}

/// The lookbehind of (a): `before` ends with `\bembed(ded|ding)?\s+(an?\s+)?`.
fn after_embed(before: &str) -> bool {
    fn ends_with_word(s: &str, words: &[&str]) -> Option<usize> {
        words
            .iter()
            .find(|w| s.ends_with(*w) && boundary_before(s, s.len() - w.len()))
            .map(|w| s.len() - w.len())
    }
    const EMBED: &[&str] = &["embedded", "embedding", "embed"];
    let b = before.trim_end();
    if b.len() == before.len() {
        return false; // \s+ needs at least one space
    }
    if ends_with_word(b, EMBED).is_some() {
        return true;
    }
    match ends_with_word(b, &["an", "a"]) {
        Some(at) => {
            let head = &b[..at];
            let h = head.trim_end();
            h.len() < head.len() && ends_with_word(h, EMBED).is_some()
        }
        None => false,
    }
}

/// (b) `\bon X\b(?!\s*-?\s*axis)`, case-sensitive.
fn on_x(q: &str) -> bool {
    q.match_indices("on X").any(|(i, m)| {
        let j = i + m.len();
        if !boundary_before(q, i) || !boundary_after(q, j) {
            return false;
        }
        let rest = q[j..].trim_start();
        let rest = rest.strip_prefix('-').unwrap_or(rest).trim_start();
        !rest.starts_with("axis")
    })
}

/// (d) `(?:^|\s)@([A-Za-z0-9_]{2,15})(?=$|[\s,!?;:)]|\.(?:\s|$))`, minus the
/// code at-words. Never an email, an npm scope (`@types/node`) or a dotted
/// call (`@app.route`).
fn lone_handle(q: &str) -> Option<String> {
    for (i, _) in q.match_indices('@') {
        if !q[..i].chars().next_back().is_none_or(char::is_whitespace) {
            continue;
        }
        let rest = &q[i + 1..];
        let n = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .count();
        if !(2..=15).contains(&n) {
            continue;
        }
        let mut after = rest[n..].chars();
        let ends = match after.next() {
            None => true,
            Some(c) if c.is_whitespace() || ",!?;:)".contains(c) => true,
            Some('.') => after.next().is_none_or(char::is_whitespace),
            Some(_) => false,
        };
        let word = &rest[..n];
        if ends && !CODE_AT_WORDS.contains(&word.to_ascii_lowercase().as_str()) {
            return Some(format!("@{word}"));
        }
    }
    None
}

/// Rule 3: `^(similar to|sites like|alternatives to|competitors (of|to)|more like)\s+(https?://\S+)$`.
fn similar_url(q: &str) -> Option<String> {
    const LEADS: &[&str] = &[
        "similar to",
        "sites like",
        "alternatives to",
        "competitors of",
        "competitors to",
        "more like",
    ];
    // Compare the original text, ASCII case-insensitively: offsets taken
    // from a lowercased copy can land inside a multi-byte character.
    let lead = LEADS
        .iter()
        .find(|l| q.get(..l.len()).is_some_and(|h| h.eq_ignore_ascii_case(l)))?;
    let rest = &q[lead.len()..];
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let url = rest.trim_start();
    is_absolute_url(url).then(|| url.to_string())
}

/// Rule 4's number form: `\b(US|EP|WO)\s?\d{6,}\b`.
fn patent_number(lower: &str) -> Option<String> {
    for office in ["us", "ep", "wo"] {
        for (i, _) in lower.match_indices(office) {
            if !boundary_before(lower, i) {
                continue;
            }
            let mut rest = &lower[i + 2..];
            if let Some(c) = rest.chars().next().filter(|c| c.is_whitespace()) {
                rest = &rest[c.len_utf8()..];
            }
            let digits = rest.chars().take_while(char::is_ascii_digit).count();
            if digits >= 6 && boundary_after(rest, digits) {
                return Some(lower[i..lower.len() - rest.len() + digits].to_string());
            }
        }
    }
    None
}

/// Rule 5: `\b(images?|photos?|pictures?|logos?|wallpapers?)\s+(of|for)\b`.
fn image_cue(lower: &str) -> Option<String> {
    const NOUNS: &[&str] = &[
        "images",
        "image",
        "photos",
        "photo",
        "pictures",
        "picture",
        "logos",
        "logo",
        "wallpapers",
        "wallpaper",
    ];
    for noun in NOUNS {
        for (i, _) in lower.match_indices(noun) {
            if !boundary_before(lower, i) {
                continue;
            }
            let after = &lower[i + noun.len()..];
            let trimmed = after.trim_start();
            if trimmed.len() == after.len() {
                continue; // \s+ needs at least one space
            }
            for prep in ["of", "for"] {
                if trimmed.starts_with(prep) && boundary_after(trimmed, prep.len()) {
                    return Some(format!("{noun} {prep}"));
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local(q: &str) -> (String, String) {
        let (d, _) = resolve_local(q);
        (d.mode, d.source)
    }

    #[test]
    fn pinned_providers_keep_auto_to_modes_they_serve() {
        let p = |names: &[&str]| names.iter().map(|n| n.to_string()).collect::<Vec<_>>();
        let d = fit_pinned(Mode::Academic, &p(&["brave"])).expect("brave has no academic");
        assert_eq!(
            (d.mode.as_str(), d.source.as_str()),
            ("general", "fallback")
        );
        assert_eq!(
            d.reason,
            "brave cannot serve academic, so general was used."
        );
        assert!(fit_pinned(Mode::Places, &p(&["exa"])).is_some());
        assert!(fit_pinned(Mode::People, &p(&["serper"])).is_some());
        // A mode every pinned provider serves stays; so does general itself.
        assert!(fit_pinned(Mode::Places, &p(&["serper"])).is_none());
        assert!(fit_pinned(Mode::General, &p(&["exa"])).is_none());
    }

    /// Mirrors the server's vectors: (query, mode, source) without JEV.
    #[test]
    fn rule_table() {
        let cases: &[(&str, &str, &str)] = &[
            ("https://example.com/post?id=1", "extract", "rule"),
            ("  HTTPS://Example.com  ", "extract", "rule"),
            ("read https://example.com", "general", "fallback"),
            ("what are people saying on X about rust", "social", "rule"),
            ("latest tweets from openai", "social", "rule"),
            ("Twitter reactions to the launch", "social", "rule"),
            ("x.com/elonmusk status", "social", "rule"),
            ("@sama thoughts on agi", "social", "rule"),
            ("mail boris@example.com", "general", "fallback"),
            ("@gmail.com smtp settings", "general", "fallback"),
            ("@john@example.com", "general", "fallback"),
            ("ping @sama. thoughts?", "social", "rule"),
            ("onx deals", "general", "fallback"),
            ("similar to https://linear.app", "similar", "rule"),
            ("Competitors of https://stripe.com", "similar", "rule"),
            ("similar to linear", "general", "fallback"),
            ("patents on solid state batteries", "patents", "rule"),
            ("prior art for touchscreen gestures", "patents", "rule"),
            ("US 10123456 claims", "patents", "rule"),
            ("EP1234567", "patents", "rule"),
            ("bus 1234567 timetable", "general", "fallback"),
            ("images of the golden gate bridge", "images", "rule"),
            ("logo for stripe", "images", "rule"),
            ("photography of birds", "general", "fallback"),
            ("ramen restaurants in soho", "places", "heuristic"),
            ("coffee near me", "places", "heuristic"),
            ("café in paris", "places", "heuristic"),
            ("h-index of geoffrey hinton", "scholar", "heuristic"),
            (
                "cited by count for attention is all you need",
                "scholar",
                "heuristic",
            ),
            (
                "meta-analysis of intermittent fasting",
                "academic",
                "heuristic",
            ),
            ("arxiv diffusion models", "academic", "heuristic"),
            ("who is the ceo of anthropic", "people", "heuristic"),
            ("linkedin jane doe", "people", "heuristic"),
            ("breaking: earthquake", "news", "heuristic"),
            ("rust release today", "news", "heuristic"),
            ("newsletter tips", "general", "fallback"),
            ("rust async runtime", "general", "fallback"),
        ];
        for (q, mode, source) in cases {
            assert_eq!(
                local(q),
                (mode.to_string(), source.to_string()),
                "query {q:?}"
            );
        }
    }

    /// The server's judge.test.ts vectors for amended rule 2: developer
    /// at-words, npm scopes, plot axes and Twitter as a web-dev word are not
    /// social (a wrong social route costs about 45x a general search).
    #[test]
    fn rule_2_ignores_code_at_words_axes_and_web_dev_twitter() {
        for q in [
            "how to install @types/node in a pnpm workspace",
            "@tanstack/react-query invalidateQueries example",
            "@aws-sdk/client-s3 presigned url",
            "python @property decorator setter",
            "css @media query for dark mode",
            "java @Override annotation meaning",
            "flask @app.route methods post",
            "matplotlib rotate tick labels on x axis",
            "seaborn barplot labels on x-axis overlapping",
            "plot y on X axis log scale",
            "twitter card meta tags for nextjs",
            "twitter bootstrap 3 grid",
            "how to embed a tweet in react",
            "angular @Input vs @Output",
            "email me at jane@example.com",
            "contact support@stripe.com",
            "linux x86 assembly",
            "write to @stripe.com",
        ] {
            assert_ne!(local(q).0, "social", "query {q:?}");
        }
        for q in [
            "twitter reactions to the debate",
            "what are people saying on X about it",
            "@elonmusk recent posts",
            "x.com/naval posts",
            "latest tweets from nasa",
            "what did @sama say about gpt-5",
            "posts from @nasa.",
            "trending on X today",
            "@vercel outage?",
        ] {
            assert_eq!(local(q), ("social".into(), "rule".into()), "query {q:?}");
        }
    }

    /// Lowercasing can change byte length (U+212A KELVIN SIGN is 3 bytes and
    /// lowercases to 'k'); rule 3 used to slice the original with offsets
    /// from the lowercased copy and panicked, leaving stdout empty.
    #[test]
    fn rule_3_never_panics_on_case_folding_that_changes_length() {
        assert_eq!(local("SITES LI\u{212A}E https://example.com").0, "general");
        assert_eq!(local("SITES LIKE https://example.com").0, "similar");
    }

    /// The server's judge.test.ts freshness vectors, through the local rules.
    #[test]
    fn freshness_vectors_match_the_server() {
        let f = |q: &str| {
            let (d, _) = resolve_local(q);
            freshness_for(q, auto_mode(&d.mode).unwrap()).map(|f| f.0)
        };
        assert_eq!(f("stock market today"), Some("day"));
        assert_eq!(f("what happened yesterday news"), Some("day"));
        assert_eq!(f("breaking news japan"), Some("week"));
        assert_eq!(f("tweets this month about rust"), Some("month"));
        assert_eq!(f("photos of fireworks this year"), None);
        assert_eq!(f("restaurants in soho open today"), None);
        // A freshness word in a general or academic search sets no filter.
        assert_eq!(
            freshness_for("latest stable version of python", Mode::General),
            None
        );
        assert_eq!(
            freshness_for("latest meta-analysis on fasting", Mode::Academic),
            None
        );
    }

    #[test]
    fn freshness_words_apply_only_in_news_and_social() {
        assert!(freshness_applies(Mode::News) && freshness_applies(Mode::Social));
        for m in [Mode::General, Mode::Academic, Mode::Places, Mode::Images] {
            assert!(!freshness_applies(m), "{m}");
        }
    }

    #[test]
    fn decisive_rules_win_over_signals() {
        // "news" (rule 10) and "patent" (rule 4): the decisive rule decides.
        assert_eq!(local("patent news").0, "patents");
        // Signal order: places (6) before news (10).
        assert_eq!(local("restaurants in london today").0, "places");
    }

    #[test]
    fn similar_searches_the_captured_url() {
        let (d, q) = resolve_local("sites like https://news.ycombinator.com");
        assert_eq!(d.mode, "similar");
        assert_eq!(q.as_deref(), Some("https://news.ycombinator.com"));
    }

    #[test]
    fn freshness_from_explicit_words_only() {
        assert_eq!(freshness("earthquake today").map(|f| f.0), Some("day"));
        assert_eq!(freshness("latest rust release").map(|f| f.0), Some("week"));
        assert_eq!(freshness("gdp this month").map(|f| f.0), Some("month"));
        assert_eq!(freshness("best films this year").map(|f| f.0), Some("year"));
        assert_eq!(freshness("todays news"), None);
        assert_eq!(freshness("rust"), None);
    }

    #[test]
    fn auto_never_resolves_to_deep() {
        assert_eq!(auto_mode("deep"), None);
        assert_eq!(auto_mode("news"), Some(Mode::News));
        assert_eq!(auto_mode("nonsense"), None);
    }

    #[test]
    fn reasons_are_plain_sentences() {
        let (d, _) = resolve_local("coffee near me");
        assert_eq!(d.reason, "Matched 'near me' (rule 6); used places.");
    }
}
