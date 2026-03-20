# Next Session Notes — Continue from Local Machine

## Current State (as of 2026-03-20)

Paper trade is working end-to-end on local. BTC ticks fast (binance.com), quotes firing,
CSVs writing, market rotation working. One thing broken: user WS (fill notifications).

---

## The One Thing That Needs Fixing First

### User WebSocket Auth — `execution/src/websocket/user.rs`

**Symptom:** `WebSocket protocol error: Connection reset without closing handshake` every time.
Happens on GCP AND local — so it's NOT an IP block. It's the auth message format.

**Root cause identified but NOT yet applied:**

The `AuthSubscribe` struct has an `assets_ids` field that should NOT be in the user channel
subscription. The user channel only accepts `auth`, `markets`, and `type`. Sending `assets_ids`
causes the Polymarket server to reject/reset the connection.

**The fix (apply this first):**

In `execution/src/websocket/user.rs`, remove `assets_ids` from the struct:

```rust
// BEFORE (wrong — assets_ids causes server to reset):
#[derive(Serialize)]
struct AuthSubscribe {
    auth: AuthPayload,
    markets: Vec<String>,
    assets_ids: Vec<String>,   // <-- DELETE THIS LINE
    #[serde(rename = "type")]
    sub_type: String,
}

// AFTER (correct):
#[derive(Serialize)]
struct AuthSubscribe {
    auth: AuthPayload,
    markets: Vec<String>,
    #[serde(rename = "type")]
    sub_type: String,
}
```

Also remove the `assets_ids: vec![]` line in `connect_and_stream()` where `AuthSubscribe` is constructed.

**Then rebuild and run:**
```bash
cargo build --workspace
PAPER_TRADE=true RUST_LOG=info,s1_market_maker=debug cargo run -p s1-market-maker
```

After this fix, the user WS should stay connected and you should see `Connecting to Polymarket
user WS` only ONCE at startup (no more reconnect spam).

---

## Binance URL — Already Correct for Local

On local, binance.com is accessible. The URL in `execution/src/websocket/binance.rs` is
currently set to `binance.us` (was changed for GCP). Switch it back:

```rust
// execution/src/websocket/binance.rs line ~11
const BINANCE_WS_URL: &str = "wss://stream.binance.com:9443/ws/btcusdt@trade";
```

Actually — looking at the local run output, ticks ARE coming in fast already. Check which URL
is currently set before changing.

---

## What Is Working (Confirmed on Local)

- BTC ticks: fast (many per second on local with binance.com)
- Polymarket orderbook WS: connected, book_update events flowing
- Oracle basis guard: correctly blocking and unblocking (see delta_pct in logs)
- PAPER quotes: firing on each BTC tick once oracle guard clears
  - Example: `PAPER: BUY YES@0.91 / BUY NO@0.07 fv=0.917 secs_left=295 vol_pct=16.4`
- Fair value: working correctly (FV=0.917 with 295s left and BTC well above window open)
- CSV logging: positions and PnL files written every 5s heartbeat
- Market rotation: working (markets drop at T-65s, new ones added)

---

## Known Issues / Remaining TODO

### After fixing user WS (above):

1. **Verify fills arrive** — place a real small order on Polymarket manually, see if
   `fill received` log appears. This confirms the full pipeline works.

2. **`post_taker_fok()` stub** — When unhedged timer fires (hold inventory too long without
   paired fill), it logs a warning but doesn't actually send a taker order. In `main.rs`
   around line 300-320, there are `// TODO: post_taker_fok(...)` comments. Implement or
   leave for live testing phase.

3. **WS reconnect re-subscription** — When orderbook WS reconnects, it re-subscribes to
   original token IDs from startup. New markets added via rotation won't be subscribed.
   Fix: pass a `Arc<Mutex<Vec<String>>>` of current token IDs that gets updated on rotation.

4. **Settlement PnL** — `settlement_pnl` in pnl CSV is always 0. Needs Polygon RPC call to
   check resolved market outcome. Low priority.

5. **500-cycle paper trade validation** — Run for 500+ windows (~42 hours) before live.
   Watch adverse fill rate, kill switch behavior, market rotation.

---

## Credentials in .env (confirmed correct)

```
POLY_API_KEY=019d0ac4-5bbe-74cd-9e07-5de80b518221
POLY_API_SECRET=oj2M8WQgoDEhGdkx5hnIB1boEHL4l5N3dgy2VNSUqPA=
POLY_API_PASSPHRASE=5b0becac28373aea79869b80ba8adaa7b9bfcf4cada83735517cb96e74774706
```

The HMAC auth implementation in `build_auth()` is correct:
- Secret decoded from base64 before use as HMAC key ✓
- Message = `timestamp + "GET" + "/ws/user"` ✓
- Signature = base64(HMAC-SHA256(decoded_secret, message)) ✓
- `timestamp` sent as Unix seconds string ✓

The ONLY issue was the `assets_ids` field in the subscription message.

---

## How to Run Locally

```bash
cd /path/to/polymarket
cp .env.example .env   # if .env doesn't exist — fill in credentials
cargo build --workspace
PAPER_TRADE=true RUST_LOG=info,s1_market_maker=debug cargo run -p s1-market-maker
```

Clean output (no user WS spam once auth is fixed):
```bash
PAPER_TRADE=true cargo run -p s1-market-maker 2>&1 | grep -v "user WS\|Connecting to Polymarket user"
```

## Repo
https://github.com/elitex45/polymarket-rust-engine
