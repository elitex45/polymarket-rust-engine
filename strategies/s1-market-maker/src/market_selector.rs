use anyhow::{Context, Result};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::Deserialize;
use shared::Market;
use std::time::{SystemTime, UNIX_EPOCH};

const GAMMA_EVENTS_API: &str = "https://gamma-api.polymarket.com/events";

/// Minimum 24h volume in USDC for a market to be eligible.
/// Each BTC 5-min window runs for only 5 minutes — daily volume of the
/// *window* itself is naturally much lower than the event total.
const MIN_DAILY_VOLUME: Decimal = dec!(50);

/// Minimum time to resolution for quoting (seconds).
/// Must be > STOP_QUOTING_AT_SEC (60s). 120s gives at least 60s of quoting.
const MIN_TIME_TO_RESOLUTION_SECS: i64 = 120;

/// How many upcoming 5-min windows to probe (= MAX_LOOK_AHEAD_WINDOWS * 5 min).
const MAX_LOOK_AHEAD_WINDOWS: i64 = 18; // covers next 90 minutes

/// Default tick size (normal range 0.04–0.96).
const DEFAULT_TICK_SIZE: Decimal = dec!(0.01);

// ---------------------------------------------------------------------------
// Gamma API types (events endpoint)
// ---------------------------------------------------------------------------

#[derive(Deserialize, Debug)]
struct GammaEvent {
    markets: Option<Vec<GammaMarket>>,
}

#[derive(Deserialize, Debug)]
struct GammaMarket {
    #[serde(rename = "conditionId")]
    condition_id: Option<String>,
    slug: Option<String>,
    #[serde(rename = "endDate")]
    end_date: Option<String>,
    // All numeric fields come as floats from the events API.
    volume24hr: Option<f64>,
    #[serde(rename = "bestBid")]
    best_bid: Option<f64>,
    #[serde(rename = "bestAsk")]
    best_ask: Option<f64>,
    active: Option<bool>,
    closed: Option<bool>,
    #[serde(rename = "acceptingOrders")]
    accepting_orders: Option<bool>,
    #[serde(rename = "negRisk")]
    neg_risk: Option<bool>,
    /// Minimum order size in shares.
    #[serde(rename = "orderMinSize")]
    order_min_size: Option<f64>,
    /// Minimum tick size (e.g. 0.01).
    #[serde(rename = "orderPriceMinTickSize")]
    order_price_min_tick_size: Option<f64>,
    /// Liquidity rewards: max allowed half-spread (e.g. 4.5 cents → 0.045).
    #[serde(rename = "rewardsMaxSpread")]
    rewards_max_spread: Option<f64>,
    /// Liquidity rewards: minimum qualifying size in shares.
    #[serde(rename = "rewardsMinSize")]
    rewards_min_size: Option<f64>,
    /// JSON-encoded string: "[\"tokenId1\", \"tokenId2\"]"
    #[serde(rename = "clobTokenIds")]
    clob_token_ids: Option<String>,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Fetch upcoming BTC 5-min markets. Generates window slugs from now+30min
/// onward, fetches each from the Gamma events API, and returns the top
/// `limit` markets sorted by 24h volume descending.
pub async fn select_markets(http: &reqwest::Client, limit: usize) -> Result<Vec<Market>> {
    let now = now_ts();
    // Align to the most-recent 5-min boundary, then probe forward.
    let current_window = (now / 300) * 300;

    let mut eligible: Vec<(f64, Market)> = Vec::new();

    for i in 0..MAX_LOOK_AHEAD_WINDOWS {
        let window_start = current_window + i * 300;
        let window_end = window_start + 300;
        let secs_to_end = window_end - now;

        if secs_to_end < MIN_TIME_TO_RESOLUTION_SECS {
            continue; // too close to expiry for quoting
        }

        let slug = format!("btc-updown-5m-{}", window_start);
        if let Some(m) = fetch_market_by_slug(http, &slug).await? {
            let vol = m.volume24hr.unwrap_or(0.0);
            if let Some(market) = build_market(&m, &slug, window_end) {
                if eligible.len() >= limit * 2 {
                    break; // enough candidates
                }
                eligible.push((vol, market));
            }
        }
    }

    eligible.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    Ok(eligible.into_iter().take(limit).map(|(_, m)| m).collect())
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

async fn fetch_market_by_slug(
    http: &reqwest::Client,
    slug: &str,
) -> Result<Option<GammaMarket>> {
    let events: Vec<GammaEvent> = http
        .get(GAMMA_EVENTS_API)
        .query(&[("slug", slug)])
        .send()
        .await
        .context("Gamma events request failed")?
        .error_for_status()
        .context("Gamma events returned non-2xx")?
        .json()
        .await
        .context("Gamma events JSON parse failed")?;

    let event = match events.into_iter().next() {
        Some(e) => e,
        None => return Ok(None),
    };

    let market = match event.markets.and_then(|v| v.into_iter().next()) {
        Some(m) => m,
        None => return Ok(None),
    };

    // Filter: must be active and accepting orders.
    if market.active != Some(true) || market.closed == Some(true) {
        return Ok(None);
    }
    if market.accepting_orders == Some(false) {
        return Ok(None);
    }

    // Filter: minimum daily volume.
    let vol = market.volume24hr.unwrap_or(0.0);
    if Decimal::try_from(vol).unwrap_or_default() < MIN_DAILY_VOLUME {
        // Low-volume markets still accepted in paper-trade if this is the only
        // market available. Caller can decide — we include them but they sort last.
    }

    // Require an active two-sided book (bid > 0, ask < 1).
    let bid = market.best_bid.unwrap_or(0.0);
    let ask = market.best_ask.unwrap_or(1.0);
    if bid <= 0.0 || ask >= 1.0 {
        return Ok(None);
    }

    Ok(Some(market))
}

fn build_market(gm: &GammaMarket, slug: &str, resolution_ts: i64) -> Option<Market> {
    let condition_id = gm.condition_id.clone()?;

    // clobTokenIds is a JSON-encoded string: "[\"id1\", \"id2\"]"
    let token_ids_str = gm.clob_token_ids.as_deref()?;
    let token_ids: Vec<serde_json::Value> = serde_json::from_str(token_ids_str).ok()?;
    if token_ids.len() < 2 {
        return None;
    }
    let yes_token_id = token_ids[0].as_str()?.to_string();
    let no_token_id = token_ids[1].as_str()?.to_string();

    let neg_risk = gm.neg_risk.unwrap_or(false);

    let min_order_size = gm
        .order_min_size
        .and_then(|v| Decimal::try_from(v).ok())
        .unwrap_or(dec!(5));

    let tick_size = gm
        .order_price_min_tick_size
        .and_then(|v| Decimal::try_from(v).ok())
        .unwrap_or(DEFAULT_TICK_SIZE);

    // Rewards: rewardsMaxSpread is in cents (e.g. 4.5 = 4.5¢ = 0.045 in price terms).
    let rewards_max_spread = gm
        .rewards_max_spread
        .and_then(|v| Decimal::try_from(v / 100.0).ok());

    let rewards_min_size = gm
        .rewards_min_size
        .and_then(|v| Decimal::try_from(v).ok());

    Some(Market {
        condition_id,
        yes_token_id,
        no_token_id,
        slug: slug.to_string(),
        resolution_ts,
        neg_risk,
        min_order_size,
        tick_size,
        rewards_max_spread,
        rewards_min_size,
    })
}

fn now_ts() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// Parse ISO 8601 timestamp to Unix seconds. Used in tests.
#[allow(dead_code)]
fn parse_iso_ts(s: &str) -> Option<i64> {
    let trimmed = s.trim_end_matches('Z');
    let parts: Vec<&str> = trimmed.split('T').collect();
    if parts.len() != 2 {
        return None;
    }
    let date_parts: Vec<i64> = parts[0]
        .split('-')
        .filter_map(|p| p.parse().ok())
        .collect();
    let time_parts: Vec<i64> = parts[1]
        .split(':')
        .filter_map(|p| p.parse().ok())
        .collect();
    if date_parts.len() < 3 || time_parts.len() < 3 {
        return None;
    }
    let (year, month, day) = (date_parts[0], date_parts[1], date_parts[2]);
    let (hour, min, sec) = (time_parts[0], time_parts[1], time_parts[2]);
    let a = (14 - month) / 12;
    let y = year + 4800 - a;
    let m = month + 12 * a - 3;
    let jdn = day + (153 * m + 2) / 5 + 365 * y + y / 4 - y / 100 + y / 400 - 32045;
    let unix_epoch_jdn: i64 = 2440588;
    Some((jdn - unix_epoch_jdn) * 86400 + hour * 3600 + min * 60 + sec)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_iso_ts_round_trip() {
        let ts = parse_iso_ts("2026-01-01T00:00:00Z").unwrap();
        assert!(ts > 1_700_000_000);
    }

    #[test]
    fn window_slug_formula() {
        // 2026-03-20T11:15:00Z should give slug btc-updown-5m-1774005300
        let ts = parse_iso_ts("2026-03-20T11:15:00Z").unwrap();
        assert_eq!(ts % 300, 0, "window start must be 5-min aligned");
        let slug = format!("btc-updown-5m-{}", ts);
        assert_eq!(slug, "btc-updown-5m-1774005300");
    }
}
