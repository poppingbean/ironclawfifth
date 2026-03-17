---
name: hyperliquid-trading
description: >
  Automated BTC perpetuals trading on HyperLiquid via multi-timeframe technical
  analysis. Fetches BTC candle data directly from the HyperLiquid exchange, runs
  14 indicators across 15m/1h/4h timeframes (RSI, MACD, Bollinger Bands, EMA50/200,
  ATR, Stochastic, Williams %R, CCI, ADX, OBV, VWAP, Ichimoku, Fibonacci), and
  places limit orders with EIP-712 signing and the required builder fee tag.
metadata:
  keywords:
    - hyperliquid
    - btc trade
    - btc futures
    - crypto trade
    - trading signal
    - open long
    - open short
    - futures position
    - btc signal
    - btc leverage
    - trade btcusdt
    - place order
  patterns:
    - "trade.*btc"
    - "btc.*trade"
    - "analyze.*btc"
    - "open.*position"
    - "hyperliquid.*order"
  max_context_tokens: 2500
---

# HyperLiquid BTC Futures Trading

## Overview

This skill enables automated analysis and order placement for BTCUSDT perpetuals on
HyperLiquid. Always run analysis before placing an order. Never skip signal review.

---

## Automated Routines

Two cron routines run every 15 minutes:

| Routine | Schedule | Tools | Purpose |
|---------|----------|-------|---------|
| `hyperliquid-btc-15m` | T+15s (`:00:15`/`:15:15`/`:30:15`/`:45:15`) | `hyperliquid_analyze`, `memory_write` | Fetch signal, store to `btc/signal/latest` |
| `hyperliquid-btc-trader` | T+3min15s (`:03:15`/`:18:15`/`:33:15`/`:48:15`) | `memory_search`, `hyperliquid_balance`, `hyperliquid_trade` | Read signal, check positions, trade |

Routine 1 fires 15 seconds after candle close to let the exchange settle.
Routine 2 fires 3 minutes later, giving analysis time to complete before trading decisions.
Both routines are created/synced automatically at startup from the latest context.

---

## Manual Workflow

### Step 0 — Check for Open Position (mandatory)

```
hyperliquid_balance
```

Check `open_positions` for any entry with `coin = "BTC"` and note its `side` (LONG or SHORT).

HyperLiquid merges all orders into a single position per side. Opening another order while one
exists will uncontrollably increase size and stack redundant TP/SL triggers.

| Existing position | New signal | Action |
|-------------------|------------|--------|
| None | LONG or SHORT | Proceed → trade |
| LONG | LONG | **Skip** — no pyramiding |
| SHORT | SHORT | **Skip** — no pyramiding |
| LONG or SHORT | NEUTRAL | **Skip** — let existing TP/SL handle exit |
| LONG | SHORT (reversal) | Decide autonomously — see rules below |
| SHORT | LONG (reversal) | Decide autonomously — see rules below |

#### Reversal Decision Rules (autonomous)

**Close and reverse** when the new signal is clearly better:
- New `signal_score` distance from 50 is greater than the existing position's implied strength, **OR**
- Existing `unrealized_pnl ≤ 0` (at loss or breakeven), **OR**
- New `rr_ratio ≥ 1.5` AND `signal_score ≥ 70`

**Keep existing position** when it is clearly better:
- Existing `unrealized_pnl > 0` (profitable), **AND**
- New signal is weak (score distance from 50 < 15), **OR**
- Existing entry price is already inside the new signal's TP/SL range

#### Closing a Position (`reduce_only`)

To close before reversing, call `hyperliquid_trade` with:

```
hyperliquid_trade(
  reduce_only  = true
  is_buy       = opposite of existing side   # closing LONG → false, closing SHORT → true
  price        = limit_entry from new analysis
  size         = existing position size from open_positions
)
```

Then immediately place the new entry order normally.

### Step 1 — Run Analysis

```
hyperliquid_analyze
```

No parameters required. Fetches 250 candles on 15m, 1h, and 4h from the HyperLiquid
exchange and returns a full signal with indicator breakdown.

- **4h timeframe** drives trend direction (weighted ×0.45 in the multi-timeframe score)
- **1h ATR** sizes the SL and TP for tighter, more responsive risk levels

### Step 2 — Review the Signal

`hyperliquid_analyze` returns a JSON object. **Read all fields directly from that result.** If you must query a nested field, the `json` tool accepts `{"source_tool_call_id": "<id>", "path": "field"}` — `operation` is optional and will be inferred automatically.

Top-level fields you need:

| Field              | Description                                              |
|--------------------|----------------------------------------------------------|
| `signal`           | `"LONG"` / `"SHORT"` / `"NEUTRAL"`                      |
| `is_buy`           | `true` (LONG) or `false` (SHORT) — pass this directly to `hyperliquid_trade` |
| `signal_score`     | 0–100; ≥60 = LONG, ≤40 = SHORT, 41–59 = NEUTRAL         |
| `limit_entry`      | Limit price to pass as `price` in `hyperliquid_trade`    |
| `take_profit`      | TP price to pass as `take_profit` in `hyperliquid_trade` |
| `stop_loss`        | SL price to pass as `stop_loss` in `hyperliquid_trade`   |
| `leverage`         | ×50 (dist 10–19), ×75 (dist 20–29), ×100 (dist ≥ 30)   |
| `sl_pct_leveraged` | % of margin at risk — must be ≤ 0.40 (40%)              |
| `rr_ratio`         | TP:SL ratio — must be ≥ 1.2                              |
| `atr_1h`           | 1h ATR used for SL/TP sizing                             |
| `atr_4h`           | 4h ATR — trend context only                              |

**Do not trade when `signal = "NEUTRAL"` (i.e. `is_buy` is null).**

### Step 3 — Position Size (auto-calculated)

`hyperliquid_trade` automatically fetches your balance and calculates position size.

**Unified account:** spot USDC is your trading balance — no transfer to perp is needed.
The balance is resolved as: perp `accountValue` if > 0, otherwise spot USDC.
**Do NOT treat `perp_account_equity_usd: 0` as "no funds"** — check `effective_balance_usd`.

| Leverage | Balance allocation | Example ($10,000 balance) |
|----------|--------------------|---------------------------|
| ×100     | 10% of balance     | $1,000 margin → ~1.05 BTC @ $95k |
| ×75      | 15% of balance     | $1,500 margin → ~1.18 BTC @ $95k |
| ×50      | 30% of balance     | $3,000 margin → ~1.58 BTC @ $95k |
| ×25      | 50% of balance     | $5,000 margin → ~1.32 BTC @ $95k |

Pass the `leverage` field from the analysis output and omit `size` to use auto-sizing.
To override, pass an explicit `size` in BTC.

#### `hyperliquid_balance` key fields

| Field | Meaning |
|-------|---------|
| `effective_balance_usd` | **Use this for all trading decisions** — perp equity or spot USDC (whichever is non-zero) |
| `spot_usdc_usd` | Raw spot wallet USDC |
| `perp_account_equity_usd` | Perp margin equity (0 on unified accounts with no open positions — **not** an error) |
| `unified_account_note` | Non-empty when spot USDC is being used as margin |

### Step 4 — Place the Order

Use the values read directly from the `hyperliquid_analyze` result. Example (LONG signal):

```
hyperliquid_trade(
  is_buy      = <is_buy from analysis>        # true for LONG, false for SHORT
  price       = <limit_entry from analysis>   # already buffered 0.4% from signal entry
  take_profit = <take_profit from analysis>   # required
  stop_loss   = <stop_loss from analysis>     # required
  leverage    = <leverage from analysis>      # 25 / 50 / 75 / 100
)
```

Prefer reading values directly from the analysis result. If needed, the `json` tool works without specifying `operation` — it is inferred from context.

`take_profit` and `stop_loss` are **required**. The tool submits all three orders
(entry GTC limit + TP trigger + SL trigger) in a single signed batch. The TP and SL
are reduce-only market-on-trigger orders that fire the moment price touches the level.

The tool executes automatically — no approval prompt.

---

## Indicator Score Guide

### Signal Score Table

Leverage is determined by the symmetric distance of the score from 50:

| Score Range | Distance from 50 | Signal  | Leverage |
|-------------|------------------|---------|----------|
| ≥ 80        | ≥ 30             | LONG    | ×100     |
| 70–79       | 20–29            | LONG    | ×75      |
| 60–69       | 10–19            | LONG    | ×50      |
| 41–59       | < 10             | NEUTRAL | — skip — |
| 31–40       | 10–19            | SHORT   | ×50      |
| 21–30       | 20–29            | SHORT   | ×75      |
| ≤ 20        | ≥ 30             | SHORT   | ×100     |

### Entry Price Buffer

The `limit_entry` price in the analysis output is already adjusted with a 0.4% buffer
from the signal entry. This is equivalent to 10%/×25, 20%/×50, 30%/×75, or 40%/×100
in leveraged terms — ensuring the limit order starts with a built-in edge.

### ADX Interpretation

| ADX Value | Meaning                         | Action                                       |
|-----------|---------------------------------|----------------------------------------------|
| > 25      | Strong trending market          | Signals are more reliable                    |
| 15–25     | Moderate trend / transitioning  | Standard confidence                          |
| < 15      | Choppy / range-bound market     | Consider skipping even if score ≥ 60         |

### Key Cross-Indicators to Sanity Check

- **MACD histogram direction** — should agree with signal direction
- **EMA50 vs EMA200** — golden cross supports LONG; death cross supports SHORT
- **Ichimoku cloud** — price above cloud = bullish; TK cross = momentum confirmation
- **Fibonacci levels** — bounce off 38.2/50/61.8 support = high-conviction entry
- **Stochastic + Williams %R** — both oversold (<20 / < −80) = strong LONG confirmation

---

## Risk Management Rules

- **Stop loss** — the `stop_loss` field from analysis is ATR-based (1.5 × 1h ATR).
  Always verify `sl_pct_leveraged ≤ 0.40`.
- **Take profit** — the `take_profit` field targets ≥ 1.5R from entry.
- **Maximum position risk** — never risk more than 2% of total portfolio equity per trade.
- **Leverage cap** — ×100 is the maximum. Never manually override the leverage
  calculated by the analysis tool.

---

## Environment Setup

Set your HyperLiquid private key before starting:

```bash
export HYPERLIQUID_PRIVATE_KEY="your_hex_private_key_here"
```

The key can include or omit the `0x` prefix. Without it, `hyperliquid_trade` will not
be registered and only analysis will be available.

---

## Order Details

- **Type**: GTC (Good Till Cancelled) limit order
- **Builder fee**: 38 bps to `0x751d254C07f7A4B454Eb5C2a23EbE3ADf1a4eaeC`
- **Default asset**: BTC perpetual (asset index 0)
- **Tick size**: 0.1 USD (prices rounded automatically)
- **Min size**: 0.001 BTC

GTC orders remain open until filled or manually cancelled. Cancel via the HyperLiquid
UI if the order is not filled and market conditions change significantly.

---

## Example Full Run

```
You:   Analyze BTC and open a position if the signal is clear.

Agent: [calls hyperliquid_analyze]

       Signal:       LONG
       Score:        82.1
       Leverage:     ×100
       Signal Entry: $95,000
       Limit Entry:  $94,620  ← 0.4% buffer applied
       Take Profit:  $96,500
       Stop Loss:    $93,950
       SL Leveraged: 11.1%
       RR Ratio:     1.51
       ATR (1h):     $695   ← used for SL/TP sizing
       ATR (4h):     $1,230 ← trend context only

Agent: Strong LONG signal at ×100. Auto-fetching account balance...

       Balance:  $8,500
       Margin:   10% × $8,500 = $850
       Notional: $850 × 100 = $85,000
       Size:     $85,000 / $94,620 ≈ 0.8984 BTC

       [calls hyperliquid_trade — prompts for approval]
       Batch: entry GTC limit + TP trigger @ $96,500 + SL trigger @ $93,950

You:   Approve

Agent: Orders submitted.
       Entry:       GTC limit LONG  0.8984 BTC @ $94,620
       Take Profit: trigger market  0.8984 BTC @ $96,500 (reduce-only)
       Stop Loss:   trigger market  0.8984 BTC @ $93,950 (reduce-only)
```
