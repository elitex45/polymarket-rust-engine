# Polymarket Rust Engine

Automated market-making bot for BTC 5-minute binary prediction markets on [Polymarket](https://polymarket.com). Written in Rust.

## Strategy

**Strategy 1 — Continuous Bid-Ask Spread Market Making**

Posts resting limit orders on both the YES and NO sides of upcoming BTC 5-min windows throughout their lifetime. Uses an adapted [Avellaneda-Stoikov](https://www.math.nyu.edu/~avellane/HighFrequencyTrading.pdf) model for fair value and spread calculation. Captures the bid-ask spread on round-trip fills regardless of outcome.

- Trades 3 markets simultaneously (ranked by 24h volume)
- Rotates into fresh windows as old ones expire
- Quotes skew automatically with inventory imbalance
- Pauses during high-impact USD economic events (ForexFactory calendar)
- Hard kill switch on daily/window loss limits

## Prerequisites

- Rust toolchain (`rustup` — stable channel)
- A Polygon wallet with USDC for live trading
- [Polymarket CLOB API credentials](https://docs.polymarket.com/#authentication) (for live fill notifications)

## Setup

### 1. Clone and build

```bash
git clone https://github.com/elitex45/polymarket-rust-engine.git
cd polymarket-rust-engine
cargo build --workspace
```

### 2. Configure environment

```bash
cp .env.example .env
```

Edit `.env`:

```env
# Required for live trading (signing orders on Polygon)
POLYMARKET_PRIVATE_KEY=0x<your_polygon_private_key>

# Required for fill notifications via authenticated WebSocket
POLY_API_KEY=<from Polymarket dashboard>
POLY_API_SECRET=<from Polymarket dashboard>
POLY_API_PASSPHRASE=<from Polymarket dashboard>

# Leave at defaults for first run
PAPER_TRADE=true
ORDER_SIZE_USDC=5
MAX_EXPOSURE_USDC=200
```

To get Polymarket API credentials: go to [polymarket.com](https://polymarket.com), connect your wallet, and generate L2 API keys from the account settings.

### 3. (Optional) Switch Binance feed

If running on a **non-cloud machine** (home, VPS), switch to the higher-frequency Binance global feed in `execution/src/websocket/binance.rs`:

```rust
// Line 11 — change .us to .com for ~100x more ticks/sec
const BINANCE_WS_URL: &str = "wss://stream.binance.com:9443/ws/btcusdt@trade";
```

> Note: `stream.binance.com` is geo-blocked (HTTP 451) on GCP/AWS. Use `stream.binance.us` on cloud.

## Running

### Paper trade (safe, no real orders)

```bash
PAPER_TRADE=true cargo run -p s1-market-maker
```

With debug output:

```bash
PAPER_TRADE=true RUST_LOG=info,s1_market_maker=debug cargo run -p s1-market-maker
```

### Live trading

```bash
PAPER_TRADE=false cargo run --release -p s1-market-maker
```

> Do not run live from GCP/AWS — Polymarket's authenticated WebSocket blocks cloud IP ranges. Use a residential IP or VPS outside major cloud provider ranges.

### Filtering log noise

The user WebSocket reconnect logs can be verbose. Filter them out:

```bash
PAPER_TRADE=true cargo run -p s1-market-maker 2>&1 | grep -v "user WS\|Connecting to Polymarket user"
```

## Output

Trade logs are written to `strategies/s1-market-maker/data/`:

| File | Contents |
|------|----------|
| `positions_YYYYMMDD.csv` | Position snapshot every 5s: shares, USDC, fair value, BTC price |
| `trades_YYYYMMDD.csv` | Fill record on each confirmed trade |
| `pnl_YYYYMMDD.csv` | Per-window PnL: spread profit, fill count, adverse fill rate |

## Before Going Live

Run paper trade for at least **500 cycles** (~42 hours of 5-min windows) and verify:

- [ ] Adverse fill rate stays below 40% over time
- [ ] Kill switch does not trigger on normal volatility
- [ ] Market rotation works cleanly at every 5-min boundary
- [ ] User WS stays connected and delivers fills (requires non-cloud IP)
- [ ] Position CSV shows correct inventory after simulated fills

Start live with 10% of target capital.

## Project Structure

```
polymarket/
├── shared/                        # Shared types (Market, Side, FillEvent, InventoryState)
├── execution/                     # Polymarket interaction library
│   └── src/
│       ├── client.rs              # ClobClient — EIP-712 signing via alloy
│       ├── orders.rs              # post_maker_limit()
│       ├── cancel.rs              # cancel_order(), batch_cancel()
│       ├── merge.rs               # CTF Exchange YES+NO pair merge
│       ├── fee_rate.rs            # GET /fee-rate with 60s TTL cache
│       ├── rate_limiter.rs        # 60 req/min token bucket
│       └── websocket/
│           ├── binance.rs         # BTC/USDT trade feed
│           ├── orderbook.rs       # Polymarket market channel (unauthenticated)
│           └── user.rs            # Polymarket user channel (fill notifications)
└── strategies/
    └── s1-market-maker/
        └── src/
            ├── main.rs            # Event loop, paper/live toggle
            ├── stoikov.rs         # Avellaneda-Stoikov pricing model
            ├── inventory.rs       # YES/NO balance, skew, exposure tracking
            ├── fair_value.rs      # Binary options model + mid blending
            ├── risk.rs            # Kill switch, vol pause, halt windows
            ├── market_selector.rs # Gamma API — rank BTC 5-min markets
            ├── news_halt.rs       # ForexFactory economic calendar halts
            └── logger.rs          # CSV writers
```

## Known Limitations

- **GCP/AWS**: Binance global WS is geo-blocked; use `binance.us` (much lower tick rate). Polymarket authenticated user WS is also blocked by cloud IP ranges — fill notifications won't work.
- **User WS on cloud**: Paper trade is unaffected (no real fills). Live trading requires a non-cloud IP for inventory tracking to function.
- **Binance US tick rate**: ~1 trade per 10–80s vs hundreds per second on binance.com. The strategy still works but quote latency increases.

## Status

See [STATUS.md](STATUS.md) for detailed pending items, technical findings, and handoff notes.
