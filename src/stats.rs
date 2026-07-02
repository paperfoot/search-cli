//! `search stats` — local analytics over the JSONL search logs: volume,
//! modes, per-provider calls/failures, estimated spend, cache-hit rate, and
//! an extract-click-through table (which providers' results actually get
//! read). Everything is computed from disk; nothing leaves the machine.

use crate::logging;
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet};

/// Rough per-call cost estimates in USD, from provider pricing pages
/// (2026-07). Estimates only — `search usage` balance snapshots are ground
/// truth where available. Keyed by base provider name.
fn estimated_cost_per_call(provider: &str) -> f64 {
    match provider {
        "serper" => 0.001,
        "jina" => 0.0005,
        "firecrawl" => 0.002,
        "brave" => 0.005,
        "parallel" => 0.005,
        "linkup" => 0.0055,
        "exa" => 0.007,
        "tavily" => 0.010,
        "serpapi" => 0.010,
        "perplexity" => 0.012,
        "browserless" => 0.004,
        "xai" => 0.05,
        _ => 0.0, // stealth and unknowns
    }
}

/// "brave_llm_context" bills as brave; "perplexity_sonar-pro" as perplexity.
fn base_provider(name: &str) -> &str {
    name.split('_').next().unwrap_or(name)
}

#[derive(Debug, Default, Serialize)]
pub struct ProviderStats {
    pub calls: u64,
    pub failures: u64,
    pub cancelled: u64,
    pub results_contributed: u64,
    pub estimated_spend_usd: f64,
    /// Times an extract/scrape later ran on a URL this provider returned —
    /// the closest local proxy for "an agent found this result useful".
    pub result_read_through: u64,
}

#[derive(Debug, Serialize)]
pub struct StatsReport {
    pub window_days: u64,
    pub searches: u64,
    pub cache_hits: u64,
    pub cache_hit_rate: f64,
    pub by_mode: BTreeMap<String, u64>,
    pub by_provider: BTreeMap<String, ProviderStats>,
    pub estimated_total_spend_usd: f64,
    pub top_queries: Vec<(String, u64)>,
    /// Latest known balance per provider from `search usage` snapshots,
    /// with the change across the window (negative = burn).
    pub balances: BTreeMap<String, BalanceTrend>,
    pub log_files_read: u64,
}

#[derive(Debug, Serialize)]
pub struct BalanceTrend {
    pub latest: f64,
    pub unit: String,
    pub change_in_window: Option<f64>,
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Every log line in the window, oldest first.
fn read_entries(days: u64) -> (Vec<serde_json::Value>, u64) {
    let cutoff = now_secs().saturating_sub(days * 86400);
    let mut entries = Vec::new();
    let mut files = 0u64;
    let Ok(dir) = std::fs::read_dir(logging::log_dir()) else {
        return (entries, files);
    };
    for entry in dir.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with("searches_") || !name.ends_with(".jsonl") {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        files += 1;
        for line in content.lines() {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                if v.get("ts").and_then(|t| t.as_u64()).unwrap_or(0) >= cutoff {
                    entries.push(v);
                }
            }
        }
    }
    entries.sort_by_key(|v| v.get("ts").and_then(|t| t.as_u64()).unwrap_or(0));
    (entries, files)
}

pub fn compute(days: u64) -> StatsReport {
    let (entries, log_files_read) = read_entries(days);

    let mut searches = 0u64;
    let mut cache_hits = 0u64;
    let mut by_mode: BTreeMap<String, u64> = BTreeMap::new();
    let mut by_provider: BTreeMap<String, ProviderStats> = BTreeMap::new();
    let mut query_counts: HashMap<String, u64> = HashMap::new();
    // url -> providers that returned it, for read-through attribution.
    let mut url_sources: HashMap<String, HashSet<String>> = HashMap::new();

    for e in &entries {
        searches += 1;
        let cached = e.get("cached").and_then(|c| c.as_bool()).unwrap_or(false);
        if cached {
            cache_hits += 1;
        }
        let mode = e.get("mode").and_then(|m| m.as_str()).unwrap_or("?");
        *by_mode.entry(mode.to_string()).or_default() += 1;

        let query = e.get("query").and_then(|q| q.as_str()).unwrap_or("");
        *query_counts.entry(query.to_lowercase()).or_default() += 1;

        // Read-through: an extract/scrape whose target URL was previously
        // returned by a search credits every provider that returned it.
        if matches!(mode, "extract" | "scrape") {
            if let Some(sources) = url_sources.get(&normalize(query)) {
                for s in sources.clone() {
                    by_provider.entry(s).or_default().result_read_through += 1;
                }
            }
        }

        // Billing/contribution only counts fresh runs.
        if !cached {
            for p in str_array(e, "providers_queried") {
                let base = base_provider(&p).to_string();
                let stat = by_provider.entry(base.clone()).or_default();
                stat.calls += 1;
                stat.estimated_spend_usd += estimated_cost_per_call(&base);
            }
            for p in str_array(e, "providers_failed") {
                by_provider
                    .entry(base_provider(&p).to_string())
                    .or_default()
                    .failures += 1;
            }
            for p in str_array(e, "providers_cancelled") {
                by_provider
                    .entry(base_provider(&p).to_string())
                    .or_default()
                    .cancelled += 1;
            }
            if let Some(obj) = e.get("provider_results").and_then(|o| o.as_object()) {
                for (p, n) in obj {
                    by_provider
                        .entry(base_provider(p).to_string())
                        .or_default()
                        .results_contributed += n.as_u64().unwrap_or(0);
                }
            }
        }

        for url in str_array(e, "urls") {
            let sources: HashSet<String> = str_array(e, "sources")
                .iter()
                .map(|s| base_provider(s).to_string())
                .collect();
            url_sources
                .entry(normalize(&url))
                .or_default()
                .extend(sources);
        }
    }

    let estimated_total_spend_usd = by_provider.values().map(|p| p.estimated_spend_usd).sum();

    let mut top_queries: Vec<(String, u64)> = query_counts.into_iter().collect();
    top_queries.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    top_queries.truncate(10);
    top_queries.retain(|(_, n)| *n > 1);

    StatsReport {
        window_days: days,
        searches,
        cache_hits,
        cache_hit_rate: if searches > 0 {
            cache_hits as f64 / searches as f64
        } else {
            0.0
        },
        by_mode,
        by_provider,
        estimated_total_spend_usd,
        top_queries,
        balances: read_balance_trends(days),
        log_files_read,
    }
}

fn str_array(e: &serde_json::Value, key: &str) -> Vec<String> {
    e.get(key)
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn normalize(url: &str) -> String {
    url.trim_end_matches('/').to_lowercase()
}

/// Latest balance per provider plus the delta across the window — actual
/// burn, measured, not estimated. Fed by `search usage` snapshots.
fn read_balance_trends(days: u64) -> BTreeMap<String, BalanceTrend> {
    let cutoff = now_secs().saturating_sub(days * 86400);
    let path = logging::log_dir().join("balances.jsonl");
    let Ok(content) = std::fs::read_to_string(path) else {
        return BTreeMap::new();
    };
    // provider -> (first_in_window, latest, unit)
    let mut trends: BTreeMap<String, (Option<f64>, f64, String)> = BTreeMap::new();
    for line in content.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let ts = v.get("ts").and_then(|t| t.as_u64()).unwrap_or(0);
        let Some(balances) = v.get("balances").and_then(|b| b.as_object()) else {
            continue;
        };
        for (provider, info) in balances {
            let Some(remaining) = info.get("credits_remaining").and_then(|c| c.as_f64()) else {
                continue;
            };
            let unit = info
                .get("unit")
                .and_then(|u| u.as_str())
                .unwrap_or("credits")
                .to_string();
            let entry = trends
                .entry(provider.clone())
                .or_insert((None, remaining, unit.clone()));
            if ts >= cutoff && entry.0.is_none() {
                entry.0 = Some(remaining);
            }
            entry.1 = remaining;
            entry.2 = unit;
        }
    }
    trends
        .into_iter()
        .map(|(p, (first, latest, unit))| {
            (
                p,
                BalanceTrend {
                    latest,
                    unit,
                    change_in_window: first.map(|f| latest - f),
                },
            )
        })
        .collect()
}

/// Delete log files older than `days`. Returns files removed.
pub fn prune_logs(days: u64) -> usize {
    let cutoff = now_secs().saturating_sub(days * 86400);
    let Ok(dir) = std::fs::read_dir(logging::log_dir()) else {
        return 0;
    };
    let mut removed = 0;
    for entry in dir.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with("searches_") || !name.ends_with(".jsonl") {
            continue;
        }
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
            .is_some_and(|d| d.as_secs() < cutoff);
        if old && std::fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

pub fn render_human(r: &StatsReport) {
    use owo_colors::OwoColorize;
    eprintln!(
        "\n{}  last {} days — {} searches, {:.0}% cache hits\n",
        "stats".bold().cyan(),
        r.window_days,
        r.searches,
        r.cache_hit_rate * 100.0
    );
    println!("modes:");
    for (mode, n) in &r.by_mode {
        println!("  {mode:<10} {n}");
    }
    println!("\nproviders (fresh calls only):");
    println!(
        "  {:<12} {:>6} {:>6} {:>7} {:>8} {:>10} {:>6}",
        "provider", "calls", "fail", "cancel", "results", "~spend", "read"
    );
    for (p, s) in &r.by_provider {
        println!(
            "  {:<12} {:>6} {:>6} {:>7} {:>8} {:>9.2}$ {:>6}",
            p,
            s.calls,
            s.failures,
            s.cancelled,
            s.results_contributed,
            s.estimated_spend_usd,
            s.result_read_through
        );
    }
    println!(
        "\nestimated total spend: ${:.2} (estimates; run `search usage` for real balances)",
        r.estimated_total_spend_usd
    );
    if !r.balances.is_empty() {
        println!("\nbalances (from usage snapshots):");
        for (p, b) in &r.balances {
            let delta = b
                .change_in_window
                .map(|d| format!(" ({d:+.1} this window)"))
                .unwrap_or_default();
            println!("  {:<12} {} {}{}", p, b.latest, b.unit, delta);
        }
    }
    if !r.top_queries.is_empty() {
        println!("\nrepeated queries:");
        for (q, n) in &r.top_queries {
            println!("  {n}x  {q}");
        }
    }
    println!();
}
