//! Keys a provider answered as out of credit or quota (failure code
//! `credits_exhausted`, decided by `Adapter::spent`), remembered so later
//! searches try them after the provider's other keys until [`SPENT_FOR`]
//! has passed since the latest such answer.
//!
//! A mark holds the provider, a fingerprint of the key (the first 16 hex
//! digits of its SHA-256, never the key), when the key was first answered as
//! out of credit and when last. A marked key is never dropped: once only
//! marked keys are left, they are still tried before the engine counts as
//! failed, and one that answers loses its mark. A key changed in the config
//! has a new fingerprint, so its mark no longer applies. Losing the file
//! only costs one refused call per marked key.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// How long after its latest "out of credit" answer a key is tried last.
pub const SPENT_FOR: u64 = 3 * 86_400;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mark {
    pub provider: String,
    pub fingerprint: String,
    /// Unix seconds of the first "out of credit" answer of this run of them.
    pub since: u64,
    /// Unix seconds of the latest one (0 in a file without it: `since`).
    #[serde(default)]
    pub last: u64,
}

impl Mark {
    pub fn until(&self) -> u64 {
        self.last.max(self.since).saturating_add(SPENT_FOR)
    }
}

/// A key marked out of credit, as reported to the caller.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpentKey {
    pub provider: String,
    /// The key's place in the provider's list (1 = the first).
    pub key: usize,
    /// First answered as out of credit (RFC 3339).
    pub since: String,
    /// Tried after the provider's other keys until then (RFC 3339).
    pub until: String,
}

/// What a command learned about its keys: (provider, fingerprint) pairs.
#[derive(Default, Clone)]
pub struct Changes {
    /// Answered "out of credit" just now.
    pub spent: Vec<(String, String)>,
    /// Answered while marked.
    pub answered: Vec<(String, String)>,
}

impl Changes {
    pub fn is_empty(&self) -> bool {
        self.spent.is_empty() && self.answered.is_empty()
    }
}

#[derive(Default, Serialize, Deserialize)]
struct File {
    #[serde(default)]
    marks: Vec<Mark>,
}

/// The marks on disk, read once per command.
#[derive(Debug, Clone)]
pub struct Store {
    path: PathBuf,
    marks: Vec<Mark>,
}

pub fn default_path() -> PathBuf {
    crate::cache::cache_dir().join("out_of_credit_keys.json")
}

pub fn fingerprint(key: &str) -> String {
    let digest = Sha256::digest(key.as_bytes());
    digest[..8].iter().map(|b| format!("{b:02x}")).collect()
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn read(path: &Path) -> Vec<Mark> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<File>(&t).ok())
        .map(|f| f.marks)
        .unwrap_or_default()
}

impl Store {
    pub fn load(path: PathBuf) -> Self {
        let marks = read(&path);
        Self { path, marks }
    }

    #[cfg(test)]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The mark on the key with this fingerprint that still applies at `now`.
    pub fn active(&self, provider: &str, fingerprint: &str, now: u64) -> Option<&Mark> {
        self.marks
            .iter()
            .find(|m| m.provider == provider && m.fingerprint == fingerprint && now < m.until())
    }

    /// Every mark that still applies to one of `keys`: the key's place in
    /// the list and the two dates.
    pub fn report(&self, provider: &str, keys: &[String], now: u64) -> Vec<SpentKey> {
        keys.iter()
            .enumerate()
            .filter_map(|(i, k)| {
                self.active(provider, &fingerprint(k), now)
                    .map(|m| SpentKey {
                        provider: provider.to_string(),
                        key: i + 1,
                        since: rfc3339(m.since),
                        until: rfc3339(m.until()),
                    })
            })
            .collect()
    }

    /// Apply a command's changes: re-read the file first (another search
    /// may have written it), then mark, renew, clear and drop lapsed marks.
    /// A renewed mark keeps its first date. Returns the marks after it.
    pub fn apply(&self, changes: &Changes, now: u64) -> Store {
        if changes.is_empty() {
            return self.clone();
        }
        let mut marks = read(&self.path);
        for (p, fp) in &changes.answered {
            marks.retain(|m| !(m.provider == *p && m.fingerprint == *fp));
        }
        for (p, fp) in &changes.spent {
            let current = marks
                .iter_mut()
                .find(|m| m.provider == *p && m.fingerprint == *fp && now < m.until());
            match current {
                Some(m) => m.last = now,
                None => {
                    marks.retain(|m| !(m.provider == *p && m.fingerprint == *fp));
                    marks.push(Mark {
                        provider: p.clone(),
                        fingerprint: fp.clone(),
                        since: now,
                        last: now,
                    });
                }
            }
        }
        // A date ahead of the clock (it was set back) counts as now.
        for m in &mut marks {
            m.since = m.since.min(now);
            m.last = m.last.min(now);
        }
        marks.retain(|m| now < m.until());
        let _ = serde_json::to_vec(&File {
            marks: marks.clone(),
        })
        .map(|body| crate::cache::write_atomic(&self.path, &body));
        Store {
            path: self.path.clone(),
            marks,
        }
    }

    /// Forget every mark; returns how many still applied.
    pub fn clear(&self) -> usize {
        let now = now();
        let n = read(&self.path).iter().filter(|m| now < m.until()).count();
        let _ = std::fs::remove_file(&self.path);
        n
    }
}

/// `YYYY-MM-DDTHH:MM:SSZ` for Unix seconds, without a date-crate dependency.
pub fn rfc3339(secs: u64) -> String {
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (y, m, d) = crate::providers::civil_from_days(days as i64);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(tag: &str) -> Store {
        let path = std::env::temp_dir().join(format!(
            "search-cli-spent-{tag}-{}.json",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        Store::load(path)
    }

    fn spent(p: &str, k: &str) -> Changes {
        Changes {
            spent: vec![(p.into(), fingerprint(k))],
            answered: vec![],
        }
    }

    #[test]
    fn a_mark_holds_a_fingerprint_never_the_key() {
        let s = store("fp");
        s.apply(&spent("exa", "exa-secret-key-0001"), 1_000);
        let text = std::fs::read_to_string(&s.path).unwrap();
        assert!(!text.contains("exa-secret-key-0001"), "{text}");
        assert!(text.contains(&fingerprint("exa-secret-key-0001")));
        assert_eq!(fingerprint("x").len(), 16);
        let _ = std::fs::remove_file(&s.path);
    }

    #[test]
    fn a_mark_lasts_three_days_then_lapses() {
        let s = store("ttl");
        s.apply(&spent("exa", "k1"), 1_000);
        let s = Store::load(s.path.clone());
        let fp = fingerprint("k1");
        assert!(s.active("exa", &fp, 1_000 + SPENT_FOR - 1).is_some());
        assert!(s.active("exa", &fp, 1_000 + SPENT_FOR).is_none());
        // Another key, or another provider, is not marked.
        assert!(s.active("exa", &fingerprint("k2"), 1_001).is_none());
        assert!(s.active("parallel", &fp, 1_001).is_none());
        let _ = std::fs::remove_file(&s.path);
    }

    /// A key refused again keeps its first date and is tried last for
    /// another three days from the new refusal; one that answers is cleared.
    #[test]
    fn a_renewal_keeps_since_and_an_answer_clears_the_mark() {
        let s = store("renew");
        s.apply(&spent("exa", "k1"), 1_000);
        let s = s.apply(&spent("exa", "k1"), 5_000);
        let m = s.active("exa", &fingerprint("k1"), 5_001).unwrap();
        assert_eq!(
            (m.since, m.last, m.until()),
            (1_000, 5_000, 5_000 + SPENT_FOR)
        );
        assert_eq!(read(&s.path).len(), 1);
        let s = s.apply(
            &Changes {
                spent: vec![],
                answered: vec![("exa".into(), fingerprint("k1"))],
            },
            6_000,
        );
        assert!(s.active("exa", &fingerprint("k1"), 6_001).is_none());
        assert!(read(&s.path).is_empty());
        let _ = std::fs::remove_file(&s.path);
    }

    #[test]
    fn report_names_the_key_by_its_place_with_both_dates() {
        let s = store("report");
        let s = s.apply(&spent("exa", "k2"), 86_400);
        let r = s.report("exa", &["k1".into(), "k2".into()], 86_401);
        assert_eq!(
            r,
            vec![SpentKey {
                provider: "exa".into(),
                key: 2,
                since: "1970-01-02T00:00:00Z".into(),
                until: "1970-01-05T00:00:00Z".into(),
            }]
        );
        assert_eq!(s.clear(), 0, "a 1970 mark no longer applies");
        assert!(Store::load(s.path.clone())
            .report("exa", &["k2".into()], 86_401)
            .is_empty());
    }

    /// A corrupt or future date neither panics nor outlasts three days.
    #[test]
    fn odd_dates_are_safe() {
        let s = store("odd");
        std::fs::write(
            &s.path,
            format!(
                r#"{{"marks":[{{"provider":"exa","fingerprint":"{}","since":{}}}]}}"#,
                fingerprint("k1"),
                u64::MAX
            ),
        )
        .unwrap();
        let s = Store::load(s.path.clone());
        assert!(s.active("exa", &fingerprint("k1"), 10).is_some());
        let s = s.apply(&spent("exa", "k9"), 1_000);
        let m = s.active("exa", &fingerprint("k1"), 1_001).unwrap();
        assert_eq!(m.until(), 1_000 + SPENT_FOR);
        let _ = std::fs::remove_file(&s.path);
    }

    #[test]
    fn rfc3339_formats_utc() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1_762_850_096), "2025-11-11T08:34:56Z");
    }
}
