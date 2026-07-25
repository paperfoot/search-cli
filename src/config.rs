use crate::errors::SearchError;
use directories::ProjectDirs;
use figment::{
    providers::{Env, Format, Serialized, Toml},
    Figment,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub keys: ApiKeys,
    pub settings: Settings,
    /// metasearch.sh account, populated by `search login`. Independent of the
    /// per-provider keys above: either, both, or neither may be set.
    #[serde(default)]
    pub metasearch: Metasearch,
}

/// Field names deliberately avoid underscores — figment splits `SEARCH_*` env
/// vars on `_`, so `token` maps cleanly from `SEARCH_METASEARCH_TOKEN` while
/// an `api_key` field would not.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Metasearch {
    /// `ms_live_…` API key issued by `search login`.
    #[serde(default)]
    pub token: String,
    /// Account email, cached purely so `search config show` can display it.
    #[serde(default)]
    pub email: String,
    /// Override the API host (self-host or local dev). Empty = metasearch.sh.
    #[serde(default)]
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKeys {
    #[serde(default)]
    pub parallel: String,
    #[serde(default)]
    pub brave: String,
    #[serde(default)]
    pub serper: String,
    #[serde(default)]
    pub exa: String,
    #[serde(default)]
    pub jina: String,
    #[serde(default)]
    pub linkup: String,
    #[serde(default)]
    pub firecrawl: String,
    #[serde(default)]
    pub tavily: String,
    #[serde(default)]
    pub serpapi: String,
    #[serde(default)]
    pub perplexity: String,
    #[serde(default)]
    pub browserless: String,
    #[serde(default)]
    pub xai: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default = "default_timeout")]
    pub timeout: u64,
    #[serde(default = "default_count")]
    pub count: usize,
}

fn default_timeout() -> u64 {
    10
}
fn default_count() -> usize {
    10
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            keys: ApiKeys {
                parallel: String::new(),
                brave: String::new(),
                serper: String::new(),
                exa: String::new(),
                jina: String::new(),
                linkup: String::new(),
                firecrawl: String::new(),
                tavily: String::new(),
                serpapi: String::new(),
                perplexity: String::new(),
                browserless: String::new(),
                xai: String::new(),
            },
            settings: Settings {
                timeout: default_timeout(),
                count: default_count(),
            },
            metasearch: Metasearch::default(),
        }
    }
}

pub fn config_dir() -> PathBuf {
    if let Some(proj) = ProjectDirs::from("", "", "search") {
        proj.config_dir().to_path_buf()
    } else {
        dirs_fallback()
    }
}

fn dirs_fallback() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".config").join("search")
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

pub fn load_config() -> Result<AppConfig, Box<figment::Error>> {
    tighten_config_permissions();
    Ok(Figment::new()
        .merge(Serialized::defaults(AppConfig::default()))
        .merge(Toml::file(config_path()))
        .merge(Env::prefixed("SEARCH_").split("_"))
        .extract()?)
}

/// One-time migration: config files written before 0.8.0 were mode 644, so
/// every local process could read the API keys. Quietly chmod to 0600.
fn tighten_config_permissions() {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let path = config_path();
        if let Ok(meta) = std::fs::metadata(&path) {
            if meta.permissions().mode() & 0o077 != 0 {
                let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
            }
        }
    }
}

/// Valid provider key names for `config set keys.<name>`.
pub const PROVIDER_KEYS: &[&str] = &[
    "parallel",
    "brave",
    "serper",
    "exa",
    "jina",
    "linkup",
    "firecrawl",
    "tavily",
    "serpapi",
    "perplexity",
    "browserless",
    "xai",
];

/// Valid setting names for `config set settings.<name>`.
pub const SETTING_KEYS: &[&str] = &["timeout", "count"];

/// Valid field names for `config set metasearch.<name>`.
pub const METASEARCH_KEYS: &[&str] = &["token", "email", "url"];

pub fn mask_key(key: &str) -> String {
    // Char-based: byte-slicing (`&key[..2]`) panics on multi-byte UTF-8 and
    // would leak part of the secret in the panic message.
    let n = key.chars().count();
    if key.is_empty() {
        "(not set)".to_string()
    } else if n <= 8 {
        let head: String = key.chars().take(2).collect();
        format!("{head}***")
    } else {
        let head: String = key.chars().take(4).collect();
        let tail: String = key.chars().skip(n - 4).collect();
        format!("{head}...{tail}")
    }
}

pub fn config_show(config: &AppConfig) {
    use owo_colors::OwoColorize;
    use std::io::IsTerminal;
    let c = std::io::stdout().is_terminal();

    if c {
        println!("\n{}  Configuration\n", "search".bold().cyan());
        println!(
            "  {} {}\n",
            "path:".dimmed(),
            config_path().display().to_string().dimmed()
        );
    } else {
        println!("Configuration ({})\n", config_path().display());
    }

    use crate::providers;

    let keys: &[(&str, &str, &str)] = &[
        ("parallel", &config.keys.parallel, "PARALLEL_API_KEY"),
        ("brave", &config.keys.brave, "BRAVE_API_KEY"),
        ("serper", &config.keys.serper, "SERPER_API_KEY"),
        ("exa", &config.keys.exa, "EXA_API_KEY"),
        ("jina", &config.keys.jina, "JINA_API_KEY"),
        ("linkup", &config.keys.linkup, "LINKUP_API_KEY"),
        ("firecrawl", &config.keys.firecrawl, "FIRECRAWL_API_KEY"),
        ("tavily", &config.keys.tavily, "TAVILY_API_KEY"),
        ("serpapi", &config.keys.serpapi, "SERPAPI_API_KEY"),
        ("perplexity", &config.keys.perplexity, "PERPLEXITY_API_KEY"),
        (
            "browserless",
            &config.keys.browserless,
            "BROWSERLESS_API_KEY",
        ),
        ("xai", &config.keys.xai, "XAI_API_KEY"),
    ];

    if c {
        println!("  {}", "[keys]".bold());
    } else {
        println!("[keys]");
    }
    for (name, config_val, env_var) in keys {
        let effective = providers::resolve_key(config_val, env_var);
        let masked = mask_key(&effective);
        if c {
            let val = if effective.is_empty() {
                masked.red().to_string()
            } else {
                masked.green().to_string()
            };
            println!("    {:<12} {}", name.white(), val);
        } else {
            println!("  {:<12} = {}", name, masked);
        }
    }

    println!();
    if c {
        println!("  {}", "[settings]".bold());
    } else {
        println!("[settings]");
    }
    if c {
        println!(
            "    {:<10} {}",
            "timeout".white(),
            format!("{}s", config.settings.timeout).cyan()
        );
        println!(
            "    {:<10} {}",
            "count".white(),
            config.settings.count.to_string().cyan()
        );
    } else {
        println!("  timeout  = {}s", config.settings.timeout);
        println!("  count    = {}", config.settings.count);
    }

    println!();
    let token = crate::auth::resolve_token(config);
    let host = crate::auth::resolve_base_url(config);
    if c {
        println!("  {}", "[metasearch]".bold());
        if token.is_empty() {
            println!(
                "    {:<10} {}",
                "account".white(),
                "not logged in — run `search login`".dimmed()
            );
        } else {
            let who = if config.metasearch.email.is_empty() {
                "logged in".to_string()
            } else {
                config.metasearch.email.clone()
            };
            println!("    {:<10} {}", "account".white(), who.green());
            println!("    {:<10} {}", "token".white(), mask_key(&token).green());
        }
        println!("    {:<10} {}", "host".white(), host.dimmed());
    } else {
        println!("[metasearch]");
        println!(
            "  account  = {}",
            if token.is_empty() {
                "(not logged in)"
            } else {
                config.metasearch.email.as_str()
            }
        );
        println!("  token    = {}", mask_key(&token));
        println!("  host     = {host}");
    }
    println!();
}

pub fn config_set(key: &str, value: &str) -> Result<(), crate::errors::SearchError> {
    let path = config_path();
    let mut doc: toml::Table = if path.exists() {
        let content = std::fs::read_to_string(&path)?;
        content
            .parse()
            .map_err(|e: toml::de::Error| crate::errors::SearchError::Config(e.to_string()))?
    } else {
        toml::Table::new()
    };

    // Only dotted keys are valid: keys.<provider> or settings.<name>.
    let parts: Vec<&str> = key.split('.').collect();
    match parts.as_slice() {
        [section, name] => {
            // Validate against the known schema AND coerce to the right TOML
            // type — writing settings.timeout as a quoted string made
            // load_config fail to deserialize and bricked every later command.
            let typed_value = match *section {
                "keys" => {
                    if !PROVIDER_KEYS.contains(name) {
                        return Err(SearchError::Config(format!(
                            "Unknown provider 'keys.{name}'. Valid: {}",
                            PROVIDER_KEYS.join(", ")
                        )));
                    }
                    toml::Value::String(value.to_string())
                }
                "settings" => parse_setting(name, value)?,
                "metasearch" => {
                    if !METASEARCH_KEYS.contains(name) {
                        return Err(SearchError::Config(format!(
                            "Unknown field 'metasearch.{name}'. Valid: {}",
                            METASEARCH_KEYS.join(", ")
                        )));
                    }
                    toml::Value::String(value.to_string())
                }
                other => {
                    return Err(SearchError::Config(format!(
                        "Unknown section '{other}'. Use 'keys.<provider>', 'settings.<name>', or 'metasearch.<name>'."
                    )));
                }
            };

            let entry = doc
                .entry(*section)
                .or_insert_with(|| toml::Value::Table(toml::Table::new()));
            match entry {
                toml::Value::Table(t) => {
                    t.insert(name.to_string(), typed_value);
                }
                // Don't silently no-op (the old `if let Table` did) — that
                // reported success while writing nothing.
                _ => {
                    return Err(SearchError::Config(format!(
                        "Cannot set '{key}': '{section}' exists but is not a table."
                    )));
                }
            }
        }
        _ => {
            return Err(SearchError::Config(format!(
                "Invalid key '{key}'. Use a dotted form like 'keys.brave' or 'settings.timeout'."
            )));
        }
    }

    write_config_atomic(&path, &doc)
}

/// Remove a dotted key (e.g. `metasearch.token`) from the config file.
/// Absent keys are not an error — `search logout` must be idempotent.
pub fn config_unset(key: &str) -> Result<(), SearchError> {
    let path = config_path();
    if !path.exists() {
        return Ok(());
    }
    let mut doc: toml::Table = std::fs::read_to_string(&path)?
        .parse()
        .map_err(|e: toml::de::Error| SearchError::Config(e.to_string()))?;

    let parts: Vec<&str> = key.split('.').collect();
    let [section, name] = parts.as_slice() else {
        return Err(SearchError::Config(format!(
            "Invalid key '{key}'. Use a dotted form like 'metasearch.token'."
        )));
    };
    match doc.get_mut(*section) {
        Some(toml::Value::Table(t)) => {
            if t.remove(*name).is_none() {
                return Ok(());
            }
        }
        _ => return Ok(()),
    }

    write_config_atomic(&path, &doc)
}

/// Atomic replace (temp + rename) so concurrent writes can't interleave a torn
/// file, and 0600 because this file holds API keys.
fn write_config_atomic(path: &std::path::Path, doc: &toml::Table) -> Result<(), SearchError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, doc.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Coerce a `settings.*` value to the TOML type figment expects. `timeout` and
/// `count` are integers; writing them as strings breaks deserialization.
fn parse_setting(name: &str, value: &str) -> Result<toml::Value, SearchError> {
    match name {
        "timeout" | "count" => {
            let n: u64 = value.parse().map_err(|_| {
                SearchError::Config(format!(
                    "settings.{name} must be a non-negative integer, got '{value}'"
                ))
            })?;
            Ok(toml::Value::Integer(n as i64))
        }
        other => Err(SearchError::Config(format!(
            "Unknown setting 'settings.{other}'. Valid: {}",
            SETTING_KEYS.join(", ")
        ))),
    }
}

pub fn config_check(config: &AppConfig) {
    use owo_colors::OwoColorize;
    use std::io::IsTerminal;
    let c = std::io::stdout().is_terminal();

    use crate::providers;

    let all: &[(&str, &str, &str, &str)] = &[
        (
            "parallel",
            &config.keys.parallel,
            "PARALLEL_API_KEY",
            "Independent web index (Parallel AI)",
        ),
        (
            "brave",
            &config.keys.brave,
            "BRAVE_API_KEY",
            "Web + News search",
        ),
        (
            "serper",
            &config.keys.serper,
            "SERPER_API_KEY",
            "Google SERP, Scholar, Patents, Images, Places",
        ),
        (
            "exa",
            &config.keys.exa,
            "EXA_API_KEY",
            "Semantic search, People, Similar pages",
        ),
        (
            "jina",
            &config.keys.jina,
            "JINA_API_KEY",
            "Web search + URL reader",
        ),
        (
            "linkup",
            &config.keys.linkup,
            "LINKUP_API_KEY",
            "High-accuracy agent search (SimpleQA leader)",
        ),
        (
            "firecrawl",
            &config.keys.firecrawl,
            "FIRECRAWL_API_KEY",
            "Web scraping + extraction",
        ),
        (
            "tavily",
            &config.keys.tavily,
            "TAVILY_API_KEY",
            "General, News, Academic, Deep search",
        ),
        (
            "serpapi",
            &config.keys.serpapi,
            "SERPAPI_API_KEY",
            "80+ engines: Google, Bing, YouTube, Baidu, Scholar",
        ),
        (
            "perplexity",
            &config.keys.perplexity,
            "PERPLEXITY_API_KEY",
            "AI-powered answers with citations (Perplexity Sonar)",
        ),
        (
            "browserless",
            &config.keys.browserless,
            "BROWSERLESS_API_KEY",
            "Cloud browser for Cloudflare/JS-heavy pages",
        ),
        (
            "xai",
            &config.keys.xai,
            "XAI_API_KEY",
            "X/Twitter social search via xAI Grok",
        ),
    ];

    if c {
        println!("\n{}  Provider Health Check\n", "search".bold().cyan());
    }

    let mut configured = 0;
    for (name, config_val, env_var, desc) in all {
        let is_configured = !providers::resolve_key(config_val, env_var).is_empty();
        if !is_configured {
            if c {
                println!(
                    "  {} {:<12} {}",
                    "x".red().bold(),
                    name.white(),
                    desc.dimmed()
                );
            } else {
                println!("  [x] {name}: NOT SET - {desc}");
            }
        } else {
            configured += 1;
            if c {
                println!(
                    "  {} {:<12} {}",
                    "+".green().bold(),
                    name.white().bold(),
                    desc.dimmed()
                );
            } else {
                println!("  [+] {name}: OK - {desc}");
            }
        }
    }

    println!();
    if configured == 0 {
        if c {
            println!("  {} No providers configured.\n", "!".yellow().bold());
            println!("  Set API keys via environment or config:");
            println!("    {} export BRAVE_API_KEY=YOUR_KEY", "$".dimmed());
            println!("    {} search config set keys.brave YOUR_KEY", "$".dimmed());
        } else {
            println!("  No providers configured. Set API keys via:");
            println!("    export BRAVE_API_KEY=<YOUR_KEY>");
            println!("    search config set keys.brave <YOUR_KEY>");
        }
    } else if c {
        println!(
            "  {}/{} providers ready",
            configured.to_string().green().bold(),
            all.len()
        );
    } else {
        println!("  {configured}/{} providers configured", all.len());
    }
    println!();
}

#[cfg(test)]
mod tests {
    use super::{mask_key, parse_setting};

    #[test]
    fn mask_key_never_panics_on_multibyte() {
        // Byte-slicing would panic here; char-based must not.
        let _ = mask_key("€€€€€€€€€€€€");
        assert_eq!(mask_key(""), "(not set)");
        assert!(mask_key("short").contains("***"));
        assert!(mask_key("abcdefghijklmnop").contains("..."));
    }

    #[test]
    fn parse_setting_coerces_integers_and_rejects_bad_input() {
        assert!(matches!(
            parse_setting("timeout", "30"),
            Ok(toml::Value::Integer(30))
        ));
        assert!(matches!(
            parse_setting("count", "5"),
            Ok(toml::Value::Integer(5))
        ));
        assert!(parse_setting("timeout", "abc").is_err());
        assert!(parse_setting("unknown_setting", "1").is_err());
    }
}
