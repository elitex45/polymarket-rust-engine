# Polymarket Bot — Status & Handoff Notes

## What Works (Verified on GCP)

### Market Detection
- Fetches upcoming BTC 5-min markets from `https://gamma-api.polymarket.com/events`
- Generates slugs: `btc-updown-5m-{window_start_unix}` (window_start = 5-min aligned Unix timestamp)
- Scans 18 windows ahead (~90 min), filters: active + accepting orders + active two-sided book
- Returns top 3 markets sorted by 24h volume
- **Confirmed working:** 3 markets found at startup, correctly rotated every 5 min

### Orderbook WebSocket (Unauthenticated)
- URL: `wss://ws-subscriptions-clob.polymarket.com/ws/market`
- Subscribe with token IDs (YES + NO) in `assets_ids` field — NOT condition IDs in `markets`
- Server sends two message formats:
  - **Snapshot** (on connect): `[{"asset_id":"...","bids":[{"price":"0.01","size":"6199"}],"asks":[...]}]` — outer array
  - **Price changes** (live): `{"market":"0x...","price_changes":[{"asset_id":"...","price":"0.49","size":"106","side":"SELL"}]}` — single object
- Both formats now parsed correctly in `execution/src/websocket/orderbook.rs`
- **Confirmed working:** Book updates flowing, `book_update received` debug logs appear

### Binance WS (BTC Price Feed)
- URL: `wss://stream.binance.us:9443/ws/btcusdt@trade` (binance.com is HTTP 451 geo-blocked on GCP)
- **Note:** Binance US has much lower volume than binance.com — trades arrive every 10-80s, not every ms
- For production on non-GCP: use `wss://stream.binance.com:9443/ws/btcusdt@trade` for higher frequency
- **Confirmed working:** BTC price ticks arriving, `vol_pct` updating (annualized %)

### Quoting Logic (Paper Trade)
- Quotes fire on every BTC price tick (not only on book updates, since book changes are rare on 5-min markets)
- Oracle basis guard: skips if |BTC_delta_from_window_open| < 0.03% (Binance vs Chainlink basis risk)
- Fair value model: binary options model blended with Avellaneda-Stoikov
- Time-adjusted spread: 1x at T>90s, 2x at T-90s to T-60s, stop quoting at T<=60s
- **Confirmed working:** `PAPER: BUY YES@X / BUY NO@Y` logs appear on each price tick after oracle guard clears

Sample output:
```
PAPER: BUY YES@0.49 / BUY NO@0.49 size=5  (fv=0.50, secs_left=672, vol_pct=26.3)
PAPER: BUY YES@0.01 / BUY NO@0.97 size=5  (fv=0.01, secs_left=72, vol_pct=53.8)
```

### CSV Logging
- Written to `strategies/s1-market-maker/data/`
- `positions_YYYYMMDD.csv`: snapshot every 5s — timestamp, slug, yes/no shares, usdc_spent/collected, net_exposure, btc_price, fair_value, unrealised_pnl
- `pnl_YYYYMMDD.csv`: window close record — condition_id, slug, spread_profit, fill_count, adverse_fills, adverse_rate_pct
- **Confirmed working:** files created and growing

### Market Rotation
- Markets are dropped when `resolution_ts - now < 65s`
- Fresh markets are fetched from Gamma API to replace them
- **Confirmed working:** slug changes visible across sessions in positions CSV

---

## What Does NOT Work on GCP

### Polymarket User WebSocket (Authenticated / Fill Notifications)
- URL: `wss://ws-subscriptions-clob.polymarket.com/ws/user`
- **Error:** `WebSocket protocol error: Connection reset without closing handshake` (~500ms after connect)
- **Root cause:** GCP IP ranges are almost certainly blocked by Polymarket's firewall/load balancer for the authenticated channel. The unauthenticated orderbook WS works fine from GCP.
- **Evidence:** TCP RST happens before any application-level auth exchange can complete — this is a network-level block, not an auth failure.
- **Impact on paper trade:** None. Paper trade generates no real fills, so fill notifications are irrelevant.
- **Impact on live trade:** Critical. Without fill notifications, inventory tracking and kill switch won't function. Must run from residential/VPS IP.

---

## How to Run

### Prerequisites
```bash
# Install Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Clone repo and cd into it
cd /path/to/polymarket
```

### Environment Setup
Create `.env` in the repo root:
```env
POLYMARKET_PRIVATE_KEY=0x<your_polygon_private_key>
POLY_API_KEY=<polymarket_clob_api_key>
POLY_API_SECRET=<polymarket_clob_api_secret>
POLY_API_PASSPHRASE=<polymarket_clob_api_passphrase>
POLYGON_RPC_URL=https://polygon-rpc.com
PAPER_TRADE=true
MAX_EXPOSURE_USDC=200
MIN_SPREAD=0.02
RISK_AVERSION=0.5
ORDER_SIZE_USDC=5
```

To get Polymarket CLOB API credentials: https://docs.polymarket.com/#authentication

### Build
```bash
cargo build --workspace
```

### Run (Paper Trade)
```bash
PAPER_TRADE=true RUST_LOG=info,s1_market_maker=debug cargo run -p s1-market-maker
```

### Run (Live — DO NOT run on GCP)
```bash
PAPER_TRADE=false RUST_LOG=info cargo run --release -p s1-market-maker
```

### Log Filtering (user WS reconnect spam removal)
```bash
PAPER_TRADE=true cargo run -p s1-market-maker 2>&1 | grep -v "user WS\|Connecting to Polymarket user"
```

---

## Pending / TODO

### High Priority (needed before live trading)

1. **Run on non-GCP IP** — Test user WS from local machine or residential VPS. Verify fill notifications arrive. The HMAC-SHA256 auth implementation is in `execution/src/websocket/user.rs` and should be correct (timestamp + "GET" + "/ws/user" signed with base64url-decoded secret).

2. **Verify user WS auth format** — If fills still don't arrive after IP change, check `build_auth()` in `execution/src/websocket/user.rs`. The Polymarket Python SDK (`py-clob-client`) is the reference. Key detail: the API secret must be base64url-decoded before use as HMAC key.

3. **Binance WS on local machine** — Switch back to `wss://stream.binance.com:9443/ws/btcusdt@trade` for much higher tick frequency (hundreds per second vs one every 10-80s on binance.us). Edit `execution/src/websocket/binance.rs` line 11.

4. **`post_taker_fok()` for force-flatten** — The unhedged timer fires after inventory is held too long, but the actual taker FOK order is stubbed with a TODO comment in `strategies/s1-market-maker/src/main.rs` (lines ~300-320). Needs implementation in `execution/src/orders.rs`.

5. **Paper trade 500-cycle validation** — Run paper trade for full 500+ cycles before going live. Watch for:
   - Adverse fill rate staying < 40% over time
   - Kill switch not triggering on normal volatility
   - Market rotation working cleanly at every 5-min boundary

### Medium Priority

6. **Trades CSV file** — The `TradeLogger` is created in `main.rs` and `record_fill()` is called on each fill, but the `trades_YYYYMMDD.csv` file is only created when fills arrive. On paper trade (no real fills), this file won't exist. Verify it appears in live mode.

7. **Oracle basis guard threshold** — 0.03% is very tight; BTC needs to move $21 from window open before any quoting starts. This causes a ~30-60s startup delay on each new window. Consider relaxing to 0.05% or removing entirely for paper trade runs.

8. **Vol estimator warm-up** — `vol_1min_annualized_pct()` starts at 0.0 until enough BTC ticks accumulate. During this period, Stoikov spread defaults to `dec!(0.008)` minimum. Acceptable but should be monitored.

9. **WS reconnect subscriptions** — When orderbook WS reconnects, it re-subscribes to the ORIGINAL token IDs from startup. If market rotation added new markets, the new markets' tokens won't be subscribed. Fix: `WsManager` needs a method to update subscriptions, or restart WS on market rotation.

### Low Priority / Future

10. **Strategy 2 (`s2-late-window`)** — Uses Binance 5-min window delta as signal to enter at T-30s to T-5s as a MAKER. Separate binary reusing the execution crate.

11. **Telegram alerts** — Add alerts for: daily kill switch triggered, unhedged timer forced-flatten, vol pause, market gaps.

12. **Settlement PnL tracking** — The `settlement_pnl` column in `pnl_YYYYMMDD.csv` is always 0. Needs a Polygon RPC call to check resolved market outcomes and credit the winning side.

---

## Key Technical Findings

### Polymarket API Quirks

| Issue | Detail |
|-------|--------|
| `feeRateBps` | Must be queried fresh before EVERY order — never hardcode. GET `/fee-rate?tokenID=...`. Stale value = silent order rejection. |
| Token IDs vs Condition IDs | Orderbook WS: use token IDs in `assets_ids`. User WS: use condition IDs in `markets`. Mixing these causes silent WS disconnect. |
| `clobTokenIds` field | Returned as a JSON-encoded STRING from the Gamma API: `"[\"id1\",\"id2\"]"`. Must double-parse. |
| WS disconnect = cancel | When Polymarket WS drops, ALL resting orders are cancelled server-side. Reconnect must re-post quotes. |
| Tick size changes | Subscribe to `tick_size_change` WS events. When price crosses 0.04 or 0.96, min tick changes and old quotes are rejected. |
| Market slugs | BTC 5-min: `btc-updown-5m-{window_start_unix}`. window_start = `(now / 300) * 300`. 288 markets/day. |
| Gamma events API | Use `/events?slug=...` not `/markets?slug=...` for BTC 5-min windows. |
| `rewardsMaxSpread` | Value from API is in CENTS (e.g. `4.5` = 4.5¢ = 0.045 in price terms). Divide by 100. |
| negRisk markets | BTC markets are NOT negRisk (`neg_risk=false`). NegRisk affects order signing — check this flag. |

### Binance WS
- `stream.binance.com` is HTTP 451 geo-blocked on GCP
- `stream.binance.us` works but has much lower volume (~1 trade/10-80s vs hundreds/sec)
- For production: use binance.com from non-cloud IP

### WS Message Formats (Polymarket orderbook)
```json
// Initial snapshot (array):
[{"market":"0x...","asset_id":"68439...","bids":[{"price":"0.48","size":"500"}],"asks":[{"price":"0.52","size":"300"}]}]

// Incremental update (single object):
{"market":"0x...","price_changes":[{"asset_id":"88529...","price":"0.49","size":"106","side":"SELL","hash":"...","best_bid":"0.48","best_ask":"0.52"}]}
```

### Architecture
```
polymarket/
├── shared/          # Types: Market, Side, OrderBook, FillEvent, InventoryState
├── execution/       # Polymarket HTTP + WS client (lib crate)
│   └── src/
│       ├── client.rs        # ClobClient (alloy signer, EIP-712)
│       ├── orders.rs        # post_maker_limit()
│       ├── cancel.rs        # batch_cancel()
│       ├── merge.rs         # CTF Exchange merge YES+NO pairs
│       ├── fee_rate.rs      # GET /fee-rate with 60s TTL cache
│       ├── rate_limiter.rs  # 60 req/min token bucket
│       └── websocket/
│           ├── binance.rs   # BTC price feed
│           ├── orderbook.rs # Polymarket market channel
│           └── user.rs      # Polymarket user channel (fills)
└── strategies/
    └── s1-market-maker/    # Binary
        └── src/
            ├── main.rs           # Event loop, paper/live toggle
            ├── stoikov.rs        # Avellaneda-Stoikov pricing
            ├── inventory.rs      # YES/NO balance tracking
            ├── fair_value.rs     # Binary options model + blending
            ├── risk.rs           # Kill switch, vol pause, halt windows
            ├── market_selector.rs # Gamma API market ranking
            ├── news_halt.rs      # ForexFactory economic calendar
            └── logger.rs         # CSV writers
```
