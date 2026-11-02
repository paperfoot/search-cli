//! `direct`: the honest markdown-first page fetch (PLAN.md D11). A stub until
//! the cli-extract lane wires it to `page::fetch_direct`; it is kept out of
//! the extract chain and reports itself unconfigured until then.

use super::spec::{FilterSupport, Price};
use super::{Adapter, Call, Output, ProviderSpec};
use crate::context::AppContext;
use crate::errors::SearchError;
use crate::types::Mode;
use async_trait::async_trait;
use std::sync::Arc;
use std::time::Duration;

pub static SPEC: ProviderSpec = ProviderSpec {
    name: "direct",
    env_keys: &[],
    modes: &[Mode::Extract],
    filters: FilterSupport {
        freshness: false,
        domains: false,
        country: false,
        lang: false,
    },
    max_count: 1,
    timeout: Duration::from_secs(8),
    billed_on_send: false,
    price: Price {
        per_call: 0,
        per_extra_result: 0,
        included_results: 0,
    },
};

pub struct Direct {
    _ctx: Arc<AppContext>,
}

impl Direct {
    pub fn new(ctx: Arc<AppContext>) -> Self {
        Self { _ctx: ctx }
    }
}

#[async_trait]
impl Adapter for Direct {
    fn spec(&self) -> &'static ProviderSpec {
        &SPEC
    }

    /// Keyless, but not wired yet: false keeps it out of runs, doctor and
    /// config until it can fetch.
    fn is_configured(&self) -> bool {
        false
    }

    async fn call(&self, _call: &Call<'_>) -> Result<Output, SearchError> {
        Err(SearchError::Api {
            provider: "direct",
            code: "not_implemented",
            message: "the direct fetch is not wired yet".to_string(),
            status: None,
        })
    }
}
