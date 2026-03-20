use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::time::{SystemTime, UNIX_EPOCH};

// ---------------------------------------------------------------------------
// Risk parameters (tunable via env in a future iteration)
// ---------------------------------------------------------------------------

/// Start skewing quotes when net exposure exceeds this (shares).
#[allow(dead_code)]
pub const SKEW_ACTIVATION_SHARES: Decimal = dec!(50);

/// Stop posting the side we're already long when net exposure exceeds this.
pub const ONE_SIDE_STOP_SHARES: Decimal = dec!(150);

/// Cancel all quotes and pause when net exposure exceeds this hard cap.
pub const MAX_NET_EXPOSURE_SHARES: Decimal = dec!(300);

/// If net exposure > 50 shares for more than this many seconds, force-flatten.
pub const MAX_UNHEDGED_HOLD_SEC: u64 = 45;

/// Widen spread starting from this many seconds before window close.
pub const WIDEN_SPREAD_AT_SEC: i64 = 90;

/// Stop quoting entirely this many seconds before window close.
pub const STOP_QUOTING_AT_SEC: i64 = 60;

/// GTD orders auto-expire this many seconds before STOP_QUOTING_AT_SEC,
/// so resting orders self-cancel before we intend to stop quoting.
/// Effective GTD lifetime = (close_ts - now) - STOP_QUOTING_AT_SEC - GTD_BUFFER_SEC.
/// Platform adds +60s on top (security threshold).
pub const GTD_BUFFER_SEC: i64 = 10;

/// Kill the bot for the day if cumulative loss exceeds this (USDC).
pub const DAILY_LOSS_KILL_USDC: Decimal = dec!(75);

/// Cancel all orders in the current window if per-window loss exceeds this.
pub const PER_WINDOW_MAX_LOSS_USDC: Decimal = dec!(30);

/// 1-min annualized vol % above which we pause quoting entirely.
pub const VOL_PAUSE_THRESHOLD_PCT: f64 = 120.0;

/// 1-min annualized vol % below which we resume after a vol pause.
pub const VOL_RESUME_THRESHOLD_PCT: f64 = 80.0;

// ---------------------------------------------------------------------------
// Kill switch
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct KillSwitch {
    daily_pnl: Decimal,
    window_pnl: Decimal,
    pub daily_killed: bool,
}

#[derive(Debug, PartialEq)]
pub enum KillDecision {
    Ok,
    /// Stop quoting this window, cancel orders, reset at next window.
    KillWindow,
    /// Shut the bot down for the day.
    KillDay,
}

impl KillSwitch {
    pub fn update_pnl(&mut self, delta: Decimal) -> KillDecision {
        self.daily_pnl += delta;
        self.window_pnl += delta;

        if self.daily_pnl <= -DAILY_LOSS_KILL_USDC {
            self.daily_killed = true;
            tracing::error!(
                loss = %self.daily_pnl,
                "DAILY KILL SWITCH triggered — shutting down"
            );
            return KillDecision::KillDay;
        }
        if self.window_pnl <= -PER_WINDOW_MAX_LOSS_USDC {
            tracing::warn!(
                loss = %self.window_pnl,
                "WINDOW KILL triggered — cancelling all orders this window"
            );
            return KillDecision::KillWindow;
        }
        KillDecision::Ok
    }

    pub fn reset_window(&mut self) {
        self.window_pnl = dec!(0);
    }

    pub fn daily_pnl(&self) -> Decimal {
        self.daily_pnl
    }
}

// ---------------------------------------------------------------------------
// Unhedged hold timer
// ---------------------------------------------------------------------------

/// Fires `should_flatten()` when net exposure > threshold for too long.
pub struct UnhedgedTimer {
    unhedged_since_ms: Option<u64>,
    exposure_threshold: Decimal,
}

impl UnhedgedTimer {
    pub fn new(exposure_threshold: Decimal) -> Self {
        Self {
            unhedged_since_ms: None,
            exposure_threshold,
        }
    }

    /// Update with current net exposure. Returns true if force-flatten is warranted.
    pub fn update(&mut self, net_exposure: Decimal) -> bool {
        let now_ms = now_ms();
        if net_exposure.abs() > self.exposure_threshold {
            match self.unhedged_since_ms {
                None => {
                    self.unhedged_since_ms = Some(now_ms);
                    false
                }
                Some(since) => {
                    let held_secs = (now_ms - since) / 1000;
                    if held_secs >= MAX_UNHEDGED_HOLD_SEC {
                        tracing::warn!(
                            net_exposure = %net_exposure,
                            held_secs,
                            "unhedged hold timer expired — force flatten required"
                        );
                        true
                    } else {
                        false
                    }
                }
            }
        } else {
            self.unhedged_since_ms = None;
            false
        }
    }

    pub fn reset(&mut self) {
        self.unhedged_since_ms = None;
    }
}

// ---------------------------------------------------------------------------
// Time-to-expiry spread adjustment
// ---------------------------------------------------------------------------

/// Returns `None` when quoting should stop entirely (T-60s).
/// Returns `Some(half_spread)` which widens linearly from T-90s to T-60s.
pub fn time_adjusted_half_spread(
    base_half_spread: Decimal,
    seconds_remaining: i64,
) -> Option<Decimal> {
    if seconds_remaining <= STOP_QUOTING_AT_SEC {
        return None; // Stop quoting.
    }
    if seconds_remaining <= WIDEN_SPREAD_AT_SEC {
        // Linear scale: 1× at T-90s → 2× at T-60s (consistent with stop-quoting boundary).
        let range = (WIDEN_SPREAD_AT_SEC - STOP_QUOTING_AT_SEC) as f64;
        let progress = ((WIDEN_SPREAD_AT_SEC - seconds_remaining) as f64) / range;
        let scale = Decimal::try_from(1.0 + progress).unwrap_or(dec!(2));
        return Some(base_half_spread * scale);
    }
    Some(base_half_spread)
}

// ---------------------------------------------------------------------------
// One-side stop logic
// ---------------------------------------------------------------------------

/// Returns `(post_bid, post_ask)` based on inventory level.
///
/// - Below SKEW_ACTIVATION_SHARES: quote both sides normally.
/// - Above ONE_SIDE_STOP_SHARES long YES: stop posting the YES bid (no more YES buys).
/// - Above ONE_SIDE_STOP_SHARES long NO:  stop posting the NO bid (no more NO buys).
/// - Above MAX_NET_EXPOSURE_SHARES: stop both sides entirely.
pub fn one_side_decision(net_exposure: Decimal) -> (bool, bool) {
    if net_exposure.abs() >= MAX_NET_EXPOSURE_SHARES {
        return (false, false);
    }
    let post_yes_bid = net_exposure < ONE_SIDE_STOP_SHARES;
    let post_no_bid = net_exposure > -ONE_SIDE_STOP_SHARES;
    (post_yes_bid, post_no_bid)
}

// ---------------------------------------------------------------------------
// Force-flatten decision
// ---------------------------------------------------------------------------

/// Which token to sell (as a taker) to flatten the net position.
/// Never buy the opposite token — that boxes the position and locks capital.
#[derive(Debug, PartialEq)]
pub enum FlattenAction {
    /// Nothing to do — already flat.
    Flat,
    /// Sell `amount` shares of the YES token.
    SellYes(Decimal),
    /// Sell `amount` shares of the NO token.
    SellNo(Decimal),
}

/// Determine how to flatten net inventory.
/// Call `try_merge` first to reduce taker fee exposure on the overlapping portion.
pub fn flatten_action(yes_shares: Decimal, no_shares: Decimal) -> FlattenAction {
    let net = yes_shares - no_shares;
    if net > dec!(0) {
        FlattenAction::SellYes(net)
    } else if net < dec!(0) {
        FlattenAction::SellNo(net.abs())
    } else {
        FlattenAction::Flat
    }
}

// ---------------------------------------------------------------------------
// News / high-risk time — uses externally managed halt schedule
// ---------------------------------------------------------------------------

/// Returns true if the current time falls inside any halt window.
/// Halt windows are loaded once at startup from a calendar feed (see `news_halt`
/// module) and passed in here to avoid hitting the API in the hot loop.
pub fn is_in_halt_window(halt_schedule: &[(u64, u64)]) -> bool {
    let now_secs = now_ms() / 1000;
    halt_schedule.iter().any(|&(start, end)| now_secs >= start && now_secs <= end)
}

/// Conservative fallback: pause during XX:25–XX:35 UTC when the calendar feed
/// is unavailable. Covers most US macro release windows.
pub fn is_in_conservative_halt() -> bool {
    let now_secs = now_ms() / 1000;
    let minute = ((now_secs % 3600) / 60) as u32;
    minute >= 25 && minute <= 35
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kill_switch_daily_fires() {
        let mut ks = KillSwitch::default();
        // Keep each step below PER_WINDOW_MAX_LOSS so window kill doesn't fire first.
        // -10 then reset window, repeat until daily crosses -75.
        for _ in 0..7 {
            assert_eq!(ks.update_pnl(dec!(-10)), KillDecision::Ok);
            ks.reset_window();
        }
        // daily = -70, now push past -75.
        assert_eq!(ks.update_pnl(dec!(-6)), KillDecision::KillDay);
        assert!(ks.daily_killed);
    }

    #[test]
    fn kill_switch_window_fires_then_resets() {
        let mut ks = KillSwitch::default();
        // PER_WINDOW_MAX_LOSS_USDC = 30
        assert_eq!(ks.update_pnl(dec!(-31)), KillDecision::KillWindow);
        ks.reset_window();
        // After reset, window_pnl = 0, small loss should be Ok.
        assert_eq!(ks.update_pnl(dec!(-1)), KillDecision::Ok);
    }

    #[test]
    fn one_side_stop_long_yes() {
        assert_eq!(one_side_decision(dec!(160)), (false, true));
        assert_eq!(one_side_decision(dec!(-160)), (true, false));
        assert_eq!(one_side_decision(dec!(310)), (false, false));
        assert_eq!(one_side_decision(dec!(30)), (true, true));
    }

    #[test]
    fn spread_widens_near_close() {
        let half = dec!(0.01);
        // Widen window: 90s → 60s, scale 1× → 2×
        let widened = time_adjusted_half_spread(half, 75).unwrap();
        assert!(widened > half && widened <= half * dec!(2), "should widen at T-75s: got {}", widened);
        // Stop boundary
        assert_eq!(time_adjusted_half_spread(half, 60), None, "should stop at T-60s");
        assert_eq!(time_adjusted_half_spread(half, 30), None, "should stop at T-30s");
        // Normal
        assert_eq!(time_adjusted_half_spread(half, 200), Some(half));
    }

    #[test]
    fn flatten_action_correct() {
        assert_eq!(flatten_action(dec!(200), dec!(50)), FlattenAction::SellYes(dec!(150)));
        assert_eq!(flatten_action(dec!(50), dec!(200)), FlattenAction::SellNo(dec!(150)));
        assert_eq!(flatten_action(dec!(100), dec!(100)), FlattenAction::Flat);
    }
}
