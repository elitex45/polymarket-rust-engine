use std::collections::VecDeque;

/// Window length used for realized vol estimation (seconds).
const VOL_WINDOW_SECS: u64 = 300;

/// Default annualized vol for BTC when insufficient history is available.
const DEFAULT_VOL: f64 = 0.80;

/// Seconds in a year (for annualisation).
const SECS_PER_YEAR: f64 = 365.25 * 24.0 * 3600.0;

// ---------------------------------------------------------------------------
// Normal CDF — Abramowitz & Stegun approximation, max error < 7.5e-8
// ---------------------------------------------------------------------------

fn normal_cdf(x: f64) -> f64 {
    if x < 0.0 {
        return 1.0 - normal_cdf(-x);
    }
    let t = 1.0 / (1.0 + 0.2316419 * x);
    let d = 0.398_942_280 * (-x * x / 2.0).exp();
    let poly = t * (0.319_381_5
        + t * (-0.356_563_8
            + t * (1.781_477_9
                + t * (-1.821_255_9
                    + t * 1.330_274_4))));
    1.0 - d * poly
}

// ---------------------------------------------------------------------------
// Binary options fair value
// ---------------------------------------------------------------------------

/// P(BTC finishes >= window_open_price) given current state.
///
/// Treats the 5-min binary market as a digital call option on BTC:
///   - underlying: BTC/USD
///   - strike:     window_open_price
///   - expiry:     seconds_remaining
///   - payout:     $1.00 if YES, $0.00 if NO
///
/// Uses log-normal model (standard for short-dated binary options).
pub fn fair_value_yes(
    current_price: f64,
    window_open_price: f64,
    seconds_remaining: f64,
    realized_vol_annualized: f64,
) -> f64 {
    if seconds_remaining <= 0.0 {
        return if current_price >= window_open_price { 1.0 } else { 0.0 };
    }
    if window_open_price <= 0.0 || current_price <= 0.0 {
        return 0.5;
    }

    let t = seconds_remaining / SECS_PER_YEAR;
    let vol = realized_vol_annualized.max(0.01);
    let sigma_remaining = vol * t.sqrt();

    if sigma_remaining < 1e-10 {
        return if current_price >= window_open_price { 1.0 } else { 0.0 };
    }

    let log_return = (current_price / window_open_price).ln();
    let d = log_return / sigma_remaining;
    normal_cdf(d).clamp(0.01, 0.99)
}

/// Blend the model's fair value with Polymarket's observed mid-price.
///
/// Near window open: trust Polymarket more (30% model / 70% market).
/// Near window close: trust the model more (70% model / 30% market).
/// Polymarket mid is noisy near close (thin liquidity); model converges to true probability.
pub fn blended_fair_value(
    model_fv: f64,
    polymarket_mid: f64,
    seconds_remaining: f64,
    window_secs: f64,
) -> f64 {
    let time_weight = 1.0 - (seconds_remaining / window_secs).clamp(0.0, 1.0);
    let model_weight = 0.30 + 0.40 * time_weight; // 0.30 at open → 0.70 at close
    (model_weight * model_fv + (1.0 - model_weight) * polymarket_mid).clamp(0.01, 0.99)
}

// ---------------------------------------------------------------------------
// Realized volatility estimator (rolling 5-min window)
// ---------------------------------------------------------------------------

/// Rolling realized vol estimator using log returns over the last `window_secs`.
/// Update every BTC trade tick. Read every 5 seconds for Stoikov input.
pub struct VolEstimator {
    prices: VecDeque<(f64, u64)>, // (price, timestamp_ms)
    window_secs: u64,
}

impl VolEstimator {
    pub fn new() -> Self {
        Self {
            prices: VecDeque::new(),
            window_secs: VOL_WINDOW_SECS,
        }
    }

    /// Record a new BTC price tick.
    pub fn update(&mut self, price: f64, timestamp_ms: u64) {
        self.prices.push_back((price, timestamp_ms));
        let cutoff = timestamp_ms.saturating_sub(self.window_secs * 1000);
        while let Some(&(_, ts)) = self.prices.front() {
            if ts < cutoff {
                self.prices.pop_front();
            } else {
                break;
            }
        }
    }

    /// Annualized realized volatility from log returns.
    /// Returns DEFAULT_VOL (80%) when insufficient history is available.
    pub fn annualized_vol(&self) -> f64 {
        if self.prices.len() < 2 {
            return DEFAULT_VOL;
        }
        let prices: Vec<f64> = self.prices.iter().map(|(p, _)| *p).collect();
        let log_returns: Vec<f64> = prices
            .windows(2)
            .map(|w| (w[1] / w[0]).ln())
            .collect();

        let n = log_returns.len() as f64;
        let mean = log_returns.iter().sum::<f64>() / n;
        let variance = log_returns.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / n;

        // Annualise: observations span window_secs, so each obs ~ window_secs/n seconds
        let secs_per_obs = (self.window_secs as f64) / n;
        (variance / secs_per_obs * SECS_PER_YEAR).sqrt().clamp(0.05, 5.0)
    }

    /// BTC price at or just before `target_ts` (unix ms). Used to find window open price.
    pub fn price_at(&self, target_ts_ms: u64) -> Option<f64> {
        // Walk backward to find the first entry at or before target_ts.
        for (price, ts) in self.prices.iter().rev() {
            if *ts <= target_ts_ms {
                return Some(*price);
            }
        }
        // Fallback: earliest known price.
        self.prices.front().map(|(p, _)| *p)
    }

    /// Most recent BTC price, or None if no data yet.
    pub fn latest_price(&self) -> Option<f64> {
        self.prices.back().map(|(p, _)| *p)
    }

    /// 1-minute realized vol as annualized %, for volatility pause detection.
    pub fn vol_1min_annualized_pct(&self) -> f64 {
        if self.prices.len() < 2 {
            return 0.0;
        }
        let cutoff_ms = self
            .prices
            .back()
            .map(|(_, ts)| ts.saturating_sub(60_000))
            .unwrap_or(0);
        let recent: Vec<f64> = self
            .prices
            .iter()
            .filter(|(_, ts)| *ts >= cutoff_ms)
            .map(|(p, _)| *p)
            .collect();
        if recent.len() < 2 {
            return 0.0;
        }
        let log_returns: Vec<f64> = recent.windows(2).map(|w| (w[1] / w[0]).ln()).collect();
        let n = log_returns.len() as f64;
        let mean = log_returns.iter().sum::<f64>() / n;
        let variance = log_returns.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / n;
        let secs_per_obs = 60.0 / n;
        (variance / secs_per_obs * SECS_PER_YEAR).sqrt() * 100.0
    }
}

impl Default for VolEstimator {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fair_value_at_money_is_half() {
        let fv = fair_value_yes(84000.0, 84000.0, 300.0, 0.80);
        assert!((fv - 0.5).abs() < 0.001, "ATM fair value should be ~0.5, got {}", fv);
    }

    #[test]
    fn fair_value_above_strike_is_above_half() {
        let fv = fair_value_yes(84200.0, 84000.0, 120.0, 0.80);
        assert!(fv > 0.5, "above strike should be > 0.5, got {}", fv);
    }

    #[test]
    fn fair_value_converges_at_expiry() {
        let fv_win = fair_value_yes(84100.0, 84000.0, 1.0, 0.80);
        assert!(fv_win > 0.9, "1s remaining above strike should be ~1, got {}", fv_win);
        let fv_lose = fair_value_yes(83900.0, 84000.0, 1.0, 0.80);
        assert!(fv_lose < 0.1, "1s remaining below strike should be ~0, got {}", fv_lose);
    }

    #[test]
    fn vol_estimator_default() {
        let est = VolEstimator::new();
        assert_eq!(est.annualized_vol(), 0.80, "default vol should be 0.80");
    }

    #[test]
    fn vol_estimator_updates() {
        let mut est = VolEstimator::new();
        let base_ts: u64 = 1_700_000_000_000;
        // Feed 60 prices at 1-second intervals
        let mut p = 84000.0_f64;
        for i in 0..60u64 {
            p *= 1.0 + if i % 2 == 0 { 0.001 } else { -0.001 };
            est.update(p, base_ts + i * 1000);
        }
        let vol = est.annualized_vol();
        assert!(vol > 0.0, "should compute positive vol");
        assert!(vol < 5.0, "vol should be reasonable");
    }

    #[test]
    fn blended_fv_weights() {
        // At window open (300s remaining), market weight should dominate.
        let fv = blended_fair_value(0.70, 0.50, 300.0, 300.0);
        // model_weight = 0.30, so fv ≈ 0.30*0.70 + 0.70*0.50 = 0.56
        assert!((fv - 0.56).abs() < 0.01, "got {}", fv);

        // At window close (10s remaining), model weight should dominate.
        let fv2 = blended_fair_value(0.70, 0.50, 10.0, 300.0);
        // model_weight ≈ 0.70, so fv ≈ 0.70*0.70 + 0.30*0.50 = 0.64
        assert!(fv2 > fv, "model-heavy fv should be higher when model says 0.70");
    }
}
