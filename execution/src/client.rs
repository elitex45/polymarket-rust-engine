use anyhow::{Context, Result};

/// Authenticated CLOB client. Wraps the private key and derives the L1/L2
/// proxy wallet address used for signing EIP-712 order messages.
///
/// In a full integration this would wrap `rs-clob-client`'s `ClobClient`.
/// The struct is kept opaque here so callers depend only on our interface.
pub struct ClobClient {
    #[allow(dead_code)]
    pub(crate) private_key: String,
    #[allow(dead_code)]
    pub(crate) chain_id: u64,
    pub(crate) http: reqwest::Client,
    pub(crate) clob_base: String,
}

impl ClobClient {
    /// Create an authenticated client.
    ///
    /// `private_key` — 0x-prefixed hex private key for the trading wallet.
    /// `chain_id`    — 137 for Polygon mainnet.
    pub fn new(private_key: impl Into<String>, chain_id: u64) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .context("failed to build HTTP client")?;

        Ok(Self {
            private_key: private_key.into(),
            chain_id,
            http,
            clob_base: "https://clob.polymarket.com".to_string(),
        })
    }

    /// Base URL for REST calls.
    pub fn clob_base(&self) -> &str {
        &self.clob_base
    }

    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }
}
