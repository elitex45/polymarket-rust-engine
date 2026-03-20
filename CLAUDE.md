# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Repository Context

This repository is a **Polymarket maker bot project** targeting BTC 5-minute binary prediction markets on Polygon. The primary reference document is `r1.md` — a complete builder's guide covering two trading strategies, existing Rust codebases to fork, and critical operational pitfalls. No code exists yet; implementation is planned in Rust.

## Market Environment (Post-Feb 2026)

Two rule changes broke the old playbook:
- **Jan 2026:** Dynamic taker fees introduced — max ~1.56% at p=0.50, near zero at extremes
- **Feb 18, 2026:** 500ms taker delay removed — takers now execute immediately, no cancellation window

**Critical:** Any AI-generated or pre-2026 Polymarket code assumes the 500ms buffer still exists. It doesn't. Code accordingly.

## Two Strategies

### Strategy 1 — Continuous Bid-Ask Spread Market Making
Post resting limit orders on both YES and NO sides throughout the window. Capture spread on round-trip fills regardless of outcome. Uses adapted Avellaneda-Stoikov pricing model. Primary risk: adverse selection (inventory imbalance when price moves sharply).

### Strategy 2 — Late-Window Directional Maker
Enter as a **maker** (not taker) at T-30s to T-5s before window close using Binance window delta as signal. Hard cutoff at T-5s (Polygon tx confirmation takes 2–5s). Target 55–60% win rate on BTC 5-min windows.

## Recommended Codebase Starting Points

| Strategy | Fork | Notes |
|----------|------|-------|
| Strategy 1 | `pontiggia/poly-bot` | Replace `strategy/math_arb.rs` with Stoikov logic; add `inventory.rs` |
| Strategy 2 | `TheOverLordEA/polymarket-hft-engine` | Implement `src/strategy.rs::execute_tick()`; change `execute_buy()` from taker to maker limit |
| Official SDK | `Polymarket/rs-clob-client` | Handles EIP-712 signing + `feeRateBps` automatically; WS heartbeat auto-cancels orders on disconnect |
| Performance | `floor-licker/polyfill-rs` | API-compatible drop-in for official SDK, ~21% faster |

## Critical Implementation Rules

**`feeRateBps` must be queried fresh before every order:**
```rust
// Query: GET https://clob.polymarket.com/fee-rate?tokenID={id}
// Never hardcode. Mismatched value = silent order rejection (no error, no fill).
// Cache max 60 seconds.
```

**Always use `rust_decimal` for prices, never `f64`:**
```rust
use rust_decimal_macros::dec;
let spread = dec!(0.52) - dec!(0.48);  // exactly 0.04, not 0.039999...
```

**WebSocket disconnect handling:** When the Polymarket WS drops, the server cancels ALL open orders. Reconnect logic must re-subscribe + re-query orderbook + re-post all quotes.

**Position merging:** Every unmerged YES+NO pair locks capital until settlement. Automate merge via CTF Exchange contract after every successful round-trip fill. Reference: `warproxxx/poly-maker` (Python) for merge logic.

**Tick size changes:** Subscribe to `tick_size_change` WS events. When market price crosses above 0.96 or below 0.04, the minimum tick size changes and orders at the old tick size will be rejected.

## Architecture

Three concurrent WebSocket connections:
1. `wss://stream.binance.com:9443/ws/btcusdt@trade` — real-time BTC price
2. `wss://ws-subscriptions-clob.polymarket.com` — Polymarket orderbook (unauthenticated)
3. `wss://ws-subscriptions-clob.polymarket.com` (authenticated) — fill notifications

Order book state: `DashMap<Decimal, Decimal>` (lock-free, price → size).

Market IDs for BTC 5-min markets are fully deterministic:
```rust
let window_ts = now - (now % 300);
let slug = format!("btc-updown-5m-{}", window_ts);
// 288 markets per day — pre-compute all slugs/tokenIds at startup
```

## Key Dependencies

```toml
tokio = { version = "1", features = ["full"] }
tokio-tungstenite = { version = "0.21", features = ["native-tls"] }
rust_decimal = "1"
rust_decimal_macros = "1"
dashmap = "5"
alloy = { version = "0.3", features = ["signers", "primitives"] }
reqwest = { version = "0.11", features = ["json", "rustls-tls"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
anyhow = "1"
tracing = "0.1"
dotenvy = "0.15"
```

## Performance Requirements

Cancel/replace loop must complete in **<100ms** total:
- WS orderbook update latency: <5ms
- Cancel request round-trip: <20ms
- Recalculate + sign: <5ms (Rust critical here)
- Submit new orders: <20ms

Target infrastructure: AWS us-east-1 or eu-west-1, 4-core dedicated VPS, <5ms to Polymarket matching engine. Home internet kills Strategy 1.

## Before Going Live

Paper trade mode (via env var toggle) is required. Run Strategy 1 for 500+ cycles and Strategy 2 for 100+ windows before using real capital. Start live with 10% of target capital.
