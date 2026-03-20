use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::str::FromStr;
use tokio::sync::broadcast;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

const RECONNECT_DELAY_SECS: u64 = 1;
const PING_INTERVAL_SECS: u64 = 8;

/// A single orderbook update event from Polymarket.
#[derive(Debug, Clone)]
pub struct BookUpdate {
    pub asset_id: String,
    pub side: String,
    pub price: Decimal,
    pub size: Decimal,
}

#[derive(Serialize)]
struct Subscribe {
    auth: serde_json::Value,
    markets: Vec<String>,
    assets_ids: Vec<String>,
    #[serde(rename = "type")]
    sub_type: String,
}

/// One entry in the `price_changes` array (incremental update format).
#[derive(Deserialize, Debug)]
struct PriceChange {
    asset_id: String,
    side: String,
    price: String,
    size: String,
}

/// One entry in the `bids`/`asks` arrays (initial snapshot format).
#[derive(Deserialize, Debug)]
struct BookLevel {
    price: String,
    size: String,
}

#[derive(Deserialize, Debug)]
struct WsEvent {
    // Shared field
    asset_id: Option<String>,

    // Incremental update format: price_changes array
    price_changes: Option<Vec<PriceChange>>,

    // Snapshot format: top-level bids/asks arrays
    bids: Option<Vec<BookLevel>>,
    asks: Option<Vec<BookLevel>>,

    // Flat fields (legacy / tick_size_change)
    side: Option<String>,
    price: Option<String>,
    size: Option<String>,
    #[serde(rename = "type")]
    event_type: Option<String>,
    new_tick_size: Option<String>,
}

/// Subscribe to price-level updates for a set of token IDs.
/// `token_ids` should contain the YES and NO token IDs for each market.
pub async fn run(
    poly_ws_url: String,
    token_ids: Vec<String>,
    tx: broadcast::Sender<BookUpdate>,
) {
    loop {
        if let Err(e) = connect_and_stream(&poly_ws_url, &token_ids, &tx).await {
            tracing::warn!(error = %e, "Polymarket orderbook WS disconnected, reconnecting in {}s", RECONNECT_DELAY_SECS);
        }
        tokio::time::sleep(tokio::time::Duration::from_secs(RECONNECT_DELAY_SECS)).await;
    }
}

async fn connect_and_stream(
    url: &str,
    token_ids: &[String],
    tx: &broadcast::Sender<BookUpdate>,
) -> Result<()> {
    tracing::info!("Connecting to Polymarket orderbook WS");
    let (ws_stream, _) = connect_async(url).await?;
    let (mut write, mut read) = ws_stream.split();

    // Subscribe to market data (unauthenticated channel).
    // The server requires token IDs in assets_ids, NOT condition IDs in markets.
    let sub = Subscribe {
        auth: serde_json::Value::Null,
        markets: vec![],
        assets_ids: token_ids.to_vec(),
        sub_type: "market".to_string(),
    };
    write
        .send(Message::Text(serde_json::to_string(&sub)?))
        .await?;

    let mut ping_interval = tokio::time::interval(
        tokio::time::Duration::from_secs(PING_INTERVAL_SECS)
    );
    ping_interval.tick().await;

    loop {
        tokio::select! {
            _ = ping_interval.tick() => {
                write.send(Message::Ping(vec![])).await?;
            }
            msg_opt = read.next() => {
                let msg = match msg_opt {
                    Some(Ok(m)) => m,
                    Some(Err(e)) => return Err(e.into()),
                    None => return Ok(()),
                };
                match msg {
                    Message::Pong(_) => {}
                    Message::Close(_) => return Ok(()),
                    Message::Text(text) => {
                        let events: Vec<WsEvent> = match serde_json::from_str(&text) {
                            Ok(v) => v,
                            Err(_) => match serde_json::from_str::<WsEvent>(&text) {
                                Ok(e) => vec![e],
                                Err(e) => {
                                    tracing::debug!(error = %e, "failed to parse orderbook event");
                                    continue;
                                }
                            },
                        };
                        for event in events {
                            if event.event_type.as_deref() == Some("tick_size_change") {
                                tracing::warn!(
                                    new_tick_size = ?event.new_tick_size,
                                    "tick_size_change — cancel all open orders and re-quote"
                                );
                                continue;
                            }

                            // Incremental update: price_changes array
                            if let Some(changes) = event.price_changes {
                                for pc in changes {
                                    let price = match Decimal::from_str(&pc.price) {
                                        Ok(p) => p, Err(_) => continue,
                                    };
                                    let size = match Decimal::from_str(&pc.size) {
                                        Ok(s) => s, Err(_) => continue,
                                    };
                                    let _ = tx.send(BookUpdate {
                                        asset_id: pc.asset_id,
                                        side: pc.side,
                                        price,
                                        size,
                                    });
                                }
                                continue;
                            }

                            // Snapshot: bids/asks arrays at top level
                            if event.bids.is_some() || event.asks.is_some() {
                                if let Some(asset_id) = event.asset_id {
                                    for (levels, side_str) in [
                                        (event.bids.unwrap_or_default(), "BUY"),
                                        (event.asks.unwrap_or_default(), "SELL"),
                                    ] {
                                        for level in levels {
                                            let price = match Decimal::from_str(&level.price) {
                                                Ok(p) => p, Err(_) => continue,
                                            };
                                            let size = match Decimal::from_str(&level.size) {
                                                Ok(s) => s, Err(_) => continue,
                                            };
                                            let _ = tx.send(BookUpdate {
                                                asset_id: asset_id.clone(),
                                                side: side_str.to_string(),
                                                price,
                                                size,
                                            });
                                        }
                                    }
                                }
                                continue;
                            }

                            // Legacy flat format fallback
                            if let (Some(asset_id), Some(side), Some(price_str), Some(size_str)) =
                                (event.asset_id, event.side, event.price, event.size)
                            {
                                let price = match Decimal::from_str(&price_str) {
                                    Ok(p) => p, Err(_) => continue,
                                };
                                let size = match Decimal::from_str(&size_str) {
                                    Ok(s) => s, Err(_) => continue,
                                };
                                let _ = tx.send(BookUpdate { asset_id, side, price, size });
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}
