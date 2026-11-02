use crate::types::SearchResponse;
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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

/// Most query-cache files examined per write when evicting expired entries.
/// Bounds the cost a single search pays; a large backlog drains over a few
/// searches instead of stalling one.
const EVICT_SCAN_LIMIT: usize = 256;

/// URL inputs (extract/scrape/similar) are case-sensitive: `/Page` and `/page`
/// can be different documents. Only text queries are case-folded.
fn is_url_query(query: &str) -> bool {
    let head = query.get(..8).unwrap_or(query).to_ascii_lowercase();
    head.starts_with("http://") || head.starts_with("https://")
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
    if is_url_query(query) {
        feed(&mut h, query.as_bytes());
    } else {
        feed(&mut h, query.to_lowercase().as_bytes());
    }
    feed(&mut h, &[0]); // delimiter so ("ab","c") != ("a","bc")
    feed(&mut h, mode.as_bytes());
    h
}

fn query_cache_path(query: &str, mode: &str) -> PathBuf {
    // `q2_` prefix: bump from the old DefaultHasher `q_` scheme so the switch
    // doesn't read stale files written under the unstable hash.
    cache_dir().join(format!("q2_{:016x}.json", stable_hash(query, mode)))
}

/// Write `bytes` to `path` via a temp file in the same directory and a rename,
/// so a concurrent reader (parallel agents share the cache) sees either the
/// old file or the new one, never a torn write.
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir)?;
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("entry");
    let tmp = dir.join(format!(
        ".{name}.{}.{}.tmp",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let result = std::fs::write(&tmp, bytes).and_then(|()| std::fs::rename(&tmp, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Delete expired query-cache entries, examining at most `EVICT_SCAN_LIMIT`
/// files. Expiry comes from the file's mtime (a stat, no JSON parse). Also
/// removes legacy `q_` files (never read since the hash scheme moved to `q2_`)
/// and temp files orphaned by a crashed write. Returns the number removed.
fn evict_expired(dir: &Path, now: SystemTime) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let ttl = Duration::from_secs(CACHE_TTL_SECS);
    let mut removed = 0;
    for entry in entries.flatten().take(EVICT_SCAN_LIMIT) {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let legacy = name.starts_with("q_") && name.ends_with(".json");
        let candidate =
            (name.starts_with("q2_") && name.ends_with(".json")) || name.ends_with(".tmp");
        if !legacy && !candidate {
            continue;
        }
        let expired = legacy
            || entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|m| now.duration_since(m).ok())
                .is_some_and(|age| age >= ttl);
        if expired && std::fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

pub fn save_last(response: &SearchResponse) {
    if let Ok(json) = serde_json::to_vec(response) {
        let _ = write_atomic(&last_path(), &json);
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
        if (name.starts_with("q2_") || name.starts_with("q_") || name == "last.json")
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
    let entry = CachedEntry {
        timestamp: now_secs(),
        count,
        response: response.clone(),
    };
    if let Ok(json) = serde_json::to_vec(&entry) {
        let _ = write_atomic(&query_cache_path(query, mode), &json);
    }
    evict_expired(&cache_dir(), SystemTime::now());
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_queries_fold_case_but_urls_keep_it() {
        assert_eq!(
            stable_hash("Rust Async", "general"),
            stable_hash("rust async", "general")
        );
        assert_ne!(
            stable_hash("https://x.com/Page", "extract"),
            stable_hash("https://x.com/page", "extract")
        );
        assert!(is_url_query("HTTPS://x.com"));
        assert!(!is_url_query("rust"));
        assert!(!is_url_query("é"));
    }

    #[test]
    fn atomic_write_replaces_and_leaves_no_temp() {
        let dir = std::env::temp_dir().join(format!("search-cache-aw-{}", std::process::id()));
        let path = dir.join("q2_test.json");
        write_atomic(&path, b"one").unwrap();
        write_atomic(&path, b"two").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"two");
        let names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(names, vec!["q2_test.json".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn eviction_removes_only_expired_cache_files() {
        let dir = std::env::temp_dir().join(format!("search-cache-ev-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["q2_a.json", "q_legacy.json", "last.json", "other.txt"] {
            std::fs::write(dir.join(name), b"{}").unwrap();
        }
        // Nothing but the legacy file is expired "now".
        assert_eq!(evict_expired(&dir, SystemTime::now()), 1);
        assert!(dir.join("q2_a.json").exists());
        // Ten minutes later the q2_ entry is past the 5-minute TTL; last.json
        // and unrelated files are never touched.
        let later = SystemTime::now() + Duration::from_secs(600);
        assert_eq!(evict_expired(&dir, later), 1);
        assert!(!dir.join("q2_a.json").exists());
        assert!(dir.join("last.json").exists());
        assert!(dir.join("other.txt").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
