pub mod binance;
pub mod orderbook;
pub mod user;

use anyhow::Result;
use shared::FillEvent;
use tokio::sync::broadcast;

pub use binance::BtcPrice;
pub use orderbook::BookUpdate;

/// Capacity for broadcast channels. Subscribers that fall behind will receive
/// `RecvError::Lagged` — the event loop should handle this gracefully.
const CHANNEL_CAPACITY: usize = 256;

/// Handles for the three concurrent WebSocket connections.
pub struct WsManager {
    pub btc_price_tx: broadcast::Sender<BtcPrice>,
    pub book_update_tx: broadcast::Sender<BookUpdate>,
    pub fill_event_tx: broadcast::Sender<FillEvent>,
}

/// Receivers that the strategy layer subscribes to.
pub struct WsReceivers {
    pub btc_price_rx: broadcast::Receiver<BtcPrice>,
    pub book_update_rx: broadcast::Receiver<BookUpdate>,
    pub fill_event_rx: broadcast::Receiver<FillEvent>,
}

impl WsManager {
    /// Spawn all three WebSocket tasks. Returns the manager (holds the senders
    /// so that callers can subscribe additional receivers) and the primary set
    /// of receivers for the strategy event loop.
    ///
    /// `api_key` / `api_secret` — Polymarket L2 credentials for the
    /// authenticated user channel.
    pub fn start(
        polymarket_ws_url: impl Into<String> + Clone,
        api_key: impl Into<String>,
        api_secret: impl Into<String>,
        api_passphrase: impl Into<String>,
        // condition_ids: for the user (authenticated fills) channel.
        condition_ids: Vec<String>,
        // token_ids: YES+NO token IDs for each market, for the orderbook channel.
        token_ids: Vec<String>,
    ) -> Result<(Self, WsReceivers)> {
        let (btc_price_tx, btc_price_rx) = broadcast::channel(CHANNEL_CAPACITY);
        let (book_update_tx, book_update_rx) = broadcast::channel(CHANNEL_CAPACITY);
        let (fill_event_tx, fill_event_rx) = broadcast::channel(CHANNEL_CAPACITY);

        let poly_url = polymarket_ws_url.into();

        // Derive the user-channel URL from the market-channel URL.
        // Market channel: .../ws/market
        // User channel:   .../ws/user
        let user_url = poly_url.replace("/ws/market", "/ws/user");

        // Spawn Binance BTC price feed.
        tokio::spawn(binance::run(btc_price_tx.clone()));

        // Spawn Polymarket orderbook feed (unauthenticated).
        // Uses token IDs in assets_ids — condition IDs are rejected server-side.
        tokio::spawn(orderbook::run(
            poly_url.clone(),
            token_ids,
            book_update_tx.clone(),
        ));

        // Spawn Polymarket user fill feed (authenticated).
        // Uses condition IDs in markets field.
        // Skip if no API key is configured (paper trade mode).
        let api_key_str = api_key.into();
        let api_secret_str = api_secret.into();
        let api_passphrase_str = api_passphrase.into();
        if !api_key_str.is_empty() {
            tokio::spawn(user::run(
                user_url,
                api_key_str,
                api_secret_str,
                api_passphrase_str,
                condition_ids,
                fill_event_tx.clone(),
            ));
        } else {
            tracing::info!("No POLY_API_KEY set — skipping user WS (paper trade mode)");
        }

        let manager = WsManager {
            btc_price_tx,
            book_update_tx,
            fill_event_tx,
        };
        let receivers = WsReceivers {
            btc_price_rx,
            book_update_rx,
            fill_event_rx,
        };

        Ok((manager, receivers))
    }

    /// Subscribe a new receiver to the BTC price channel.
    pub fn subscribe_btc_price(&self) -> broadcast::Receiver<BtcPrice> {
        self.btc_price_tx.subscribe()
    }

    /// Subscribe a new receiver to the orderbook update channel.
    pub fn subscribe_book(&self) -> broadcast::Receiver<BookUpdate> {
        self.book_update_tx.subscribe()
    }

    /// Subscribe a new receiver to the fill event channel.
    pub fn subscribe_fills(&self) -> broadcast::Receiver<FillEvent> {
        self.fill_event_tx.subscribe()
    }
}
