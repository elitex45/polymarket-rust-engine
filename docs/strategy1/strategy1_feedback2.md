# Strategy 1: Seven Corrections

> This document only addresses the seven specific errors identified in the critical review. It is a patch, not a re-explanation. Read it alongside the previous guide.

---

## Fix 1 — force_flatten() Logic Was Wrong for Short YES (Long NO)

### The Bug

The previous code defined `net_exposure = yes_shares - no_shares`. When `net_exposure < 0` (you hold more NO than YES), it triggered a **BUY YES** order. That is wrong. Buying YES when you're already long NO does not flatten — it creates a boxed YES+NO position, locks up capital in both tokens, and increases gross inventory without reducing net risk.

The correct flatten action depends on which token you actually hold in excess.

### The Fix

Inventory has two independent token balances. Flatten by selling the token you hold in excess, not by buying the opposite.

```python
def force_flatten(yes_shares: float, no_shares: float,
                  yes_token_id: str, no_token_id: str,
                  clob_client):
    """
    Flatten net inventory by selling the excess token as a taker.
    Never buy the opposite token — that boxes the position.

    yes_shares: shares of YES token currently held
    no_shares:  shares of NO token currently held
    """
    net = yes_shares - no_shares

    if net > 0:
        # Long YES — sell the excess YES
        excess = net
        clob_client.create_market_order(
            token_id=yes_token_id,
            side="SELL",
            amount=excess,
            order_type="FOK"
        )

    elif net < 0:
        # Long NO (short YES) — sell the excess NO
        excess = abs(net)
        clob_client.create_market_order(
            token_id=no_token_id,
            side="SELL",
            amount=excess,
            order_type="FOK"
        )

    # If net == 0, nothing to do
```

**Why never buy the opposite:** If you hold 200 NO and buy 200 YES, you now hold 200 YES + 200 NO = a $200 locked position that returns exactly $200 at settlement regardless of outcome. You have paid taker fees, locked capital, and gained nothing. Sell the token you have instead.

**Merge as an alternative to selling:** If you hold significant quantities of both YES and NO, you can merge them 1:1 to receive USDC instead of selling at market. This avoids taker fees and slippage. Only applicable when you hold both tokens in meaningful size.

```python
def try_merge_before_flatten(yes_shares: float, no_shares: float,
                              min_merge_size: float = 10.0):
    """
    Merge overlapping YES/NO positions back to USDC before selling surplus.
    Returns remaining yes_shares and no_shares after merge.
    """
    mergeable = min(yes_shares, no_shares)
    if mergeable >= min_merge_size:
        # Call CTF merge: burns 1 YES + 1 NO → $1.00 USDC
        clob_client.merge(amount=mergeable)
        yes_shares -= mergeable
        no_shares  -= mergeable
    return yes_shares, no_shares
```

Call `try_merge_before_flatten` first, then `force_flatten` on whatever remains.

---

## Fix 2 — Time-Based Rules Were Internally Inconsistent

### The Conflict

The previous document contained two contradictory statements:
- "Stop quoting at `<= STOP_QUOTING_AT_SEC`" where `STOP_QUOTING_AT_SEC = 60`
- "At T-60s: spread doubles"

Both cannot be true. If quoting stops at 60s, there is nothing to double at 60s.

There was a second conflict: GTD expiry was set to `window_close - 10s` even though quoting was supposed to stop 60s before close. That GTD order would still be live and fillable for 50 seconds after quoting was meant to have stopped — defeating the purpose entirely.

### The Fix: One Consistent Schedule

Pick one set of numbers and apply them everywhere — the quote loop, GTD expiry, and spread widening must all use the same boundaries.

```python
# ── Time-to-expiry thresholds ─────────────────────────────────────
WIDEN_AT_SEC   = 90   # start widening spread at T-90s
STOP_AT_SEC    = 60   # stop posting new quotes at T-60s
GTD_BUFFER_SEC = 10   # GTD orders expire STOP_AT_SEC + GTD_BUFFER_SEC before close
                      # so they expire before we stop quoting, not after

# ── Quote loop ────────────────────────────────────────────────────
def get_half_spread(base: float, seconds_remaining: float) -> float | None:
    """
    Returns adjusted half-spread, or None if quoting should stop.
    """
    if seconds_remaining <= STOP_AT_SEC:
        return None  # stop quoting

    if seconds_remaining <= WIDEN_AT_SEC:
        # Linear scale from 1× at WIDEN_AT_SEC to 2× at STOP_AT_SEC
        progress = 1 - (seconds_remaining - STOP_AT_SEC) / (WIDEN_AT_SEC - STOP_AT_SEC)
        return base * (1 + progress)  # 1× → 2× as we approach STOP_AT_SEC

    return base  # normal spread

# ── GTD expiry ────────────────────────────────────────────────────
def gtd_expiry(window_close_ts: int) -> int:
    """
    Orders expire at STOP_AT_SEC + GTD_BUFFER_SEC before window close.
    This ensures orders auto-cancel before we intend to stop quoting,
    not after. The 60s platform security offset is added on top.
    """
    effective_lifetime = (window_close_ts - int(time.time())) \
                         - STOP_AT_SEC \
                         - GTD_BUFFER_SEC
    if effective_lifetime <= 0:
        raise ValueError("Too late to post GTD order for this window")
    # Platform requires: expiration = now + 60 + intended_lifetime
    return int(time.time()) + 60 + effective_lifetime
```

**Readable summary of the consistent schedule:**

| Seconds remaining | Action |
|---|---|
| > 90s | Normal spread |
| 90s → 60s | Spread widens linearly from 1× to 2× |
| ≤ 60s | Stop quoting. Cancel all resting orders. |
| GTD orders | Auto-expire at T-70s (60s stop + 10s buffer) |

GTD orders now expire 10 seconds *before* the stop-quoting boundary, giving you a clean safety net that aligns with your actual intent.

---

## Fix 3 — Heartbeats Are Not Optional Infrastructure

### Why It Matters

This was missing from the previous checklist entirely. Confirmed from official Polymarket docs:

> "Send PING every 10 seconds" for market/user WebSocket channels.

Separately, the trading API has an **order heartbeat** requirement:

> If a valid CLOB heartbeat is not received within the required interval, open orders can be cancelled by the server.

These are two different mechanisms:

- **WebSocket PING/PONG** keeps your market/user data streams alive
- **CLOB order heartbeat** keeps your resting orders alive on the trading side

For a maker bot, neither is cosmetic. Losing the WebSocket means stale state. Missing the CLOB heartbeat can mean your open orders are cancelled by the server.

### The Fix: Two Independent Heartbeat Loops

The keepalive logic runs on its own tasks, independent of the main quote loop. It is not optional and must be monitored.

```python
import asyncio
import websockets
import time

HEARTBEAT_INTERVAL_SEC = 8   # send every 8s to stay inside the 10s + 5s buffer window
HEARTBEAT_TIMEOUT_SEC  = 5   # consider connection dead if no pong within 5s

class PolymarketWSSession:
    def __init__(self, uri: str):
        self.uri = uri
        self.ws = None
        self.last_pong = time.monotonic()
        self.connected = False

    async def connect(self):
        self.ws = await websockets.connect(self.uri)
        self.connected = True
        self.last_pong = time.monotonic()
        # Start heartbeat loop as a background task
        asyncio.create_task(self._heartbeat_loop())
        asyncio.create_task(self._receive_loop())

    async def _heartbeat_loop(self):
        """Send PING every HEARTBEAT_INTERVAL_SEC seconds."""
        while self.connected:
            await asyncio.sleep(HEARTBEAT_INTERVAL_SEC)
            try:
                await self.ws.ping()
                # Give server HEARTBEAT_TIMEOUT_SEC to respond
                await asyncio.sleep(HEARTBEAT_TIMEOUT_SEC)
                if time.monotonic() - self.last_pong > HEARTBEAT_INTERVAL_SEC + HEARTBEAT_TIMEOUT_SEC:
                    # No pong received in time
                    await self._handle_disconnect("heartbeat timeout")
            except Exception as e:
                await self._handle_disconnect(f"heartbeat error: {e}")

    async def _receive_loop(self):
        """Process incoming messages; update last_pong on PONG frames."""
        try:
            async for message in self.ws:
                self.last_pong = time.monotonic()
                await self._on_message(message)
        except websockets.ConnectionClosed:
            await self._handle_disconnect("connection closed by server")

    async def _handle_disconnect(self, reason: str):
        self.connected = False
        print(f"[WARN] WebSocket disconnected: {reason}")
        # Cancel all orders immediately before attempting reconnect
        # (server may already have cancelled them — this ensures consistency)
        await self._emergency_cancel_all()
        await self._reconnect()

    async def _emergency_cancel_all(self):
        """Best-effort: cancel all orders via REST in case WS is down."""
        try:
            clob_client.cancel_all()
        except Exception as e:
            print(f"[ERROR] Emergency cancel failed: {e}")

    async def _reconnect(self, max_retries: int = 10):
        for attempt in range(max_retries):
            wait = min(2 ** attempt, 30)  # exponential backoff, cap at 30s
            print(f"[INFO] Reconnecting in {wait}s (attempt {attempt+1})")
            await asyncio.sleep(wait)
            try:
                await self.connect()
                print("[INFO] Reconnected successfully")
                return
            except Exception as e:
                print(f"[WARN] Reconnect attempt {attempt+1} failed: {e}")
        print("[FATAL] Reconnect exhausted. Bot shutting down.")
        # Trigger kill switch
```

That handles **WebSocket keepalive only**. You also need a separate trading-side heartbeat loop:

```python
async def clob_order_heartbeat_loop(clob_client, interval_sec: int = 8):
    """
    Send authenticated CLOB heartbeat requests frequently enough that
    resting orders are not cancelled server-side.
    """
    while True:
        try:
            await clob_client.send_heartbeat()
        except Exception as e:
            print(f"[WARN] CLOB heartbeat failed: {e}")
        await asyncio.sleep(interval_sec)
```

**Key point:** On disconnect, assume your local state may be wrong. After reconnect:
- re-establish WebSocket subscriptions
- send/restore CLOB heartbeat
- audit open orders via REST
- only then resume quoting

**Checklist addition:**
- [ ] WebSocket PING/PONG keepalive implemented as independent async task
- [ ] CLOB order-heartbeat loop implemented separately from WebSocket keepalive
- [ ] Both heartbeat intervals set comfortably inside Polymarket's required window
- [ ] Disconnect handler: reconnect, restore heartbeats, then audit state
- [ ] Post-reconnect order audit before resuming quoting

---

## Fix 4 — Adverse Selection Measurement Was Incomplete

### The Two Problems

**Problem A:** The previous `FillAnalyzer` only classified YES token fills. When `token != "YES"`, it defaulted `adverse = False`. NO token fills were silently excluded from the adverse selection rate, understating the true number.

**Problem B:** "BTC price 30 seconds later" is a rough proxy. It measures whether BTC moved, not whether the *contract* moved against you. For a window closing in 45 seconds, a 30-second BTC price check gives you less than half a window of look-ahead — not a true markout of whether the fill was toxic.

### Fix A: Cover Both Token Fills

```python
def record_fill(self, side: str, token: str, price: float,
                btc_at_fill: float, btc_30s_later: float):
    """
    Classify a fill as adverse or not.

    Adverse = you took on directional risk that immediately went against you.

    YES BUY  is adverse if BTC fell after your fill (YES likely to lose)
    YES SELL is adverse if BTC rose  after your fill (YES likely to win, you sold cheap)
    NO  BUY  is adverse if BTC rose  after your fill (NO likely to lose)
    NO  SELL is adverse if BTC fell  after your fill (NO likely to win, you sold cheap)
    """
    btc_rose = btc_30s_later > btc_at_fill

    if token == "YES":
        adverse = (side == "BUY" and not btc_rose) or \
                  (side == "SELL" and btc_rose)
    elif token == "NO":
        adverse = (side == "BUY" and btc_rose) or \
                  (side == "SELL" and not btc_rose)
    else:
        return  # unknown token, skip

    self.fills.append({"side": side, "token": token,
                       "price": price, "adverse": adverse})
```

### Fix B: Acknowledge It as a Diagnostic, Not a KPI

The 30-second BTC proxy is a useful first-pass diagnostic but should not be the primary metric you optimize for live. The honest limitation: for short windows, there is often not enough time between fill and settlement to measure true markout without knowing the final settlement price. Your primary real-time signal should be:

- **Gross spread P&L per window**: did both sides fill? What was the net USDC captured?
- **Inventory carry loss per window**: what was the settlement P&L on any unhedged position?
- **Adverse fill rate** (using Fix A above) as a leading diagnostic: rising adverse rate = tighten latency or widen spread, not a profitability number itself

For a more rigorous markout, use settlement prices retrospectively during backtesting:

```python
def true_markout(fill_price: float, token: str, side: str,
                 settlement_price: float) -> float:
    """
    settlement_price: 1.0 if YES won, 0.0 if NO won
    Returns P&L per share from this fill's perspective.
    """
    if token == "YES":
        pnl_per_share = settlement_price - fill_price if side == "BUY" \
                        else fill_price - settlement_price
    else:  # NO
        no_settlement = 1.0 - settlement_price
        pnl_per_share = no_settlement - fill_price if side == "BUY" \
                        else fill_price - no_settlement
    return pnl_per_share
```

Run this in backtesting to measure true adverse selection magnitude per fill, not just direction.

---

## Fix 5 — Fair Value Model: Mark as Baseline, Not Validated

### What the Previous Model Is

The log-normal fair value model is a reasonable **starting point** — it accounts for current delta, time remaining, and realized volatility. It is not a validated model. Three things it does not handle:

**Oracle basis risk:** Chainlink settles on an aggregated price across multiple sources at a specific timestamp. Binance spot at that moment is not the same number. In normal conditions the difference is small (~0.01–0.05%). Near Binance-specific volatility spikes (e.g., Binance-local liquidation cascades), Binance can temporarily diverge from the Chainlink aggregate by 0.1–0.3%. Your fair value uses Binance; the settlement uses Chainlink. These can disagree at exactly the moments you most care about.

**Settlement-source mismatch:** Chainlink Data Streams uses a pull-based, aggregated feed. It is not a direct Binance tick. For borderline outcomes (delta < 0.03%), the difference between your Binance-based fair value and what Chainlink will actually snapshot at settlement is large relative to your edge.

**Driftless assumption:** The model assumes zero drift (µ = 0). For very short windows (5 minutes), this is a reasonable approximation. Over 15-minute windows, near macroeconomic events, momentum drift can be non-trivial.

### How to Handle It Honestly

**Label it correctly in your code:**

```python
def fair_value_yes_BASELINE(
    current_price, window_open_price, seconds_remaining, realized_vol
) -> float:
    """
    BASELINE MODEL: driftless log-normal.
    Known limitations:
      - Uses Binance spot; settlement uses Chainlink aggregate
      - Assumes zero drift (µ=0)
      - Does not model oracle update timing
    Do not trade on this alone for delta < 0.03% or seconds_remaining > 240s.
    """
    # ... (same implementation as before)
```

**Add oracle basis guard:**

```python
ORACLE_BASIS_GUARD = 0.0003  # skip quoting if delta is within ±0.03% of strike

def should_skip_window(window_delta_pct: float) -> bool:
    """
    Near the strike, oracle basis risk exceeds our model edge.
    Do not quote — the outcome is genuinely uncertain at this precision.
    """
    return abs(window_delta_pct) < ORACLE_BASIS_GUARD * 100
```

**Validation path (before trusting the model with real capital):**

Backtest minimum 200 resolved windows. For each window, compare:
- Model's P(YES) at T-120s, T-60s, T-30s
- Actual settlement outcome (1.0 or 0.0)

Compute Brier score: `BS = mean((predicted - actual)²)`. A random model scores 0.25. A perfect model scores 0.0. Target < 0.18 before using model output as a quoting input.

---

## Fix 6 — Gas Cost Claim Was Wallet-Path Dependent

### What the Previous Document Said

"Gas cost per matched trade: ~$0.001–$0.01. Gas cost per redemption: ~$0.001–$0.01."

This is only true for EOA wallets (standard Ethereum wallets like MetaMask). It is wrong for Safe/Proxy wallets using Polymarket's relayer.

### The Correct Model

Confirmed from official docs (`/trading/gasless`):

| Wallet type | On-chain gas for trade settlement | On-chain gas for approvals/redemptions |
|---|---|---|
| **EOA** | You pay (~$0.001–$0.01 POL) | You pay (~$0.01–$0.05 POL for one-time approvals) |
| **Safe wallet via Relayer** | **Polymarket pays** | **Polymarket pays** |
| **Proxy wallet via Relayer** | **Polymarket pays** | **Polymarket pays** |

Safe wallets deployed through Polymarket's Relayer Client are fully gasless for all covered operations: wallet deployment, token approvals, CTF splits/merges/redemptions, and trade settlement.

### What Applies to This Bot

For the bot described in this strategy, the intended operating model is:

- quote placement via the Polymarket CLOB API
- quote cancellation via the Polymarket CLOB API
- authenticated trading through Polymarket's relayed/gasless path

In that setup, **gas is not part of the normal operating cost of the bot**.

That means:

- **Posting maker quotes:** no gas required
- **Cancel/requote loop:** no gas required
- **Normal order management through the API:** no gas required

So for this specific bot design, you should think of gas as **not required in normal operation**. The strategy's cost model should focus on:

- adverse selection
- spread width
- queue position loss
- rate limits
- missed heartbeats / cancelled orders

not Polygon gas fees.

### When Gas Would Matter Anyway

Gas only becomes relevant if you intentionally choose a different wallet/execution path, for example:

- you trade from a plain EOA instead of the relayed Safe/Proxy path
- you send approvals yourself on-chain
- you perform direct contract interactions for merge/redeem/split instead of using the relayed flow
- you build custom settlement tooling outside Polymarket's gasless path

So the correct wording for this document is:

> For this bot, as designed, gas is not required for normal quoting, cancelling, or API-driven trading operations. Gas only appears if you deliberately use a direct on-chain / EOA workflow instead of Polymarket's relayed gasless path.

**For a market maker running high-frequency, the practical difference:**

```
EOA wallet, 500 trade settlements/day:
  ~500 × $0.005 = ~$2.50/day in gas
  Low but real, and spikes during Polygon congestion

Safe wallet via Relayer:
  $0.00/day in gas
  Polymarket's relayer absorbs all costs
  Requires Builder or Relayer API key (free to obtain)
```

For a maker bot, the Safe wallet path is strictly better on cost. The tradeoff is slightly more complex one-time setup (deploy Safe, generate Relayer API key). After setup, the operational model is identical.

**Correct statement for your documentation:** "Cancel and replace operations are off-chain and always free regardless of wallet type. On-chain settlement costs depend on wallet path: EOA wallets pay Polygon gas, Safe wallets via the Polymarket Relayer are gasless."

### Rate Limit Claim: Use Current Docs, Not Guesses

The previous document stated "50–100 cancels/minute before throttling." That was not documented and should not be presented as fact.

Use the published endpoint-specific limits from Polymarket's rate-limit docs instead. These are much higher than the old guessed numbers and may differ by route.

Correct guidance:

- Read the current official limits for each endpoint you use
- Rate-limit by endpoint class, not one blanket global guess
- Monitor HTTP 429 responses and back off exponentially
- Treat documented limits as upper bounds, not targets for normal operation

For a maker bot, the practical reason to stay conservative is still queue quality and operational stability, not just avoiding throttles.

---

## Fix 7 — News Halt Schedule: Replace Hardcoded UTC With Calendar Feed

### Why Hardcoded Times Fail

The previous document used hardcoded UTC strings (`"08:30"`, `"14:00"`, `"20:00"`). Two problems:

**DST:** US economic data releases are at 08:30 ET. In EST (winter) that is 13:30 UTC. In EDT (summer) that is 12:30 UTC. A hardcoded `"08:30"` UTC is wrong for both. If you hardcode `"13:30"` UTC you will miss the release every summer.

**"20:00 UTC as crypto news spike"** was explicitly noted as too vague to encode as a real control. There is no reliable crypto news spike at a fixed time of day.

### The Fix: Use an Actual Economic Calendar Feed

For US macro releases, use a free economic calendar API rather than static strings:

```python
import httpx
from datetime import date, datetime, timezone

async def fetch_high_impact_events_today() -> list[datetime]:
    """
    Fetch today's high-impact economic events from a free calendar API.
    Returns list of UTC datetimes for high-impact US releases.

    Free options:
      - https://nfs.faireconomy.media/ff_calendar_thisweek.json  (ForexFactory)
      - https://economic-calendar.tradingeconomics.com/calendar  (TradingEconomics)
      - Twelve Data, Alpha Vantage (free tiers)

    Filter for: impact=high, country=US
    """
    url = "https://nfs.faireconomy.media/ff_calendar_thisweek.json"
    async with httpx.AsyncClient() as client:
        resp = await client.get(url, timeout=5.0)
        events = resp.json()

    today = date.today()
    high_impact_today = []

    for event in events:
        if event.get("impact") != "High":
            continue
        if event.get("country") != "USD":
            continue
        try:
            event_dt = datetime.fromisoformat(event["date"]).replace(tzinfo=timezone.utc)
            if event_dt.date() == today:
                high_impact_today.append(event_dt)
        except (KeyError, ValueError):
            continue

    return high_impact_today


async def build_halt_schedule() -> list[tuple[datetime, datetime]]:
    """
    Returns list of (halt_start, halt_end) UTC datetime pairs.
    Pause quoting 5 minutes before each event, resume 15 minutes after.
    """
    events = await fetch_high_impact_events_today()
    from datetime import timedelta
    schedule = []
    for ev in events:
        halt_start = ev - timedelta(minutes=5)
        halt_end   = ev + timedelta(minutes=15)
        schedule.append((halt_start, halt_end))
    return schedule


def is_in_halt_window(now_utc: datetime,
                      halt_schedule: list[tuple[datetime, datetime]]) -> bool:
    return any(start <= now_utc <= end for start, end in halt_schedule)
```

**Operational notes:**
- Fetch the schedule once at bot startup (or refresh daily at midnight UTC)
- Cache locally — do not hit the API inside the hot loop
- If the calendar API is unreachable, default to halting during all `XX:25–XX:35` UTC windows (covers most US release slots conservatively) rather than failing open

**Drop the "20:00 UTC crypto spike" entirely.** There is no reliable time-of-day pattern for crypto-specific news. The correct control for that is the volatility detector (pause when 1-minute realized vol exceeds threshold), not a clock.

---

## Summary: What Changed and Where

| Issue | Previous Error | Fix |
|---|---|---|
| 1. force_flatten() | Bought opposite token for negative exposure | Sell excess token; optionally merge 1:1 first |
| 2. Timing rules | `STOP_AT_SEC=60` contradicted "doubles at T-60s"; GTD expired after stop | One consistent schedule: widen 90s→60s, stop at 60s, GTD expires at T-70s |
| 3. Heartbeat | Missing from checklist entirely | Independent async task, PING every 8s, reconnect with emergency cancel |
| 4. Adverse selection | NO fills excluded; 30s proxy presented as KPI | Cover both tokens; use true settlement markout in backtesting |
| 5. Fair value | Presented as validated | Labelled as baseline; oracle basis guard added; Brier score validation path |
| 6. Gas cost | Stated as fact without wallet-path qualification | EOA pays gas; Safe/Proxy via Relayer is gasless; throttle number removed |
| 7. News halt | Hardcoded UTC strings, DST-broken, vague crypto time | Calendar feed with DST-aware datetimes; crypto spike removed in favour of vol detector |
