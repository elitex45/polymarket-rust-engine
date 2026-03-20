mod fair_value;
mod inventory;
mod logger;
mod market_selector;
mod news_halt;
mod risk;
mod stoikov;

use anyhow::{Context, Result};
use execution::{
    cancel::batch_cancel,
    client::ClobClient,
    fee_rate::FeeRateCache,
    orders::{post_maker_limit, OrderId},
    rate_limiter::RateLimiter,
    websocket::WsManager,
};
use fair_value::{blended_fair_value, fair_value_yes, VolEstimator};
use inventory::InventoryManager;
use logger::{PositionLogger, TradeLogger, WindowPnlLogger};
use risk::{
    flatten_action, is_in_conservative_halt, is_in_halt_window, one_side_decision,
    time_adjusted_half_spread, FlattenAction, KillDecision, KillSwitch, UnhedgedTimer,
};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use shared::{Market, Side};
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{info, warn};

// ---------------------------------------------------------------------------
// Environment variable names
// ---------------------------------------------------------------------------
const ENV_PRIVATE_KEY: &str = "POLYMARKET_PRIVATE_KEY";
const ENV_PAPER_TRADE: &str = "PAPER_TRADE";
const ENV_MAX_EXPOSURE: &str = "MAX_EXPOSURE_USDC";
const ENV_RISK_AVERSION: &str = "RISK_AVERSION";
const ENV_ORDER_SIZE: &str = "ORDER_SIZE_USDC";
const ENV_POLY_WS_URL: &str = "POLY_WS_URL";
const ENV_POLY_API_KEY: &str = "POLY_API_KEY";
const ENV_POLY_API_SECRET: &str = "POLY_API_SECRET";
const ENV_POLY_API_PASS: &str = "POLY_API_PASSPHRASE";

// ---------------------------------------------------------------------------
// Strategy constants
// ---------------------------------------------------------------------------

/// Price-move threshold (in USDC) that triggers a cancel+replace cycle.
const REQUOTE_THRESHOLD: Decimal = dec!(0.005);

/// How many markets to trade simultaneously.
const MAX_MARKETS: usize = 3;

/// How often to refresh the market list (seconds).
const MARKET_REFRESH_SECS: u64 = 300;

/// Drop a market from rotation when fewer than this many seconds remain.
/// For 5-min windows (300s total), drop at STOP_QUOTING_AT_SEC + small buffer.
const MARKET_EXPIRY_BUFFER_SECS: i64 = 65;

/// How often to check unhedged timers and log position snapshots (seconds).
const HEARTBEAT_SECS: u64 = 5;

/// Window length for 5-minute markets (seconds).
const WINDOW_SECS: f64 = 300.0;

/// Skip quoting when BTC delta from window open is within ±this % of the strike.
/// Oracle basis risk (Binance vs Chainlink) exceeds model edge in this band.
const ORACLE_BASIS_GUARD_PCT: f64 = 0.03;

// ---------------------------------------------------------------------------
// Per-market runtime state
// ---------------------------------------------------------------------------

struct ActiveQuote {
    yes_order_id: Option<OrderId>,
    no_order_id: Option<OrderId>,
    bid: Decimal,
    ask: Decimal,
}

/// State tracked per market throughout its lifetime.
struct MarketState {
    inventory: InventoryManager,
    unhedged_timer: UnhedgedTimer,
    kill_switch: KillSwitch,
    /// BTC price at window open (= resolution_ts - 300s). Used for fair value.
    window_open_btc: Option<f64>,
    /// Running fill count for adverse selection tracking.
    fill_count: u32,
    /// Estimated adverse fills (BTC moved against us within 30s of fill).
    adverse_fill_count: u32,
    /// Running spread profit for the current window.
    spread_profit: Decimal,
    /// Vol paused flag for this market.
    vol_paused: bool,
}

impl MarketState {
    fn new(max_exposure: Decimal) -> Self {
        Self {
            inventory: InventoryManager::new(max_exposure),
            unhedged_timer: UnhedgedTimer::new(dec!(50)),
            kill_switch: KillSwitch::default(),
            window_open_btc: None,
            fill_count: 0,
            adverse_fill_count: 0,
            spread_profit: dec!(0),
            vol_paused: false,
        }
    }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let _ = dotenvy::dotenv();

    let paper_trade = std::env::var(ENV_PAPER_TRADE)
        .map(|v| v.to_lowercase() == "true")
        .unwrap_or(true);

    if paper_trade {
        info!("*** PAPER TRADE MODE — no real orders will be submitted ***");
    } else {
        info!("*** LIVE MODE ***");
    }

    let private_key = std::env::var(ENV_PRIVATE_KEY)
        .context("POLYMARKET_PRIVATE_KEY env var not set")?;
    let max_exposure: Decimal = std::env::var(ENV_MAX_EXPOSURE)
        .unwrap_or_else(|_| "200".to_string())
        .parse()
        .context("MAX_EXPOSURE_USDC must be a number")?;
    let gamma: Decimal = std::env::var(ENV_RISK_AVERSION)
        .unwrap_or_else(|_| "0.5".to_string())
        .parse()
        .context("RISK_AVERSION must be a number")?;
    let order_size: Decimal = std::env::var(ENV_ORDER_SIZE)
        .unwrap_or_else(|_| "5".to_string())
        .parse()
        .context("ORDER_SIZE_USDC must be a number")?;

    let poly_ws_url = std::env::var(ENV_POLY_WS_URL)
        .unwrap_or_else(|_| "wss://ws-subscriptions-clob.polymarket.com/ws/market".to_string());
    let api_key = std::env::var(ENV_POLY_API_KEY).unwrap_or_default();
    let api_secret = std::env::var(ENV_POLY_API_SECRET).unwrap_or_default();
    let api_passphrase = std::env::var(ENV_POLY_API_PASS).unwrap_or_default();

    let clob_client = ClobClient::new(&private_key, 137)?;
    let fee_cache = FeeRateCache::new(
        clob_client.http().clone(),
        clob_client.clob_base().to_string(),
    );
    let http = clob_client.http().clone();

    // Fetch news halt schedule from ForexFactory (high-impact USD events today).
    // Falls back to empty schedule — conservative_halt() covers the fallback.
    info!("Fetching news halt schedule...");
    let halt_schedule = news_halt::fetch_halt_schedule(&http).await.unwrap_or_else(|e| {
        warn!(error = %e, "news halt fetch failed — using conservative fallback only");
        vec![]
    });
    info!(event_count = halt_schedule.len(), "halt schedule loaded");

    // CLOB order heartbeat: keeps resting orders alive on the server side.
    // Must fire every <10s; we use 8s to stay within the buffer.
    {
        let hb_clob_base = clob_client.clob_base().to_string();
        let hb_http = clob_client.http().clone();
        let hb_api_key = api_key.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(
                tokio::time::Duration::from_secs(8)
            );
            loop {
                interval.tick().await;
                let url = format!("{}/heartbeat", hb_clob_base);
                if let Err(e) = hb_http.post(&url)
                    .header("POLY_API_KEY", &hb_api_key)
                    .send()
                    .await
                {
                    tracing::warn!(error = %e, "CLOB order heartbeat failed");
                }
            }
        });
    }

    // Rate limiter: stay under 60 cancel/place requests per minute.
    let rate_limiter = RateLimiter::new(60);

    // Loggers — one set per day, written to strategies/s1-market-maker/data/
    let trade_log = TradeLogger::new()?;
    let pnl_log = WindowPnlLogger::new()?;
    let pos_log = PositionLogger::new()?;

    // Select initial markets.
    info!("Selecting markets from Gamma API...");
    let markets = market_selector::select_markets(&http, MAX_MARKETS)
        .await
        .context("market selection failed")?;
    if markets.is_empty() {
        warn!("No eligible markets found. Exiting.");
        return Ok(());
    }
    log_markets(&markets);

    let condition_ids: Vec<String> = markets.iter().map(|m| m.condition_id.clone()).collect();
    let token_ids: Vec<String> = markets
        .iter()
        .flat_map(|m| vec![m.yes_token_id.clone(), m.no_token_id.clone()])
        .collect();
    let (_ws_mgr, mut rx) = WsManager::start(
        poly_ws_url,
        api_key,
        api_secret,
        api_passphrase,
        condition_ids,
        token_ids,
    )?;

    let mut active_markets: Vec<Market> = markets;
    let mut states: HashMap<String, MarketState> = active_markets
        .iter()
        .map(|m| (m.condition_id.clone(), MarketState::new(max_exposure)))
        .collect();
    let mut active_quotes: HashMap<String, ActiveQuote> = HashMap::new();

    let mut vol_estimator = VolEstimator::new();

    let mut market_refresh = tokio::time::interval(
        tokio::time::Duration::from_secs(MARKET_REFRESH_SECS)
    );
    market_refresh.tick().await; // consume immediate first tick

    let mut heartbeat = tokio::time::interval(
        tokio::time::Duration::from_secs(HEARTBEAT_SECS)
    );

    info!("Event loop started. Ctrl+C to stop.");

    loop {
        tokio::select! {
            // ----------------------------------------------------------------
            // Market rotation: every 5 minutes
            // ----------------------------------------------------------------
            _ = market_refresh.tick() => {
                rotate_markets(
                    &http, &clob_client, &mut active_markets,
                    &mut states, &mut active_quotes,
                    max_exposure, paper_trade,
                ).await;
            }

            // ----------------------------------------------------------------
            // Heartbeat: check risk timers, log snapshots
            // ----------------------------------------------------------------
            _ = heartbeat.tick() => {
                let now = now_ts();
                let latest_btc = vol_estimator.latest_price().unwrap_or(0.0);
                let vol_1min_pct = vol_estimator.vol_1min_annualized_pct();
                let high_risk = is_in_halt_window(&halt_schedule) || is_in_conservative_halt();

                for market in &active_markets {
                    let cid = &market.condition_id;
                    let state = match states.get_mut(cid) { Some(s) => s, None => continue };
                    let net_exp = state.inventory.state().net_directional_exposure();
                    let secs_remaining = market.resolution_ts - now;

                    // Volatility pause / resume.
                    if !state.vol_paused && vol_1min_pct > risk::VOL_PAUSE_THRESHOLD_PCT {
                        warn!(market = %market.slug, vol = vol_1min_pct, "vol pause triggered");
                        state.vol_paused = true;
                        cancel_all_for_market(&clob_client, cid, &mut active_quotes, paper_trade).await;
                    } else if state.vol_paused && vol_1min_pct < risk::VOL_RESUME_THRESHOLD_PCT {
                        info!(market = %market.slug, "vol resume");
                        state.vol_paused = false;
                    }

                    // News / high-risk time pause.
                    if high_risk {
                        cancel_all_for_market(&clob_client, cid, &mut active_quotes, paper_trade).await;
                    }

                    // Unhedged hold timer — force-flatten via sell of excess token.
                    if state.unhedged_timer.update(net_exp) {
                        let s = state.inventory.state();
                        let action = flatten_action(s.yes_shares, s.no_shares);
                        match &action {
                            FlattenAction::Flat => {}
                            FlattenAction::SellYes(amount) => {
                                warn!(
                                    market = %market.slug,
                                    amount = %amount,
                                    paper = paper_trade,
                                    "force-flatten: SELL YES (unhedged timer expired)"
                                );
                                // TODO: post_taker_fok(yes_token_id, SELL, amount) in live mode
                            }
                            FlattenAction::SellNo(amount) => {
                                warn!(
                                    market = %market.slug,
                                    amount = %amount,
                                    paper = paper_trade,
                                    "force-flatten: SELL NO (unhedged timer expired)"
                                );
                                // TODO: post_taker_fok(no_token_id, SELL, amount) in live mode
                            }
                        }
                        state.unhedged_timer.reset();
                    }

                    // Position snapshot log every heartbeat.
                    if latest_btc > 0.0 {
                        let fv = compute_fair_value(market, &vol_estimator, latest_btc);
                        let fv_dec = Decimal::try_from(fv).unwrap_or(dec!(0.5));
                        let s = state.inventory.state();
                        let _ = pos_log.record_snapshot(
                            &market.slug,
                            s.yes_shares, s.no_shares,
                            s.usdc_spent, s.usdc_collected,
                            net_exp,
                            latest_btc,
                            fv_dec,
                        );
                    }

                    // Log window PnL and reset at window close.
                    if secs_remaining <= 0 {
                        let _ = pnl_log.record_window(
                            cid, &market.slug, market.resolution_ts,
                            state.spread_profit, dec!(0), // settlement_pnl tracked separately
                            state.fill_count, state.adverse_fill_count,
                            state.kill_switch.daily_pnl(),
                        );
                        state.spread_profit = dec!(0);
                        state.fill_count = 0;
                        state.adverse_fill_count = 0;
                        state.kill_switch.reset_window();
                    }
                }
            }

            // ----------------------------------------------------------------
            // BTC price tick from Binance
            // ----------------------------------------------------------------
            Ok(price_tick) = rx.btc_price_rx.recv() => {
                let btc_f64 = price_tick.price.to_f64().unwrap_or(0.0);
                tracing::debug!(price = %price_tick.price, ts = price_tick.timestamp_ms, "BTC tick");
                vol_estimator.update(btc_f64, price_tick.timestamp_ms);

                // Seed window_open_btc for any market whose window just opened.
                let markets_for_open: Vec<Market> = active_markets.clone();
                for market in &markets_for_open {
                    let state = match states.get_mut(&market.condition_id) {
                        Some(s) => s, None => continue
                    };
                    if state.window_open_btc.is_none() {
                        let window_open_ts_ms = ((market.resolution_ts - 300) * 1000) as u64;
                        state.window_open_btc = vol_estimator
                            .price_at(window_open_ts_ms)
                            .or_else(|| vol_estimator.latest_price());
                        if let Some(open_price) = state.window_open_btc {
                            tracing::debug!(
                                market = %market.slug,
                                open_price,
                                "window_open_btc set"
                            );
                        }
                    }
                }

                // Re-quote all active markets on BTC price move.
                // This is the primary trigger for low-liquidity markets
                // where orderbook price_changes are infrequent.
                let markets_to_quote: Vec<Market> = active_markets.clone();
                for market in &markets_to_quote {
                    let cid = market.condition_id.clone();
                    let state = match states.get_mut(&cid) { Some(s) => s, None => continue };
                    try_requote_market(
                        market, state, &vol_estimator, &mut active_quotes,
                        &clob_client, &fee_cache, &rate_limiter, &halt_schedule,
                        gamma, order_size, paper_trade, dec!(0.5),
                    ).await;
                }
            }

            // ----------------------------------------------------------------
            // Orderbook update from Polymarket WS
            // ----------------------------------------------------------------
            Ok(book_update) = rx.book_update_rx.recv() => {
                tracing::debug!(
                    asset_id = %book_update.asset_id,
                    side = %book_update.side,
                    price = %book_update.price,
                    "book_update received"
                );
                let market = match active_markets.iter().find(|m| {
                    m.yes_token_id == book_update.asset_id || m.no_token_id == book_update.asset_id
                }) {
                    Some(m) => m.clone(),
                    None => {
                        tracing::debug!(asset_id = %book_update.asset_id, "book_update: no matching market");
                        continue;
                    }
                };
                let cid = market.condition_id.clone();
                let state = match states.get_mut(&cid) { Some(s) => s, None => continue };
                try_requote_market(
                    &market, state, &vol_estimator, &mut active_quotes,
                    &clob_client, &fee_cache, &rate_limiter, &halt_schedule,
                    gamma, order_size, paper_trade, book_update.price,
                ).await;
            }

            // ----------------------------------------------------------------
            // Fill notification from Polymarket user WS
            // ----------------------------------------------------------------
            Ok(fill) = rx.fill_event_rx.recv() => {
                let market = match active_markets.iter().find(|m| {
                    m.yes_token_id == fill.token_id || m.no_token_id == fill.token_id
                }) {
                    Some(m) => m.clone(),
                    None => continue,
                };

                let cid = market.condition_id.clone();
                let state = match states.get_mut(&cid) { Some(s) => s, None => continue };

                let latest_btc = vol_estimator.latest_price().unwrap_or(0.0);
                let fv = compute_fair_value(&market, &vol_estimator, latest_btc);
                let fv_dec = Decimal::try_from(fv).unwrap_or(dec!(0.5));
                let side_str = if fill.side == Side::Yes { "YES" } else { "NO" };
                let token_label = if fill.token_id == market.yes_token_id { "YES" } else { "NO" };

                info!(
                    market = %market.slug,
                    order_id = %fill.order_id,
                    side = %fill.side,
                    price = %fill.price,
                    size = %fill.size,
                    fv = %fv_dec,
                    "fill received"
                );

                let _ = trade_log.record_fill(
                    &now_iso(),
                    &market.slug,
                    &fill.order_id,
                    side_str,
                    token_label,
                    fill.price,
                    fill.size,
                    fv_dec,
                    latest_btc,
                );

                state.inventory.apply_fill(&fill);
                state.fill_count += 1;

                // Adverse selection: we always BUY from USDC (never SELL).
                // A fill is potentially adverse if we paid more than fair value
                // for the token we received.
                //   YES BUY adverse: fill price > FV  (overpaid for YES)
                //   NO  BUY adverse: fill price > (1 - FV)  (overpaid for NO)
                let is_potentially_adverse = if token_label == "YES" {
                    fill.price > fv_dec
                } else {
                    fill.price > (dec!(1) - fv_dec)
                };
                if is_potentially_adverse {
                    state.adverse_fill_count += 1;
                }

                // Update kill switch PnL estimate.
                let fill_pnl = (fv_dec - fill.price) * fill.size;
                match state.kill_switch.update_pnl(fill_pnl) {
                    KillDecision::KillDay => {
                        warn!(market = %market.slug, "DAILY KILL — cancelling all orders");
                        cancel_all_for_market(&clob_client, &cid, &mut active_quotes, paper_trade).await;
                    }
                    KillDecision::KillWindow => {
                        warn!(market = %market.slug, "WINDOW KILL — cancelling orders for this window");
                        cancel_all_for_market(&clob_client, &cid, &mut active_quotes, paper_trade).await;
                    }
                    KillDecision::Ok => {}
                }

                // Attempt to merge YES+NO pairs.
                let mergeable = {
                    let s = state.inventory.state();
                    s.yes_shares.min(s.no_shares)
                };
                if mergeable > dec!(0) {
                    state.spread_profit += mergeable; // $1 payout per merged pair minus costs
                    if paper_trade {
                        info!(market = %market.slug, "PAPER: merge {} YES+NO pairs", mergeable);
                    } else {
                        rate_limiter.acquire().await;
                        if let Err(e) = execution::merge::merge_positions(&clob_client, &cid).await {
                            warn!(market = %market.slug, error = %e, "merge failed");
                        } else {
                            state.inventory.apply_merge(mergeable);
                        }
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Try to post/replace maker quotes for one market. Returns immediately if any
/// guard condition (halt, kill switch, oracle basis, T-60s) fires.
#[allow(clippy::too_many_arguments)]
async fn try_requote_market(
    market: &Market,
    state: &mut MarketState,
    vol_estimator: &VolEstimator,
    active_quotes: &mut HashMap<String, ActiveQuote>,
    clob_client: &ClobClient,
    fee_cache: &FeeRateCache,
    rate_limiter: &RateLimiter,
    halt_schedule: &[(u64, u64)],
    gamma: Decimal,
    order_size: Decimal,
    paper_trade: bool,
    fv_fallback: Decimal,
) {
    let cid = &market.condition_id;

    if state.kill_switch.daily_killed {
        return;
    }
    if state.vol_paused || is_in_halt_window(halt_schedule) || is_in_conservative_halt() {
        return;
    }

    let secs_remaining = market.resolution_ts - now_ts();
    let latest_btc = match vol_estimator.latest_price() {
        Some(p) => p,
        None => return,
    };

    if let Some(window_open_btc) = state.window_open_btc {
        if window_open_btc > 0.0 {
            let delta_pct = (latest_btc - window_open_btc).abs() / window_open_btc * 100.0;
            if delta_pct < ORACLE_BASIS_GUARD_PCT {
                tracing::debug!(
                    market = %market.slug, delta_pct,
                    "oracle basis guard: skip (within ±0.03% of open)"
                );
                return;
            }
        }
    }

    let fv = compute_fair_value(market, vol_estimator, latest_btc);
    let fv_dec = Decimal::try_from(fv).unwrap_or(fv_fallback);

    let base_half_spread = {
        let skew = state.inventory.inventory_skew();
        let sigma = Decimal::try_from(vol_estimator.annualized_vol() / 100.0)
            .unwrap_or(dec!(0.008));
        let (_, ask) = stoikov::compute_quotes(fv_dec, sigma, gamma, skew);
        (ask - fv_dec).max(stoikov::MIN_SPREAD / dec!(2))
    };

    let half_spread = match time_adjusted_half_spread(base_half_spread, secs_remaining) {
        Some(h) => h,
        None => {
            cancel_all_for_market(clob_client, cid, active_quotes, paper_trade).await;
            return;
        }
    };

    let skew = state.inventory.inventory_skew();
    let sigma = Decimal::try_from(vol_estimator.annualized_vol() / 100.0)
        .unwrap_or(dec!(0.008));
    let new_bid = fv_dec - half_spread;
    let new_ask = fv_dec + half_spread;
    let _ = stoikov::compute_quotes(fv_dec, sigma, gamma, skew);

    let new_bid = stoikov::round_to_tick(new_bid, market.tick_size);
    let new_no_bid = stoikov::round_to_tick(dec!(1) - new_ask, market.tick_size);

    let (post_yes, post_no) =
        one_side_decision(state.inventory.state().net_directional_exposure());
    if !post_yes && !post_no {
        cancel_all_for_market(clob_client, cid, active_quotes, paper_trade).await;
        return;
    }

    let should_requote = match active_quotes.get(cid) {
        Some(q) => stoikov::quotes_moved(q.bid, q.ask, new_bid, new_ask, REQUOTE_THRESHOLD),
        None => post_yes || post_no,
    };
    if !should_requote {
        return;
    }

    let effective_size = if order_size < market.min_order_size {
        warn!(
            market = %market.slug,
            order_size = %order_size,
            min = %market.min_order_size,
            "order_size below market minimum — skipping"
        );
        return;
    } else {
        order_size
    };

    if let Some(q) = active_quotes.get(cid) {
        let ids: Vec<OrderId> = [q.yes_order_id.clone(), q.no_order_id.clone()]
            .into_iter()
            .flatten()
            .collect();
        if !ids.is_empty() {
            if paper_trade {
                info!(market = %market.slug, "PAPER: cancel {} resting orders", ids.len());
            } else {
                rate_limiter.acquire().await;
                let _ = batch_cancel(clob_client, &ids).await;
            }
        }
    }

    if let Some(ms) = market.rewards_max_spread {
        if !stoikov::qualifies_for_rebate(
            new_bid, new_no_bid, fv_dec,
            Some(ms), market.rewards_min_size, effective_size,
        ) {
            tracing::debug!(market = %market.slug, "quotes outside rebate window");
        }
    }

    let (yes_oid, no_oid) = if paper_trade {
        info!(
            market = %market.slug,
            yes_bid = %new_bid,
            no_bid = %new_no_bid,
            fv = %fv_dec,
            secs_left = secs_remaining,
            vol_pct = vol_estimator.vol_1min_annualized_pct(),
            "PAPER: BUY YES@{} / BUY NO@{} size={}",
            new_bid, new_no_bid, effective_size
        );
        (None, None)
    } else {
        let mut yes_oid = None;
        let mut no_oid = None;
        if post_yes {
            rate_limiter.acquire().await;
            match post_maker_limit(
                clob_client, fee_cache, &market.yes_token_id,
                Side::Yes, new_bid, effective_size, market.neg_risk,
            )
            .await
            {
                Ok(id) => yes_oid = Some(id),
                Err(e) => warn!(market = %market.slug, error = %e, "YES order rejected"),
            }
        }
        if post_no {
            rate_limiter.acquire().await;
            match post_maker_limit(
                clob_client, fee_cache, &market.no_token_id,
                Side::Yes,
                new_no_bid, effective_size, market.neg_risk,
            )
            .await
            {
                Ok(id) => no_oid = Some(id),
                Err(e) => warn!(market = %market.slug, error = %e, "NO order rejected"),
            }
        }
        (yes_oid, no_oid)
    };

    active_quotes.insert(cid.clone(), ActiveQuote {
        yes_order_id: yes_oid,
        no_order_id: no_oid,
        bid: new_bid,
        ask: new_ask,
    });
}

/// Compute blended fair value using the binary options model + Polymarket mid.
fn compute_fair_value(market: &Market, vol_est: &VolEstimator, btc_price: f64) -> f64 {
    let secs_remaining = (market.resolution_ts - now_ts()).max(0) as f64;
    let window_open_btc = vol_est
        .price_at(((market.resolution_ts - 300) * 1000) as u64)
        .or_else(|| vol_est.latest_price())
        .unwrap_or(btc_price);

    let model_fv = fair_value_yes(
        btc_price,
        window_open_btc,
        secs_remaining,
        vol_est.annualized_vol(),
    );

    // Fallback to model-only if no Polymarket mid available.
    blended_fair_value(model_fv, model_fv, secs_remaining, WINDOW_SECS)
}

async fn cancel_all_for_market(
    clob_client: &ClobClient,
    cid: &str,
    active_quotes: &mut HashMap<String, ActiveQuote>,
    paper_trade: bool,
) {
    if let Some(q) = active_quotes.remove(cid) {
        let ids: Vec<OrderId> = [q.yes_order_id, q.no_order_id].into_iter().flatten().collect();
        if !ids.is_empty() {
            if paper_trade {
                info!(%cid, "PAPER: cancel {} orders", ids.len());
            } else if let Err(e) = batch_cancel(clob_client, &ids).await {
                warn!(%cid, error = %e, "cancel failed");
            }
        }
    }
}

async fn rotate_markets(
    http: &reqwest::Client,
    clob_client: &ClobClient,
    active_markets: &mut Vec<Market>,
    states: &mut HashMap<String, MarketState>,
    active_quotes: &mut HashMap<String, ActiveQuote>,
    max_exposure: Decimal,
    paper_trade: bool,
) {
    let now = now_ts();
    let expiring: Vec<String> = active_markets
        .iter()
        .filter(|m| m.resolution_ts - now < MARKET_EXPIRY_BUFFER_SECS)
        .map(|m| m.condition_id.clone())
        .collect();

    for cid in &expiring {
        info!(%cid, "dropping expiring market");
        cancel_all_for_market(clob_client, cid, active_quotes, paper_trade).await;
        active_markets.retain(|m| &m.condition_id != cid);
    }

    if active_markets.len() < MAX_MARKETS {
        match market_selector::select_markets(http, MAX_MARKETS + expiring.len()).await {
            Ok(fresh) => {
                let known: std::collections::HashSet<String> =
                    active_markets.iter().map(|m| m.condition_id.clone()).collect();
                for m in fresh {
                    if !known.contains(&m.condition_id) {
                        info!(slug = %m.slug, "adding new market");
                        states
                            .entry(m.condition_id.clone())
                            .or_insert_with(|| MarketState::new(max_exposure));
                        active_markets.push(m);
                        if active_markets.len() >= MAX_MARKETS {
                            break;
                        }
                    }
                }
            }
            Err(e) => warn!(error = %e, "market refresh failed — keeping existing markets"),
        }
    }

    if !expiring.is_empty() {
        log_markets(active_markets);
    }
}

fn log_markets(markets: &[Market]) {
    info!(count = markets.len(), "Active markets:");
    for m in markets {
        let mins = (m.resolution_ts - now_ts()) / 60;
        info!(
            slug = %m.slug,
            neg_risk = m.neg_risk,
            tick = %m.tick_size,
            min_order = %m.min_order_size,
            "  {} ({}min left)", m.slug, mins
        );
    }
}

fn now_ts() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn now_iso() -> String {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let secs = (ms / 1000) as i64;
    let millis = ms % 1000;
    let s = secs % 60;
    let mi = (secs / 60) % 60;
    let h = (secs / 3600) % 24;
    let days = secs / 86400;
    let jdn = days + 2440588;
    let a = jdn + 32044;
    let b = (4 * a + 3) / 146097;
    let c = a - (146097 * b) / 4;
    let d_raw = (4 * c + 3) / 1461;
    let e = c - (1461 * d_raw) / 4;
    let mn = (5 * e + 2) / 153;
    let day = e - (153 * mn + 2) / 5 + 1;
    let month = mn + 3 - 12 * (mn / 10);
    let year = 100 * b + d_raw - 4800 + mn / 10;
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z", year, month, day, h, mi, s, millis)
}
