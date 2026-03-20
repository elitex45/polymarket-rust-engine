use crate::client::ClobClient;
use crate::orders::OrderId;
use anyhow::{Context, Result};
use serde_json::json;

/// Cancel a single resting order by ID.
pub async fn cancel_order(client: &ClobClient, order_id: &str) -> Result<()> {
    let url = format!("{}/order/{}", client.clob_base(), order_id);
    client
        .http()
        .delete(&url)
        .send()
        .await
        .context("DELETE /order/{id} request failed")?
        .error_for_status()
        .context("DELETE /order/{id} returned non-2xx")?;

    tracing::info!(order_id, "order cancelled");
    Ok(())
}

/// Cancel multiple orders in a single request.
///
/// Uses POST /cancel-orders with a JSON array of order IDs.
pub async fn batch_cancel(client: &ClobClient, order_ids: &[OrderId]) -> Result<()> {
    if order_ids.is_empty() {
        return Ok(());
    }

    let url = format!("{}/cancel-orders", client.clob_base());
    let body = json!({ "orderIDs": order_ids });

    client
        .http()
        .post(&url)
        .json(&body)
        .send()
        .await
        .context("POST /cancel-orders request failed")?
        .error_for_status()
        .context("POST /cancel-orders returned non-2xx")?;

    tracing::info!(count = order_ids.len(), "batch cancel submitted");
    Ok(())
}
