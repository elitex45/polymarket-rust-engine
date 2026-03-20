use anyhow::Result;
use rust_decimal::Decimal;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Directory for CSV logs, relative to the binary's working directory.
const DATA_DIR: &str = "strategies/s1-market-maker/data";

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn today_str() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // YYYYMMDD from Unix timestamp (no external deps)
    let days = secs / 86400;
    // Julian day to Gregorian calendar
    let jdn = days as i64 + 2440588; // 2440588 = Julian day of 1970-01-01
    let a = jdn + 32044;
    let b = (4 * a + 3) / 146097;
    let c = a - (146097 * b) / 4;
    let d = (4 * c + 3) / 1461;
    let e = c - (1461 * d) / 4;
    let m = (5 * e + 2) / 153;
    let day = e - (153 * m + 2) / 5 + 1;
    let month = m + 3 - 12 * (m / 10);
    let year = 100 * b + d - 4800 + m / 10;
    format!("{:04}{:02}{:02}", year, month, day)
}

fn ensure_data_dir() -> Result<PathBuf> {
    let path = Path::new(DATA_DIR);
    fs::create_dir_all(path)?;
    Ok(path.to_path_buf())
}

fn open_csv(path: &Path, header: &str) -> Result<File> {
    let new_file = !path.exists();
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    if new_file {
        let mut f = file;
        writeln!(f, "{}", header)?;
        // Re-open in append mode (write consumed the handle).
        Ok(OpenOptions::new().append(true).open(path)?)
    } else {
        Ok(file)
    }
}

fn now_iso() -> String {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    // Format: YYYY-MM-DDTHH:MM:SS.mmmZ (manual, no external deps)
    let secs = (ms / 1000) as i64;
    let millis = ms % 1000;
    let s = secs % 60;
    let m = (secs / 60) % 60;
    let h = (secs / 3600) % 24;
    let days = secs / 86400;
    let jdn = days + 2440588;
    let a = jdn + 32044;
    let b = (4 * a + 3) / 146097;
    let c = a - (146097 * b) / 4;
    let d = (4 * c + 3) / 1461;
    let e = c - (1461 * d) / 4;
    let mn = (5 * e + 2) / 153;
    let day = e - (153 * mn + 2) / 5 + 1;
    let month = mn + 3 - 12 * (mn / 10);
    let year = 100 * b + d - 4800 + mn / 10;
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        year, month, day, h, m, s, millis
    )
}

// ---------------------------------------------------------------------------
// Trade logger — one row per fill
// ---------------------------------------------------------------------------

pub struct TradeLogger {
    path: PathBuf,
}

impl TradeLogger {
    pub fn new() -> Result<Self> {
        let dir = ensure_data_dir()?;
        let path = dir.join(format!("trades_{}.csv", today_str()));
        Ok(Self { path })
    }

    /// Record a fill event.
    ///
    /// `fair_value_at_fill` — blended FV at the moment of fill (for adverse selection scoring).
    /// `btc_price_at_fill`  — Binance BTC price at fill time.
    pub fn record_fill(
        &self,
        timestamp: &str,
        market_slug: &str,
        order_id: &str,
        side: &str,
        token: &str,   // "YES" or "NO"
        fill_price: Decimal,
        fill_size: Decimal,
        fair_value_at_fill: Decimal,
        btc_price_at_fill: f64,
    ) -> Result<()> {
        let header = "timestamp,market_slug,order_id,side,token,fill_price,fill_size,fair_value,btc_price,slippage_cents,pnl_estimate";
        let mut file = open_csv(&self.path, header)?;

        // Slippage: how far fill_price is from fair_value (negative = sold cheap)
        let slippage = (fill_price - fair_value_at_fill) * Decimal::from(100);

        // Estimated PnL contribution: for a BUY fill, we paid fill_price and expect
        // fair_value outcome → immediate mark-to-market = (fair_value - fill_price) * size
        let pnl_estimate = (fair_value_at_fill - fill_price) * fill_size;

        writeln!(
            file,
            "{},{},{},{},{},{},{},{},{:.2},{:.4},{:.4}",
            timestamp,
            market_slug,
            order_id,
            side,
            token,
            fill_price,
            fill_size,
            fair_value_at_fill,
            btc_price_at_fill,
            slippage,
            pnl_estimate,
        )?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Window PnL logger — one row per 5-min window close
// ---------------------------------------------------------------------------

pub struct WindowPnlLogger {
    path: PathBuf,
}

impl WindowPnlLogger {
    pub fn new() -> Result<Self> {
        let dir = ensure_data_dir()?;
        let path = dir.join(format!("pnl_{}.csv", today_str()));
        Ok(Self { path })
    }

    /// Record a window summary.
    ///
    /// `spread_profit`    — realised from round-trip fills (ask - bid on matched pairs).
    /// `settlement_pnl`   — from shares held at settlement (positive if won, negative if lost).
    /// `fill_count`       — total fills in the window.
    /// `adverse_fills`    — fills where BTC moved against us in the 30s after fill.
    /// `daily_cumulative` — running daily PnL.
    #[allow(clippy::too_many_arguments)]
    pub fn record_window(
        &self,
        condition_id: &str,
        slug: &str,
        resolution_ts: i64,
        spread_profit: Decimal,
        settlement_pnl: Decimal,
        fill_count: u32,
        adverse_fills: u32,
        daily_cumulative: Decimal,
    ) -> Result<()> {
        let header = "timestamp,condition_id,slug,resolution_ts,spread_profit,settlement_pnl,total_pnl,fill_count,adverse_fills,adverse_rate_pct,daily_cumulative";
        let mut file = open_csv(&self.path, header)?;

        let total_pnl = spread_profit + settlement_pnl;
        let adverse_rate = if fill_count > 0 {
            (adverse_fills as f64 / fill_count as f64) * 100.0
        } else {
            0.0
        };

        writeln!(
            file,
            "{},{},{},{},{},{},{},{},{},{:.1},{} ",
            now_iso(),
            condition_id,
            slug,
            resolution_ts,
            spread_profit,
            settlement_pnl,
            total_pnl,
            fill_count,
            adverse_fills,
            adverse_rate,
            daily_cumulative,
        )?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Position snapshot logger — periodic state dump
// ---------------------------------------------------------------------------

pub struct PositionLogger {
    path: PathBuf,
}

impl PositionLogger {
    pub fn new() -> Result<Self> {
        let dir = ensure_data_dir()?;
        let path = dir.join(format!("positions_{}.csv", today_str()));
        Ok(Self { path })
    }

    pub fn record_snapshot(
        &self,
        slug: &str,
        yes_shares: Decimal,
        no_shares: Decimal,
        usdc_spent: Decimal,
        usdc_collected: Decimal,
        net_exposure: Decimal,
        btc_price: f64,
        fair_value: Decimal,
    ) -> Result<()> {
        let header = "timestamp,slug,yes_shares,no_shares,usdc_spent,usdc_collected,net_exposure,btc_price,fair_value,unrealised_pnl";
        let mut file = open_csv(&self.path, header)?;

        // Unrealised: mark yes_shares at fair_value, no_shares at (1-fair_value)
        let unrealised = yes_shares * fair_value
            + no_shares * (Decimal::ONE - fair_value)
            - usdc_spent
            + usdc_collected;

        writeln!(
            file,
            "{},{},{},{},{},{},{},{:.2},{},{:.4}",
            now_iso(),
            slug,
            yes_shares,
            no_shares,
            usdc_spent,
            usdc_collected,
            net_exposure,
            btc_price,
            fair_value,
            unrealised,
        )?;
        Ok(())
    }
}
