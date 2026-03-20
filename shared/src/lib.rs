use dashmap::DashMap;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// A single BTC 5-min prediction market on Polymarket.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Market {
    pub condition_id: String,
    pub yes_token_id: String,
    pub no_token_id: String,
    pub slug: String,
    /// Unix timestamp (seconds) when this 5-min window resolves.
    pub resolution_ts: i64,
    /// Must be passed to CTF Exchange on order creation for neg-risk markets.
    pub neg_risk: bool,
    /// Minimum order size in shares (reject below this — no error returned by CLOB).
    pub min_order_size: Decimal,
    /// Current tick size (0.01 in normal range, 0.001 near extremes).
    pub tick_size: Decimal,
    /// Rebate program: max spread from mid that qualifies (None = not in program).
    pub rewards_max_spread: Option<Decimal>,
    /// Rebate program: minimum order size that qualifies.
    pub rewards_min_size: Option<Decimal>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Side {
    Yes,
    No,
}

impl std::fmt::Display for Side {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Side::Yes => write!(f, "YES"),
            Side::No => write!(f, "NO"),
        }
    }
}

/// Lock-free order book for a single market. Keys are price levels (Decimal),
/// values are resting size at that level.
pub struct OrderBook {
    /// YES side bids (buy orders), price → size
    pub yes_bids: DashMap<Decimal, Decimal>,
    /// YES side asks (sell orders), price → size
    pub yes_asks: DashMap<Decimal, Decimal>,
    /// NO side bids
    pub no_bids: DashMap<Decimal, Decimal>,
    /// NO side asks
    pub no_asks: DashMap<Decimal, Decimal>,
}

impl OrderBook {
    pub fn new() -> Self {
        Self {
            yes_bids: DashMap::new(),
            yes_asks: DashMap::new(),
            no_bids: DashMap::new(),
            no_asks: DashMap::new(),
        }
    }

    /// Best bid (highest buy price) for YES side, if any.
    pub fn yes_best_bid(&self) -> Option<Decimal> {
        self.yes_bids.iter().map(|e| *e.key()).reduce(Decimal::max)
    }

    /// Best ask (lowest sell price) for YES side, if any.
    pub fn yes_best_ask(&self) -> Option<Decimal> {
        self.yes_asks.iter().map(|e| *e.key()).reduce(Decimal::min)
    }

    /// Mid-price for YES side. Returns None if either side is empty.
    pub fn yes_mid(&self) -> Option<Decimal> {
        let bid = self.yes_best_bid()?;
        let ask = self.yes_best_ask()?;
        Some((bid + ask) / Decimal::TWO)
    }
}

impl Default for OrderBook {
    fn default() -> Self {
        Self::new()
    }
}

/// Emitted when one of our resting orders is filled (partial or full).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FillEvent {
    pub order_id: String,
    pub token_id: String,
    pub price: Decimal,
    pub size: Decimal,
    pub side: Side,
    pub timestamp_ms: u64,
}

/// Tracks capital deployed in a single market for inventory management.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InventoryState {
    pub yes_shares: Decimal,
    pub no_shares: Decimal,
    pub usdc_spent: Decimal,
    pub usdc_collected: Decimal,
}

impl InventoryState {
    /// Net directional exposure in USDC equivalent (positive = long YES bias).
    pub fn net_directional_exposure(&self) -> Decimal {
        self.yes_shares - self.no_shares
    }

    /// Absolute exposure magnitude.
    pub fn absolute_exposure(&self) -> Decimal {
        self.net_directional_exposure().abs()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn test_order_book_mid() {
        let book = OrderBook::new();
        book.yes_bids.insert(dec!(0.48), dec!(100));
        book.yes_asks.insert(dec!(0.52), dec!(100));
        assert_eq!(book.yes_mid(), Some(dec!(0.50)));
    }

    #[test]
    fn test_inventory_net_exposure() {
        let inv = InventoryState {
            yes_shares: dec!(80),
            no_shares: dec!(20),
            usdc_spent: dec!(50),
            usdc_collected: dec!(10),
        };
        assert_eq!(inv.net_directional_exposure(), dec!(60));
        assert_eq!(inv.absolute_exposure(), dec!(60));
    }
}
