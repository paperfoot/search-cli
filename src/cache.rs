use crate::types::SearchResponse;
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

const CACHE_TTL_SECS: u64 = 300; // 5 minutes

fn cache_dir() -> PathBuf {
    if let Some(proj) = ProjectDirs::from("", "", "search") {
        proj.cache_dir().to_path_buf()
    } else {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        PathBuf::from(home).join(".cache").join("search")
    }
}

fn last_path() -> PathBuf {
    cache_dir().join("last.json")
}

/// Stable FNV-1a hash of (query, mode). Unlike `DefaultHasher`, the digest is
/// fixed across Rust versions, so cache files survive a toolchain upgrade
/// instead of being silently orphaned (and slowly leaking disk).
fn stable_hash(query: &str, mode: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let feed = |h: &mut u64, bytes: &[u8]| {
        for &b in bytes {
            *h ^= b as u64;
            *h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    feed(&mut h, query.to_lowercase().as_bytes());
    feed(&mut h, &[0]); // delimiter so ("ab","c") != ("a","bc")
    feed(&mut h, mode.as_bytes());
    h
}

fn query_cache_path(query: &str, mode: &str) -> PathBuf {
    // `q2_` prefix: bump from the old DefaultHasher `q_` scheme so the switch
    // doesn't read stale files written under the unstable hash.
    cache_dir().join(format!("q2_{:016x}.json", stable_hash(query, mode)))
}

pub fn save_last(response: &SearchResponse) {
    let dir = cache_dir();
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(json) = serde_json::to_string(response) {
        let _ = std::fs::write(last_path(), &json);
    }
}

pub fn load_last() -> Option<SearchResponse> {
    let content = std::fs::read_to_string(last_path()).ok()?;
    serde_json::from_str(&content).ok()
}

/// Age of the `--last` snapshot from the file's mtime (the snapshot itself
/// predates timestamped entries), so replays can report how stale they are.
pub fn last_age_secs() -> Option<u64> {
    let modified = std::fs::metadata(last_path()).ok()?.modified().ok()?;
    SystemTime::now()
        .duration_since(modified)
        .ok()
        .map(|d| d.as_secs())
}

/// Delete every cached entry (query cache + --last snapshot). Returns the
/// number of files removed.
pub fn clear() -> usize {
    let Ok(entries) = std::fs::read_dir(cache_dir()) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if (name.starts_with("q2_") || name == "last.json")
            && name.ends_with(".json")
            && std::fs::remove_file(entry.path()).is_ok()
        {
            removed += 1;
        }
    }
    removed
}

#[derive(Serialize, Deserialize)]
struct CachedEntry {
    timestamp: u64,
    /// Requested count at cache time. An entry only satisfies a request for the
    /// same or fewer results — otherwise `-c 20` would silently get a cached
    /// 5-result response.
    #[serde(default)]
    count: usize,
    response: SearchResponse,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Save a query result to the TTL cache. Callers should skip empty results.
pub fn save_query(query: &str, mode: &str, count: usize, response: &SearchResponse) {
    let dir = cache_dir();
    let _ = std::fs::create_dir_all(&dir);
    let entry = CachedEntry {
        timestamp: now_secs(),
        count,
        response: response.clone(),
    };
    if let Ok(json) = serde_json::to_string(&entry) {
        let _ = std::fs::write(query_cache_path(query, mode), json);
    }
}

/// Load a cached query result if fresh and it held at least `count` results.
/// Returns the response plus its age in seconds so the caller can mark the
/// replay as cached in the envelope.
pub fn load_query(query: &str, mode: &str, count: usize) -> Option<(SearchResponse, u64)> {
    let path = query_cache_path(query, mode);
    let content = std::fs::read_to_string(path).ok()?;
    let entry: CachedEntry = serde_json::from_str(&content).ok()?;
    // saturating_sub: a backward clock step (NTP/VM resume) must not underflow.
    let age = now_secs().saturating_sub(entry.timestamp);
    if age < CACHE_TTL_SECS && entry.count >= count {
        let mut resp = entry.response;
        resp.results.truncate(count);
        resp.metadata.result_count = resp.results.len();
        Some((resp, age))
    } else {
        None
    }
}
