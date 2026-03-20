use crate::client::ClobClient;
use crate::fee_rate::FeeRateCache;
use anyhow::{bail, Context, Result};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use shared::Side;

pub type OrderId = String;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PlaceOrderRequest {
    token_id: String,
    price: String,
    size: String,
    side: String,
    #[serde(rename = "feeRateBps")]
    fee_rate_bps: u64,
    /// Always GTC for maker limit orders.
    #[serde(rename = "type")]
    order_type: String,
    /// Reject if the order would cross the spread (execute as taker).
    /// Required for all maker orders — prevents accidentally paying taker fees.
    #[serde(rename = "postOnly")]
    post_only: bool,
    /// Must be true for neg-risk markets (multi-outcome CTF). False for binary up/down.
    #[serde(rename = "negRisk")]
    neg_risk: bool,
}

#[derive(Debug, Deserialize)]
struct PlaceOrderResponse {
    /// The CLOB returns HTTP 200 even for rejected orders. Always check this field.
    success: Option<bool>,
    #[serde(rename = "errorMsg")]
    error_msg: Option<String>,
    #[serde(rename = "orderID")]
    order_id: Option<String>,
}

/// Post a maker limit order. Returns the assigned order ID on success.
///
/// Critical rules enforced here:
/// - `feeRateBps` queried fresh (≤60s cache) — mismatched value = silent rejection.
/// - `postOnly=true` — rejects if order would cross spread (prevents taker fees).
/// - `negRisk` must match the market's neg_risk field.
/// - Checks `success: false` in response — CLOB returns HTTP 200 for rejections.
///
/// `price` and `size` must use `rust_decimal::Decimal` — never `f64`.
pub async fn post_maker_limit(
    client: &ClobClient,
    fee_cache: &FeeRateCache,
    token_id: &str,
    side: Side,
    price: Decimal,
    size: Decimal,
    neg_risk: bool,
) -> Result<OrderId> {
    let fee_rate_bps = fee_cache.get(token_id).await.context("get fee rate")?;

    let side_str = match side {
        Side::Yes => "BUY",
        Side::No => "SELL",
    };

    let body = PlaceOrderRequest {
        token_id: token_id.to_string(),
        price: price.to_string(),
        size: size.to_string(),
        side: side_str.to_string(),
        fee_rate_bps,
        order_type: "GTC".to_string(),
        post_only: true,
        neg_risk,
    };

    let url = format!("{}/order", client.clob_base());
    let resp: PlaceOrderResponse = client
        .http()
        .post(&url)
        .json(&body)
        .send()
        .await
        .context("POST /order failed")?
        .error_for_status()
        .context("POST /order returned non-2xx")?
        .json()
        .await
        .context("POST /order JSON parse failed")?;

    // The CLOB returns HTTP 200 with success:false for logical rejections.
    // Always check this — common errors: tick_size, min_order_size, post_only, balance.
    if resp.success == Some(false) {
        let msg = resp.error_msg.unwrap_or_else(|| "unknown".to_string());
        bail!("order rejected: {}", msg);
    }

    let order_id = resp
        .order_id
        .context("order response missing orderID field")?;

    tracing::info!(
        order_id = %order_id,
        token_id,
        side = %side,
        price = %price,
        size = %size,
        fee_rate_bps,
        neg_risk,
        "maker limit order placed"
    );

    Ok(order_id)
}
