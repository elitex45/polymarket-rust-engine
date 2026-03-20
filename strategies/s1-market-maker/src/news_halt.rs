use anyhow::Result;
use reqwest::Client;
use serde::Deserialize;
use std::time::{SystemTime, UNIX_EPOCH};

const FF_CALENDAR_URL: &str = "https://nfs.faireconomy.media/ff_calendar_thisweek.json";

/// Halt quoting this many seconds before a high-impact event.
const HALT_BEFORE_SECS: u64 = 300;

/// Resume quoting this many seconds after a high-impact event.
const HALT_AFTER_SECS: u64 = 900;

#[derive(Deserialize)]
struct FfEvent {
    date: Option<String>,
    impact: Option<String>,
    country: Option<String>,
}

/// Fetch this week's high-impact USD events from ForexFactory.
/// Returns (halt_start_unix_secs, halt_end_unix_secs) pairs for today only.
/// Falls back to an empty schedule on error — caller should use
/// `is_in_conservative_halt()` as a fallback.
pub async fn fetch_halt_schedule(http: &Client) -> Result<Vec<(u64, u64)>> {
    let events: Vec<FfEvent> = http
        .get(FF_CALENDAR_URL)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await?
        .json()
        .await?;

    let today_start = today_start_unix();
    let today_end = today_start + 86400;
    let mut schedule = Vec::new();

    for ev in &events {
        if ev.impact.as_deref() != Some("High") {
            continue;
        }
        if ev.country.as_deref() != Some("USD") {
            continue;
        }
        if let Some(date_str) = &ev.date {
            if let Some(event_ts) = parse_event_ts(date_str) {
                if event_ts >= today_start && event_ts < today_end {
                    let halt_start = event_ts.saturating_sub(HALT_BEFORE_SECS);
                    let halt_end = event_ts + HALT_AFTER_SECS;
                    tracing::info!(
                        event_ts,
                        halt_start,
                        halt_end,
                        "news halt window scheduled"
                    );
                    schedule.push((halt_start, halt_end));
                }
            }
        }
    }

    Ok(schedule)
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Returns Unix timestamp for the start of today (UTC midnight).
fn today_start_unix() -> u64 {
    (now_secs() / 86400) * 86400
}

/// Parse ISO-8601 datetime with optional UTC offset, e.g. "2026-03-20T13:30:00-0500".
/// Returns Unix seconds (UTC), or None if parsing fails.
fn parse_event_ts(s: &str) -> Option<u64> {
    // Minimum length: "YYYY-MM-DDTHH:MM:SS" = 19 chars
    if s.len() < 19 {
        return None;
    }

    let year: i64 = s.get(0..4)?.parse().ok()?;
    let month: i64 = s.get(5..7)?.parse().ok()?;
    let day: i64 = s.get(8..10)?.parse().ok()?;
    let hour: i64 = s.get(11..13)?.parse().ok()?;
    let min: i64 = s.get(14..16)?.parse().ok()?;
    let sec: i64 = s.get(17..19)?.parse().ok()?;

    // Parse optional timezone offset: ±HHMM or Z
    let offset_secs: i64 = if s.len() > 19 {
        let suffix = &s[19..];
        if suffix == "Z" || suffix == "z" {
            0
        } else if suffix.starts_with('+') || suffix.starts_with('-') {
            let sign: i64 = if suffix.starts_with('-') { 1 } else { -1 };
            let digits = &suffix[1..];
            let h: i64 = digits.get(..2).and_then(|d| d.parse().ok()).unwrap_or(0);
            let m: i64 = digits.get(2..4).and_then(|d| d.parse().ok()).unwrap_or(0);
            sign * (h * 3600 + m * 60)
        } else {
            0
        }
    } else {
        0
    };

    // Civil date → Unix timestamp (proleptic Gregorian, no leap-second correction).
    let a = (14 - month) / 12;
    let y = year + 4800 - a;
    let m_adj = month + 12 * a - 3;
    let jdn = day + (153 * m_adj + 2) / 5 + 365 * y + y / 4 - y / 100 + y / 400 - 32045;
    let unix_days = jdn - 2440588;
    let local_secs = unix_days * 86400 + hour * 3600 + min * 60 + sec;
    let utc_secs = local_secs + offset_secs;

    if utc_secs < 0 {
        None
    } else {
        Some(utc_secs as u64)
    }
}
