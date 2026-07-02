//! `search doctor` — test-fires every configured provider with a minimal
//! real request and reports health, latency, and failure cause. This is the
//! preflight an agent fleet runs before trusting the tool: `config check`
//! says a key exists; doctor says it works. Each check is one billed
//! (minimal) request; scrape-only providers are probed against example.com.

use crate::context::AppContext;
use crate::providers;
use crate::types::SearchOpts;
use serde::Serialize;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::task::JoinSet;
use tokio::time::timeout;

const PROBE_QUERY: &str = "health";
const PROBE_URL: &str = "https://example.com/";

#[derive(Debug, Serialize)]
pub struct Check {
    pub provider: String,
    pub configured: bool,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u128>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub results: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
}

pub async fn run(ctx: Arc<AppContext>) -> Vec<Check> {
    let mut set: JoinSet<Check> = JoinSet::new();
    let mut out = Vec::new();

    for provider in providers::build_providers(&ctx) {
        let name = provider.name();
        if !provider.is_configured() {
            // Unconfigured is healthy-by-absence: nothing to check, and the
            // engine will never route to it.
            out.push(Check {
                provider: name.to_string(),
                configured: false,
                ok: true,
                latency_ms: None,
                results: None,
                error: None,
                category: None,
            });
            continue;
        }
        let ctx2 = ctx.clone();
        set.spawn(async move {
            let start = Instant::now();
            let outcome = match name {
                // Scrape-only providers: probe with a fetch of example.com.
                #[cfg(feature = "stealth")]
                "stealth" => {
                    timeout(
                        Duration::from_secs(20),
                        providers::stealth::Stealth::new(ctx2).scrape_url(PROBE_URL),
                    )
                    .await
                }
                "browserless" => {
                    timeout(
                        Duration::from_secs(20),
                        providers::browserless::Browserless::new(ctx2).scrape_url(PROBE_URL),
                    )
                    .await
                }
                "firecrawl" => {
                    timeout(
                        Duration::from_secs(20),
                        providers::firecrawl::Firecrawl::new(ctx2).scrape_url(PROBE_URL),
                    )
                    .await
                }
                // Everyone else: minimal 1-result search.
                _ => {
                    timeout(
                        provider.timeout(),
                        provider.search(PROBE_QUERY, 1, &SearchOpts::default()),
                    )
                    .await
                }
            };
            let latency = start.elapsed().as_millis();
            match outcome {
                Ok(Ok(results)) => Check {
                    provider: name.to_string(),
                    configured: true,
                    ok: true,
                    latency_ms: Some(latency),
                    results: Some(results.len()),
                    error: None,
                    category: None,
                },
                Ok(Err(e)) => Check {
                    provider: name.to_string(),
                    configured: true,
                    ok: false,
                    latency_ms: Some(latency),
                    results: None,
                    category: Some(e.to_provider_failure(name).category.as_str().to_string()),
                    error: Some(truncate(&e.to_string(), 160)),
                },
                Err(_) => Check {
                    provider: name.to_string(),
                    configured: true,
                    ok: false,
                    latency_ms: Some(latency),
                    results: None,
                    category: Some("timeout".to_string()),
                    error: Some("timed out".to_string()),
                },
            }
        });
    }

    while let Some(Ok(check)) = set.join_next().await {
        out.push(check);
    }
    out.sort_by(|a, b| a.provider.cmp(&b.provider));
    out
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect::<String>() + "…"
    }
}

pub fn render_human(checks: &[Check]) {
    use owo_colors::OwoColorize;
    eprintln!("\n{}  provider health\n", "doctor".bold().cyan());
    for c in checks {
        if !c.configured {
            println!("  {:<12} {}", c.provider, "not configured".dimmed());
        } else if c.ok {
            println!(
                "  {:<12} {} {:>5}ms  {} result(s)",
                c.provider.bold(),
                "ok".green(),
                c.latency_ms.unwrap_or(0),
                c.results.unwrap_or(0)
            );
        } else {
            println!(
                "  {:<12} {} [{}] {}",
                c.provider.bold(),
                "FAIL".red(),
                c.category.as_deref().unwrap_or("?"),
                c.error.as_deref().unwrap_or("")
            );
        }
    }
    println!();
}
