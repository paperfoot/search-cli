use crate::types::SearchResponse;
use directories::ProjectDirs;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

pub fn log_dir() -> PathBuf {
    if let Some(proj) = ProjectDirs::from("", "", "search") {
        proj.data_dir().join("logs")
    } else {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        PathBuf::from(home)
            .join(".local")
            .join("share")
            .join("search")
            .join("logs")
    }
}

/// Local logging is on by default; SEARCH_LOG=off disables it entirely
/// (privacy opt-out — queries and result URLs otherwise land on disk).
fn logging_disabled() -> bool {
    std::env::var("SEARCH_LOG").is_ok_and(|v| v.eq_ignore_ascii_case("off"))
}

fn log_path() -> PathBuf {
    // One log file per day: searches_2026-03-22.jsonl
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let days = now / 86400;
    let date = epoch_days_to_date(days);
    log_dir().join(format!("searches_{date}.jsonl"))
}

/// Log a completed search (or cache replay) to the daily JSONL log file.
/// The entry carries a schema version (`v`) so `search stats` and external
/// analysis survive future field changes. The line is written with ONE
/// write_all call: concurrent agent invocations append to the same file, and
/// multi-write appends can interleave.
pub fn log_search(response: &SearchResponse) {
    if logging_disabled() {
        return;
    }
    let path = log_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let entry = serde_json::json!({
        "v": 1,
        "ts": now,
        "query": response.query,
        "mode": response.mode,
        "result_count": response.metadata.result_count,
        "elapsed_ms": response.metadata.elapsed_ms,
        "cached": response.metadata.cached,
        "providers_queried": response.metadata.providers_queried,
        "providers_failed": response.metadata.providers_failed,
        "providers_cancelled": response.metadata.providers_cancelled,
        "provider_results": response.metadata.provider_results,
        "failure_categories": response
            .metadata
            .provider_failures
            .iter()
            .map(|f| format!("{}:{}", f.provider, f.category.as_str()))
            .collect::<Vec<_>>(),
        "answers": response.answers.iter().map(|a| &a.provider).collect::<Vec<_>>(),
        "warnings": response.metadata.warnings.len(),
        "sources": response.results.iter().map(|r| &r.source).collect::<Vec<_>>(),
        "urls": response.results.iter().take(10).map(|r| &r.url).collect::<Vec<_>>(),
    });

    let mut line = serde_json::to_string(&entry).unwrap_or_default();
    line.push('\n');
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = file.write_all(line.as_bytes());
    }
}

/// Append a `search usage` balance snapshot to balances.jsonl. Deltas
/// between snapshots give `search stats` measured credit burn — ground truth
/// the per-call estimates can't provide.
pub fn log_balances(balances: &serde_json::Value) {
    if logging_disabled() {
        return;
    }
    let dir = log_dir();
    let _ = fs::create_dir_all(&dir);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let entry = serde_json::json!({ "v": 1, "ts": now, "balances": balances });
    let mut line = serde_json::to_string(&entry).unwrap_or_default();
    line.push('\n');
    if let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("balances.jsonl"))
    {
        let _ = file.write_all(line.as_bytes());
    }
}

fn epoch_days_to_date(total_days: u64) -> String {
    let z = total_days as i64 + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}
