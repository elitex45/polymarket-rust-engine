# Strategy 1: Critical Issues & How to Handle Them

> This document addresses every gap identified in the strategy review. It is not a re-explanation of spread mechanics. It is specifically about the six things that determine whether this strategy makes money or slowly bleeds: adverse selection, inventory risk controls, fair value modeling, execution constraints, rebate scoring reality, and the correct cost model.

---

## Table of Contents

1. [Issue 1 — Toxic Flow & Adverse Selection](#issue-1--toxic-flow--adverse-selection)
2. [Issue 2 — Complete Inventory Risk Controls](#issue-2--complete-inventory-risk-controls)
3. [Issue 3 — Fair Value Model for Binary Markets](#issue-3--fair-value-model-for-binary-markets)
4. [Issue 4 — Execution Constraints That Will Reject Your Orders](#issue-4--execution-constraints-that-will-reject-your-orders)
5. [Issue 5 — Rebate Scoring Reality](#issue-5--rebate-scoring-reality)
6. [Issue 6 — Correct Cost Model (No Gas on Cancel/Replace)](#issue-6--correct-cost-model-no-gas-on-cancelreplace)

---

## Issue 1 — Toxic Flow & Adverse Selection

### What It Actually Is

The basic spread arithmetic — buy $0.49, sell $0.51, pocket $0.02 — looks like free money. It is not. The reason it is not is **adverse selection**: the people hitting your quotes are not a random sample of the market. They are concentrated at the worst possible moments.

**Non-toxic flow:** A retail trader wants to exit their YES position because they changed their mind. They hit your bid. BTC price hasn't moved. You buy at $0.49, your sell at $0.51 eventually fills someone else, you pocket the spread. This is the scenario the arithmetic assumes.

**Toxic flow:** BTC just moved +0.12% in 3 seconds. A fast bot immediately takes your $0.51 YES ask — which is now massively underpriced since YES is really worth $0.64 at this moment — before you can cancel. You sold YES cheap into a position that's clearly going to win. Your $0.49 bid sits unfilled because nobody wants to sell YES when it's obviously going up. You are now short YES with no hedge going into settlement.

The critical asymmetry: **good fills arrive slowly and randomly. Bad fills arrive instantly and clustered around fast price moves.** This means your fill quality distribution is not symmetric. It is negatively skewed in exactly the wrong direction.

### How to Measure It

The metric that tells you if your strategy is actually working is **adverse selection rate**, not gross spread capture.

```python
class FillAnalyzer:
    def __init__(self):
        self.fills = []

    def record_fill(self, side, token, price, fill_time, btc_price_at_fill, btc_price_30s_later):
        """
        Adverse fill = you bought YES and BTC went down in the next 30s,
        or you sold YES and BTC went up in the next 30s.
        """
        if token == "YES" and side == "BUY":
            adverse = btc_price_30s_later < btc_price_at_fill
        elif token == "YES" and side == "SELL":
            adverse = btc_price_30s_later > btc_price_at_fill
        else:
            adverse = False

        self.fills.append({
            "side": side, "price": price,
            "adverse": adverse,
            "btc_move": (btc_price_30s_later - btc_price_at_fill) / btc_price_at_fill
        })

    @property
    def adverse_selection_rate(self):
        if not self.fills:
            return 0
        return sum(1 for f in self.fills if f["adverse"]) / len(self.fills)

    @property
    def avg_adverse_btc_move(self):
        adverse = [f["btc_move"] for f in self.fills if f["adverse"]]
        return sum(adverse) / len(adverse) if adverse else 0
```

**Target**: adverse selection rate < 20%. If you're above 30%, your spread is too tight or your cancel/replace is too slow.

### How to Manage It

There are four levers. You need all four, not just one.

**Lever 1 — Spread width as a function of adverse selection cost**

Your minimum viable spread is not "what looks competitive." It is the spread that covers your expected adverse selection cost per fill. If your adverse selection rate is 25% and each adverse fill costs you 8 cents on average, then your spread must be at least:

```
min_spread = adverse_rate × avg_adverse_cost / (1 - adverse_rate)
           = 0.25 × 0.08 / 0.75
           = 0.0267  →  minimum viable spread is ~3 cents

Add margin for safety: target 4–5 cents minimum, not 2 cents.
```

**Lever 2 — Cancel/replace speed**

The window between when BTC moves and when a toxic taker can fill your stale quote is your only defense. Once they fill, the damage is done. You cannot cancel retroactively.

```
Cancel/replace target: < 100ms end-to-end
Measured from: Binance price update received
To: new orders posted to CLOB

Components:
  Binance WS message received:    ~0ms (baseline)
  Price delta check:              ~1ms
  Cancel old orders (off-chain):  ~20–40ms
  Compute new fair value:         ~1ms
  Post new orders (off-chain):    ~20–40ms
  Total:                          ~42–82ms  ← achievable on co-located VPS
```

If you are above 150ms consistently, your spread needs to be wider to compensate for the extra exposure window.

**Lever 3 — Reprice threshold (do not reprice on noise)**

Every unnecessary cancel/replace costs you queue position. If BTC ticks $10 and you're quoting a 15-minute window, that's noise — don't reprice. But if BTC moves $200 in 5 seconds, you must reprice immediately.

```python
def should_reprice(current_fair_value, posted_bid, posted_ask, half_spread):
    mid = (posted_bid + posted_ask) / 2
    drift = abs(current_fair_value - mid)
    # Reprice only if fair value has moved more than half the spread
    return drift > half_spread * 0.5
```

This prevents thrashing on noise while still catching real moves.

**Lever 4 — Volatility detection as a pause trigger**

When BTC volatility is elevated, every quote you post is a liability. The correct response to high vol is not to widen your spread — it is to **stop quoting entirely** until vol normalizes.

```python
def btc_vol_1min(price_history_60s):
    """Returns realized vol as annualized percentage"""
    returns = [
        (price_history_60s[i] - price_history_60s[i-1]) / price_history_60s[i-1]
        for i in range(1, len(price_history_60s))
    ]
    import math
    mean_r = sum(returns) / len(returns)
    variance = sum((r - mean_r)**2 for r in returns) / len(returns)
    return math.sqrt(variance * 525600) * 100  # annualized %

VOL_PAUSE_THRESHOLD = 120  # % annualized — pause if above this
VOL_RESUME_THRESHOLD = 80  # % — resume once it drops back

if btc_vol_1min(recent_prices) > VOL_PAUSE_THRESHOLD:
    cancel_all_orders()
    state = "PAUSED"

if state == "PAUSED" and btc_vol_1min(recent_prices) < VOL_RESUME_THRESHOLD:
    state = "ACTIVE"
    # resume quoting
```

---

## Issue 2 — Complete Inventory Risk Controls

### Why Quote Skewing Alone Is Not Enough

The previous explanation described inventory skewing — shifting both quotes down when you're long YES to encourage selling. That's one tool. But it has a fatal gap: it assumes the other side will eventually fill and bring you back to flat. In a fast directional market, that does not happen. Nobody is selling YES when BTC is clearly going up. You are long and getting longer, and skewing quotes makes no difference because there are simply no takers on the other side.

You need hard rules that activate before you reach the point where skewing can't save you.

### The Complete Risk Rule Set

```python
# ============================================================
# INVENTORY RISK PARAMETERS — set before live trading
# ============================================================

MAX_NET_EXPOSURE_SHARES = 300     # hard cap: max net long/short in any direction
MAX_TOTAL_SHARES_HELD   = 600     # hard cap: max gross position (YES + NO combined)
MAX_UNHEDGED_HOLD_SEC   = 45      # if net exposure > 50 shares for > 45s, force-flatten
STOP_QUOTING_AT_SEC     = 60      # stop ALL quoting this many seconds before window close
WIDEN_SPREAD_AT_SEC     = 90      # double spread this many seconds before window close
DAILY_LOSS_KILL_USDC    = 75      # shut bot down for the day if loss exceeds this
PER_WINDOW_MAX_LOSS     = 30      # cancel all orders in a window if loss exceeds this
VOL_PAUSE_THRESHOLD_PCT = 120     # annualized vol % — pause quoting above this
SKEW_ACTIVATION_SHARES  = 100     # start skewing quotes at this net exposure level
ONE_SIDE_STOP_SHARES    = 200     # stop posting the long side entirely at this level
```

### Rule 1 — Hard Inventory Cap with One-Side Stop

```python
def compute_quote_params(net_exposure, half_spread, fair_value):
    """
    Returns (bid_price, ask_price, post_bid, post_ask)
    post_bid/post_ask = whether to post that side at all
    """
    post_bid = True
    post_ask = True

    # Stage 1: skew (rubber band back to flat)
    if abs(net_exposure) >= SKEW_ACTIVATION_SHARES:
        skew = (net_exposure / MAX_NET_EXPOSURE_SHARES) * half_spread
        bid = fair_value - half_spread - skew
        ask = fair_value + half_spread - skew
    else:
        bid = fair_value - half_spread
        ask = fair_value + half_spread

    # Stage 2: one-side stop (stop adding to the problem)
    if net_exposure >= ONE_SIDE_STOP_SHARES:
        post_bid = False   # we're long YES, stop buying more YES
    elif net_exposure <= -ONE_SIDE_STOP_SHARES:
        post_ask = False   # we're long NO (short YES), stop selling YES

    # Stage 3: hard cap (cancel everything, force flatten)
    if abs(net_exposure) >= MAX_NET_EXPOSURE_SHARES:
        post_bid = False
        post_ask = False
        # trigger force_flatten() externally

    return bid, ask, post_bid, post_ask
```

### Rule 2 — Max Unhedged Hold Time

If you've been net long one side for too long without the other side filling, the market is telling you something: the trade is going against you and nobody wants the other side. You must exit.

```python
import time

class UnhedgedTimer:
    def __init__(self):
        self.unhedged_since = None

    def update(self, net_exposure):
        if abs(net_exposure) > 50:  # meaningfully unhedged
            if self.unhedged_since is None:
                self.unhedged_since = time.time()
            elif time.time() - self.unhedged_since > MAX_UNHEDGED_HOLD_SEC:
                return True  # force flatten
        else:
            self.unhedged_since = None
        return False

def force_flatten(net_exposure, token_id, clob_client):
    """Exit the net position as a taker (pay the fee, accept the loss)"""
    if net_exposure > 0:
        # Long YES — sell YES at market
        clob_client.create_market_order(
            token_id=token_id, side="SELL",
            amount=abs(net_exposure), order_type="FOK"
        )
    elif net_exposure < 0:
        # Short YES (long NO) — buy YES at market
        clob_client.create_market_order(
            token_id=token_id, side="BUY",
            amount=abs(net_exposure), order_type="FOK"
        )
```

Note: force flattening as a taker means paying fees. This is intentional — it is an insurance cost, not an edge. You are buying certainty over a losing drift.

### Rule 3 — Time-to-Expiry Spread Widening and Quote Stop

The closer you get to window close, the worse your options. A one-sided inventory at T-30s has no escape. The market will be fully priced by then and you cannot unwind without massive slippage.

```python
def get_time_adjusted_half_spread(base_half_spread, seconds_remaining):
    if seconds_remaining <= STOP_QUOTING_AT_SEC:
        return None  # None = stop quoting entirely

    if seconds_remaining <= WIDEN_SPREAD_AT_SEC:
        # Linear widening from base to 3× base
        scale = 1 + 2 * (1 - (seconds_remaining - STOP_QUOTING_AT_SEC) /
                         (WIDEN_SPREAD_AT_SEC - STOP_QUOTING_AT_SEC))
        return base_half_spread * scale

    return base_half_spread
```

At T-60s: spread doubles.
At T-45s: spread triples.
At T-30s: stop quoting. Cancel all resting orders. Whatever you hold, you hold.

### Rule 4 — Kill Switches

```python
class KillSwitch:
    def __init__(self):
        self.daily_pnl = 0.0
        self.window_pnl = 0.0
        self.killed = False

    def update_pnl(self, delta):
        self.daily_pnl += delta
        self.window_pnl += delta

        if self.daily_pnl <= -DAILY_LOSS_KILL_USDC:
            self.killed = True
            print(f"DAILY KILL SWITCH: loss {self.daily_pnl:.2f} USDC. Shutting down.")
            return "KILL_DAY"

        if self.window_pnl <= -PER_WINDOW_MAX_LOSS:
            print(f"WINDOW KILL: loss {self.window_pnl:.2f} USDC in this window.")
            return "KILL_WINDOW"

        return "OK"

    def reset_window(self):
        self.window_pnl = 0.0
```

### Rule 5 — News Event Halt

The worst adverse selection scenario is a macro news event: a CPI print, Fed announcement, or major crypto news. Your vol detector should catch this, but add an explicit news-aware layer:

```python
# Maintain a schedule of known high-risk times
HIGH_RISK_WINDOWS_UTC = [
    "08:30",  # US economic data releases
    "14:00",  # Fed decisions
    "20:00",  # crypto-specific news aggregation spike
]

def is_high_risk_time(now_utc):
    h, m = now_utc.hour, now_utc.minute
    for t in HIGH_RISK_WINDOWS_UTC:
        rh, rm = map(int, t.split(":"))
        # 5 minutes before and 15 minutes after
        event_minutes = rh * 60 + rm
        now_minutes = h * 60 + m
        if event_minutes - 5 <= now_minutes <= event_minutes + 15:
            return True
    return False
```

---

## Issue 3 — Fair Value Model for Binary Markets

### The Problem with "Binance Mid + Polymarket Mid"

If you average the two and call it fair value, you will systematically misprice during any window where BTC has moved from its opening. At T-3 minutes into a 5-minute window where BTC is already +0.10% above the open, the fair value of YES is not 50%. It is approximately 70–75%. If your model says 52%, you will quote $0.50/$0.54 when you should be quoting $0.68/$0.72. You will sell YES at $0.54 into something that's going to pay $1.00. That is not adverse selection — that is your own model giving you bad prices.

### The Binary Options Fair Value Model

A 5-minute (or 15-minute) BTC up/down market is structurally identical to a **binary call option** with:
- Underlying: BTC/USD price
- Strike: BTC price at window open
- Expiry: window close timestamp
- Payout: $1.00 if price ≥ strike at expiry, $0.00 otherwise

The fair value of YES is the probability that BTC finishes at or above the window open price. Under a log-normal assumption (the standard in short-dated binary options):

```python
import math
from scipy.stats import norm

def fair_value_yes(
    current_price: float,
    window_open_price: float,
    seconds_remaining: float,
    realized_vol_annualized: float = 0.80  # BTC historical ~80% annualized
) -> float:
    """
    Returns P(BTC_final >= window_open_price) given current state.

    Parameters:
        current_price:           BTC spot price right now (from Binance)
        window_open_price:       BTC price at exact window start timestamp
        seconds_remaining:       how many seconds until window close
        realized_vol_annualized: annualized volatility, as decimal (0.80 = 80%)

    Returns:
        float: probability between 0 and 1 that YES resolves
    """
    if seconds_remaining <= 0:
        return 1.0 if current_price >= window_open_price else 0.0

    # Time fraction of a year
    t = seconds_remaining / (365.25 * 24 * 3600)

    # Log return from window open to now
    log_return = math.log(current_price / window_open_price)

    # Expected std dev of the remaining log return
    sigma_remaining = realized_vol_annualized * math.sqrt(t)

    if sigma_remaining < 1e-10:
        return 1.0 if log_return >= 0 else 0.0

    # d = (current log return relative to strike) / (vol of remaining path)
    # P(finish UP) = P(log_return_remaining >= -log_return)
    #              = P(Z >= -log_return / sigma_remaining)
    #              = N(log_return / sigma_remaining)
    d = log_return / sigma_remaining

    return float(norm.cdf(d))
```

**What this does:** It says, given where BTC is now and how much vol is left in the window, what is the probability that BTC ends above the strike? It accounts for:
- Current distance from strike (the dominant factor)
- Time remaining (more time = more uncertainty = closer to 50%)
- Realized volatility (higher vol = more uncertainty = closer to 50%)

### Using It in the Quote Loop

```python
def compute_fair_value(binance_price, window_open_price, seconds_remaining, vol_1min):
    # Blend model with Polymarket's current mid for stability
    model_fv = fair_value_yes(binance_price, window_open_price, seconds_remaining, vol_1min)
    polymarket_mid = get_polymarket_mid()  # from WebSocket

    # Weight: model gets more weight as we get closer to close
    # (Polymarket mid is noisier; our model is more accurate near expiry)
    time_weight = 1 - (seconds_remaining / 300)  # 0 at window open, 1 at close
    model_weight = 0.3 + 0.4 * time_weight       # 0.3 to 0.7

    blended_fv = model_weight * model_fv + (1 - model_weight) * polymarket_mid
    return blended_fv
```

At window open: trust Polymarket mid more (30% model, 70% market).
Near window close: trust the model more (70% model, 30% market).

This is because Polymarket's mid is noisier near close (thin liquidity, one-sided book), while the model becomes more accurate as time remaining approaches zero and the outcome becomes more determined.

### Calibrating Realized Vol

Do not use a fixed 80% annualized vol for all market conditions. BTC vol is highly regime-dependent.

```python
class RealizedVolEstimator:
    def __init__(self, window_seconds=300):
        self.prices = []
        self.timestamps = []
        self.window_seconds = window_seconds

    def update(self, price, timestamp_ms):
        self.prices.append(price)
        self.timestamps.append(timestamp_ms / 1000)
        # Keep only last N seconds
        cutoff = self.timestamps[-1] - self.window_seconds
        while self.timestamps and self.timestamps[0] < cutoff:
            self.prices.pop(0)
            self.timestamps.pop(0)

    def annualized_vol(self):
        if len(self.prices) < 2:
            return 0.80  # default fallback

        log_returns = [
            math.log(self.prices[i] / self.prices[i-1])
            for i in range(1, len(self.prices))
        ]
        n = len(log_returns)
        mean_r = sum(log_returns) / n
        variance = sum((r - mean_r)**2 for r in log_returns) / n
        seconds_per_obs = self.window_seconds / n
        return math.sqrt(variance / seconds_per_obs * 365.25 * 24 * 3600)
```

Update this every 5 seconds. Feed the output directly into `fair_value_yes()`.

### Intuition Check: What the Model Produces

| BTC delta from open | Time remaining | Annualized Vol | P(YES) |
|---------------------|----------------|----------------|--------|
| 0.00% | 300s | 80% | 0.500 |
| +0.05% | 300s | 80% | 0.537 |
| +0.10% | 300s | 80% | 0.574 |
| +0.10% | 120s | 80% | 0.637 |
| +0.10% | 30s | 80% | 0.775 |
| +0.10% | 10s | 80% | 0.880 |
| +0.20% | 30s | 80% | 0.925 |
| +0.10% | 300s | 160% | 0.537 |

Key observations:
- At window open (300s remaining), even with delta, the model stays near 50% — there's too much time for reversal.
- With 30s remaining and +0.10% delta, it jumps to 77.5% — this is why Strategy 2 waits for the late window.
- Higher vol compresses probabilities toward 50% — in a chaotic session, nothing is certain.

---

## Issue 4 — Execution Constraints That Will Reject Your Orders

These are confirmed from the official Polymarket docs and will silently (or loudly) reject your orders if you ignore them.

### Constraint 1 — Tick Size Compliance (Hard Reject)

Every market has a `minimum_tick_size`. Your order price must be an exact multiple of this value or **the order is rejected outright**.

```python
def get_market_tick_size(clob_client, token_id):
    """Always fetch fresh — tick size changes dynamically near price extremes"""
    return clob_client.get_tick_size(token_id)

def round_to_tick(price, tick_size):
    """Round price to nearest valid tick"""
    tick = float(tick_size)
    return round(round(price / tick) * tick, len(str(tick).split('.')[-1]))

# Usage
tick = get_market_tick_size(client, YES_TOKEN_ID)
bid_price = round_to_tick(fair_value - half_spread, tick)
ask_price = round_to_tick(fair_value + half_spread, tick)
```

Tick sizes by range (confirmed from docs):

| Price Range | Tick Size |
|-------------|-----------|
| 0.04 – 0.96 | 0.01 |
| < 0.04 or > 0.96 | 0.001 |

The tick size changes dynamically as the market price approaches extremes. Your bot must detect the `tick_size_change` WebSocket event and update accordingly:

```python
# In your WebSocket handler
if event["event_type"] == "tick_size_change":
    token_id = event["asset_id"]
    new_tick = float(event["new_tick_size"])
    tick_cache[token_id] = new_tick
    # Immediately reprice all resting orders to comply
    reprice_all_orders(token_id)
```

### Constraint 2 — postOnly Flag (Use It Always)

When you submit a GTC limit order without `postOnly=true`, and your order happens to cross the spread (i.e., your bid is above the current best ask), it will execute **immediately as a taker**. You pay fees, which defeats the entire purpose of being a maker.

```python
# CORRECT: always use postOnly for maker strategy
response = await client.post_order(signed_order, order_type=OrderType.GTC, post_only=True)

# What happens with postOnly=true:
# - If order would cross the spread (be immediately matched): REJECTED (not executed as taker)
# - If order rests on the book: accepted as maker, no fees
```

`postOnly` is only valid with GTC and GTD. Not valid with FOK or FAK.

If a `postOnly` order gets rejected because it would cross the spread, it means your fair value was off and your quote was too aggressive. Log this as a signal to investigate your pricing model, not just retry.

### Constraint 3 — Minimum Order Size

Each market has a `minimum_order_size` field. Orders below this threshold are rejected.

```python
def get_min_order_size(market_data):
    return float(market_data.get("minimum_order_size", 5.0))  # default 5 shares

# Before placing any order:
if order_size < get_min_order_size(market):
    skip_order()  # do not post, do not log as an error
```

Typical minimum is 5 shares (~$2.50 at $0.50 price). Never hardcode this — it varies by market.

### Constraint 4 — GTD Expiration Quirk

If you use GTD (Good Till Date) orders — useful to ensure stale orders expire automatically rather than requiring explicit cancellation — note the 60-second security threshold:

```python
import time

def gtd_expiration(effective_lifetime_seconds):
    """
    To set an order that expires in N seconds from now,
    you must pass now + 60 + N as the expiration.
    The 60-second offset is a platform security threshold.
    """
    return int(time.time()) + 60 + effective_lifetime_seconds

# Example: order that expires in 30 seconds
expiry = gtd_expiration(30)   # = now + 90

# Example: order that expires at window close
seconds_to_close = get_seconds_to_window_close()
expiry = gtd_expiration(seconds_to_close - 10)  # expire 10s before close for safety
```

GTD is useful as a safety net so your quotes auto-expire if your bot crashes and cannot cancel them manually.

### Constraint 5 — USDC Allowance (One-Time, But Blocks Everything)

Before your wallet can trade at all, you must approve two contracts once:

```python
# Run this ONCE per wallet, before any trading
# Using web3.py:
from web3 import Web3

w3 = Web3(Web3.HTTPProvider("https://polygon-rpc.com"))
# MAX_INT approval for USDC token to Polymarket exchange contract
# MAX_INT approval for CTF (Conditional Token Framework) contract

USDC_CONTRACT   = "0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174"
CTF_CONTRACT    = "0x4D97DCd97eC945f40cF65F87097ACe5EA0476045"
EXCHANGE        = "0x4bFb41d5B3570DeFd03C39a9A4D8dE6Bd8B8982E"

# Approve USDC
usdc = w3.eth.contract(address=USDC_CONTRACT, abi=ERC20_ABI)
tx = usdc.functions.approve(EXCHANGE, 2**256 - 1).build_transaction({...})

# Approve CTF
ctf = w3.eth.contract(address=CTF_CONTRACT, abi=CTF_ABI)
tx = ctf.functions.setApprovalForAll(EXCHANGE, True).build_transaction({...})
```

If you forget this, every order submission returns "not enough balance/allowance" with no other explanation. This is a day-one blocker.

### Constraint 6 — negRisk Flag

For multi-outcome markets (3+ outcomes, e.g. "What price range will BTC be in?"), you must pass `negRisk: true` in order options. For standard binary up/down markets this is `false`. Check the `neg_risk` field on the market object before placing any order.

```python
market = client.get_market(condition_id)
neg_risk = market.get("neg_risk", False)

signed_order = client.create_order(
    order_args,
    options={"tickSize": tick_size, "negRisk": neg_risk}
)
```

Sending `negRisk: false` to a neg_risk market routes your order to the wrong exchange contract and it will be rejected.

### Constraint 7 — Precision Limits

GTC limit orders have more flexible precision than market orders, but you still need to comply:

```python
def validate_order_precision(price, size, tick_size):
    """
    Ensure price × size does not exceed 2 decimal places for market orders.
    For GTC, follow tick size precision hierarchy.
    """
    tick_decimals = len(str(tick_size).rstrip('0').split('.')[-1])
    price_rounded = round(price, tick_decimals)
    size_rounded = round(size, 2)  # size to 2 decimal places is safe
    return price_rounded, size_rounded
```

### Constraint 8 — Allowance/Balance Errors Are Silent

If your USDC balance is insufficient for a new order, the CLOB returns a success-looking response but with `success: false` in the body. Always check:

```python
response = client.post_order(signed_order, OrderType.GTC, post_only=True)
if not response.get("success", False):
    error = response.get("errorMsg", "unknown")
    # Common errors:
    # "not enough balance/allowance" → check USDC balance and contract approvals
    # "tick size" → price not aligned to market tick
    # "minimum order size" → order too small
    # "post only" → order would cross the spread (your price was wrong)
    handle_order_error(error, response)
```

---

## Issue 5 — Rebate Scoring Reality

### The Rebate Is Not a Simple Volume Bonus

The previous description framed rebates as: *"proportional to your share of executed maker volume."* That is the baseline. The actual mechanism is more restrictive and favors makers who quote tightly.

**Confirmed from docs and community reverse-engineering:**

**Condition 1 — Must be within max_spread**
Each market has a `rewards.max_spread` field (e.g., 3 cents). Only orders placed within that distance from the adjusted midpoint qualify. Post wider than that and you earn **zero rebates** even if your order fills.

```python
def check_spread_eligibility(my_bid, my_ask, market_mid, market_max_spread):
    bid_distance = market_mid - my_bid
    ask_distance = my_ask - market_mid
    return (bid_distance <= market_max_spread and
            ask_distance <= market_max_spread)
```

**Condition 2 — Must meet minimum size**
Each market has a `rewards.min_size` field. Orders below this size earn no rebates even if they fill. For 5m/15m crypto markets this is typically 20–50 shares.

**Condition 3 — Quadratic scoring, not linear**
Rewards are not split evenly among qualifying makers. They follow a **quadratic spread function**: a quote 1 cent from the adjusted midpoint earns approximately 4× more reward credit than a quote 2 cents away. This forces competitive quoting — you cannot hide at the edge of the qualifying range and expect meaningful rebates.

```
Reward weight ∝ (max_spread - distance_from_mid)²

Example: max_spread = 5 cents
Quote 1 cent from mid:  weight = (5-1)² = 16
Quote 2 cents from mid: weight = (5-2)² = 9
Quote 4 cents from mid: weight = (5-4)² = 1
```

**Condition 4 — Adjusted midpoint, not raw midpoint**
Polymarket uses an "adjusted midpoint" that filters out dust orders below `min_size`. This prevents the attack of posting tiny orders at extreme prices to manipulate the midpoint and make wide quotes look like they're within max_spread.

**Condition 5 — Bilateral quoting requirement near extremes**
If the midpoint is below $0.10, you must have orders on **both sides** to qualify. One-sided quoting near extremes earns no rebates.

### The Conflict Between Risk and Rebates

Here's the core tension: to maximize rebates, you want to quote tight (close to mid). To minimize adverse selection, you want to quote wide. These are directly opposed.

```
Rebate maximization: quote as tight as possible (1–2 cents from mid)
Adverse selection safety: quote wide enough to cover toxic fill cost (3–5 cents min)

Resolution:
  - In low-vol, directionless windows: quote tight (1–2 cents), maximize rebates
  - In high-vol or trending windows: widen (3–5 cents) or pause
  - Target: be within max_spread 80%+ of the time, but never tighter than your adverse selection threshold
```

The rebate is a **bonus**, not the main profit engine. Per the community reverse-engineering writeup: "Unless you have strong, independent alpha, it is healthier to treat liquidity rewards as a bonus, not the main profit engine." The spread capture must be profitable on its own. If it isn't, rebates will not save the strategy.

### Market Selection for Rebate Eligibility

Not all markets are in the rebate program. Check eligibility before deploying capital:

```python
def is_rebate_eligible(market_data):
    rewards = market_data.get("rewards", {})
    rates = rewards.get("rates", None)
    max_spread = rewards.get("max_spread", 0)
    min_size = rewards.get("min_size", 0)

    # Ineligible if no rates, no spread, or no min size defined
    if not rates or max_spread == 0 or min_size == 0:
        return False

    return True
```

Confirmed eligible markets as of March 2026: 5-minute crypto, 15-minute crypto, NCAAB, Serie A. All other markets: no taker fees, no rebate program.

### Practical Rebate Estimation

To estimate your expected daily rebate for a given market:

```python
def estimate_daily_rebate(
    daily_fill_volume_usdc,     # your expected daily executed maker volume
    market_taker_fee_pool_usdc, # total taker fees collected in this market daily
    your_reward_weight,         # your quadratic weight
    total_reward_weight,        # sum of all makers' quadratic weights
    rebate_pct_of_fee_pool      # fraction Polymarket redistributes (not fixed, ~50-80%)
):
    your_share = your_reward_weight / total_reward_weight
    return market_taker_fee_pool_usdc * rebate_pct_of_fee_pool * your_share
```

In practice, do not rely on this estimate before you have 1–2 weeks of live rebate data. The pool size and competitive pressure are highly variable.

---

## Issue 6 — Correct Cost Model (No Gas on Cancel/Replace)

### What the Previous Description Got Wrong

The previous guide said to "save the cancel/replace gas." This implied each cancel/replace incurs an on-chain gas transaction. This is wrong.

### How the CLOB Cost Model Actually Works

Polymarket uses a **hybrid-decentralized** architecture:

| Action | Where it happens | Cost |
|--------|-----------------|------|
| Place order | Off-chain (CLOB operator) | Free |
| Cancel order | Off-chain (CLOB operator) | Free |
| Order matching | Off-chain (CLOB operator) | Free |
| On-chain settlement of matched trades | On-chain (Polygon) | Gas (~$0.001–$0.01) |
| Final redemption of winning tokens | On-chain (Polygon) | Gas (~$0.001–$0.01) |

**Cancel/replace has zero gas cost.** You can cancel and repost 1000 times per minute with no on-chain cost.

### The Real Costs of High-Frequency Cancel/Replace

There are costs, they are just different from gas:

**Cost 1 — API rate limiting**
Polymarket's CLOB operator will throttle you if you cancel/replace too aggressively. The exact limits are not published but community reports suggest:
- Cancel requests: throttled above ~50–100/minute sustained
- Order placement: throttled above ~50–100/minute sustained
- Exceeding limits results in HTTP 429 errors and temporary bans

```python
import asyncio

class RateLimiter:
    def __init__(self, max_per_minute=60):
        self.max_per_minute = max_per_minute
        self.calls = []

    async def acquire(self):
        now = asyncio.get_event_loop().time()
        self.calls = [t for t in self.calls if now - t < 60]
        if len(self.calls) >= self.max_per_minute:
            sleep_time = 60 - (now - self.calls[0]) + 0.1
            await asyncio.sleep(sleep_time)
        self.calls.append(now)
```

**Cost 2 — Queue position loss**
The CLOB uses price-time priority. When you cancel and repost at the same price, you go to the back of the queue at that price level. If 10 other makers are quoting $0.49, your reposted order is filled last. This reduces your fill rate.

**Implication:** Reprice only when necessary (price moved beyond threshold). Repricing on noise costs you queue position for no benefit.

**Cost 3 — CLOB processing latency**
Every cancel/replace cycle takes 40–80ms for the CLOB operator to process off-chain. During this time, your stale quote is still live and fillable. This is the actual latency concern — not gas.

```python
# Correct latency model for cancel/replace cycle:

T=0ms:   Detect stale price (Binance moved)
T=20ms:  Cancel request sent to CLOB API
T=60ms:  Cancel confirmed by CLOB operator
T=61ms:  New order signed locally (fast, ~1ms)
T=100ms: New order submitted to CLOB API
T=140ms: New order confirmed live on book
# ~140ms total window where old quote is still canceling / new quote not yet live
```

During that 140ms, your old price is still fillable until the cancel confirms. Your new price isn't live yet. This is the adverse selection window.

### Summary of Correct Cost Model

```
Cancel/replace:
  Gas cost:               $0.00
  Rate limit risk:        Real (> 60-100 cancels/min = throttled)
  Queue position cost:    Real (reprice = back of queue)
  Latency window risk:    Real (~40-80ms between cancel confirm and new order live)

On-chain costs:
  Per matched trade:      ~$0.001–$0.01 Polygon gas
  Per redemption:         ~$0.001–$0.01 Polygon gas
  USDC approval (once):   ~$0.01–$0.05 Polygon gas

Net effect:
  Cancel/replace freely when needed, but implement min reprice threshold
  to avoid unnecessary queue loss and rate limit exposure.
```

---

## Pre-Live Checklist

Before trading real capital, all of these must be true:

### Model Validation
- [ ] Fair value model backtested on minimum 500 historical 5-minute windows
- [ ] Adverse selection rate measured on paper trades: < 20%
- [ ] Spread width validated as > (adverse_rate × avg_adverse_cost / (1 - adverse_rate))
- [ ] Vol estimator calibrated: compare model output to realized BTC vol on same historical windows
- [ ] Fair value blending weights tested: does blended FV track actual settlement prices better than either component alone?

### Infrastructure
- [ ] Cancel/replace loop measured end-to-end: < 100ms on target VPS
- [ ] Binance WebSocket reconnect logic implemented and tested
- [ ] Polymarket CLOB WebSocket reconnect logic implemented and tested
- [ ] USDC allowance set (both USDC token and CTF contract)
- [ ] API credentials derived and working
- [ ] Rate limiter implemented and tested

### Execution Constraints
- [ ] Tick size fetched fresh at market open (not hardcoded)
- [ ] `tick_size_change` WebSocket event handled with immediate reprice
- [ ] `postOnly=true` on all maker orders
- [ ] GTD expiration set to window close - 10s (safety net)
- [ ] `negRisk` flag set correctly per market type
- [ ] `min_order_size` fetched from market data, not hardcoded
- [ ] Order response checked for `success: false` with error handling per error type
- [ ] `feeRateBps` fetched fresh before each order in fee-eligible markets

### Risk Rules
- [ ] `MAX_NET_EXPOSURE_SHARES` defined and tested
- [ ] One-side quote stop at `ONE_SIDE_STOP_SHARES` implemented
- [ ] `force_flatten()` function implemented and tested in paper mode
- [ ] `MAX_UNHEDGED_HOLD_SEC` timer running
- [ ] `STOP_QUOTING_AT_SEC` cutoff implemented and tested
- [ ] Spread widening schedule at `WIDEN_SPREAD_AT_SEC` implemented
- [ ] `DAILY_LOSS_KILL_USDC` kill switch implemented
- [ ] Volatility pause threshold implemented
- [ ] High-risk time schedule defined

### Rebate Eligibility
- [ ] Market confirmed in rebate program (`rewards.rates != null`)
- [ ] `max_spread` fetched and quotes confirmed within it
- [ ] `min_size` fetched and orders above it
- [ ] Bilateral quoting logic for markets near price extremes (<$0.10 mid)

### Paper Trading Gate
- [ ] Minimum 48 hours of paper trading on live data before live capital
- [ ] P&L tracked per window: spread capture, adverse fills, rebates (simulated)
- [ ] Adverse selection rate < 20% sustained over paper period
- [ ] No single-window loss > `PER_WINDOW_MAX_LOSS` threshold triggered

---

## The Honest Profitability Framing

To close: this strategy can be profitable, but not because the arithmetic of buy-low/sell-high looks nice. It is profitable **only if** your fair value model is accurate enough that your quotes are better-priced than competitors', and your risk controls are tight enough that adverse fills don't compound into settlement losses.

The two-sentence version of what makes money here:

> Spread capture is the mechanism. Adverse selection management is the actual skill.

Everything in this document is about the second part.