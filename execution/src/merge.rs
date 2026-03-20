use crate::client::ClobClient;
use anyhow::{Context, Result};
use serde_json::json;

/// Merge matched YES+NO share pairs for `condition_id` via the CTF Exchange
/// contract. Every unmerged YES+NO pair locks capital until settlement — call
/// this after every successful round-trip fill.
///
/// In a full on-chain integration this would call the CTF Exchange contract's
/// `redeemPositions` function via `alloy`. Here we call the CLOB REST endpoint
/// that proxies the merge for simplicity; swap to direct contract call if
/// latency becomes an issue.
pub async fn merge_positions(client: &ClobClient, condition_id: &str) -> Result<()> {
    let url = format!("{}/merge", client.clob_base());
    let body = json!({ "condition_id": condition_id });

    client
        .http()
        .post(&url)
        .json(&body)
        .send()
        .await
        .context("POST /merge request failed")?
        .error_for_status()
        .context("POST /merge returned non-2xx")?;

    tracing::info!(condition_id, "YES+NO positions merged");
    Ok(())
}
