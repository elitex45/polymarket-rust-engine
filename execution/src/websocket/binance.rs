use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use rust_decimal::Decimal;
use serde::Deserialize;
use std::str::FromStr;
use tokio::sync::broadcast;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

// binance.com is geo-blocked in some regions (HTTP 451); use binance.us as fallback.
const BINANCE_WS_URL: &str = "wss://stream.binance.us:9443/ws/btcusdt@trade";
const RECONNECT_DELAY_SECS: u64 = 1;
/// PING interval — Binance requires PING/PONG to keep streams alive.
const PING_INTERVAL_SECS: u64 = 8;

/// A single BTC/USDT trade tick from Binance.
#[derive(Debug, Clone)]
pub struct BtcPrice {
    pub price: Decimal,
    pub timestamp_ms: u64,
}

#[derive(Deserialize)]
struct BinanceTrade {
    #[serde(rename = "p")]
    price: String,
    #[serde(rename = "T")]
    trade_time: u64,
}

/// Long-running task. Reconnects with 1s backoff on any disconnect.
pub async fn run(tx: broadcast::Sender<BtcPrice>) {
    loop {
        if let Err(e) = connect_and_stream(&tx).await {
            tracing::warn!(error = %e, "Binance WS disconnected, reconnecting in {}s", RECONNECT_DELAY_SECS);
        }
        tokio::time::sleep(tokio::time::Duration::from_secs(RECONNECT_DELAY_SECS)).await;
    }
}

async fn connect_and_stream(tx: &broadcast::Sender<BtcPrice>) -> Result<()> {
    tracing::info!("Connecting to Binance WS: {}", BINANCE_WS_URL);
    let (ws_stream, _) = connect_async(BINANCE_WS_URL).await?;
    let (mut write, mut read) = ws_stream.split();

    let mut ping_interval = tokio::time::interval(
        tokio::time::Duration::from_secs(PING_INTERVAL_SECS)
    );
    ping_interval.tick().await; // consume immediate first tick

    loop {
        tokio::select! {
            _ = ping_interval.tick() => {
                write.send(Message::Ping(vec![])).await?;
            }
            msg_opt = read.next() => {
                let msg = match msg_opt {
                    Some(Ok(m)) => m,
                    Some(Err(e)) => return Err(e.into()),
                    None => return Ok(()), // stream ended cleanly
                };
                match msg {
                    Message::Pong(_) => {} // connection alive
                    Message::Text(text) => {
                        match serde_json::from_str::<BinanceTrade>(&text) {
                            Ok(trade) => {
                                let price = Decimal::from_str(&trade.price)?;
                                let _ = tx.send(BtcPrice {
                                    price,
                                    timestamp_ms: trade.trade_time,
                                });
                            }
                            Err(e) => {
                                tracing::debug!(error = %e, raw = %text, "failed to parse Binance trade");
                            }
                        }
                    }
                    Message::Close(_) => return Ok(()),
                    _ => {}
                }
            }
        }
    }
}
