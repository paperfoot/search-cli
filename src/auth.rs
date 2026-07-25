//! Terminal login against metasearch.sh using the OAuth 2.0 device
//! authorization grant (RFC 8628).
//!
//! `search login` asks the server for a pair of codes, opens the browser at
//! the short one, and polls until the human approves. What comes back is a
//! normal `ms_live_…` API key, stored in the same 0600 config file as the
//! per-provider keys. Nothing here changes how searches are dispatched — the
//! key is credentials only.

use crate::config::{config_set, config_unset, AppConfig};
use crate::errors::SearchError;
use crate::output::Ctx;
use serde::Deserialize;
use std::time::{Duration, Instant};

pub const DEFAULT_BASE_URL: &str = "https://metasearch.sh";

/// Ceiling on the poll loop, independent of what the server reports, so a bad
/// `expires_in` can never leave a terminal spinning.
const MAX_WAIT: Duration = Duration::from_secs(900);
const MIN_INTERVAL_SECS: u64 = 2;
/// Consecutive transport errors tolerated while polling before giving up.
const MAX_TRANSPORT_FAILURES: u32 = 10;

#[derive(Debug, Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    verification_uri_complete: String,
    expires_in: u64,
    interval: u64,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    api_key: String,
    key_prefix: String,
    #[serde(default)]
    key_name: Option<String>,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    balance_usd: f64,
}

#[derive(Debug, Deserialize)]
struct ErrorResponse {
    #[serde(default)]
    error: String,
}

#[derive(Debug, Deserialize)]
pub struct Identity {
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub balance_usd: f64,
    #[serde(default)]
    pub key: IdentityKey,
}

#[derive(Debug, Default, Deserialize)]
pub struct IdentityKey {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub prefix: Option<String>,
    #[serde(default)]
    pub last_used_at: Option<String>,
}

/// Account base URL: `METASEARCH_URL` > config > metasearch.sh.
pub fn resolve_base_url(config: &AppConfig) -> String {
    if let Ok(v) = std::env::var("METASEARCH_URL") {
        if !v.is_empty() {
            return v.trim_end_matches('/').to_string();
        }
    }
    if !config.metasearch.url.is_empty() {
        return config.metasearch.url.trim_end_matches('/').to_string();
    }
    DEFAULT_BASE_URL.to_string()
}

/// Hosts `search login` will hand an API key to without an explicit override.
const TRUSTED_LOGIN_HOSTS: &[&str] = &["metasearch.sh", "www.metasearch.sh"];

/// Environment variable that consents to logging in against another host.
const OVERRIDE_VAR: &str = "METASEARCH_ALLOW_INSECURE_HOST";

/// Guard the host `search login` will send credentials to.
///
/// The login flow ends with the server handing back an API key that can spend
/// real money. An agent reading a poisoned web page can run
/// `search config set metasearch.url https://attacker.tld` — or pass
/// `--url` — and the next login delivers the key to whoever asked. Anything
/// but the known hosts now needs deliberate consent, and plain http is
/// refused outright: a bearer token must not cross the wire in clear.
fn assert_login_host_allowed(base: &str) -> Result<(), SearchError> {
    let parsed = url::Url::parse(base)
        .map_err(|e| SearchError::Config(format!("'{base}' is not a valid URL: {e}")))?;
    let host = parsed.host_str().unwrap_or_default().to_ascii_lowercase();

    if TRUSTED_LOGIN_HOSTS.contains(&host.as_str()) && parsed.scheme() == "https" {
        return Ok(());
    }

    let consented = std::env::var(OVERRIDE_VAR).is_ok_and(|v| v == "1" || v == "true");
    if !consented {
        return Err(SearchError::Config(format!(
            "refusing to send credentials to '{base}'. `search login` only trusts \
             https://metasearch.sh. If you are running your own instance, set \
             {OVERRIDE_VAR}=1 to confirm."
        )));
    }

    // Consent covers an unfamiliar host, never an unencrypted one — except on
    // loopback, where there is no wire to intercept.
    let loopback = matches!(host.as_str(), "localhost" | "127.0.0.1" | "[::1]" | "::1");
    if parsed.scheme() != "https" && !loopback {
        return Err(SearchError::Config(format!(
            "refusing to send credentials over plain http to '{base}'"
        )));
    }

    Ok(())
}

/// Account API key: `METASEARCH_TOKEN` > config. Empty means "not logged in".
pub fn resolve_token(config: &AppConfig) -> String {
    if let Ok(v) = std::env::var("METASEARCH_TOKEN") {
        if !v.is_empty() {
            return v;
        }
    }
    config.metasearch.token.clone()
}

fn api_error(message: impl Into<String>) -> SearchError {
    SearchError::Api {
        provider: "metasearch",
        code: "api_error",
        message: message.into(),
        status: None,
    }
}

/// Best-effort machine name for the key label, so the dashboard shows which
/// laptop a key belongs to. Absence is fine — the server defaults gracefully.
fn hostname() -> Option<String> {
    for var in ["HOSTNAME", "COMPUTERNAME"] {
        if let Ok(v) = std::env::var(var) {
            if !v.trim().is_empty() {
                return Some(v.trim().to_string());
            }
        }
    }
    let out = std::process::Command::new("hostname").output().ok()?;
    let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
    // Strip the .local / .lan suffix macOS and most routers append.
    let name = name.split('.').next().unwrap_or(&name).to_string();
    (!name.is_empty()).then_some(name)
}

/// Fire-and-forget browser open. Failure is expected on headless boxes and is
/// never fatal — the URL is always printed as well.
fn open_browser(url: &str) -> bool {
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = std::process::Command::new("open");
        c.arg(url);
        c
    };
    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", "start", "", url]);
        c
    };
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let mut cmd = {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(url);
        c
    };

    cmd.stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .is_ok()
}

/// `search login` — device flow, then persist the key.
pub async fn login(
    config: &AppConfig,
    client: &reqwest::Client,
    ctx: &Ctx,
    url_override: Option<String>,
    no_browser: bool,
) -> Result<i32, SearchError> {
    use owo_colors::OwoColorize;

    let base = match url_override {
        Some(u) => u.trim_end_matches('/').to_string(),
        None => resolve_base_url(config),
    };
    assert_login_host_allowed(&base)?;

    let start = client
        .post(format!("{base}/api/v1/device/code"))
        .json(&serde_json::json!({
            "client_name": "search-cli",
            "client_version": env!("CARGO_PKG_VERSION"),
            "hostname": hostname(),
        }))
        .send()
        .await
        .map_err(|e| api_error(format!("could not reach {base}: {e}")))?;

    if !start.status().is_success() {
        return Err(api_error(format!(
            "device authorization failed (HTTP {})",
            start.status().as_u16()
        )));
    }
    let start: DeviceCodeResponse = start
        .json()
        .await
        .map_err(|e| api_error(format!("malformed device response: {e}")))?;

    // The prompt goes to stderr under --json so stdout stays a clean envelope.
    let prompt = |line: String| {
        if ctx.is_json() {
            eprintln!("{line}");
        } else {
            println!("{line}");
        }
    };

    prompt(String::new());
    prompt(format!("  confirmation code   {}", start.user_code.bold()));
    prompt(format!("  approve at          {}", start.verification_uri));
    prompt(String::new());

    if no_browser {
        prompt(format!(
            "  open this directly: {}",
            start.verification_uri_complete
        ));
    } else if open_browser(&start.verification_uri_complete) {
        prompt("  opening your browser…".dimmed().to_string());
    } else {
        prompt(format!(
            "  could not open a browser — visit {}",
            start.verification_uri_complete
        ));
    }
    prompt(format!("\n  {}", "waiting for approval…".dimmed()));

    let deadline = Instant::now() + Duration::from_secs(start.expires_in).min(MAX_WAIT);
    let mut interval = Duration::from_secs(start.interval.max(MIN_INTERVAL_SECS));
    let mut transport_failures = 0u32;

    let token = loop {
        tokio::time::sleep(interval).await;
        if Instant::now() >= deadline {
            return Err(api_error(
                "the code expired before it was approved — run `search login` again",
            ));
        }

        // A poll can span minutes of idle time, so dropped keep-alive
        // connections, sleeping laptops, and wifi blips are all expected.
        // Keep waiting through them; only a persistent outage aborts.
        let resp = match client
            .post(format!("{base}/api/v1/device/token"))
            .json(&serde_json::json!({ "device_code": start.device_code }))
            .send()
            .await
        {
            Ok(r) => {
                transport_failures = 0;
                r
            }
            Err(e) => {
                transport_failures += 1;
                if transport_failures > MAX_TRANSPORT_FAILURES {
                    return Err(api_error(format!("lost contact with {base}: {e}")));
                }
                continue;
            }
        };

        if resp.status().is_success() {
            break resp
                .json::<TokenResponse>()
                .await
                .map_err(|e| api_error(format!("malformed token response: {e}")))?;
        }

        let status = resp.status().as_u16();
        let err = resp.json::<ErrorResponse>().await.unwrap_or(ErrorResponse {
            error: format!("http_{status}"),
        });
        match err.error.as_str() {
            "authorization_pending" => continue,
            // The server asked us to back off; honor it for the rest of the flow.
            "slow_down" => {
                interval += Duration::from_secs(2);
                continue;
            }
            "access_denied" => return Err(api_error("the request was denied in the browser")),
            "expired_token" => {
                return Err(api_error(
                    "the code expired before it was approved — run `search login` again",
                ))
            }
            other => return Err(api_error(format!("login failed ({other})"))),
        }
    };

    config_set("metasearch.token", &token.api_key)?;
    if let Some(email) = &token.email {
        config_set("metasearch.email", email)?;
    }
    if base != DEFAULT_BASE_URL {
        config_set("metasearch.url", &base)?;
    }

    if ctx.is_json() {
        crate::output::json::render_value(&serde_json::json!({
            "version": crate::types::ENVELOPE_VERSION,
            "status": "success",
            "account": {
                "email": token.email,
                "key_prefix": token.key_prefix,
                "key_name": token.key_name,
                "balance_usd": token.balance_usd,
                "url": base,
            },
        }));
    } else if !ctx.suppress_human() {
        println!(
            "\n  {} logged in as {}",
            "✓".green().bold(),
            token.email.as_deref().unwrap_or("unknown").bold()
        );
        println!("  key      {}…", token.key_prefix.dimmed());
        println!(
            "  balance  {}",
            format!("${:.2}", token.balance_usd).green()
        );
        println!(
            "  config   {}\n",
            crate::config::config_path().display().to_string().dimmed()
        );
    }

    Ok(0)
}

/// `search logout` — drop the stored key, optionally revoking it server-side.
pub async fn logout(
    config: &AppConfig,
    client: &reqwest::Client,
    ctx: &Ctx,
    revoke: bool,
) -> Result<i32, SearchError> {
    use owo_colors::OwoColorize;

    let token = resolve_token(config);
    if token.is_empty() {
        if ctx.is_json() {
            crate::output::json::render_value(&serde_json::json!({
                "version": crate::types::ENVELOPE_VERSION,
                "status": "success",
                "logged_out": false,
                "note": "no stored credentials",
            }));
        } else if !ctx.suppress_human() {
            println!("  not logged in");
        }
        return Ok(0);
    }

    let mut revoked = false;
    if revoke {
        let base = resolve_base_url(config);
        let resp = client
            .delete(format!("{base}/api/v1/me"))
            .bearer_auth(&token)
            .send()
            .await;
        revoked = matches!(resp, Ok(r) if r.status().is_success());
    }

    // Always clear locally, even when revocation failed — leaving a key the
    // user believes is gone is worse than an orphaned row in the dashboard.
    config_unset("metasearch.token")?;
    config_unset("metasearch.email")?;

    if ctx.is_json() {
        crate::output::json::render_value(&serde_json::json!({
            "version": crate::types::ENVELOPE_VERSION,
            "status": "success",
            "logged_out": true,
            "revoked": revoked,
        }));
    } else if !ctx.suppress_human() {
        println!("  {} local credentials cleared", "✓".green().bold());
        if revoke && !revoked {
            println!(
                "  {} could not revoke the key server-side — revoke it in the dashboard",
                "!".yellow().bold()
            );
        } else if revoked {
            println!("  {} key revoked server-side", "✓".green().bold());
        }
    }

    Ok(0)
}

/// `search whoami` — ask the server who the stored key belongs to.
pub async fn whoami(
    config: &AppConfig,
    client: &reqwest::Client,
    ctx: &Ctx,
) -> Result<i32, SearchError> {
    use owo_colors::OwoColorize;

    let token = resolve_token(config);
    if token.is_empty() {
        return Err(SearchError::Config(
            "not logged in — run `search login`".to_string(),
        ));
    }

    let base = resolve_base_url(config);
    let resp = client
        .get(format!("{base}/api/v1/me"))
        .bearer_auth(&token)
        .send()
        .await
        .map_err(|e| api_error(format!("could not reach {base}: {e}")))?;

    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Err(SearchError::Config(
            "stored key is not valid — run `search login` again".to_string(),
        ));
    }
    if !resp.status().is_success() {
        return Err(api_error(format!("HTTP {}", resp.status().as_u16())));
    }

    let me: Identity = resp
        .json()
        .await
        .map_err(|e| api_error(format!("malformed response: {e}")))?;

    if ctx.is_json() {
        crate::output::json::render_value(&serde_json::json!({
            "version": crate::types::ENVELOPE_VERSION,
            "status": "success",
            "account": {
                "email": me.email,
                "balance_usd": me.balance_usd,
                "key_name": me.key.name,
                "key_prefix": me.key.prefix,
                "last_used_at": me.key.last_used_at,
                "url": base,
            },
        }));
    } else if !ctx.suppress_human() {
        println!();
        println!(
            "  account  {}",
            me.email.as_deref().unwrap_or("unknown").bold()
        );
        println!("  balance  {}", format!("${:.2}", me.balance_usd).green());
        if let Some(name) = &me.key.name {
            println!(
                "  key      {} ({}…)",
                name,
                me.key.prefix.as_deref().unwrap_or("").dimmed()
            );
        }
        println!("  host     {}", base.dimmed());
        println!();
    }

    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::{resolve_base_url, resolve_token, DEFAULT_BASE_URL};
    use crate::config::AppConfig;

    #[test]
    fn base_url_falls_back_and_trims() {
        let mut cfg = AppConfig::default();
        assert_eq!(resolve_base_url(&cfg), DEFAULT_BASE_URL);
        // A trailing slash would produce "https://host//api/v1/..." on join.
        cfg.metasearch.url = "https://ms.example.com/".to_string();
        assert_eq!(resolve_base_url(&cfg), "https://ms.example.com");
    }

    #[test]
    fn login_host_is_pinned_unless_consented() {
        use super::assert_login_host_allowed;
        // The default host is fine.
        assert!(assert_login_host_allowed("https://metasearch.sh").is_ok());
        // A prompt-injected agent redirecting the flow gets refused.
        assert!(assert_login_host_allowed("https://evil.example").is_err());
        // So does plain http on the real host.
        assert!(assert_login_host_allowed("http://metasearch.sh").is_err());
    }

    #[test]
    fn token_is_empty_when_not_logged_in() {
        let cfg = AppConfig::default();
        // Only meaningful when the ambient env is clean; skip if a real login
        // is exported into this shell.
        if std::env::var("METASEARCH_TOKEN").is_err() {
            assert!(resolve_token(&cfg).is_empty());
        }
    }
}
