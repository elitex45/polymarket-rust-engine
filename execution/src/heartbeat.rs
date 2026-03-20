use anyhow::Result;
use crate::client::ClobClient;

/// POST /heartbeat to keep resting CLOB orders alive server-side.
/// Requires the L2 API key in the header — if auth is missing the call fails
/// silently (best-effort). Must be called at least every 10s for orders to
/// remain active.
pub async fn send_heartbeat(client: &ClobClient, api_key: &str) -> Result<()> {
    let url = format!("{}/heartbeat", client.clob_base());
    client
        .http()
        .post(&url)
        .header("POLY_API_KEY", api_key)
        .send()
        .await?
        .error_for_status()?;
    Ok(())
}
