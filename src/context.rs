use crate::config::AppConfig;
use std::time::Duration;

pub struct AppContext {
    pub client: reqwest::Client,
    pub config: AppConfig,
}

/// Backstop for a stalled socket on requests that set no timeout of their own
/// (account calls). Longer than every mode deadline, so it never cuts a
/// provider call short: those set `.timeout(min(spec.timeout, deadline left))`.
const READ_BACKSTOP: Duration = Duration::from_secs(90);

impl AppContext {
    pub fn new(config: AppConfig) -> Self {
        // One client for every provider, with no whole-request timeout. A
        // client-wide timeout shorter than a provider's own budget used to cut
        // slow calls off mid-flight, and the retry then paid for them again.
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(READ_BACKSTOP)
            .pool_idle_timeout(Duration::from_secs(60))
            .tcp_nodelay(true)
            .user_agent(format!("search-cli/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("failed to build HTTP client");

        Self { client, config }
    }
}
