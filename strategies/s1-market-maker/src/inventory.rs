use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use shared::{FillEvent, InventoryState, Side};

/// Tracks inventory for a single market and provides Stoikov skew input.
pub struct InventoryManager {
    state: InventoryState,
    max_exposure_usdc: Decimal,
}

impl InventoryManager {
    pub fn new(max_exposure_usdc: Decimal) -> Self {
        Self {
            state: InventoryState::default(),
            max_exposure_usdc,
        }
    }

    /// Apply a fill event, updating share counts and USDC flows.
    pub fn apply_fill(&mut self, fill: &FillEvent) {
        match fill.side {
            Side::Yes => {
                self.state.yes_shares += fill.size;
                self.state.usdc_spent += fill.price * fill.size;
            }
            Side::No => {
                self.state.no_shares += fill.size;
                self.state.usdc_spent += fill.price * fill.size;
            }
        }
    }

    /// Called when a YES+NO pair is merged (redeemed for $1). Reduces both
    /// share counts by the merged quantity.
    pub fn apply_merge(&mut self, quantity: Decimal) {
        let merged = quantity.min(self.state.yes_shares).min(self.state.no_shares);
        self.state.yes_shares -= merged;
        self.state.no_shares -= merged;
        self.state.usdc_collected += merged; // merged pair redeems at $1
    }

    /// Directional skew to feed into Stoikov. Positive = net long YES.
    /// Normalised by max_exposure so that at 100% utilisation skew = ±1.
    pub fn inventory_skew(&self) -> Decimal {
        if self.max_exposure_usdc.is_zero() {
            return dec!(0);
        }
        self.state.net_directional_exposure() / self.max_exposure_usdc
    }

    /// True when absolute exposure exceeds the configured cap.
    #[allow(dead_code)]
    pub fn should_pause_quoting(&self) -> bool {
        self.state.absolute_exposure() >= self.max_exposure_usdc
    }

    pub fn state(&self) -> &InventoryState {
        &self.state
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn yes_fill(size: Decimal, price: Decimal) -> FillEvent {
        FillEvent {
            order_id: "o1".into(),
            token_id: "tok".into(),
            price,
            size,
            side: Side::Yes,
            timestamp_ms: 0,
        }
    }

    fn no_fill(size: Decimal, price: Decimal) -> FillEvent {
        FillEvent {
            order_id: "o2".into(),
            token_id: "tok".into(),
            price,
            size,
            side: Side::No,
            timestamp_ms: 0,
        }
    }

    #[test]
    fn pause_triggers_at_threshold() {
        let mut mgr = InventoryManager::new(dec!(100));
        // Fill 80 YES shares at $0.50 → net exposure = 80
        mgr.apply_fill(&yes_fill(dec!(80), dec!(0.50)));
        assert!(!mgr.should_pause_quoting(), "80 < 100, should not pause");

        // Fill 20 more → net exposure = 100
        mgr.apply_fill(&yes_fill(dec!(20), dec!(0.50)));
        assert!(mgr.should_pause_quoting(), "at threshold, should pause");
    }

    #[test]
    fn bid_shades_lower_when_long_yes() {
        let mut mgr = InventoryManager::new(dec!(100));
        mgr.apply_fill(&yes_fill(dec!(80), dec!(0.50)));
        let skew = mgr.inventory_skew();
        // Skew should be positive (long YES) → Stoikov will shade bid lower.
        assert!(skew > dec!(0), "long YES position should yield positive skew");
    }

    #[test]
    fn merge_reduces_exposure() {
        let mut mgr = InventoryManager::new(dec!(200));
        mgr.apply_fill(&yes_fill(dec!(50), dec!(0.50)));
        mgr.apply_fill(&no_fill(dec!(50), dec!(0.50)));
        assert_eq!(mgr.state().absolute_exposure(), dec!(0));

        mgr.apply_merge(dec!(50));
        assert_eq!(mgr.state().yes_shares, dec!(0));
        assert_eq!(mgr.state().no_shares, dec!(0));
    }

    #[test]
    fn skew_normalised_to_max_exposure() {
        let mut mgr = InventoryManager::new(dec!(100));
        mgr.apply_fill(&yes_fill(dec!(100), dec!(0.50)));
        assert_eq!(mgr.inventory_skew(), dec!(1));
    }
}
