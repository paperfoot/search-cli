//! `auto` mode: the deterministic rules of ROUTING.md, as a pure function.
//!
//! Rules 1-5 and 11 are decisive (the first match decides, the routing
//! model is skipped); rule 11, a community source named in the query
//! (Reddit, Hacker News), is checked right after rule 2. Rules 6-10 are signals: when not logged in the first one in table
//! order decides; logged in, metasearch.sh's routing model decides. Auto never picks `deep`.
//! Rule 2 makes the search `social` only when the query asks for a
//! platform's own posts ("on tiktok", "youtube videos", "tweets", "on X");
//! the cue pins the search to the platforms it names ([`social_pins`]).
//!
//! Rule 12 is the CLI's own (19 Nov 2026, widened the same week): every
//! auto search whose pages are `general` or `news` also runs `social` and
//! `community`, and their posts are returned beside the pages
//! ([`posts_apply`], [`PEOPLE_LANES`], `blend`). Only a lookup is left
//! alone: an exact phrase, a `site:` query, an error or API-reference
//! question. Until then auto reached `social` only on a platform cue, a
//! query that merely mentioned TikTok or YouTube was searched on that
//! platform alone, and "what do people say about" searched `community`
//! alone. The server still has those older rules; its vectors are
//! mirrored below only where the two still agree (rules 1, 3-10, the
//! explicit forms of rule 2 and rule 11's named sources).

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

/// The decision without the routing model: decisive rules, then the first signal, then general.
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
        _ => format!("Matched '{}'", h.cue),
    };
    AutoDecision {
        mode: h.mode.to_string(),
        source: source.to_string(),
        reason: format!("{why} (rule {}); used {}.", h.rule, h.mode),
        probabilities: None,
        filters_applied: None,
        latency_ms: None,
        added: Vec::new(),
    }
}

/// A mode the routing model may choose. `social` and `community` are the
/// client's to decide (rule 2's explicit forms and rule 11): the server
/// still sends a bare platform mention to `social` and "what do people say
/// about" to `community`, where the client keeps the pages and adds posts.
pub fn server_may_pick(name: &str) -> bool {
    auto_mode(name).is_some_and(|m| !matches!(m, Mode::Social | Mode::Community))
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
            // "twitter" alone names the subject ("twitter algorithm
            // changes"): only "on twitter", "from twitter" or "twitter
            // threads/posts" asks for its posts. (0.14: the server still
            // treats the bare word as a cue.)
            if word == "twitter" {
                let before = lower[..i].trim_end();
                let asked = ["on", "from", "via"].iter().any(|w| {
                    before.ends_with(w) && boundary_before(before, before.len() - w.len())
                });
                let content = [
                    "thread",
                    "threads",
                    "post",
                    "posts",
                    "users",
                    "replies",
                    "reactions",
                    "reaction",
                ]
                .iter()
                .any(|w| next.starts_with(w) && boundary_after(next, w.len()));
                if !asked && !content {
                    continue;
                }
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
            "on tik tok",
            "tiktok videos",
            "tiktok video",
            "tiktoks",
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

/// A platform named as the subject, not as the place to look ("TikTok
/// trending sound reach test", "YouTube Studio schedule limit"). Such a
/// query keeps its pages; the social search beside them asks X and the
/// platform named ([`pick_social_platforms`]). Until 0.14 a bare name was a
/// rule 2 cue and the search ran on that platform alone, with no pages.
const PLATFORM_MENTIONS: &[(&str, &[&str])] = &[
    ("tiktok", &["tiktok", "tik tok"]),
    ("instagram", &["instagram"]),
    ("youtube", &["youtube", "youtubers", "youtuber"]),
];

/// The first platform `query` names as its subject, if any.
pub fn platform_mention(query: &str) -> Option<&'static str> {
    let lower = query.to_lowercase();
    PLATFORM_MENTIONS
        .iter()
        .find(|(_, names)| names.iter().any(|n| cue_at(&lower, n).is_some()))
        .map(|(provider, _)| *provider)
}

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
/// paid calls: X plus the one most likely to hold the answer.
/// - A platform the query names as its subject comes with X.
/// - Breaking news is X alone; trends, memes and sounds add TikTok; visual
///   topics add Instagram.
/// - Anything else adds YouTube. It was TikTok until 0.14: over six of the
///   owner's agents' queries (19 Nov 2026) TikTok returned off-topic clips
///   for three, and YouTube was on topic for all four it was tried on
///   (talks, tutorials and reviews, with transcripts to extract).
///
/// metasearch.sh's own pick (`pickSocialPlatforms`) still defaults to
/// TikTok and has no mention rule; logged in, the server chooses.
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
    if let Some(named) = platform_mention(query) {
        vec!["xai", named]
    } else if first_phrase(&lower, REAL_TIME).is_some() {
        vec!["xai"]
    } else if first_phrase(&lower, SHORT_FORM).is_some() {
        vec!["xai", "tiktok"]
    } else if first_phrase(&lower, VIDEO_HOW).is_some() {
        vec!["xai", "youtube"]
    } else if first_phrase(&lower, VISUAL).is_some() {
        vec!["xai", "instagram"]
    } else {
        vec!["xai", "youtube"]
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

/// The modes rule 12 adds beside the pages, in the order their posts are
/// dealt into the list.
pub const PEOPLE_LANES: [Mode; 2] = [Mode::Social, Mode::Community];

/// Words that make the query a lookup of an error or a reference, where a
/// post would only be noise ("stripe api rate limit error"). Kept short on
/// purpose: a post that was not fetched is the costly mistake, an unneeded
/// one costs cents.
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
    "syntax",
    "regex",
    "stack trace",
    "stacktrace",
    "traceback",
    // Finding how to reach someone: the answer is on a page.
    "contact email",
    "contact details",
    "contact info",
    "email address",
    "editor email",
    "phone number",
    "press contact",
];

/// Search operators: the caller is steering the web search itself.
const OPERATORS: &[&str] = &[
    "site:",
    "filetype:",
    "inurl:",
    "intitle:",
    "http://",
    "https://",
];

/// A question's frame: an explicit request for what people say. It makes
/// rule 12 apply beside a lookup word, and it is left out of the query the
/// added modes search ([`people_query`]).
const PEOPLE_FRAMES: &[&str] = &[
    "what do people say about",
    "what do people think about",
    "what do people think of",
    "what are people saying about",
    "opinions about",
    "experiences with",
];
// Every frame is ASCII: `people_query` relies on it.

/// Phrases in which a frame's words mean something else.
const FRAME_NOT: &[&str] = &[
    "court opinions",
    "judicial opinions",
    "advisory opinions",
    "dissenting opinions",
    "legal opinions",
];

/// Rule 12: whether `query` gets posts beside its pages. Yes, unless it is
/// a lookup: one word (a name or an id), an exact phrase (two quotes; one
/// is an inch mark), a search operator or URL, or an error, reference or
/// contact word. A frame ("what do people say about the stripe api")
/// always gets them.
///
/// Checked against 25 searches drawn at random from a month of one
/// machine's log and graded source by source (19 Nov 2026): on the plain
/// queries Reddit was useful or partly useful on 11 of 11, YouTube on 10
/// and X on 9; on the quoted and `site:` queries 5 of 33 source results
/// were useful, and none for `site:`.
pub fn posts_apply(query: &str) -> bool {
    let lower = query
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if !lower.contains(' ')
        || lower.matches('"').count() >= 2
        || OPERATORS.iter().any(|op| lower.contains(op))
        // A capital OR is the boolean operator ("Ray 3.2 OR Ray 4").
        || query.split_whitespace().any(|w| w == "OR")
    {
        return false;
    }
    first_phrase(&cut_phrases(&lower, FRAME_NOT), PEOPLE_FRAMES).is_some()
        || first_phrase(&lower, LOOKUP_WORDS).is_none()
}

/// The query the added modes search: `query` without a frame such as "what
/// do people say about", because Reddit, Hacker News and the platforms match
/// the words they are given. Unchanged when nothing else would be left.
pub fn people_query(query: &str) -> String {
    // Collapsed first, like the rule, so a frame typed with two spaces is
    // still cut. ASCII lowercasing keeps every byte offset, whatever else
    // the query holds (full lowercasing can move them: "İ" shrinks, "K"
    // grows).
    let mut text = query.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut low = text.to_ascii_lowercase();
    if first_phrase(&cut_phrases(&low, FRAME_NOT), PEOPLE_FRAMES).is_none() {
        return text;
    }
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
    let cut = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let cut = cut.trim_end_matches('?').trim();
    if cut.is_empty() {
        query.split_whitespace().collect::<Vec<_>>().join(" ")
    } else {
        cut.to_string()
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
        let (d, _) = resolve_local("best mechanical keyboard reddit");
        assert_eq!(
            d.reason,
            "Names a community source ('reddit') (rule 11); used community."
        );
        assert!(d.added.is_empty());
    }

    /// Rule 12 (the CLI's own): posts are added beside the pages of any
    /// query that is not a lookup. Nearly all of these got pages only, or
    /// one platform only, before 0.14.
    #[test]
    fn rule_12_adds_posts_unless_the_query_is_a_lookup() {
        for q in [
            "rapamycin dosing protocol longevity side effects",
            "Lemon Squeezy payouts held founders",
            "Claude Code subagents workflow tips",
            "WoW Forever feral druid best weapon leveling",
            "Google core update November 2026",
            "is the uplift v2 standing desk worth it",
            "notion vs obsidian",
            "what do people say about the framework laptop",
            "opinions about remote work",
            // A platform as the subject keeps its pages (it used to run on
            // that platform alone).
            "TikTok trending sound low volume under voiceover reach test",
            "YouTube Studio can't schedule video more than two years ahead",
            "twitter algorithm changes 2026",
            // One quote is an inch mark.
            "best 27\" monitor",
            // A frame is an explicit request, whatever else is in the query.
            "what do people say about the stripe api",
            "experiences with the openai sdk",
        ] {
            assert!(posts_apply(q), "{q}");
            let mode = local(q).0;
            assert!(
                mode != "social" && mode != "community",
                "{q}: {mode} would drop the pages"
            );
        }
        for q in [
            // A name or an id.
            "stripe",
            "DXhNFjCkTyX",
            // An exact phrase, an operator, a URL.
            "\"SAGE KE\" Science of Aging Knowledge Environment ceased publication",
            "site:edu gerontology library guide",
            "summary of https://example.com/post",
            "filetype:pdf annual report 2026",
            // An error or a reference.
            "stripe api rate limit",
            "tokio vs async-std docs",
            "rust borrow checker error E0502",
            "python regex lookbehind",
            "court opinions about qualified immunity sdk",
            // The boolean operator, and a way to reach someone.
            "Luma Ray 3.2 OR Ray 4 pricing",
            "Brave Search enterprise license contact email",
        ] {
            assert!(!posts_apply(q), "{q}");
        }
    }

    /// A platform's own posts are searched alone only when the query asks
    /// for them; a platform named as the subject goes with X beside the
    /// pages.
    #[test]
    fn a_platform_named_as_the_subject_is_a_mention_not_a_pin() {
        for (q, named) in [
            ("TikTok trending sound reach test", "tiktok"),
            ("how to create a tik tok series", "tiktok"),
            ("instagram algorithm 2026 changes", "instagram"),
            ("YouTube Studio schedule limit", "youtube"),
            ("how much do youtubers earn", "youtube"),
        ] {
            assert_eq!(platform_mention(q), Some(named), "{q}");
            assert!(social_pins(q).is_empty(), "{q}");
            assert_eq!(pick_social_platforms(q), ["xai", named], "{q}");
            assert_ne!(local(q).0, "social", "{q}");
        }
        // Developer questions name no platform to search.
        assert_eq!(platform_mention("youtube api quota"), None);
        // Asking for the posts still pins.
        for (q, pins) in [
            ("mechanical keyboard on tiktok", &["tiktok"][..]),
            ("funny tiktoks about cats", &["tiktok"][..]),
            ("iphone 17 youtube videos", &["youtube"][..]),
            ("skincare instagram reels", &["instagram"][..]),
            ("what people post on twitter about htmx", &["xai"][..]),
            ("twitter threads on rust async", &["xai"][..]),
            ("tweets about the outage", &["xai"][..]),
        ] {
            assert_eq!(social_pins(q), pins, "{q}");
            assert_eq!(local(q).0, "social", "{q}");
        }
    }

    /// The routing model may not pick social or community: the server
    /// still sends a bare platform name and "what do people say about"
    /// there, and the client keeps the pages for both.
    #[test]
    fn the_server_may_not_pick_social_or_community() {
        assert!(server_may_pick("general"));
        assert!(server_may_pick("news"));
        assert!(server_may_pick("academic"));
        assert!(!server_may_pick("social"));
        assert!(!server_may_pick("community"));
        assert!(!server_may_pick("deep"));
        assert!(!server_may_pick("nonsense"));
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
        assert_eq!(people_query("notion  vs   obsidian"), "notion vs obsidian");
        // Nothing but the frame: the query is kept.
        assert_eq!(people_query("opinions about"), "opinions about");
        // A court's opinions are not a frame.
        assert_eq!(
            people_query("court opinions about qualified immunity"),
            "court opinions about qualified immunity"
        );
        // Letters whose lowercase has another byte length keep their
        // offsets: these used to cut inside a character.
        assert_eq!(
            people_query("\u{0130}\u{0130} opinions about \u{212B}\u{212B}"),
            "\u{0130}\u{0130} \u{212B}\u{212B}"
        );
        assert_eq!(
            people_query("\u{0130}stanbul opinions about \u{1E9E}"),
            "\u{0130}stanbul \u{1E9E}"
        );
        // A frame typed with two spaces is still a frame.
        assert_eq!(people_query("opinions  about   tailwind v4"), "tailwind v4");
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

    /// The pick with no platform cue. (Rule for rule the server's
    /// pickSocialPlatforms until 0.14; the default is YouTube here now,
    /// and TikTok there.)
    #[test]
    fn social_pick_with_no_platform_cue() {
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
            ["xai", "youtube"]
        );
        // Words, not substrings: "livestock" is not "live".
        assert_eq!(
            pick_social_platforms("livestock prices"),
            ["xai", "youtube"]
        );
    }
}
