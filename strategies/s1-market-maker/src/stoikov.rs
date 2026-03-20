use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// Minimum spread we'll ever post, in USDC (2 cents).
/// Must exceed the max dynamic taker fee (~1.56% at p=0.50) to avoid being
/// picked off immediately.
pub const MIN_SPREAD: Decimal = dec!(0.02);

/// Avellaneda-Stoikov quote calculator.
///
/// Returns `(bid_price, ask_price)` rounded to the current tick size.
///
/// Parameters:
/// - `mid`            — current YES mid-price from the orderbook (0..1)
/// - `sigma`          — recent BTC price volatility (normalised, e.g. 0.001)
/// - `gamma`          — risk-aversion coefficient (env: RISK_AVERSION, default 0.5)
/// - `inventory_skew` — directional skew from inventory module (positive = long bias)
///
/// Formulas (simplified Avellaneda-Stoikov for binary prediction markets):
///   reservation_price = mid - gamma * sigma^2 * inventory_skew
///   spread             = max(MIN_SPREAD, sigma * gamma * |inventory_skew| + MIN_SPREAD)
///   bid               = reservation_price - spread / 2
///   ask               = reservation_price + spread / 2
///
/// Both prices are clamped to [0.01, 0.99] — Polymarket rejects prices
/// outside this range.
pub fn compute_quotes(
    mid: Decimal,
    sigma: Decimal,
    gamma: Decimal,
    inventory_skew: Decimal,
) -> (Decimal, Decimal) {
    let two = dec!(2);

    // Reservation price: shift mid away from our inventory-heavy side.
    let reservation = mid - gamma * sigma * sigma * inventory_skew;

    // Spread: widens with volatility, risk aversion, and inventory imbalance.
    let dynamic_spread = sigma * gamma * inventory_skew.abs();
    let half_spread = (MIN_SPREAD + dynamic_spread).max(MIN_SPREAD) / two;

    let bid = clamp(reservation - half_spread);
    let ask = clamp(reservation + half_spread);

    (bid, ask)
}

fn clamp(price: Decimal) -> Decimal {
    price.max(dec!(0.01)).min(dec!(0.99))
}

/// Round a price to the nearest valid tick size and clamp to [0.01, 0.99].
/// Polymarket rejects orders whose price is not an exact multiple of tick_size.
pub fn round_to_tick(price: Decimal, tick_size: Decimal) -> Decimal {
    if tick_size.is_zero() {
        return clamp(price);
    }
    let rounded = (price / tick_size).round() * tick_size;
    clamp(rounded)
}

/// Check whether our quotes qualify for rebates given the market's max_spread.
/// Returns false if the market is not in the rebate program (max_spread is None).
pub fn qualifies_for_rebate(
    bid: Decimal,
    no_bid: Decimal,
    mid: Decimal,
    max_spread: Option<Decimal>,
    min_size: Option<Decimal>,
    order_size: Decimal,
) -> bool {
    let Some(ms) = max_spread else { return false };
    if let Some(sz) = min_size {
        if order_size < sz {
            return false;
        }
    }
    (mid - bid) <= ms && (mid - no_bid) <= ms
}

/// Returns true if the new quotes differ from the current quotes by more than
/// `threshold`. Used to decide whether to cancel+replace.
pub fn quotes_moved(
    old_bid: Decimal,
    old_ask: Decimal,
    new_bid: Decimal,
    new_ask: Decimal,
    threshold: Decimal,
) -> bool {
    (new_bid - old_bid).abs() > threshold || (new_ask - old_ask).abs() > threshold
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn bid_below_mid_ask_above() {
        let (bid, ask) = compute_quotes(dec!(0.50), dec!(0.001), dec!(0.5), dec!(0));
        assert!(bid < dec!(0.50), "bid should be below mid");
        assert!(ask > dec!(0.50), "ask should be above mid");
        assert!(ask > bid, "ask must be greater than bid");
    }

    #[test]
    fn spread_widens_with_higher_sigma() {
        let (bid_low, ask_low) = compute_quotes(dec!(0.50), dec!(0.001), dec!(0.5), dec!(0));
        let (bid_high, ask_high) = compute_quotes(dec!(0.50), dec!(0.01), dec!(0.5), dec!(0));
        let spread_low = ask_low - bid_low;
        let spread_high = ask_high - bid_high;
        assert!(spread_high >= spread_low, "higher sigma should produce equal or wider spread");
    }

    #[test]
    fn spread_widens_with_higher_gamma() {
        let (bid_low, ask_low) = compute_quotes(dec!(0.50), dec!(0.005), dec!(0.1), dec!(1));
        let (bid_high, ask_high) = compute_quotes(dec!(0.50), dec!(0.005), dec!(1.0), dec!(1));
        let spread_low = ask_low - bid_low;
        let spread_high = ask_high - bid_high;
        assert!(spread_high >= spread_low, "higher gamma should produce equal or wider spread");
    }

    #[test]
    fn min_spread_enforced() {
        // Even with zero sigma/gamma/skew, spread >= MIN_SPREAD.
        let (bid, ask) = compute_quotes(dec!(0.50), dec!(0), dec!(0), dec!(0));
        let spread = ask - bid;
        assert!(spread >= MIN_SPREAD, "spread must be at least MIN_SPREAD");
    }

    #[test]
    fn inventory_skew_shifts_quotes_down_when_long() {
        // Positive skew (long YES) shifts the reservation price lower, so the
        // bid shifts down. The spread also widens with skew, so the ask may not
        // decrease — but the mid-point of our quotes (reservation) must be lower.
        let (bid_flat, ask_flat) = compute_quotes(dec!(0.50), dec!(0.002), dec!(0.5), dec!(0));
        let (bid_long, ask_long) = compute_quotes(dec!(0.50), dec!(0.002), dec!(0.5), dec!(10));
        // Bid must shift lower (we shade our YES buy price down to reduce further buys).
        assert!(bid_long < bid_flat, "bid should be lower when long YES");
        // Reservation price (mid of quotes) must be lower than the flat case.
        let reservation_flat = (bid_flat + ask_flat) / dec!(2);
        let reservation_long = (bid_long + ask_long) / dec!(2);
        assert!(
            reservation_long < reservation_flat,
            "reservation price should decrease when long YES"
        );
    }

    #[test]
    fn prices_clamped_to_valid_range() {
        // Near-zero mid should not produce prices below 0.01.
        let (bid, ask) = compute_quotes(dec!(0.02), dec!(0.01), dec!(2.0), dec!(5));
        assert!(bid >= dec!(0.01));
        assert!(ask >= dec!(0.01));
        assert!(ask <= dec!(0.99));
    }

    #[test]
    fn quotes_moved_detects_threshold() {
        assert!(quotes_moved(
            dec!(0.48), dec!(0.52),
            dec!(0.47), dec!(0.51),
            dec!(0.005)
        ));
        assert!(!quotes_moved(
            dec!(0.48), dec!(0.52),
            dec!(0.4801), dec!(0.5201),
            dec!(0.005)
        ));
    }
}
