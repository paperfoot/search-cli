use assert_cmd::Command;
use predicates::prelude::*;
use std::time::Instant;

fn search_cmd() -> Command {
    Command::cargo_bin("search").unwrap()
}

/// Providers configured on this machine, from `config check --json` (env or
/// config file). Empty when the command fails or prints something unexpected.
fn configured_providers() -> Vec<String> {
    let output = search_cmd()
        .args(["config", "check", "--json"])
        .output()
        .unwrap();
    serde_json::from_slice::<serde_json::Value>(&output.stdout)
        .ok()
        .and_then(|v| v["configured"].as_array().cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|p| p.as_str().map(str::to_string))
        .collect()
}

/// A fused search that returned results is a pass even if one configured
/// provider failed (`partial_success`): the live tests check the CLI, not the
/// health of every upstream on the day.
fn assert_has_answer_status(json: &serde_json::Value) {
    let status = json["status"].as_str().unwrap_or("");
    assert!(
        matches!(status, "success" | "partial_success"),
        "unexpected status {status:?}: {}",
        json["metadata"]["provider_failures"]
    );
}

/// Live tests make paid upstream calls, so they run only when asked for
/// explicitly: `SEARCH_LIVE_TESTS=1`. A machine that merely has keys (every
/// developer laptop) must not spend money on a plain `cargo test`.
fn live_tests_enabled() -> bool {
    std::env::var("SEARCH_LIVE_TESTS").is_ok_and(|v| v == "1")
}

fn has_any_provider() -> bool {
    live_tests_enabled() && !configured_providers().is_empty()
}

fn has_provider(name: &str) -> bool {
    live_tests_enabled() && configured_providers().iter().any(|p| p == name)
}

#[test]
fn test_config_check_json_lists_configured_providers() {
    let output = search_cmd()
        .args(["config", "check", "--json"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(json["configured"].is_array());
    assert!(json["unconfigured"].is_array());
    assert_eq!(
        json["configured_count"].as_u64().unwrap() as usize,
        json["configured"].as_array().unwrap().len()
    );
}

// =============================================================================
// CLI structure tests (no API keys needed)
// =============================================================================

#[test]
fn test_help_output() {
    search_cmd()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("Aggregates 14 providers"))
        .stdout(predicate::str::contains("brave"))
        .stdout(predicate::str::contains("serper"))
        .stdout(predicate::str::contains("exa"));
}

#[test]
fn test_version() {
    search_cmd()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains("search 0."));
}

#[test]
fn test_search_help_shows_modes() {
    search_cmd()
        .args(["search", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("academic"))
        .stdout(predicate::str::contains("people"))
        .stdout(predicate::str::contains("scholar"))
        .stdout(predicate::str::contains("patents"));
}

#[test]
fn test_agent_info_json() {
    let output = search_cmd().arg("agent-info").output().unwrap();
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["name"], "search");
    // scrape is an alias of extract and is not listed twice.
    assert!(json["modes"].as_array().unwrap().len() >= 12);
    assert!(json["providers"].as_array().unwrap().len() >= 5);
    assert_eq!(json["config"]["env_prefix"], "SEARCH_");
    assert_eq!(json["auto_json_when_piped"], true);
    assert!(json["commands"].is_object());
    assert!(json["exit_codes"].is_object());
    assert!(json["routing"].as_str().unwrap().contains("auto"));
    for p in json["providers"].as_array().unwrap() {
        assert!(
            p["transport"].is_string() && p["usd_per_call"].is_number(),
            "{p}"
        );
    }
    // About half of the 0.9 manifest (21 KB): agents pay for every byte.
    assert!(
        output.stdout.len() < 13_000,
        "{} bytes",
        output.stdout.len()
    );
}

#[test]
fn test_providers_json() {
    let output = search_cmd().args(["providers", "--json"]).output().unwrap();
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["status"], "success");
    let providers = json["providers"].as_array().unwrap();
    let names: Vec<&str> = providers
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"brave"));
    assert!(names.contains(&"serper"));
    assert!(names.contains(&"exa"));
    assert!(names.contains(&"jina"));
    assert!(names.contains(&"firecrawl"));
    assert!(names.contains(&"tavily"));
}

#[test]
fn test_config_check() {
    search_cmd()
        .args(["config", "check"])
        .assert()
        .success()
        .stdout(predicate::str::contains("brave"))
        .stdout(predicate::str::contains("serper"))
        .stdout(predicate::str::contains("exa"))
        .stdout(predicate::str::contains("jina"))
        .stdout(predicate::str::contains("firecrawl"))
        .stdout(predicate::str::contains("tavily"));
}

#[test]
fn test_config_show() {
    search_cmd()
        .args(["config", "show"])
        .assert()
        .success()
        .stdout(predicate::str::contains("count"))
        .stdout(predicate::str::contains("metasearch"));
}

/// The JSON-mode stream contract: one JSON document on stdout even on error,
/// bad input exits 3, and stderr carries no ANSI escapes when it is a pipe.
#[test]
fn test_json_error_goes_to_stdout_without_ansi() {
    let output = search_cmd()
        .args(["search", "-q", "x", "-p", "nosuch", "--json"])
        .output()
        .unwrap();

    assert_eq!(
        output.status.code(),
        Some(3),
        "unknown provider is bad input"
    );
    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout must hold exactly one JSON document");
    assert_eq!(json["status"], "error");
    assert_eq!(json["error"]["code"], "invalid_input");
    assert!(json["error"]["message"]
        .as_str()
        .unwrap()
        .contains("nosuch"));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains('\x1b'),
        "ANSI escape on piped stderr: {stderr:?}"
    );
}

#[test]
fn test_verify_without_emails_is_bad_input() {
    let output = search_cmd().args(["verify", "--json"]).output().unwrap();
    assert_eq!(output.status.code(), Some(3));
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["status"], "error");
}

/// Parse errors from clap follow the same contract as runtime errors.
#[test]
fn test_parse_error_json_on_stdout() {
    let output = search_cmd()
        .args(["search", "--no-such-flag", "--json"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(3));
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["status"], "error");
}

// =============================================================================
// Real API tests (paid calls: need configured providers AND SEARCH_LIVE_TESTS=1)
// =============================================================================

#[test]
fn test_real_general_search() {
    if !has_any_provider() {
        eprintln!("SKIP: set SEARCH_LIVE_TESTS=1 with a provider configured");
        return;
    }

    let start = Instant::now();
    let output = search_cmd()
        .args([
            "search",
            "-q",
            "Rust programming language",
            "--json",
            "-c",
            "5",
        ])
        .output()
        .unwrap();
    let elapsed = start.elapsed();

    assert!(
        output.status.success(),
        "search failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_has_answer_status(&json);
    assert_eq!(json["mode"], "general");

    let results = json["results"].as_array().unwrap();
    assert!(!results.is_empty(), "expected results, got 0");

    // Check result structure
    let first = &results[0];
    assert!(first["title"].as_str().is_some());
    assert!(first["url"].as_str().is_some());
    assert!(first["source"].as_str().is_some());

    // Metadata
    assert!(json["metadata"]["elapsed_ms"].as_u64().unwrap() > 0);
    assert!(!json["metadata"]["providers_queried"]
        .as_array()
        .unwrap()
        .is_empty());

    eprintln!(
        "  PASS general search: {} results in {}ms (wall: {}ms)",
        results.len(),
        json["metadata"]["elapsed_ms"],
        elapsed.as_millis()
    );
}

#[test]
fn test_real_news_search() {
    if !has_provider("serper") && !has_provider("brave") {
        eprintln!("SKIP: need serper or brave for news");
        return;
    }

    let output = search_cmd()
        .args([
            "search",
            "-q",
            "technology news",
            "-m",
            "news",
            "--json",
            "-c",
            "5",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["mode"], "news");
    let results = json["results"].as_array().unwrap();
    assert!(!results.is_empty(), "news search returned 0 results");

    eprintln!("  PASS news search: {} results", results.len());
}

#[test]
fn test_real_academic_search() {
    if !has_provider("exa") && !has_provider("serper") {
        eprintln!("SKIP: need exa or serper for academic");
        return;
    }

    let output = search_cmd()
        .args([
            "search",
            "-q",
            "CRISPR gene editing",
            "-m",
            "academic",
            "--json",
            "-c",
            "5",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["mode"], "academic");
    let results = json["results"].as_array().unwrap();
    assert!(!results.is_empty(), "academic search returned 0 results");

    eprintln!("  PASS academic search: {} results", results.len());
}

#[test]
fn test_real_provider_filter_single() {
    if !has_provider("exa") {
        eprintln!("SKIP: need exa");
        return;
    }

    let output = search_cmd()
        .args([
            "search",
            "-q",
            "artificial intelligence",
            "-p",
            "exa",
            "--json",
            "-c",
            "3",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let providers = json["metadata"]["providers_queried"].as_array().unwrap();
    assert_eq!(providers.len(), 1);
    assert_eq!(providers[0], "exa");

    // All results should be from exa
    for r in json["results"].as_array().unwrap() {
        assert!(
            r["source"].as_str().unwrap().starts_with("exa"),
            "unexpected source: {}",
            r["source"]
        );
    }

    eprintln!(
        "  PASS provider filter (exa only): {} results",
        json["results"].as_array().unwrap().len()
    );
}

#[test]
fn test_real_domain_filter() {
    if !has_provider("exa") {
        eprintln!("SKIP: need exa for domain filter");
        return;
    }

    let output = search_cmd()
        .args([
            "search",
            "-q",
            "machine learning",
            "-d",
            "arxiv.org",
            "-p",
            "exa",
            "--json",
            "-c",
            "5",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let results = json["results"].as_array().unwrap();
    assert!(!results.is_empty());

    // All results should be from arxiv.org
    for r in results {
        let url = r["url"].as_str().unwrap();
        assert!(
            url.contains("arxiv.org"),
            "expected arxiv.org URL, got: {}",
            url
        );
    }

    eprintln!(
        "  PASS domain filter (arxiv.org): {} results",
        results.len()
    );
}

#[test]
fn test_real_freshness_filter() {
    if !has_provider("serper") {
        eprintln!("SKIP: need serper for freshness test");
        return;
    }

    let output = search_cmd()
        .args([
            "search", "-q", "AI news", "-f", "day", "-p", "serper", "--json", "-c", "5",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let results = json["results"].as_array().unwrap();
    assert!(!results.is_empty(), "freshness filter returned 0 results");

    eprintln!("  PASS freshness filter (day): {} results", results.len());
}

#[test]
fn test_real_last_cache() {
    if !has_any_provider() {
        eprintln!("SKIP: set SEARCH_LIVE_TESTS=1 with a provider configured");
        return;
    }

    // First search to populate cache
    let output1 = search_cmd()
        .args(["search", "-q", "cache test query", "--json", "-c", "3"])
        .output()
        .unwrap();
    assert!(output1.status.success());

    // Replay from cache
    let start = Instant::now();
    let output2 = search_cmd().args(["--last", "--json"]).output().unwrap();
    let cache_elapsed = start.elapsed();

    assert!(output2.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output2.stdout).unwrap();
    assert_has_answer_status(&json);
    assert!(!json["results"].as_array().unwrap().is_empty());

    // Cache replay should be near-instant (< 100ms)
    assert!(
        cache_elapsed.as_millis() < 500,
        "cache replay took {}ms",
        cache_elapsed.as_millis()
    );

    eprintln!("  PASS cache replay: {}ms", cache_elapsed.as_millis());
}

// =============================================================================
// Performance benchmark
// =============================================================================

#[test]
fn test_performance_benchmark() {
    if !has_any_provider() {
        eprintln!("SKIP: set SEARCH_LIVE_TESTS=1 with a provider configured");
        return;
    }

    let queries = [
        "rust programming",
        "machine learning",
        "quantum computing",
        "climate change",
        "blockchain technology",
    ];

    let mut latencies = Vec::new();

    for q in &queries {
        let start = Instant::now();
        let output = search_cmd()
            .args(["search", "-q", q, "--json", "-c", "5"])
            .output()
            .unwrap();
        let elapsed = start.elapsed().as_millis();

        if output.status.success() {
            let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            let api_ms = json["metadata"]["elapsed_ms"].as_u64().unwrap_or(0);
            let count = json["results"].as_array().map(|a| a.len()).unwrap_or(0);
            eprintln!(
                "  query={:<25} results={:<3} api={}ms  wall={}ms",
                q, count, api_ms, elapsed
            );
            latencies.push(elapsed);
        } else {
            eprintln!("  query={:<25} FAILED", q);
        }
    }

    if latencies.is_empty() {
        eprintln!("  No successful queries for benchmark");
        return;
    }

    latencies.sort();
    let avg = latencies.iter().sum::<u128>() / latencies.len() as u128;
    let p50 = latencies[latencies.len() / 2];
    let p95 = latencies[latencies.len() * 95 / 100];
    let min = latencies[0];
    let max = latencies[latencies.len() - 1];

    eprintln!("\n  BENCHMARK ({} queries):", latencies.len());
    eprintln!("    avg:  {}ms", avg);
    eprintln!("    p50:  {}ms", p50);
    eprintln!("    p95:  {}ms", p95);
    eprintln!("    min:  {}ms", min);
    eprintln!("    max:  {}ms", max);
}

/// A clean environment: an empty HOME, no keys, no login, no log.
fn offline_cmd(tag: &str) -> Command {
    let home = std::env::temp_dir().join(format!("search-cli-test-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    let mut c = search_cmd();
    c.env_clear()
        .env("HOME", home)
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("SEARCH_LOG", "off");
    c
}

/// An unknown -f used to be dropped silently by some providers and sent raw
/// to others; it is now refused before any call (exit 3).
#[test]
fn unknown_freshness_is_refused_before_any_call() {
    let out = offline_cmd("fresh")
        .args(["search", "-q", "rust", "-f", "hour", "--json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("-f must be one of day, week, month, year"),
        "{stdout}"
    );
}

/// Rule 1 trims and ignores scheme case, so the URL check after it must too:
/// " HTTPS://..." used to route to extract and then fail as "not a URL".
#[test]
fn auto_extract_accepts_padded_uppercase_urls() {
    let out = offline_cmd("upurl")
        .args(["search", "-q", " HTTPS://127.0.0.1:9/page ", "--json"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        serde_json::from_str::<serde_json::Value>(&stdout).is_ok(),
        "{stdout}"
    );
    assert!(!stdout.contains("takes a URL"), "{stdout}");
}

/// Search flags after a shorthand query used to become query words, so
/// `search "q" -p serper` searched every provider for "q -p serper".
#[test]
fn shorthand_refuses_search_flags_instead_of_searching_them() {
    for flag in ["-p", "--no-cache", "--mode=news"] {
        let out = offline_cmd("shortflag")
            .args(["rust async", flag, "serper", "--json"])
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(3), "{flag}");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.contains("search search -q"), "{stdout}");
    }
}

#[test]
fn agent_info_lists_auto_first() {
    let out = offline_cmd("autofirst")
        .args(["agent-info", "--json"])
        .output()
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["modes"][0]["name"], "auto", "{}", v["modes"]);
}
