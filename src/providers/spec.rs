//! The one provider list in the crate. Every adapter file defines `pub static SPEC: ProviderSpec` and implements
//! `Adapter`; routing, agent-info, config, doctor, usage, stats and hosted transport all read these records.
//! A provider is called per (provider, mode), exactly like the hosted API, so a hosted call is the same call
//! sent to metasearch.sh instead of the upstream.

use crate::errors::SearchError;
use crate::types::{Answer, FailureCategory, Mode, SearchOpts, SearchResult, Transport};
use async_trait::async_trait;
use std::time::Duration;

/// Which normalized filters the provider honours natively or by query rewrite (site:).
#[derive(Debug, Clone, Copy)]
pub struct FilterSupport {
    pub freshness: bool,
    pub domains: bool,
    pub country: bool,
    pub lang: bool,
}

/// Upstream list price estimate in micro-USD: agent-info, stats, and `metadata.cost` for own-key calls
/// when the upstream reports no cost of its own. Numbers match the server's rate card (PLAN.md §5).
#[derive(Debug, Clone, Copy)]
pub struct Price {
    pub per_call: u32,
    pub per_extra_result: u32,
    pub included_results: u16,
}

impl Price {
    pub const fn estimate(&self, results: u16) -> u64 {
        let extra = if results > self.included_results {
            (results - self.included_results) as u64
        } else {
            0
        };
        self.per_call as u64 + extra * self.per_extra_result as u64
    }
}

#[derive(Debug)]
pub struct ProviderSpec {
    pub name: &'static str,
    /// Bare env names, e.g. ["EXA_API_KEY"]; empty = keyless. Precedence stays env > SEARCH_KEYS_* > file (settled).
    pub env_keys: &'static [&'static str],
    /// Modes this provider serves. `Scrape` never appears: the engine normalizes it to `Extract`.
    pub modes: &'static [Mode],
    pub filters: FilterSupport,
    /// The engine clamps -c to this per provider.
    pub max_count: u16,
    /// Per-call cap; the engine uses min(timeout, mode deadline left) on the one shared client.
    pub timeout: Duration,
    /// Billed when the request is sent: never retry a read timeout, never abort before the mode deadline.
    pub billed_on_send: bool,
    pub price: Price,
}

pub struct Call<'a> {
    pub mode: Mode,
    pub query: &'a str,
    pub count: u16,
    pub opts: &'a SearchOpts,
    pub timeout: Duration,
    /// The key this call sends: one of [`Adapter::keys`], chosen by the
    /// engine; empty for keyless and hosted calls.
    pub key: &'a str,
}

#[derive(Debug, Default)]
pub struct Output {
    pub results: Vec<SearchResult>,
    pub answers: Vec<Answer>,
    /// Upstream-reported cost of this call in micro-USD, when the response carries it.
    pub cost_micros: Option<u64>,
}

#[async_trait]
pub trait Adapter: Send + Sync {
    fn spec(&self) -> &'static ProviderSpec;
    /// Own key present (env > SEARCH_KEYS_* > file), or keyless.
    fn is_configured(&self) -> bool;
    /// The own keys, in the order the engine tries them (see
    /// [`super::keys_for`]); empty for keyless and hosted adapters.
    fn keys(&self) -> Vec<String> {
        Vec::new()
    }
    /// One upstream call for `call.mode`. Errors carry a FailureCategory (via `classify` for non-2xx bodies).
    async fn call(&self, call: &Call<'_>) -> Result<Output, SearchError>;
    /// Body-aware classification of a non-2xx response, tried before the status-only default.
    fn classify(&self, _status: u16, _body: &str) -> Option<FailureCategory> {
        None
    }
    /// What one call in `mode` costs once sent, before any answer: the price
    /// of a call that times out or whose 2xx fails to parse. Override when a
    /// mode sends more than the base call (Tavily's advanced depth, Brave
    /// deep's two requests). Only counted for `billed_on_send` providers.
    fn sent_cost(&self, _mode: Mode) -> u64 {
        self.spec().price.estimate(0)
    }
    /// How this adapter reaches its upstream: own key (direct), keyless on
    /// this machine (local); the hosted adapter overrides it.
    fn transport(&self) -> Transport {
        if self.spec().env_keys.is_empty() {
            Transport::Local
        } else {
            Transport::Direct
        }
    }
    /// Which of the caller's filters this adapter actually sends for `opts`;
    /// the engine warns about every requested filter left out. Direct calls
    /// follow the spec; the hosted adapter follows metasearch.sh's manifest.
    fn applied_filters(&self, _opts: &SearchOpts) -> FilterSupport {
        self.spec().filters
    }
    /// The hosted twin of a provider you hold no key for; the engine folds
    /// these into one metasearch.sh call per search.
    fn as_hosted(&self) -> Option<&crate::hosted::HostedAdapter> {
        None
    }
    fn name(&self) -> &'static str {
        self.spec().name
    }
    fn capabilities(&self) -> Vec<&'static str> {
        self.spec().modes.iter().map(|m| mode_str(*m)).collect()
    }
}

pub fn mode_str(m: Mode) -> &'static str {
    match m {
        Mode::General => "general",
        Mode::News => "news",
        Mode::Academic => "academic",
        Mode::People => "people",
        Mode::Deep => "deep",
        Mode::Extract => "extract",
        Mode::Similar => "similar",
        Mode::Scrape => "scrape",
        Mode::Scholar => "scholar",
        Mode::Patents => "patents",
        Mode::Images => "images",
        Mode::Places => "places",
        Mode::Social => "social",
        Mode::Community => "community",
    }
}
