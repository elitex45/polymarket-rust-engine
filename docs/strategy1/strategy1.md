Let me walk you through it with a concrete, real-numbers example.

---

## Strategy 1: How It Actually Works

### The Setup

You are not betting on direction. You are a **shopkeeper** who buys and sells YES and NO shares, pocketing the difference between your buy price and sell price on every transaction. You don't care who wins.

The CLOB enforces: `YES price + NO price ≈ $1.00`

So if YES is at $0.50, NO is at $0.50. If YES moves to $0.60, NO moves to $0.40. They're mirrors of each other.

---

### The Positions Available to You

As a market maker you can post 4 types of orders:

| Order | Meaning | You profit if... |
|-------|---------|-----------------|
| BUY YES @ $0.48 | You pay $0.48/share, receive YES tokens | YES resolves ($1.00) |
| SELL YES @ $0.52 | You receive $0.52/share, give up YES tokens | NO resolves ($0.00 for YES) |
| BUY NO @ $0.48 | You pay $0.48/share, receive NO tokens | NO resolves ($1.00) |
| SELL NO @ $0.52 | You receive $0.52/share, give up NO tokens | YES resolves ($0.00 for NO) |

**Key insight:** BUY YES @ $0.48 and SELL NO @ $0.52 are mathematically equivalent positions — both pay you when YES wins. So you can think of the book as just one dimension.

---

### The Example: One Complete Cycle

**Context:** BTC 15-minute market. Currently 7 minutes into the window. BTC is roughly flat, odds sitting at ~50/50.

**Current order book (YES token):**
```
ASKS:  $0.54 — 500 shares
       $0.53 — 300 shares
       $0.52 — 200 shares   ← best ask
------- spread = $0.06 ------
       $0.48 — 150 shares   ← best bid
BIDS:  $0.47 — 400 shares
       $0.46 — 200 shares
```

Natural spread is 6 cents. You step inside it.

**You post two limit orders simultaneously:**
```
BUY  YES @ $0.49   (100 shares)   ← you are now best bid
SELL YES @ $0.51   (100 shares)   ← you are now best ask
```

New book looks like:
```
ASKS:  $0.54 — 500 shares
       ...
       $0.51 — 100 shares   ← YOU (best ask)
------- spread = $0.02 ------
       $0.49 — 100 shares   ← YOU (best bid)
BIDS:  $0.48 — 150 shares
       ...
```

You've tightened the spread from 6 cents to 2 cents. You're now at the front of the queue on both sides.

---

### Scenario A: Both Sides Fill, YES Wins

Over the next few minutes:

1. A retail trader wants to sell YES (take profit). They hit your bid: **you buy 100 YES @ $0.49**. Cost: $49.00
2. Another trader wants to buy YES (thinks BTC going up). They hit your ask: **you sell 100 YES @ $0.51**. Revenue: $51.00

**Window closes. BTC finished UP. YES resolves to $1.00.**

Your P&L:
```
Bought 100 YES @ $0.49  →  cost = $49.00
Sold   100 YES @ $0.51  →  revenue = $51.00

Net position after both fills: FLAT (zero YES held)
Spread profit: $51.00 - $49.00 = +$2.00
Settlement impact: $0 (you're flat, nothing to settle)

+ Maker rebates on $49 + $51 = $100 of executed maker volume
```

**You made $2.00 + rebates. Did not care that YES won.**

---

### Scenario B: Both Sides Fill, NO Wins

Same fills as above:
1. You bought 100 YES @ $0.49 → cost $49.00
2. You sold 100 YES @ $0.51 → revenue $51.00

**Window closes. BTC finished DOWN. YES resolves to $0.00.**

```
Net position: FLAT (you sold everything you bought)
Spread profit: $51.00 - $49.00 = +$2.00
Settlement impact: $0

+ Maker rebates
```

**Exact same result. $2.00 profit. Direction irrelevant.**

This is the core magic — when both sides fill and you're flat, outcome doesn't matter.

---

### Scenario C: Only ONE Side Fills — The Dangerous Case

Same setup, you post BUY @ $0.49 and SELL @ $0.51.

Only your BUY fills: **you bought 100 YES @ $0.49.** Your sell at $0.51 never got hit.

**Window closes. NO wins. YES → $0.00.**

```
Bought 100 YES @ $0.49 → cost $49.00
Sold   0 YES           → revenue $0
Settlement: 100 shares × $0.00 = $0

Loss: -$49.00
```

This is **inventory risk** — the core danger of Strategy 1. You got partially filled and held a directional position into settlement without a hedge.

---

### How You Manage Scenario C: Inventory Skew

The bot tracks your running net position at all times:

```
net_position = yes_shares_held - no_shares_held

Flat:      net = 0    → symmetric quotes
Long YES:  net = +50  → you are exposed if NO wins
Long NO:   net = -50  → you are exposed if YES wins
```

When you're long YES (only buy side filled so far), you **skew your quotes** to offload:

```
Normal quotes:   BUY @ $0.49,  SELL @ $0.51
Long YES (skew): BUY @ $0.47,  SELL @ $0.49   ← both shifted down
```

Lowering your ask makes YES cheaper, attracting buyers who take your YES off your hands. You get back to flat faster. The cost is you might sell slightly cheaper than you would otherwise — but that's better than holding into settlement unhedged.

The further you drift from flat, the more aggressively you skew. Think of it as a rubber band pulling you back to neutral.

---

### The Cancel/Replace Loop

BTC price doesn't sit still. Your quotes go stale. A stale quote is a liability — someone will fill you at a price that no longer reflects fair value.

Every ~100ms the bot checks:

```
1. What is BTC doing on Binance right now?
2. What is Polymarket's current mid-price?
3. Are my resting orders within X cents of where they should be?

If NO  → do nothing, save the cancel/replace gas and latency
If YES → cancel both orders, calculate new fair value, post new quotes
```

Example:
```
T=0:    BTC at $84,000. You quote BUY $0.49 / SELL $0.51
T=8s:   BTC spikes to $84,200. Polymarket mid moves to $0.54
        Your $0.51 ask is now massively underpriced — someone will take it instantly
        Bot detects: current ask ($0.51) < fair_value - threshold ($0.54 - 0.01)
T=8.1s: Cancel both orders
T=8.2s: Post new: BUY $0.52 / SELL $0.56
```

If you don't cancel fast enough, that $0.51 ask gets hit when YES is really worth $0.54 — you just sold YES cheap and will lose $0.03/share at settlement. This is called **adverse selection** — the sophisticated takers know the price moved before you do.

This is exactly why the 500ms delay removal matters. Before, makers had 500ms to cancel stale quotes. Now there's no buffer. Your cancel/replace loop must be fast — target under 100ms end-to-end.

---

### The Full Revenue Picture Over One Day

Say you run this on a BTC 15-minute market. 96 windows per day. In each window you execute on average:

```
Per window:
  Spread captures:    3 round-trips × $0.02 = $0.06
  Avg position size:  100 shares per order
  Revenue per window: $6.00 spread + rebates

96 windows/day:
  Gross spread revenue: 96 × $6.00 = $576
  Minus adverse selection losses (estimate 10–15% of gross): -$70
  Plus maker rebates (proportional to volume): +$30–50
  
Net daily target: ~$500–$550 on $5,000–$10,000 deployed capital
```

These are illustrative numbers. Real performance depends on how competitive the market is, your fill rate, and how well your inventory management works.

---

### The One-Sentence Mental Model

You are a currency exchange booth at an airport. You buy dollars at $0.49 and sell them at $0.51. You don't care if the dollar goes up or down — you just need enough people walking through buying and selling that you clip the spread on every transaction. Your only risk is if too many people sell to you and nobody buys, leaving you holding a pile of dollars when the plane lands somewhere unexpected.