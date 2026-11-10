use crate::types::Mode;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "search",
    version,
    about = "Agent-friendly multi-provider search CLI",
    long_about = "Eleven search engines across 14 modes; TikTok, Instagram, YouTube,\n\
        Facebook, Threads and X (social mode); Reddit and Hacker News\n\
        (community mode, no key); and page extraction (local fetchers first,\n\
        then Jina Reader, Firecrawl or Browserless).\n\
        With no -m the mode is auto: a URL extracts (a TikTok, YouTube or\n\
        Reddit link reads the post whole: transcript, comments), \"on tiktok\"\n\
        or \"twitter\" searches that platform, \"reddit\" or \"what do people\n\
        say about\" searches community, and\n\
        so on; the decision is reported in metadata.routing.auto. An\n\
        explicit -m or -p is always obeyed. Auto never picks deep.\n\
        Engines without your own key run through metasearch.sh after\n\
        `search login`, charged to your balance; each result's cost is in\n\
        metadata.cost. Run `search agent-info` for the machine-readable map.\n\
        Prints tables on a terminal and one compact JSON line when piped\n\
        (--pretty indents it). NO_COLOR turns colour off.\n\n\
        PROVIDERS:\n  \
          parallel   Agent-native search (Parallel AI): LLM-ready excerpts\n  \
          keenable   Own web index built for agents (Keenable): page text\n  \
          brave      Independent web index (not Google/Bing) + LLM grounding\n  \
          serper     Cheapest raw Google SERP: web, news, scholar, patents, places\n  \
          exa        Neural/semantic search, LinkedIn people, find-similar\n  \
          jina       Fast web search + URL-to-markdown reader\n  \
          linkup     High-accuracy agent search (SimpleQA leader)\n  \
          firecrawl  JS-rendered page scraping + structured extraction\n  \
          tavily     RAG-oriented search: general, news, academic, deep\n  \
          serpapi    Many engines (Google, Bing, YouTube, Baidu, Scholar)\n  \
          perplexity Ranked web results with dates (Perplexity Search API)\n  \
          browserless Cloud browser for Cloudflare/JS-heavy pages\n  \
          direct     Honest local page fetch, markdown first (no API key)\n  \
          stealth    Local Chrome-impersonating fetch (no API key)\n  \
          xai        Real-time X/Twitter search (Grok agentic)\n  \
          reddit     Reddit posts and whole threads (no key; paced ~10/min)\n  \
          hackernews Hacker News stories, comments, threads (no key)\n  \
          tiktok     TikTok videos: search, transcript, comments (search login)\n  \
          instagram  Instagram reels and posts (search login)\n  \
          youtube    YouTube videos and shorts, transcripts (search login)\n  \
          facebook   Facebook pages and posts (search login)\n  \
          threads    Threads posts and replies (search login)\n\n\
        EXAMPLES:\n  \
          search \"rust error handling\"                    # auto mode\n  \
          search search -q \"CRISPR\" -m academic           # academic papers\n  \
          search search -q \"CEO of Stripe\" -m people      # LinkedIn profiles via Exa\n  \
          search search -q \"AI news\" -m news              # breaking news\n  \
          search search -q \"trending on twitter\" -m social # X/Twitter search\n  \
          search \"mechanical keyboard on tiktok\"          # TikTok (social)\n  \
          search \"https://youtu.be/3rKPzh3Pr0o\"           # video, transcript, comments\n  \
          search \"opinions about tailwind v4\"            # Reddit + HN (community)\n  \
          search search -q \"query\" -p exa                 # force Exa only\n  \
          search search -q \"query\" -p exa,brave           # only Exa + Brave\n  \
          search --x \"AI agents\"                          # search X (Twitter) only\n  \
          search --youtube \"AI agents\"                    # also --tiktok --instagram --threads\n  \
          search \"query\" --json | jq '.results[].url'     # pipe JSON to jq"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Commands>,

    /// Search query (shorthand for `search -q`)
    #[arg(trailing_var_arg = true, global = false)]
    pub query_words: Vec<String>,

    /// Output as JSON (auto-enabled when piped)
    #[arg(long, global = true)]
    pub json: bool,

    /// Indent JSON output (it is one compact line when piped)
    #[arg(long, global = true)]
    pub pretty: bool,

    /// Suppress non-essential output
    #[arg(long, global = true)]
    pub quiet: bool,

    /// Replay the last search result from cache
    #[arg(long, global = true)]
    pub last: bool,

    /// Search X (Twitter) only — shorthand for -m social -p xai
    #[arg(long = "x", global = true)]
    pub x_only: bool,

    /// Search TikTok only — shorthand for -m social -p tiktok
    #[arg(long, global = true)]
    pub tiktok: bool,

    /// Search Instagram reels only — shorthand for -m social -p instagram
    #[arg(long, global = true)]
    pub instagram: bool,

    /// Search YouTube only — shorthand for -m social -p youtube
    #[arg(long, global = true)]
    pub youtube: bool,

    /// Search Threads only — shorthand for -m social -p threads
    #[arg(long, global = true)]
    pub threads: bool,

    /// The platform a shorthand flag pinned (set in main, not by clap).
    #[arg(skip)]
    pub platform: Option<&'static str>,

    /// Verbose debug logging to stderr (sets log level to debug unless RUST_LOG is set)
    #[arg(long, global = true, visible_alias = "verbose")]
    pub debug: bool,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Search across providers (use -m for mode, -p to pick providers)
    Search(SearchArgs),

    /// Manage configuration (show, set, check)
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },

    /// Sign in to metasearch.sh and store an API key for this machine
    Login(LoginArgs),

    /// Remove the stored metasearch.sh credentials from this machine
    Logout {
        /// Also revoke the key server-side so it stops working everywhere
        #[arg(long)]
        revoke: bool,
    },

    /// Show which metasearch.sh account this machine is signed in to
    Whoami,

    /// Show machine-readable capabilities (for agents)
    AgentInfo,

    /// List all providers with status and capabilities
    Providers,

    /// Verify if email addresses exist via SMTP (no API key needed). Probes
    /// mail servers directly (RCPT TO) — bulk probing can hurt IP reputation
    Verify(VerifyArgs),

    /// Manage skill file installation for agent platforms
    Skill {
        #[command(subcommand)]
        action: SkillAction,
    },

    /// Check for updates or self-update
    Update {
        /// Only check, don't install
        #[arg(long)]
        check: bool,
    },

    /// Show remaining credits/quota for providers that expose a usage API
    Usage,

    /// Test-fire every configured provider with a minimal query and report
    /// health, latency, and failure causes (each check is one billed request)
    Doctor,

    /// Usage statistics from the local search logs (spend, modes, providers)
    Stats(StatsArgs),

    /// Manage the local query cache
    Cache {
        #[command(subcommand)]
        action: CacheAction,
    },
}

#[derive(Parser)]
pub struct LoginArgs {
    /// Print the URL instead of launching a browser (headless boxes, SSH)
    #[arg(long)]
    pub no_browser: bool,

    /// API host to sign in to (default: https://metasearch.sh)
    #[arg(long)]
    pub url: Option<String>,
}

#[derive(Parser)]
pub struct StatsArgs {
    /// Analysis window in days
    #[arg(long, default_value = "30")]
    pub days: u64,

    /// Delete log files older than this many days, then exit
    #[arg(long)]
    pub prune: Option<u64>,
}

#[derive(Subcommand)]
pub enum CacheAction {
    /// Delete all cached query results
    Clear,
}

#[derive(Parser)]
pub struct VerifyArgs {
    /// Email addresses to verify
    pub emails: Vec<String>,

    /// Read emails from file (one per line, use - for stdin)
    #[arg(short, long)]
    pub file: Option<String>,
}

#[derive(Parser)]
pub struct SearchArgs {
    /// Search query
    #[arg(short, long)]
    pub query: String,

    /// Search mode (default: auto). auto picks one of general, news,
    /// academic, scholar, people, images, places, patents, social (an X,
    /// TikTok, Instagram, YouTube, Threads or Facebook page cue), community (a Reddit/HN or "what do people say" cue), extract
    /// (a URL) or similar, and reports why; never deep. Or choose: general,
    /// news, academic, deep, social, community, scholar, patents, people,
    /// places, images, similar, scrape, extract.
    #[arg(short, long, default_value = "auto", value_parser = parse_mode_arg)]
    pub mode: ModeArg,

    /// Keep the fused order: skip the relevance rerank that runs for
    /// logged-in users in general, news, academic, scholar, people and deep
    #[arg(long)]
    pub no_rerank: bool,

    /// Number of results to return
    #[arg(short, long)]
    pub count: Option<usize>,

    /// Use only specific providers (comma-separated: parallel,keenable,brave,serper,exa,jina,linkup,firecrawl,tavily,serpapi,perplexity,browserless,direct,stealth,xai,reddit,hackernews,tiktok,instagram,youtube,facebook,threads)
    #[arg(short, long, value_delimiter = ',')]
    pub providers: Option<Vec<String>>,

    /// Include only results from these domains (comma-separated)
    #[arg(short, long, value_delimiter = ',')]
    pub domain: Option<Vec<String>>,

    /// Exclude results from these domains (comma-separated)
    #[arg(long, value_delimiter = ',')]
    pub exclude_domain: Option<Vec<String>>,

    /// Freshness filter: day, week, month or year (anything else exits 3)
    #[arg(short, long)]
    pub freshness: Option<String>,

    /// Fresh results: skip the local 5-minute query cache and, logged in,
    /// metasearch.sh's result cache (sends fresh: true)
    #[arg(long)]
    pub no_cache: bool,

    /// Cap each result snippet (and answer) at this many characters
    #[arg(long)]
    pub max_chars: Option<usize>,

    /// Country code for region-biased results (e.g. gb, de, jp) — applied by
    /// serper, brave, parallel and perplexity (agent-info lists filters)
    #[arg(long)]
    pub country: Option<String>,

    /// Language code for results (e.g. en, de, fr) — applied by serper,
    /// brave and perplexity (agent-info lists filters)
    #[arg(long)]
    pub lang: Option<String>,

    /// Allow extract/scrape of private, loopback, and link-local addresses
    /// (blocked by default to keep prompt-injected agents away from
    /// localhost and cloud metadata endpoints)
    #[arg(long)]
    pub allow_private: bool,
}

/// `-m`: `auto` (resolved before planning) or a fixed mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModeArg {
    Auto,
    Fixed(Mode),
}

impl std::fmt::Display for ModeArg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Auto => f.write_str("auto"),
            Self::Fixed(m) => write!(f, "{m}"),
        }
    }
}

fn parse_mode_arg(s: &str) -> Result<ModeArg, String> {
    use clap::ValueEnum;
    if s.eq_ignore_ascii_case("auto") {
        return Ok(ModeArg::Auto);
    }
    Mode::from_str(s, true).map(ModeArg::Fixed).map_err(|_| {
        let names: Vec<String> = Mode::value_variants()
            .iter()
            .filter_map(|m| m.to_possible_value().map(|v| v.get_name().to_string()))
            .collect();
        format!("invalid mode '{s}'; valid: auto, {}", names.join(", "))
    })
}

#[derive(Subcommand)]
pub enum ConfigAction {
    /// Show current configuration (API keys masked)
    Show,
    /// Set a configuration value (e.g. keys.brave YOUR_KEY)
    Set {
        /// Config key (e.g. keys.brave, settings.count, metasearch.url)
        key: String,
        /// Value to set. Pass "-" to read it from stdin, keeping secrets out
        /// of shell history and process listings
        value: String,
    },
    /// Health-check which providers are configured and ready
    Check,
    /// Show configuration file path
    Path,
}

#[derive(Subcommand)]
pub enum SkillAction {
    /// Write skill file to all detected agent platforms
    Install,
    /// Check which platforms have the skill installed
    Status,
}

pub mod skill {
    use crate::output::Ctx;
    use std::path::{Path, PathBuf};

    const SKILL_CONTENT: &str = include_str!("../SKILL.md");

    struct Target {
        name: &'static str,
        path: PathBuf,
    }

    /// A place an older `search skill install` wrote to that no agent should
    /// read any more.
    struct Stale {
        name: &'static str,
        path: PathBuf,
        why: &'static str,
    }

    fn home() -> PathBuf {
        std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("."))
    }

    /// Claude Code reads `~/.claude/skills`. Codex reads `~/.agents/skills`,
    /// and Gemini CLI reads it as an alias of `~/.gemini/skills`, so one copy
    /// there serves both.
    fn targets(h: &Path) -> Vec<Target> {
        vec![
            Target {
                name: "Claude Code",
                path: h.join(".claude/skills/search"),
            },
            Target {
                name: "Codex CLI, Gemini CLI",
                path: h.join(".agents/skills/search"),
            },
        ]
    }

    fn stale_locations(h: &Path) -> Vec<Stale> {
        vec![
            Stale {
                name: "Codex CLI (old path)",
                path: h.join(".codex/skills/search"),
                why: "Codex reads ~/.agents/skills now",
            },
            Stale {
                name: "Gemini CLI (old path)",
                path: h.join(".gemini/skills/search"),
                why: "Gemini also reads ~/.agents/skills, so this loads the skill twice",
            },
        ]
    }

    /// The stale path holds a `search` skill (a symlink to one, or a folder
    /// with a SKILL.md named `search`).
    fn is_search_skill(path: &Path) -> bool {
        std::fs::read_to_string(path.join("SKILL.md"))
            .is_ok_and(|c| c.lines().take(5).any(|l| l.trim() == "name: search"))
    }

    /// Safe to delete: a symlink, or a folder holding nothing but the
    /// SKILL.md an install wrote (Finder's .DS_Store aside). Anything else
    /// is someone's own work and is only reported.
    fn removable(path: &Path) -> bool {
        let Ok(meta) = std::fs::symlink_metadata(path) else {
            return false;
        };
        if meta.file_type().is_symlink() {
            return true;
        }
        std::fs::read_dir(path).is_ok_and(|entries| {
            entries.flatten().all(|e| {
                let n = e.file_name();
                n == "SKILL.md" || n == ".DS_Store"
            })
        })
    }

    fn stale_fix(path: &Path) -> String {
        if removable(path) {
            "`search skill install` removes it".to_string()
        } else {
            format!("it holds other files; delete {} by hand", path.display())
        }
    }

    fn remove(path: &Path) -> std::io::Result<()> {
        let meta = std::fs::symlink_metadata(path)?;
        if meta.file_type().is_symlink() || meta.is_file() {
            std::fs::remove_file(path)
        } else {
            std::fs::remove_dir_all(path)
        }
    }

    pub fn install(ctx: &Ctx) {
        let h = home();
        let mut results = Vec::new();
        for t in &targets(&h) {
            let skill_path = t.path.join("SKILL.md");
            // A failed install is reported in the result (JSON readers see it
            // on stdout), not only as a stderr line.
            let (status, error) = if skill_path.exists()
                && std::fs::read_to_string(&skill_path).is_ok_and(|c| c == SKILL_CONTENT)
            {
                ("already_current", None)
            } else {
                match std::fs::create_dir_all(&t.path)
                    .and_then(|()| std::fs::write(&skill_path, SKILL_CONTENT))
                {
                    Ok(()) => ("installed", None),
                    Err(e) => ("failed", Some(e.to_string())),
                }
            };
            results.push((t.name, skill_path.display().to_string(), status, error));
        }

        let mut stale = Vec::new();
        for s in stale_locations(&h) {
            if std::fs::symlink_metadata(&s.path).is_err() || !is_search_skill(&s.path) {
                continue;
            }
            let (status, hint) = if removable(&s.path) {
                match remove(&s.path) {
                    Ok(()) => ("removed", None),
                    Err(e) => ("left", Some(format!("could not remove it: {e}"))),
                }
            } else {
                (
                    "left",
                    Some(format!(
                        "it holds other files; delete {} by hand ({})",
                        s.path.display(),
                        s.why
                    )),
                )
            };
            stale.push((s.name, s.path.display().to_string(), status, s.why, hint));
        }

        if ctx.is_json() {
            let items: Vec<serde_json::Value> = results
                .iter()
                .map(|(name, path, status, error)| {
                    let mut item =
                        serde_json::json!({"platform": name, "path": path, "status": status});
                    if let Some(e) = error {
                        item["error"] = serde_json::json!(e);
                    }
                    item
                })
                .chain(stale.iter().map(|(name, path, status, why, hint)| {
                    let mut item = serde_json::json!({
                        "platform": name,
                        "path": path,
                        "status": if *status == "removed" { "stale_removed" } else { "stale_left" },
                        "reason": why,
                    });
                    if let Some(h) = hint {
                        item["suggestion"] = serde_json::json!(h);
                    }
                    item
                }))
                .collect();
            let any_ok = results.iter().any(|r| r.2 != "failed");
            let all_ok = results.iter().all(|r| r.2 != "failed");
            crate::output::json::render_value(&serde_json::json!({
                "version": "1",
                "status": if all_ok { "success" } else if any_ok { "partial_success" } else { "error" },
                "data": items,
            }));
        } else if !ctx.suppress_human() {
            use crate::output::style::Paint;
            for (name, path, status, error) in &results {
                if let Some(e) = error {
                    eprintln!("  Failed {name}: {e}");
                    continue;
                }
                let marker = if *status == "installed" { "+" } else { "=" };
                println!(" {} {} -> {}", marker.green(), name.bold(), path.dimmed());
            }
            for (name, path, status, why, hint) in &stale {
                if *status == "removed" {
                    println!(
                        " {} {} {} ({why})",
                        "-".yellow(),
                        name.bold(),
                        path.dimmed()
                    );
                } else if let Some(h) = hint {
                    eprintln!("  {name}: {h}");
                }
            }
        }
    }

    pub fn status(ctx: &Ctx) {
        let h = home();
        let mut results = Vec::new();
        for t in &targets(&h) {
            let skill_path = t.path.join("SKILL.md");
            let (installed, current) = if skill_path.exists() {
                let current =
                    std::fs::read_to_string(&skill_path).is_ok_and(|c| c == SKILL_CONTENT);
                (true, current)
            } else {
                (false, false)
            };
            results.push((t.name, skill_path.display().to_string(), installed, current));
        }
        let stale: Vec<Stale> = stale_locations(&h)
            .into_iter()
            .filter(|s| std::fs::symlink_metadata(&s.path).is_ok() && is_search_skill(&s.path))
            .collect();

        if ctx.is_json() {
            let items: Vec<serde_json::Value> = results
                .iter()
                .map(|(name, path, installed, current)| {
                    serde_json::json!({"platform": name, "path": path, "installed": installed, "current": current})
                })
                .chain(stale.iter().map(|s| {
                    serde_json::json!({
                        "platform": s.name,
                        "path": s.path.display().to_string(),
                        "stale": true,
                        "reason": s.why,
                        "suggestion": stale_fix(&s.path),
                    })
                }))
                .collect();
            crate::output::json::render_value(&serde_json::json!({
                "version": "1",
                "status": "success",
                "data": items,
            }));
        } else if !ctx.suppress_human() {
            use crate::output::style::Paint;
            for (name, _, installed, current) in &results {
                let status = if *current {
                    "current".green()
                } else if *installed {
                    "outdated".yellow()
                } else {
                    "not installed".red()
                };
                println!("  {} {}", name.bold(), status);
            }
            for s in &stale {
                println!(
                    "  {} {} {} ({}; {})",
                    s.name.bold(),
                    "stale".yellow(),
                    s.path.display().to_string().dimmed(),
                    s.why,
                    stale_fix(&s.path)
                );
            }
        }
    }
}
