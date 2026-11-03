//! `search doctor` — test-fires every configured provider with a minimal
//! real request and reports health, latency, and failure cause. This is the
//! preflight an agent fleet runs before trusting the tool: `config check`
//! says a key exists; doctor says it works. Each check is one billed
//! (minimal) request; scrape-only providers are probed against example.com.
//! Providers you have no key for are labelled with their transport (hosted
//! when logged in) but not probed: a hosted probe would be charged. The
//! `metasearch` row checks the account itself (`GET /api/v1/me`, free).

use crate::context::AppContext;
use crate::errors::redact_secrets;
use crate::hosted::MetasearchClient;
use crate::providers::{self, Call};
use crate::types::{Mode, SearchOpts};
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
    /// direct | local | hosted | none: how a search would reach it.
    pub transport: String,
    /// Account balance (the `metasearch` row only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub balance_usd: Option<f64>,
}

/// The cheapest mode that exercises the provider: general search where it
/// has one, else its first mode (extract-only providers fetch example.com).
fn probe_mode(modes: &[Mode]) -> Mode {
    if modes.contains(&Mode::General) {
        Mode::General
    } else {
        modes.first().copied().unwrap_or(Mode::General)
    }
}

/// The account row: is the stored token valid, and what is the balance.
async fn metasearch_check(ctx: &AppContext) -> Option<Check> {
    let row =
        |ok: bool, error: Option<String>, latency: Option<u128>, balance: Option<f64>| Check {
            provider: "metasearch".to_string(),
            configured: true,
            ok,
            latency_ms: latency,
            results: None,
            category: (!ok).then(|| "auth".to_string()),
            error,
            transport: "hosted".to_string(),
            balance_usd: balance,
        };
    let client = match MetasearchClient::new(ctx) {
        Ok(Some(c)) => c,
        Ok(None) => return None,
        Err(e) => return Some(row(false, Some(redact_secrets(&e.to_string())), None, None)),
    };
    let start = Instant::now();
    Some(match client.me().await {
        Ok(me) => row(
            true,
            None,
            Some(start.elapsed().as_millis()),
            Some(me.balance_usd),
        ),
        Err(e) => row(
            false,
            Some(truncate(&redact_secrets(&e.to_string()), 160)),
            Some(start.elapsed().as_millis()),
            None,
        ),
    })
}

pub async fn run(ctx: Arc<AppContext>) -> Vec<Check> {
    let mut set: JoinSet<Check> = JoinSet::new();
    let mut out = Vec::new();
    let account = metasearch_check(&ctx).await;
    let hosted_ok = account.as_ref().is_some_and(|c| c.ok);

    for provider in providers::build_providers(&ctx) {
        let name = provider.name();
        if !provider.is_configured() {
            // Not probed: without a key it runs hosted (when logged in) or
            // not at all.
            let hosted = hosted_ok && !provider.spec().env_keys.is_empty();
            out.push(Check {
                provider: name.to_string(),
                configured: false,
                ok: true,
                latency_ms: None,
                results: None,
                error: None,
                category: None,
                transport: if hosted { "hosted" } else { "none" }.to_string(),
                balance_usd: None,
            });
            continue;
        }
        let transport = provider.transport();
        set.spawn(async move {
            let spec = provider.spec();
            let mode = probe_mode(spec.modes);
            let query = if registry_input_is_url(mode) {
                PROBE_URL
            } else {
                PROBE_QUERY
            };
            let opts = SearchOpts::default();
            let call = Call {
                mode,
                query,
                count: 1,
                opts: &opts,
                timeout: spec.timeout,
            };
            let start = Instant::now();
            let outcome =
                timeout(spec.timeout + Duration::from_secs(1), provider.call(&call)).await;
            let latency = start.elapsed().as_millis();
            match outcome {
                Ok(Ok(output)) => Check {
                    provider: name.to_string(),
                    configured: true,
                    ok: true,
                    latency_ms: Some(latency),
                    results: Some(output.results.len()),
                    error: None,
                    category: None,
                    transport: transport.as_str().to_string(),
                    balance_usd: None,
                },
                Ok(Err(e)) => Check {
                    provider: name.to_string(),
                    configured: true,
                    ok: false,
                    latency_ms: Some(latency),
                    results: None,
                    category: Some(e.category().as_str().to_string()),
                    error: Some(truncate(&redact_secrets(&e.to_string()), 160)),
                    transport: transport.as_str().to_string(),
                    balance_usd: None,
                },
                Err(_) => Check {
                    provider: name.to_string(),
                    configured: true,
                    ok: false,
                    latency_ms: Some(latency),
                    results: None,
                    category: Some("timeout".to_string()),
                    error: Some("timed out".to_string()),
                    transport: transport.as_str().to_string(),
                    balance_usd: None,
                },
            }
        });
    }

    // A panicked probe is skipped, not the end of the report.
    while let Some(joined) = set.join_next().await {
        if let Ok(check) = joined {
            out.push(check);
        }
    }
    out.sort_by(|a, b| a.provider.cmp(&b.provider));
    out.extend(account);
    out
}

fn registry_input_is_url(mode: Mode) -> bool {
    crate::registry::spec(mode).input == crate::registry::InputKind::Url
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect::<String>() + "…"
    }
}

pub fn render_human(checks: &[Check]) {
    use crate::output::style::Paint;
    eprintln!("\n{}  provider health\n", "doctor".bold().cyan());
    for c in checks {
        if c.provider == "metasearch" && c.ok {
            println!(
                "  {:<12} {} {:>5}ms  balance ${:.2}",
                c.provider.bold(),
                "ok".green(),
                c.latency_ms.unwrap_or(0),
                c.balance_usd.unwrap_or(0.0)
            );
        } else if !c.configured && c.transport == "hosted" {
            println!("  {:<12} {}", c.provider, "hosted (metasearch.sh)".dimmed());
        } else if !c.configured {
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
