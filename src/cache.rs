use crate::config::home_dir;
use crate::types::SearchResponse;
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

const CACHE_TTL_SECS: u64 = 300; // 5 minutes

fn cache_dir() -> PathBuf {
    if let Some(proj) = ProjectDirs::from("", "", "search") {
        proj.cache_dir().to_path_buf()
    } else {
        home_dir().join(".cache").join("search")
    }
}

fn last_path() -> PathBuf {
    cache_dir().join("last.json")
}

fn query_cache_path(query: &str, mode: &str, providers: &[String], domains: &[String], excludes: &[String], freshness: Option<&str>) -> PathBuf {
    let mut h = DefaultHasher::new();
    query.to_lowercase().hash(&mut h);
    mode.hash(&mut h);
    // Sort to produce stable keys regardless of argument order
    let mut p = providers.to_vec();
    p.sort();
    for prov in &p { prov.hash(&mut h); }
    let mut d = domains.to_vec();
    d.sort();
    for dom in &d { dom.hash(&mut h); }
    let mut e = excludes.to_vec();
    e.sort();
    for excl in &e { excl.hash(&mut h); }
    freshness.hash(&mut h);
    cache_dir().join(format!("q_{:x}.json", h.finish()))
}

pub fn save_last(response: &SearchResponse) {
    let dir = cache_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!(event = "cache_dir_create_failed", error = %e, path = %dir.display());
    }
    if let Ok(json) = serde_json::to_string(response) {
        let path = last_path();
        if let Err(e) = std::fs::write(&path, &json) {
            tracing::warn!(event = "cache_write_failed", error = %e, path = %path.display());
        }
    }
}

pub fn load_last() -> Option<SearchResponse> {
    let content = std::fs::read_to_string(last_path()).ok()?;
    serde_json::from_str(&content).ok()
}

#[derive(Serialize, Deserialize)]
struct CachedEntry {
    timestamp: u64,
    response: SearchResponse,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Returns whether a response is safe/useful to persist in query cache.
///
/// We intentionally skip caching failure artifacts so repeated queries do not
/// replay stale failed/degraded-empty responses.
pub fn should_cache_query_response(response: &SearchResponse) -> bool {
    // Explicit provider-failure terminal state.
    if response.status == "all_providers_failed" {
        return false;
    }

    // Defensive degraded-empty check (0 results with provider failures), even
    // if status naming changes in the future.
    if response.results.is_empty() && !response.metadata.providers_failed.is_empty() {
        return false;
    }

    true
}

/// Save a query result to the TTL cache
pub fn save_query(query: &str, mode: &str, providers: &[String], domains: &[String], excludes: &[String], freshness: Option<&str>, response: &SearchResponse) {
    if !should_cache_query_response(response) {
        return;
    }

    let dir = cache_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!(event = "cache_dir_create_failed", error = %e, path = %dir.display());
    }
    let entry = CachedEntry {
        timestamp: now_secs(),
        response: response.clone(),
    };
    if let Ok(json) = serde_json::to_string(&entry) {
        let path = query_cache_path(query, mode, providers, domains, excludes, freshness);
        if let Err(e) = std::fs::write(&path, json) {
            tracing::warn!(event = "cache_write_failed", error = %e, path = %path.display());
        }
    }
}

/// Load a cached query result if not expired
pub fn load_query(query: &str, mode: &str, providers: &[String], domains: &[String], excludes: &[String], freshness: Option<&str>) -> Option<SearchResponse> {
    let path = query_cache_path(query, mode, providers, domains, excludes, freshness);
    let content = std::fs::read_to_string(path).ok()?;
    let entry: CachedEntry = serde_json::from_str(&content).ok()?;
    if now_secs() - entry.timestamp < CACHE_TTL_SECS {
        Some(entry.response)
    } else {
        None // expired
    }
}

/// Remove expired query cache files on startup.
/// Silently ignores any errors — eviction is best-effort.
pub fn evict_expired() {
    let dir = cache_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    let now = now_secs();
    let mut evicted = 0u64;
    let mut kept = 0u64;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        if !name.starts_with("q_") {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(entry): Result<CachedEntry, _> = serde_json::from_str(&content) else {
            // Unparseable file — remove it
            let _ = std::fs::remove_file(&path);
            evicted += 1;
            continue;
        };
        if now.saturating_sub(entry.timestamp) >= CACHE_TTL_SECS {
            let _ = std::fs::remove_file(&path);
            evicted += 1;
        } else {
            kept += 1;
        }
    }
    if evicted > 0 || kept > 0 {
        tracing::debug!(event = "cache_eviction", evicted, kept);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ResponseMetadata, SearchResult};

    fn minimal_response(status: &str, results: Vec<SearchResult>, failed: Vec<String>) -> SearchResponse {
        SearchResponse {
            version: "1".into(),
            status: status.into(),
            query: "test".into(),
            mode: "general".into(),
            results,
            metadata: ResponseMetadata {
                elapsed_ms: 0,
                result_count: 0,
                providers_queried: vec![],
                providers_failed: failed,
                providers_failed_detail: vec![],
                providers_skipped: vec![],
            },
        }
    }

    #[test]
    fn test_should_cache_successful_response() {
        let resp = minimal_response("ok", vec![SearchResult {
            title: "test".into(),
            url: "https://example.com".into(),
            snippet: "snippet".into(),
            source: "brave".into(),
            published: None,
            image_url: None,
            extra: None,
        }], vec![]);
        assert!(should_cache_query_response(&resp));
    }

    #[test]
    fn test_should_not_cache_all_providers_failed() {
        let resp = minimal_response("all_providers_failed", vec![], vec!["brave".into()]);
        assert!(!should_cache_query_response(&resp));
    }

    #[test]
    fn test_should_not_cache_degraded_empty() {
        // 0 results + provider failures = degraded-empty
        let resp = minimal_response("partial", vec![], vec!["brave".into()]);
        assert!(!should_cache_query_response(&resp));
    }

    #[test]
    fn test_should_cache_empty_but_no_failures() {
        // 0 results but no failures (e.g., no results for query) — cacheable
        let resp = minimal_response("ok", vec![], vec![]);
        assert!(should_cache_query_response(&resp));
    }

    fn empty_strs() -> Vec<String> { vec![] }

    #[test]
    fn test_query_cache_path_deterministic() {
        let p1 = query_cache_path("hello world", "general", &empty_strs(), &empty_strs(), &empty_strs(), None);
        let p2 = query_cache_path("hello world", "general", &empty_strs(), &empty_strs(), &empty_strs(), None);
        assert_eq!(p1, p2);
    }

    #[test]
    fn test_query_cache_path_mode_sensitive() {
        let p1 = query_cache_path("hello", "general", &empty_strs(), &empty_strs(), &empty_strs(), None);
        let p2 = query_cache_path("hello", "news", &empty_strs(), &empty_strs(), &empty_strs(), None);
        assert_ne!(p1, p2);
    }

    #[test]
    fn test_query_cache_path_provider_sensitive() {
        let p1 = query_cache_path("hello", "general", &["brave".to_string()], &empty_strs(), &empty_strs(), None);
        let p2 = query_cache_path("hello", "general", &empty_strs(), &empty_strs(), &empty_strs(), None);
        assert_ne!(p1, p2);
    }

    #[test]
    fn test_query_cache_path_case_insensitive_query() {
        let p1 = query_cache_path("Rust Language", "general", &empty_strs(), &empty_strs(), &empty_strs(), None);
        let p2 = query_cache_path("rust language", "general", &empty_strs(), &empty_strs(), &empty_strs(), None);
        assert_eq!(p1, p2);
    }

    #[test]
    fn test_query_cache_path_starts_with_q_prefix() {
        let p = query_cache_path("test", "general", &empty_strs(), &empty_strs(), &empty_strs(), None);
        let name = p.file_name().unwrap().to_string_lossy();
        assert!(name.starts_with("q_"));
        assert!(name.ends_with(".json"));
    }
}
