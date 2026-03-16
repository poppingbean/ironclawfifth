---
name: hyperliquid-trading
version: 0.2.0
description: >
  Automated BTC perpetuals trading on HyperLiquid via multi-timeframe technical
  analysis. Fetches BTCUSDT data from Binance Futures, runs 14 indicators across
  15m/1h/4h timeframes (RSI, MACD, Bollinger Bands, EMA50/200, ATR, Stochastic,
  Williams %R, CCI, ADX, OBV, VWAP, Ichimoku, Fibonacci), and places limit orders
  with EIP-712 signing and the required builder fee tag.
activation:
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

## Workflow

### Step 1 — Run Analysis

```
hyperliquid_analyze
```

No parameters required. Fetches 500 candles on 15m, 1h, and 4h from Binance Futures
and returns a full signal with indicator breakdown.

- **4h timeframe** drives trend direction (weighted ×0.45 in the multi-timeframe score)
- **1h ATR** sizes the SL and TP for tighter, more responsive risk levels

### Step 2 — Review the Signal

Check these fields before proceeding:

| Field              | Description                                              |
|--------------------|----------------------------------------------------------|
| `signal`           | LONG / SHORT / NEUTRAL                                   |
| `signal_score`     | 0–100; ≥60 = LONG, ≤40 = SHORT, 41–59 = NEUTRAL         |
| `leverage`         | ×50 (dist 10–19), ×75 (dist 20–29), ×100 (dist ≥ 30)   |
| `sl_pct_leveraged` | % of margin at risk — must be ≤ 0.40 (40%)              |
| `rr_ratio`         | TP:SL ratio — must be ≥ 1.2                              |
| `atr_1h`           | 1h ATR used for SL/TP sizing                             |
| `atr_4h`           | 4h ATR — trend context only                              |

**Do not trade when `signal = NEUTRAL`.**

### Step 3 — Position Size (auto-calculated)

`hyperliquid_trade` automatically fetches your HyperLiquid account balance and
calculates position size based on the leverage tier:

| Leverage | Balance allocation | Example ($10,000 balance) |
|----------|--------------------|---------------------------|
| ×100     | 10% of balance     | $1,000 margin → ~1.05 BTC @ $95k |
| ×75      | 15% of balance     | $1,500 margin → ~1.18 BTC @ $95k |
| ×50      | 30% of balance     | $3,000 margin → ~1.58 BTC @ $95k |
| ×25      | 50% of balance     | $5,000 margin → ~1.32 BTC @ $95k |

Pass the `leverage` field from the analysis output and omit `size` to use auto-sizing.
To override, pass an explicit `size` in BTC.

### Step 4 — Place the Order

```
hyperliquid_trade(
  is_buy      = true          # or false for SHORT
  price       = limit_entry   # from analysis output
  take_profit = take_profit   # from analysis output — required
  stop_loss   = stop_loss     # from analysis output — required
  leverage    = 75            # from analysis output
)
```

`take_profit` and `stop_loss` are **required**. The tool submits all three orders
(entry GTC limit + TP trigger + SL trigger) in a single signed batch. The TP and SL
are reduce-only market-on-trigger orders that fire the moment price touches the level.

The tool will prompt for explicit approval before executing.

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
