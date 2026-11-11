//! `search doctor` — test-fires every configured provider with a minimal
//! real request and reports health, latency, and failure cause. This is the
//! preflight an agent fleet runs before trusting the tool: `config check`
//! says a key exists; doctor says it works. Each check is one billed
//! (minimal) request per key, so a spent fallback key shows; scrape-only
//! providers are probed against example.com.
//! Providers you have no key for are labelled with their transport (hosted
//! when logged in) but not probed: a hosted probe would be charged. The
//! `metasearch` row checks the account itself (`GET /api/v1/me`, free).

use crate::context::AppContext;
use crate::errors::redact_secrets;
use crate::hosted::MetasearchClient;
use crate::providers::{self, Call};
use crate::spent;
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
    /// Which of the provider's keys this row probed (1 = the first), when
    /// it has more than one: each key is probed, so a spent one shows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<usize>,
    /// direct | local | hosted | none: how a search would reach it.
    pub transport: String,
    /// The provider answered this key as out of credit (`credits_exhausted`)
    /// at `spent_since`; searches try it after the other keys until
    /// `spent_until`. A probe that answers clears it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spent_since: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spent_until: Option<String>,
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
            key: None,
            transport: "hosted".to_string(),
            spent_since: None,
            spent_until: None,
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
    // Each probe also says what it learned about its key: answered, out of
    // credit, or neither.
    let mut set: JoinSet<(Check, String, Option<bool>)> = JoinSet::new();
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
                key: None,
                transport: if hosted { "hosted" } else { "none" }.to_string(),
                spent_since: None,
                spent_until: None,
                balance_usd: None,
            });
            continue;
        }
        let transport = provider.transport();
        let keys = provider.keys();
        let provider: Arc<dyn providers::Adapter> = Arc::from(provider);
        // A keyless provider is probed once, with no key.
        let slots: Vec<(Option<usize>, String)> = match keys.len() {
            0 => vec![(None, String::new())],
            1 => vec![(None, keys.into_iter().next().unwrap_or_default())],
            _ => keys
                .into_iter()
                .enumerate()
                .map(|(i, k)| (Some(i + 1), k))
                .collect(),
        };
        for (slot, key) in slots {
            let provider = provider.clone();
            set.spawn(async move {
                let (check, learned) = probe(provider.as_ref(), name, slot, &key, transport).await;
                (check, key, learned)
            });
        }
    }

    // A panicked probe is skipped, not the end of the report.
    let mut probed: Vec<(Check, String)> = Vec::new();
    let mut changes = spent::Changes::default();
    let now = spent::now();
    let before = spent::Store::load(spent::default_path());
    while let Some(joined) = set.join_next().await {
        if let Ok((check, key, learned)) = joined {
            let fp = spent::fingerprint(&key);
            let marked = before.active(&check.provider, &fp, now).is_some();
            match learned {
                Some(true) if marked => changes.answered.push((check.provider.clone(), fp.clone())),
                Some(false) => changes.spent.push((check.provider.clone(), fp.clone())),
                _ => {}
            }
            probed.push((check, fp));
        }
    }
    let marks = before.apply(&changes, now);
    for (mut check, fp) in probed {
        if let Some(m) = marks.active(&check.provider, &fp, now) {
            check.spent_since = Some(spent::rfc3339(m.since));
            check.spent_until = Some(spent::rfc3339(m.until()));
        }
        out.push(check);
    }
    out.sort_by(|a, b| a.provider.cmp(&b.provider).then(a.key.cmp(&b.key)));
    out.extend(account);
    out
}

/// One minimal real request with one key.
async fn probe(
    provider: &dyn providers::Adapter,
    name: &'static str,
    slot: Option<usize>,
    key: &str,
    transport: crate::types::Transport,
) -> (Check, Option<bool>) {
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
        key,
    };
    let start = Instant::now();
    let outcome = timeout(spec.timeout + Duration::from_secs(1), provider.call(&call)).await;
    let latency = start.elapsed().as_millis();
    let row =
        |ok: bool, results: Option<usize>, category: Option<String>, error: Option<String>| Check {
            provider: name.to_string(),
            configured: true,
            ok,
            latency_ms: Some(latency),
            results,
            error,
            category,
            key: slot,
            transport: transport.as_str().to_string(),
            spent_since: None,
            spent_until: None,
            balance_usd: None,
        };
    let keyed = !key.is_empty();
    match outcome {
        Ok(Ok(output)) => (
            row(true, Some(output.results.len()), None, None),
            keyed.then_some(true),
        ),
        Ok(Err(e)) => {
            let out_of_credit = matches!(e, crate::errors::SearchError::CreditsExhausted { .. });
            (
                row(
                    false,
                    None,
                    Some(e.category().as_str().to_string()),
                    Some(truncate(&redact_secrets(&e.to_string()), 160)),
                ),
                (keyed && out_of_credit).then_some(false),
            )
        }
        Err(_) => (
            row(
                false,
                None,
                Some("timeout".to_string()),
                Some("timed out".to_string()),
            ),
            None,
        ),
    }
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
        let label = match c.key {
            Some(k) => format!("{} key {k}", c.provider),
            None => c.provider.clone(),
        };
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
                label.bold(),
                "ok".green(),
                c.latency_ms.unwrap_or(0),
                c.results.unwrap_or(0)
            );
        } else if let Some(until) = &c.spent_until {
            println!(
                "  {:<12} {} [{}] {} (marked out of credit since {}; tried after the other keys until {until})",
                label.bold(),
                "FAIL".red(),
                c.category.as_deref().unwrap_or("?"),
                c.error.as_deref().unwrap_or(""),
                c.spent_since.as_deref().unwrap_or("?"),
            );
        } else {
            println!(
                "  {:<12} {} [{}] {}",
                label.bold(),
                "FAIL".red(),
                c.category.as_deref().unwrap_or("?"),
                c.error.as_deref().unwrap_or("")
            );
        }
    }
    println!();
}
