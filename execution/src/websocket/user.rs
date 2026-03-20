use anyhow::Result;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use futures_util::{SinkExt, StreamExt};
use hmac::{Hmac, Mac};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use shared::{FillEvent, Side};
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::broadcast;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

type HmacSha256 = Hmac<Sha256>;

const RECONNECT_DELAY_SECS_MIN: u64 = 1;
const RECONNECT_DELAY_SECS_MAX: u64 = 60;
const PING_INTERVAL_SECS: u64 = 8;

#[derive(Serialize)]
struct AuthSubscribe {
    auth: AuthPayload,
    markets: Vec<String>,
    assets_ids: Vec<String>,
    #[serde(rename = "type")]
    sub_type: String,
}

#[derive(Serialize)]
struct AuthPayload {
    #[serde(rename = "apiKey")]
    api_key: String,
    secret: String,
    passphrase: String,
    timestamp: String,
}

/// Build the HMAC-SHA256 auth payload for the Polymarket user WS.
///
/// Polymarket API secrets are stored as base64url strings. The signature is:
///   base64( HMAC-SHA256( base64url_decode(api_secret), timestamp + "GET" + "/ws/user" ) )
fn build_auth(api_key: &str, api_secret: &str, api_passphrase: &str) -> AuthPayload {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .to_string();

    let message = format!("{}GET/ws/user", timestamp);

    // Polymarket secrets use base64url (- and _ instead of + and /).
    // Convert to standard base64 and decode to get the raw key bytes.
    let normalized = api_secret.replace('-', "+").replace('_', "/");
    let secret_bytes = BASE64
        .decode(&normalized)
        .unwrap_or_else(|_| api_secret.as_bytes().to_vec());

    let mut mac = HmacSha256::new_from_slice(&secret_bytes)
        .expect("HMAC accepts any key length");
    mac.update(message.as_bytes());
    let signature = BASE64.encode(mac.finalize().into_bytes());

    AuthPayload {
        api_key: api_key.to_string(),
        secret: signature,
        passphrase: api_passphrase.to_string(),
        timestamp,
    }
}

#[derive(Deserialize, Debug)]
struct WsFillEvent {
    order_id: Option<String>,
    asset_id: Option<String>,
    price: Option<String>,
    size: Option<String>,
    side: Option<String>,
    timestamp: Option<String>,
    #[serde(rename = "type")]
    event_type: Option<String>,
}

pub async fn run(
    poly_ws_url: String,
    api_key: String,
    api_secret: String,
    api_passphrase: String,
    markets: Vec<String>,
    tx: broadcast::Sender<FillEvent>,
) {
    let mut delay = RECONNECT_DELAY_SECS_MIN;
    loop {
        match connect_and_stream(&poly_ws_url, &api_key, &api_secret, &api_passphrase, &markets, &tx).await {
            Ok(()) => {
                // Clean disconnect (server closed) — reset backoff.
                delay = RECONNECT_DELAY_SECS_MIN;
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    delay,
                    "Polymarket user WS disconnected, reconnecting"
                );
                // Exponential backoff with cap: 1s → 2s → 4s → ... → 60s
                delay = (delay * 2).min(RECONNECT_DELAY_SECS_MAX);
            }
        }
        tokio::time::sleep(tokio::time::Duration::from_secs(delay)).await;
    }
}

async fn connect_and_stream(
    url: &str,
    api_key: &str,
    api_secret: &str,
    api_passphrase: &str,
    markets: &[String],
    tx: &broadcast::Sender<FillEvent>,
) -> Result<()> {
    tracing::info!("Connecting to Polymarket user WS");
    let (ws_stream, _) = connect_async(url).await?;
    let (mut write, mut read) = ws_stream.split();

    let sub = AuthSubscribe {
        auth: build_auth(api_key, api_secret, api_passphrase),
        markets: markets.to_vec(),
        assets_ids: vec![],
        sub_type: "user".to_string(),
    };
    write
        .send(Message::Text(serde_json::to_string(&sub)?))
        .await?;

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
                    None => return Ok(()),
                };
                match msg {
                    Message::Pong(_) => {}
                    Message::Close(_) => return Ok(()),
                    Message::Text(text) => {
                        let events: Vec<WsFillEvent> = match serde_json::from_str(&text) {
                            Ok(v) => v,
                            Err(_) => match serde_json::from_str::<WsFillEvent>(&text) {
                                Ok(e) => vec![e],
                                Err(e) => {
                                    tracing::debug!(error = %e, "failed to parse user WS event");
                                    continue;
                                }
                            },
                        };

                        for event in events {
                            if event.event_type.as_deref() != Some("trade") {
                                continue;
                            }
                            if let (Some(order_id), Some(token_id), Some(price_str), Some(size_str), Some(side_str), Some(ts_str)) = (
                                event.order_id,
                                event.asset_id,
                                event.price,
                                event.size,
                                event.side,
                                event.timestamp,
                            ) {
                                let price = match Decimal::from_str(&price_str) {
                                    Ok(p) => p,
                                    Err(_) => continue,
                                };
                                let size = match Decimal::from_str(&size_str) {
                                    Ok(s) => s,
                                    Err(_) => continue,
                                };
                                let timestamp_ms: u64 = ts_str.parse().unwrap_or(0);
                                let side = if side_str.to_uppercase() == "BUY" {
                                    Side::Yes
                                } else {
                                    Side::No
                                };

                                let _ = tx.send(FillEvent {
                                    order_id,
                                    token_id,
                                    price,
                                    size,
                                    side,
                                    timestamp_ms,
                                });
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}
