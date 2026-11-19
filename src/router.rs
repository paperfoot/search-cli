//! `auto` mode: the deterministic rules of ROUTING.md, as a pure function.
//!
//! Rules 1-5 and 11 are decisive (the first match decides, the routing
//! model is skipped); rule 11, a community source named in the query
//! (Reddit, Hacker News), is checked right after rule 2. Rules 6-10 are signals: when not logged in the first one in table
//! order decides; logged in, metasearch.sh's routing model decides. Auto never picks `deep`.
//! A platform cue (X/Twitter, TikTok, Instagram, YouTube, Threads or a
//! Facebook page) makes the search `social` and pins it to the platforms it
//! names ([`social_pins`]). The server runs the same
//! patterns for rules 1-10 and for rule 11's named sources; the table tests
//! below mirror its vectors. (Its rule 11 still holds the phrases that are
//! rule 12's here.)
//!
//! Rule 12 is the CLI's own (19 Nov 2026): a query that asks what people
//! think, whether something is worth buying, how two things compare or what
//! is spreading stays `general` (`news` when it has a news word) and has
//! `social` and `community` added to it ([`people_cue`], [`PEOPLE_LANES`]).
//! Until that day auto reached `social` only on a platform cue, and "what do
//! people say about" was a rule 11 phrase that searched `community` alone.

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
    /// Rule 2: the platforms named, for the reason ("X/Twitter", "TikTok").
    pub label: Option<String>,
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
    if cannot.is_empty() {
        return None;
    }
    // Pinned community sources (-p reddit, -p hackernews) search community;
    // pinned platforms (-p tiktok) search social.
    let to = if specs.iter().all(|s| s.modes.contains(&Mode::General)) {
        Mode::General
    } else if specs.iter().all(|s| s.modes.contains(&Mode::Community)) {
        Mode::Community
    } else if specs.iter().all(|s| s.modes.contains(&Mode::Social)) {
        Mode::Social
    } else {
        return None;
    };
    Some(AutoDecision {
        mode: to.to_string(),
        source: "fallback".to_string(),
        reason: format!(
            "{} cannot serve {mode}, so {to} was used.",
            cannot.join(", ")
        ),
        probabilities: None,
        filters_applied: None,
        latency_ms: None,
        added: Vec::new(),
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
    if let Some(h) = social_hit(q) {
        return Some(h);
    }
    if let Some(cue) = community_cue(q) {
        return Some(hit(11, Mode::Community, &cue));
    }
    if let Some(url) = similar_url(q) {
        let mut h = hit(3, Mode::Similar, "a request for similar sites");
        h.query = Some(url);
        return Some(h);
    }
    if let Some(cue) = patent_word(&lower)
        .or_else(|| first_phrase(&lower, &["prior art"]))
        .or_else(|| patent_number(q))
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

/// Rules 6-10 that fire, in table order (names go to the routing model as hints).
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

/// The decision without the routing model: decisive rules, then rule 12
/// when `people` allows it, then the first signal, then general.
pub fn resolve_local(query: &str, people: bool) -> (AutoDecision, Option<String>) {
    if let Some(h) = decisive(query) {
        return (decision_for(&h, "rule"), h.query);
    }
    if let Some(h) = people.then(|| people_hit(query)).flatten() {
        return (decision_for(&h, "rule"), None);
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
            latency_ms: None,
            added: Vec::new(),
        },
        None,
    )
}

pub fn decision_for(h: &Hit, source: &str) -> AutoDecision {
    let why = match h.rule {
        1 => "The query is a URL".to_string(),
        2 => format!(
            "Mentions {} ('{}')",
            h.label.as_deref().unwrap_or("X/Twitter"),
            h.cue
        ),
        3 => "Asked for sites similar to a URL".to_string(),
        11 => format!("Names a community source ('{}')", h.cue),
        12 => format!("Asks what people think ('{}')", h.cue),
        _ => format!("Matched '{}'", h.cue),
    };
    let added: Vec<String> = if h.rule == 12 {
        PEOPLE_LANES.iter().map(|m| m.to_string()).collect()
    } else {
        Vec::new()
    };
    let tail = if added.is_empty() { "" } else { PEOPLE_TAIL };
    AutoDecision {
        mode: h.mode.to_string(),
        source: source.to_string(),
        reason: format!("{why} (rule {}); used {}{tail}.", h.rule, h.mode),
        probabilities: None,
        filters_applied: None,
        latency_ms: None,
        added,
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
            | Mode::Community
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
        label: None,
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

/// Rule 2's platform cues: (provider, name, phrases). A phrase matches as
/// whole words; [`PLATFORM_NOT`] after it, or "embed" before it, makes it a
/// developer question, not a request for posts. Bare "threads" (code,
/// Reddit threads) and bare "facebook" (React, Prophet) are not cues.
pub const PLATFORM_CUES: &[(&str, &str, &[&str])] = &[
    (
        "tiktok",
        "TikTok",
        &[
            "on tiktok",
            "tiktok videos",
            "tiktok video",
            "tiktoks",
            "tiktok",
            "tik tok",
        ],
    ),
    (
        "instagram",
        "Instagram",
        &[
            "on instagram",
            "instagram reels",
            "instagram reel",
            "instagram posts",
            "instagram",
            "on insta",
            "insta reels",
            "ig reels",
            "ig reel",
            "on ig",
        ],
    ),
    (
        "youtube",
        "YouTube",
        &[
            "on youtube",
            "youtube comments",
            "youtube videos",
            "youtube video",
            "youtube shorts",
            "youtubers",
            "youtuber",
            "youtube",
            "on yt",
        ],
    ),
    (
        "threads",
        "Threads",
        &[
            "on threads",
            "threads posts",
            "threads app",
            "threads.net",
            "threads.com",
            "meta threads",
        ],
    ),
    (
        "facebook",
        "Facebook",
        &[
            "on facebook",
            "facebook pages",
            "facebook page",
            "facebook posts",
            "facebook post",
            "fb pages",
            "fb page",
            "on fb",
        ],
    ),
];

/// Words after a platform name that make the query about building on it.
const PLATFORM_NOT: &[&str] = &[
    "api",
    "apis",
    "sdk",
    "oauth",
    "pixel",
    "stock",
    "ipo",
    "shares",
    "earnings",
    "logo",
    "inc",
    "embed",
    "embeds",
    "login",
    "downloader",
    "dl",
    "dlp",
    "graph",
    "clone",
    "developer",
    "developers",
    "iframe",
    "player",
    "data",
];

/// Where `phrase` occurs in `lower` as a cue: whole words, not before a
/// developer word, not after "embed".
fn cue_at(lower: &str, phrase: &str) -> Option<usize> {
    lower.match_indices(phrase).map(|(i, _)| i).find(|&i| {
        let end = i + phrase.len();
        if !boundary_before(lower, i) || !boundary_after(lower, end) {
            return false;
        }
        let next = lower[end..].trim_start_matches([' ', '-', '_']);
        let dev = PLATFORM_NOT
            .iter()
            .any(|w| next.starts_with(w) && boundary_after(next, w.len()));
        !dev && !after_embed(&lower[..i])
    })
}

/// The platforms a query names, in table order, with the phrase that named
/// each.
pub fn platform_cues(query: &str) -> Vec<(&'static str, &'static str, String)> {
    let lower = query.to_lowercase();
    PLATFORM_CUES
        .iter()
        .filter_map(|(provider, label, phrases)| {
            phrases
                .iter()
                .find(|p| cue_at(&lower, p).is_some())
                .map(|p| (*provider, *label, p.to_string()))
        })
        .collect()
}

/// The social providers a query pins: `xai` for an X cue, then each
/// platform it names. Empty means the default fan-out.
pub fn social_pins(query: &str) -> Vec<&'static str> {
    let x = x_cue(query.trim()).map(|_| "xai");
    x.into_iter()
        .chain(platform_cues(query).into_iter().map(|(p, _, _)| p))
        .collect()
}

/// The platforms a social search with no platform cue asks, at most two
/// paid calls: X plus the one most likely to hold the answer. The same
/// rules, in the same order, as metasearch.sh's rule-based pick
/// (`pickSocialPlatforms`), so both sides choose alike; logged in, the
/// server refines this with its own model.
pub fn pick_social_platforms(query: &str) -> Vec<&'static str> {
    const REAL_TIME: &[&str] = &[
        "breaking",
        "right now",
        "just now",
        "live",
        "happening",
        "today",
        "tonight",
        "this morning",
        "outage",
        "down right now",
        "earthquake",
        "election results",
        "election result",
        "announced",
        "announcement",
    ];
    const SHORT_FORM: &[&str] = &[
        "trends",
        "trend",
        "trending",
        "viral",
        "memes",
        "meme",
        "dances",
        "dance",
        "challenges",
        "challenge",
        "sounds",
        "sound",
        "songs",
        "song",
        "hacks",
        "hack",
        "duets",
        "duet",
        "fyp",
    ];
    const VIDEO_HOW: &[&str] = &[
        "how to",
        "tutorials",
        "tutorial",
        "guides",
        "guide",
        "walkthrough",
        "walk-through",
        "explained",
        "reviews",
        "reviewed",
        "review",
        "unboxing",
        "vs",
        "versus",
        "compare",
        "comparison",
        "setup",
        "installing",
        "installation",
        "install",
        "building",
        "build",
        "teardown",
        "hands-on",
        "first look",
        "lecture",
        "course",
        "podcast",
        "interview",
    ];
    const VISUAL: &[&str] = &[
        "fashion",
        "outfits",
        "outfit",
        "ootd",
        "styling",
        "style",
        "makeup",
        "beauty",
        "skincare",
        "nails",
        "nail",
        "hairstyles",
        "hairstyle",
        "hairs",
        "hair",
        "food",
        "recipes",
        "recipe",
        "restaurants",
        "restaurant",
        "cafes",
        "cafe",
        "coffee",
        "travel",
        "hotels",
        "hotel",
        "interiors",
        "interior",
        "decor",
        "aesthetic",
        "photography",
        "photos",
        "photo",
        "brands",
        "brand",
        "luxury",
        "jewellery",
        "jewelry",
        "tattoos",
        "tattoo",
        "weddings",
        "wedding",
        "fitness",
        "workouts",
        "workout",
    ];
    let lower = query
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if first_phrase(&lower, REAL_TIME).is_some() {
        vec!["xai"]
    } else if first_phrase(&lower, SHORT_FORM).is_some() {
        vec!["xai", "tiktok"]
    } else if first_phrase(&lower, VIDEO_HOW).is_some() {
        vec!["xai", "youtube"]
    } else if first_phrase(&lower, VISUAL).is_some() {
        vec!["xai", "instagram"]
    } else {
        vec!["xai", "tiktok"]
    }
}

/// Rule 2: an X cue or a platform cue, labelled with every platform named.
fn social_hit(q: &str) -> Option<Hit> {
    let x = x_cue(q);
    let platforms = platform_cues(q);
    let cue = x
        .clone()
        .or_else(|| platforms.first().map(|(_, _, c)| c.clone()))?;
    let labels: Vec<&str> = x
        .iter()
        .map(|_| "X/Twitter")
        .chain(platforms.iter().map(|(_, l, _)| *l))
        .collect();
    let mut h = hit(2, Mode::Social, &cue);
    h.label = Some(labels.join(" and "));
    Some(h)
}

/// `query` without its platform cue phrases, whitespace collapsed: what a
/// platform's own search is sent.
pub fn strip_platform_cues(query: &str) -> String {
    let mut phrases: Vec<&str> = PLATFORM_CUES
        .iter()
        .flat_map(|(_, _, p)| p.iter().copied())
        .collect();
    phrases.sort_by_key(|p| std::cmp::Reverse(p.len()));
    let mut text = query.to_string();
    for p in phrases {
        loop {
            let lower = text.to_lowercase();
            // Lowercasing can change byte lengths outside ASCII; only cut
            // when the offsets line up.
            if lower.len() != text.len() {
                break;
            }
            let Some(i) = lower
                .match_indices(p)
                .map(|(i, _)| i)
                .find(|&i| boundary_before(&lower, i) && boundary_after(&lower, i + p.len()))
            else {
                break;
            };
            text.replace_range(i..i + p.len(), " ");
        }
    }
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Words that name a community source. The phrases that ask what people
/// say without naming one ("what do people say about", "opinions about",
/// "experiences with") were here until 19 Nov 2026; they are rule 12's now
/// ([`PEOPLE_PHRASES`]), which keeps the pages and adds the posts.
const COMMUNITY_PHRASES: &[&str] = &[
    "reddit",
    "subreddit",
    "hacker news",
    "hackernews",
    "ask hn",
    "show hn",
];

/// Words after "reddit" that make it the company, not its threads.
const REDDIT_COMPANY: &[&str] = &["api", "stock", "ipo", "earnings", "shares", "logo", "inc"];

/// Units written like a subreddit: `r/min`, `r/sec` ("3000 r/min").
const UNIT_NAMES: &[&str] = &[
    "min", "mins", "sec", "secs", "hr", "hrs", "hour", "hours", "day", "days", "rad", "deg", "rev",
    "revs", "mol", "kg", "km", "mph", "kmh", "rpm", "ms",
];

/// Rule 11: a community source named in the query. "reddit" (not "reddit
/// api" or "reddit stock"), Hacker News or "on HN" (capitals; bare "HN" is
/// also Honduras), or an `r/<name>` of 3+ characters that is not a unit
/// (`r/min`).
fn community_cue(q: &str) -> Option<String> {
    let lower = q.to_lowercase();
    for p in COMMUNITY_PHRASES {
        let hit = lower.match_indices(*p).any(|(i, m)| {
            let end = i + m.len();
            if !boundary_before(&lower, i) || !boundary_after(&lower, end) {
                return false;
            }
            if *p == "reddit" {
                let next = lower[end..].trim_start();
                return !REDDIT_COMPANY
                    .iter()
                    .any(|w| next.starts_with(w) && boundary_after(next, w.len()));
            }
            true
        });
        if hit {
            return Some(p.to_string());
        }
    }
    if q.match_indices("on HN")
        .any(|(i, m)| boundary_before(q, i) && boundary_after(q, i + m.len()))
    {
        return Some("on HN".to_string());
    }
    q.split_whitespace()
        .find(|w| {
            let w = w.trim_start_matches('/');
            let Some(name) = w.strip_prefix("r/") else {
                return false;
            };
            let name = name.trim_end_matches(['/', ',', '.', '?']);
            name.len() >= 3
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                && !UNIT_NAMES.contains(&name.to_ascii_lowercase().as_str())
        })
        .map(|w| w.trim_start_matches('/').to_string())
}

/// The modes rule 12 adds to a general search, in the order they share the
/// result slots: the first takes the odd one.
pub const PEOPLE_LANES: [Mode; 2] = [Mode::Social, Mode::Community];

/// How rule 12's reason ends. `main` cuts it when no added mode could run.
pub const PEOPLE_TAIL: &str = " and added posts";

/// Rule 12's cues: the query asks what people think, whether something is
/// worth buying, how two things compare, or what is spreading. Chosen
/// against 30 days of logged general searches (19 Nov 2026) and a review of
/// biomedical and code queries: words that fired on lookups were left out
/// ("creators", "recommendations", "versus", "comparison", "reaction to",
/// "feedback on", a bare "complaints" or "meme", "experience with").
const PEOPLE_PHRASES: &[&str] = &[
    // Asking people.
    "what do people say about",
    "what do people think about",
    "what do people think of",
    "what are people saying about",
    "what people think",
    "what people say",
    "people are saying",
    "opinions about",
    "opinions on",
    "experiences with",
    "thoughts on",
    "reactions to",
    "public opinion",
    "sentiment",
    "word of mouth",
    "testimonials",
    "complaints about",
    "customer complaints",
    "user complaints",
    "user feedback",
    "customer feedback",
    "anyone tried",
    "anyone used",
    "anyone using",
    "has anyone",
    "does anyone",
    "real users",
    "pain points",
    "what customers want",
    "what users want",
    "why people",
    "people love",
    "people hate",
    "frustrated with",
    "frustrations with",
    "backlash",
    "controversy",
    "outrage",
    "hype",
    "overrated",
    "underrated",
    // Buying and comparing.
    "worth it",
    "worth buying",
    "worth the money",
    "worth the price",
    "worth the hype",
    "worth getting",
    "worth paying",
    "should i buy",
    "should i get",
    "should i use",
    "should i switch",
    "should i upgrade",
    "is it good",
    "any good",
    "anyone recommend",
    "would you recommend",
    "which is better",
    "better than",
    "alternatives to",
    "alternative to",
    "pros and cons",
    "reviews",
    "honest review",
    "user review",
    "customer review",
    "vs",
    "compared to",
    "best",
    // Spreading.
    "trending",
    "viral",
    "memes",
    // Trust.
    "scam",
    "legit",
    "trustworthy",
];

/// Phrases in which a rule 12 cue word means something else. They are cut
/// from the query before the cues are looked for.
const PEOPLE_NOT: &[&str] = &[
    "best practices",
    "best practice",
    "best effort",
    "best-effort",
    "best fit",
    "best case",
    "best-case",
    "best-first",
    "best way",
    "best guess",
    "best friend",
    "best buy",
    "best supportive care",
    "at best",
    "code reviews",
    "peer reviews",
    "systematic reviews",
    "literature reviews",
    "annual reviews",
    "nature reviews",
    "cochrane reviews",
    "research reviews",
    "performance reviews",
    "resource reviews",
    "choice reviews",
    "code review",
    "peer review",
    "systematic review",
    "literature review",
    "annual review",
    "performance review",
    "security review",
    "design review",
    "business review",
    "technology review",
    "law review",
    "app store review",
    "app review",
    "in review",
    "court opinions",
    "judicial opinions",
    "advisory opinions",
    "dissenting opinions",
    "legal opinions",
    "vs code",
    "vs. code",
    "sentiment analysis",
    "sentiment classifier",
    "sentiment model",
    "sentiment score",
    "hype cycle",
    "allergic reactions",
    "adverse reactions",
    "skin reactions",
    "immune reactions",
    "chemical reactions",
    "non-viral",
    "viral loads",
    "viral load",
    "viral vectors",
    "viral vector",
    "viral infections",
    "viral infection",
    "viral rna",
    "viral dna",
    "viral proteins",
    "viral protein",
    "viral replication",
    "viral genome",
    "viral disease",
    "viral shedding",
    "viral titers",
    "viral titer",
    "viral entry",
    "viral particles",
    "viral particle",
];

/// Words that make the query a lookup of documents or code, where posts
/// would only take the pages' places.
const LOOKUP_WORDS: &[&str] = &[
    "api",
    "sdk",
    "docs",
    "documentation",
    "error",
    "exception",
    "changelog",
    "release notes",
    "source code",
    "github",
    "npm",
    "syntax",
    "regex",
    "stack trace",
    "stacktrace",
    "traceback",
];

/// Words of a biomedical question, where "vs", "best", "viral" and
/// "reviews" are terms of art ("metformin vs placebo", "best supportive
/// care", "viral hepatitis", "Ageing Research Reviews"). Words with an
/// everyday sense are left out ("mouse", "cell", "protein", "treatment").
const SCIENCE_WORDS: &[&str] = &[
    "disease",
    "diseases",
    "cancer",
    "tumor",
    "tumour",
    "clinical",
    "placebo",
    "mortality",
    "patients",
    "dosage",
    "syndrome",
    "infection",
    "infections",
    "inhibitor",
    "inhibitors",
    "receptor",
    "receptors",
    "proteins",
    "gene",
    "genes",
    "genome",
    "cells",
    "mice",
    "in vivo",
    "in vitro",
    "rna",
    "dna",
    "virus",
    "viruses",
    "hepatitis",
    "vaccine",
    "vaccines",
    "screening",
    "efficacy",
    "randomized",
    "randomised",
    "cohort",
    "biomarker",
    "biomarkers",
    "secretion",
    "dementia",
    "motif",
    "nanoparticle",
    "nanoparticles",
    "capsid",
    "senolytic",
    "senolytics",
    "enzyme",
    "antibody",
    "antibodies",
    "assay",
];

/// Rule 12's cue, or None.
/// - Never on an exact-phrase or operator query (a quote, `site:`, a URL).
/// - A question's frame ("what do people say about", [`PEOPLE_FRAMES`]) is
///   an explicit request and always counts.
/// - Any other cue yields to a lookup word, a biomedical word, and a places,
///   scholar, academic or people signal (rules 6-9): those ask for a page, a
///   paper, a place or a person. "X review" counts only as the last words.
pub fn people_cue(query: &str) -> Option<String> {
    let lower = query
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if lower.contains('"')
        || lower.contains("site:")
        || lower.contains("http://")
        || lower.contains("https://")
    {
        return None;
    }
    if let Some(frame) = first_phrase(&lower, PEOPLE_FRAMES) {
        return Some(frame);
    }
    if first_phrase(&lower, LOOKUP_WORDS).is_some()
        || first_phrase(&lower, SCIENCE_WORDS).is_some()
        || signals(&lower).iter().any(|h| (6..=9).contains(&h.rule))
    {
        return None;
    }
    let cut = cut_phrases(&lower, PEOPLE_NOT);
    first_phrase(&cut, PEOPLE_PHRASES).or_else(|| {
        let cut = cut.trim_end();
        (cut.ends_with(" review") && cut.len() > " review".len()).then(|| "review".to_string())
    })
}

/// Rule 12 as a hit. The pages stay what they would have been: `news` when
/// the query has a news word (rule 10), else `general`.
pub fn people_hit(query: &str) -> Option<Hit> {
    let cue = people_cue(query)?;
    let base = if signals(query).iter().any(|h| h.rule == 10) {
        Mode::News
    } else {
        Mode::General
    };
    Some(hit(12, base, &cue))
}

/// The phrases of rule 12 that are a question's frame, not its subject.
const PEOPLE_FRAMES: &[&str] = &[
    "what do people say about",
    "what do people think about",
    "what do people think of",
    "what are people saying about",
    "opinions about",
    "experiences with",
    "thoughts on",
];
// Every frame is ASCII: `people_query` relies on it.

/// The query the added modes search: `query` without a frame such as "what
/// do people say about", because Reddit, Hacker News and the platforms match
/// the words they are given. Unchanged when nothing else would be left.
pub fn people_query(query: &str) -> String {
    // ASCII lowercasing keeps every byte offset, whatever else the query
    // holds (full lowercasing can move them: "İ" shrinks, "K" grows).
    let mut low = query.to_ascii_lowercase();
    let mut text = query.to_string();
    for p in PEOPLE_FRAMES {
        while let Some(i) = low
            .match_indices(p)
            .map(|(i, _)| i)
            .find(|&i| boundary_before(&low, i) && boundary_after(&low, i + p.len()))
        {
            text.replace_range(i..i + p.len(), " ");
            low.replace_range(i..i + p.len(), " ");
        }
    }
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let text = text.trim_end_matches('?').trim();
    if text.is_empty() {
        query.to_string()
    } else {
        text.to_string()
    }
}

/// `lower` with every bounded occurrence of `phrases` replaced by a space.
fn cut_phrases(lower: &str, phrases: &[&str]) -> String {
    let mut text = lower.to_string();
    for p in phrases {
        while let Some(i) = text
            .match_indices(p)
            .map(|(i, _)| i)
            .find(|&i| boundary_before(&text, i) && boundary_after(&text, i + p.len()))
        {
            text.replace_range(i..i + p.len(), " ");
        }
    }
    text
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

/// Rule 4's words: `\bpatents?\b(?!\s+leather)`. Never "patent leather"
/// (shoes and bags).
fn patent_word(lower: &str) -> Option<String> {
    ["patent", "patents"]
        .iter()
        .find(|w| {
            lower.match_indices(*w).any(|(i, m)| {
                let end = i + m.len();
                let after = &lower[end..];
                let next = after.trim_start();
                boundary_before(lower, i)
                    && boundary_after(lower, end)
                    && !(next.len() < after.len() && next.starts_with("leather"))
            })
        })
        .map(|w| w.to_string())
}

/// Rule 4's numbers, case-sensitive like the server's `PATENT_NUMBER`:
/// `\b(?:US|EP)\s?\d{7,8}(?:\s?[AB]\d)?\b|\bWO\s?\d{4}\/?\d{6}\b`. A phone
/// number after "call us" or a long order number is not a patent.
fn patent_number(q: &str) -> Option<String> {
    let digits = |s: &str| s.chars().take_while(char::is_ascii_digit).count();
    let skip_space = |s: &str| match s.chars().next() {
        Some(c) if c.is_whitespace() => s.len() - c.len_utf8(),
        _ => s.len(),
    };
    for office in ["US", "EP", "WO"] {
        for (i, _) in q.match_indices(office) {
            if !boundary_before(q, i) {
                continue;
            }
            let after_office = &q[i + 2..];
            let rest = &after_office[after_office.len() - skip_space(after_office)..];
            // Byte offset in `rest` where the number ends, if it is one.
            let end = if office == "WO" {
                (digits(rest) >= 4)
                    .then(|| {
                        let tail = rest[4..].strip_prefix('/').unwrap_or(&rest[4..]);
                        let at = rest.len() - tail.len();
                        (digits(tail) >= 6 && boundary_after(tail, 6)).then_some(at + 6)
                    })
                    .flatten()
            } else {
                let n = digits(rest);
                if !(7..=8).contains(&n) {
                    None
                } else {
                    // Optional kind code (`\s?[AB]\d`), then a word boundary.
                    let tail = &rest[n..];
                    let kind = &tail[tail.len() - skip_space(tail)..];
                    let mut k = kind.chars();
                    let with_kind = matches!(k.next(), Some('A' | 'B'))
                        && k.next().is_some_and(|c| c.is_ascii_digit())
                        && boundary_after(kind, 2);
                    if with_kind {
                        Some(rest.len() - kind.len() + 2)
                    } else {
                        boundary_after(rest, n).then_some(n)
                    }
                }
            };
            if let Some(end) = end {
                return Some(q[i..q.len() - rest.len() + end].to_string());
            }
        }
    }
    None
}

/// Words before "images" that make them system images, which want docs, not
/// pictures ("base docker images for node 22", "vm images for arm64").
const SYSTEM_IMAGE_WORDS: &[&str] = &[
    "docker",
    "container",
    "disk",
    "iso",
    "vm",
    "base",
    "os",
    "system",
    "boot",
    "cloud",
    "machine",
];

/// Rule 5, except system images:
/// `(?<!\b(?:docker|container|disk|iso|vm|base|os|system|boot|cloud|machine)\s+)\b(images?|photos?|pictures?|logos?|wallpapers?)\s+(of|for)\b`.
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
            if !boundary_before(lower, i) || after_system_word(&lower[..i]) {
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

/// The lookbehind of rule 5: `before` ends in a system-image word and at
/// least one space.
fn after_system_word(before: &str) -> bool {
    let word_end = before.trim_end();
    word_end.len() < before.len()
        && SYSTEM_IMAGE_WORDS
            .iter()
            .any(|w| word_end.ends_with(w) && boundary_before(word_end, word_end.len() - w.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local(q: &str) -> (String, String) {
        let (d, _) = resolve_local(q, true);
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

    /// The server's rule 4 and 5 vectors (metasearch `judge.test.ts`, "rules
    /// 4 and 5: patents and images fire on their patterns and stay quiet on
    /// traps"), amended 2 Nov 2026: system images and look-alike numbers are
    /// not decisive. The extra cases were checked against the server's
    /// regexes in node.
    #[test]
    fn rules_4_and_5_fire_on_their_patterns_and_stay_quiet_on_traps() {
        let rule = |q: &str| decisive(q).map(|h| h.mode.to_string());
        for (q, mode) in [
            ("prior art for swipe to unlock", "patents"),
            ("US 10123456 B2", "patents"),
            ("photos of the Eiffel Tower", "images"),
            ("wallpapers for a 4k monitor", "images"),
            ("logo for stripe", "images"),
            ("EP1234567", "patents"),
            ("WO2020123456 claims", "patents"),
            ("WO 2020/123456", "patents"),
            ("US 1234567B1", "patents"),
            ("US 1234567 A1x", "patents"),
            ("EP 12345678 B1", "patents"),
            ("dockerbase images for x", "images"),
            ("my photos of iso images for", "images"),
        ] {
            assert_eq!(rule(q).as_deref(), Some(mode), "{q}");
        }
        for q in [
            "best base docker images for node 22",
            "official container images for arm64 raspberry pi",
            "ubuntu iso images for virtualbox",
            "patent leather chelsea boots women",
            "call us 0800123456 scam",
            "order US 0800123456 status",
            "image processing library for rust",
            "us 1234567 lowercase office",
            "WO2020123456A1",
            "patents leather tanning",
            "US 123456789",
            "cloud  images for arm",
        ] {
            let r = rule(q);
            assert!(
                r.as_deref() != Some("images") && r.as_deref() != Some("patents"),
                "{q} -> {r:?}"
            );
        }
    }

    /// Mirrors the server's vectors: (query, mode, source) without the routing model.
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

    /// Rule 11: one case per cue, and look-alikes that stay out.
    #[test]
    fn rule_11_routes_community_cues_and_nothing_else() {
        for (q, cue) in [
            ("best mechanical keyboard reddit", "reddit"),
            ("which subreddit for rust jobs", "subreddit"),
            ("hacker news thread on sqlite", "hacker news"),
            ("hackernews rust async", "hackernews"),
            ("Ask HN: freelancing in 2026", "ask hn"),
            ("show hn launch tips", "show hn"),
            ("what they said on HN about the outage", "on HN"),
            ("r/rust async runtimes", "r/rust"),
        ] {
            let h = decisive(q).unwrap_or_else(|| panic!("{q}: no rule"));
            assert_eq!(
                (h.rule, h.mode, h.cue.as_str()),
                (11, Mode::Community, cue),
                "{q}"
            );
            assert_eq!(local(q), ("community".into(), "rule".into()), "{q}");
        }
        for q in [
            "rust async runtime",
            "reddit api pricing",
            "reddit stock price",
            "hn stands for what",
            "experience with python jobs",
            "redditor meaning",
            "opinion on the supreme court ruling",
            // Reviewer's false routes (4 Nov): a unit, a court, a country code.
            "convert 3000 r/min to rad/s",
            "supreme court opinions on affirmative action",
            "HN visa requirements Honduras",
            "HN discussion of the outage",
            "r/m2 conversion",
        ] {
            assert_ne!(local(q).0, "community", "{q}");
        }
        // X wins over a community source: rule 2 comes first.
        assert_eq!(local("what are people saying on X about htmx").0, "social");
        let (d, _) = resolve_local("best mechanical keyboard reddit", true);
        assert_eq!(
            d.reason,
            "Names a community source ('reddit') (rule 11); used community."
        );
        assert!(d.added.is_empty());
    }

    /// Rule 12 (the CLI's own, not mirrored by the server): a query that
    /// asks what people think stays general and has social and community
    /// added. The first six were rule 11 phrases until 19 Nov 2026, when
    /// they searched community alone.
    #[test]
    fn rule_12_adds_posts_to_general_for_people_questions() {
        for (q, cue) in [
            (
                "what do people say about the framework laptop",
                "what do people say about",
            ),
            (
                "what do people think about bun",
                "what do people think about",
            ),
            ("what do people think of zed", "what do people think of"),
            (
                "what are people saying about htmx",
                "what are people saying about",
            ),
            ("opinions about remote work", "opinions about"),
            ("experiences with hetzner", "experiences with"),
            ("opinions on tailwind v4", "opinions on"),
            ("is the uplift v2 standing desk worth it", "worth it"),
            ("Uplift V2 standing desk worth buying 2026", "worth buying"),
            ("air fryer reviews", "reviews"),
            ("iphone 17 pro review", "review"),
            ("notion vs obsidian", "vs"),
            ("git merge vs rebase", "vs"),
            ("alternatives to notion", "alternatives to"),
            ("best standing desk for tall people", "best"),
            ("Lemon Squeezy payouts held founders backlash", "backlash"),
            ("pet video formats going viral this month", "viral"),
            ("is temu legit", "legit"),
            ("customer feedback on linear", "customer feedback"),
            // A frame is an explicit request: it counts beside a lookup
            // word, a biomedical word or a person.
            (
                "what do people say about the stripe api",
                "what do people say about",
            ),
            ("thoughts on the ceo of stripe", "thoughts on"),
            ("experiences with rapamycin dosage", "experiences with"),
        ] {
            let h = people_hit(q).unwrap_or_else(|| panic!("{q}: no rule"));
            assert_eq!(
                (h.rule, h.mode, h.cue.as_str()),
                (12, Mode::General, cue),
                "{q}"
            );
            let (d, rewritten) = resolve_local(q, true);
            assert_eq!(
                (d.mode.as_str(), d.source.as_str()),
                ("general", "rule"),
                "{q}"
            );
            assert_eq!(d.added, ["social", "community"], "{q}");
            assert_eq!(rewritten, None, "{q}");
            // Not allowed (-p or a domain filter): the rule is skipped.
            assert!(resolve_local(q, false).0.added.is_empty(), "{q}");
        }
        let (d, _) = resolve_local("opinions about tailwind v4", true);
        assert_eq!(
            d.reason,
            "Asks what people think ('opinions about') (rule 12); used general and added posts."
        );
    }

    /// A news word keeps the pages on news (rule 10, with its freshness);
    /// the posts are added to that.
    #[test]
    fn rule_12_keeps_news_for_the_pages_when_the_query_has_a_news_word() {
        for q in [
            "latest iphone reviews",
            "what's trending today",
            "reactions to the fed rate decision today",
        ] {
            let (d, _) = resolve_local(q, true);
            assert_eq!(
                (d.mode.as_str(), d.source.as_str()),
                ("news", "rule"),
                "{q}"
            );
            assert_eq!(d.added, ["social", "community"], "{q}");
            assert!(
                d.reason.ends_with("used news and added posts."),
                "{q}: {}",
                d.reason
            );
            assert!(freshness_for(q, Mode::News).is_some(), "{q}");
        }
        // Without the rule the same query is plain news, as before.
        let (d, _) = resolve_local("latest iphone reviews", false);
        assert_eq!((d.mode.as_str(), d.source.as_str()), ("news", "heuristic"));
        assert!(d.added.is_empty());
    }

    /// Lookups stay pages only. The first group misfired, or would have, in
    /// the 30 days of logged searches the cues were chosen against; the
    /// second is the reviewer's (19 Nov): terms of art in biomedicine, code
    /// and law, a shop, and a person.
    #[test]
    fn rule_12_leaves_lookups_alone() {
        for q in [
            "rust async runtime",
            "vs code extension api",
            "vs code vim keybindings",
            "best practices for rust error handling",
            "best way to parse json in swift",
            "code reviews checklist",
            "libguides systematic reviews AI tools",
            "Annual Reviews editor contact",
            "Nature Reviews Drug Discovery senolytics",
            "sentiment analysis python",
            "gartner hype cycle 2026",
            "viral load after antiretroviral therapy",
            "aav viral vector capsid",
            "allergic reaction to penicillin",
            "comparison operators in sql",
            "best rust http client github",
            "stripe api reviews endpoint",
            "tokio vs async-std docs",
            "healthspan vs lifespan study",
            "best restaurants in lisbon",
            "best paper on crispr delivery",
            "\"rack up views but man, people hate it\"",
            "site:news.ycombinator.com rust vs go",
            "reviews https://example.com/product",
            "experience with python jobs",
            "opinion on the supreme court ruling",
            "supreme court opinions on affirmative action",
            "Understanding TikTok's Recommendation System",
            "YouTube health source information panel eligibility creators",
            // Reviewer's vectors.
            "allergic reactions to penicillin",
            "adverse reactions to metformin",
            "hiv viral loads undetectable",
            "aav viral titers quantification",
            "viral particle assembly",
            "graft versus host disease treatment",
            "metformin vs placebo all-cause mortality",
            "non-viral gene delivery lipid nanoparticles",
            "viral hepatitis treatment guidelines",
            "polymerase chain reaction to detect sars-cov-2",
            "USPSTF recommendations for colorectal cancer screening",
            "best supportive care pancreatic cancer",
            "ageing research reviews senolytics",
            "negative feedback on gnrh secretion",
            "subjective memory complaints dementia risk",
            "comparison of rapamycin and everolimus",
            "MEME suite motif discovery",
            "string comparison in java",
            "best-first search",
            "best buy return policy",
            "who is the best orthopedic surgeon in singapore",
            "harvard business review",
            "mit technology review",
            "app store review",
            "year in review",
            "review",
        ] {
            assert_eq!(people_cue(q), None, "{q}");
        }
        // A named source or platform still decides first.
        assert_eq!(local("best mechanical keyboard reddit").0, "community");
        assert_eq!(local("standing desk reviews on tiktok").0, "social");
        assert!(resolve_local("standing desk reviews on tiktok", true)
            .0
            .added
            .is_empty());
    }

    /// The added modes search the subject, not the question's frame.
    #[test]
    fn people_query_drops_the_question_frame() {
        assert_eq!(
            people_query("What do people say about the Framework laptop?"),
            "the Framework laptop"
        );
        assert_eq!(people_query("opinions about tailwind v4"), "tailwind v4");
        assert_eq!(people_query("air fryer reviews"), "air fryer reviews");
        assert_eq!(people_query("notion vs obsidian"), "notion vs obsidian");
        // Nothing but the frame: the query is kept.
        assert_eq!(people_query("thoughts on"), "thoughts on");
        // Letters whose lowercase has another byte length keep their
        // offsets: these used to cut inside a character.
        assert_eq!(
            people_query("\u{0130}\u{0130} thoughts on \u{212B}\u{212B}"),
            "\u{0130}\u{0130} \u{212B}\u{212B}"
        );
        assert_eq!(
            people_query("\u{0130}stanbul thoughts on \u{1E9E}"),
            "\u{0130}stanbul \u{1E9E}"
        );
    }

    /// Rule 2's platform cues: one per cue, each pinning its platform, and
    /// developer questions that stay out.
    #[test]
    fn platform_cues_route_social_and_pin_their_platform() {
        for (q, pins, cue) in [
            ("mechanical keyboard on tiktok", vec!["tiktok"], "on tiktok"),
            ("tiktok latte art", vec!["tiktok"], "tiktok"),
            (
                "instagram reels latte art",
                vec!["instagram"],
                "instagram reels",
            ),
            ("best desk setups on ig", vec!["instagram"], "on ig"),
            (
                "youtube comments on the m5 macbook",
                vec!["youtube"],
                "youtube comments",
            ),
            ("keyboard reviews youtube", vec!["youtube"], "youtube"),
            (
                "what people post on threads about bluesky",
                vec!["threads"],
                "on threads",
            ),
            ("keychron facebook page", vec!["facebook"], "facebook page"),
            (
                "tiktok vs instagram reels for bakeries",
                vec!["tiktok", "instagram"],
                "tiktok",
            ),
            (
                "what twitter says about tiktok",
                vec!["xai", "tiktok"],
                "twitter",
            ),
        ] {
            let h = decisive(q).unwrap_or_else(|| panic!("{q}: no rule"));
            assert_eq!(
                (h.rule, h.mode, h.cue.as_str()),
                (2, Mode::Social, cue),
                "{q}"
            );
            assert_eq!(social_pins(q), pins, "{q}");
        }
        for q in [
            "youtube api quota limits",
            "embed a youtube video in react",
            "youtube-dl alternatives",
            "tiktok sdk android login",
            "instagram graph api permissions",
            "rust threads vs async tasks",
            "facebook prophet forecasting",
            "reddit threads about rust",
        ] {
            assert_ne!(local(q).0, "social", "{q}");
            assert!(social_pins(q).is_empty(), "{q}");
        }
        let (d, _) = resolve_local("mechanical keyboard on tiktok", true);
        assert_eq!(
            d.reason,
            "Mentions TikTok ('on tiktok') (rule 2); used social."
        );
        let (d, _) = resolve_local("trending on twitter", true);
        assert!(
            d.reason.starts_with("Mentions X/Twitter ('twitter')"),
            "{}",
            d.reason
        );
        assert_eq!(
            strip_platform_cues("Mechanical keyboard on TikTok"),
            "Mechanical keyboard"
        );
        let p = |names: &[&str]| names.iter().map(|n| n.to_string()).collect::<Vec<_>>();
        assert_eq!(
            fit_pinned(Mode::General, &p(&["tiktok"])).unwrap().mode,
            "social"
        );
    }

    #[test]
    fn pinned_community_sources_search_community() {
        let p = |names: &[&str]| names.iter().map(|n| n.to_string()).collect::<Vec<_>>();
        let d = fit_pinned(Mode::General, &p(&["hackernews"])).expect("hn has no general");
        assert_eq!(d.mode, "community");
        assert!(fit_pinned(Mode::Community, &p(&["reddit", "hackernews"])).is_none());
        // A mixed pin with no common mode is left for the engine to refuse.
        assert!(fit_pinned(Mode::General, &p(&["reddit", "serper"])).is_none());
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
            let (d, _) = resolve_local(q, true);
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
        let (d, q) = resolve_local("sites like https://news.ycombinator.com", true);
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
        let (d, _) = resolve_local("coffee near me", true);
        assert_eq!(d.reason, "Matched 'near me' (rule 6); used places.");
    }

    /// The same pick, rule for rule, as metasearch.sh's pickSocialPlatforms.
    #[test]
    fn social_pick_matches_the_server_rules() {
        assert_eq!(pick_social_platforms("earthquake right now"), ["xai"]);
        assert_eq!(pick_social_platforms("viral dance"), ["xai", "tiktok"]);
        assert_eq!(
            pick_social_platforms("iphone 17 review"),
            ["xai", "youtube"]
        );
        assert_eq!(
            pick_social_platforms("how to  fix a bike"),
            ["xai", "youtube"]
        );
        assert_eq!(
            pick_social_platforms("summer outfits"),
            ["xai", "instagram"]
        );
        assert_eq!(
            pick_social_platforms("mechanical keyboard"),
            ["xai", "tiktok"]
        );
        // Words, not substrings: "livestock" is not "live".
        assert_eq!(pick_social_platforms("livestock prices"), ["xai", "tiktok"]);
    }
}
