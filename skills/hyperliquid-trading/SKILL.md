---
name: hyperliquid-trading
description: >
  Automated BTC perpetuals trading on HyperLiquid via multi-timeframe technical
  analysis. Fetches BTC candle data directly from the HyperLiquid exchange, runs
  14 indicators across 15m/1h/4h timeframes, and places limit orders with EIP-712
  signing and the required builder fee tag.
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

## Automated Routines

Two cron routines run every 15 minutes (created/synced at startup):

| Routine | Schedule | Tool | Purpose |
|---------|----------|------|---------|
| `hyperliquid-btc-15m` | `:00:15` / `:15:15` / `:30:15` / `:45:15` | `hyperliquid_analyze` | Fetch signal — **tool auto-saves to `btc/signal/latest`** |
| `hyperliquid-btc-trader` | `:03:15` / `:18:15` / `:33:15` / `:48:15` | `hyperliquid_execute` | Single-call executor: reads signal → checks positions → trades |

Routine 1 fires 15s after candle close. Routine 2 fires 3 min later.

---

## Tools

### `hyperliquid_analyze` — no parameters

Returns signal JSON **and automatically writes it to `btc/signal/latest`**:

| Field | Values |
|-------|--------|
| `signal` | `"LONG"` / `"SHORT"` / `"NEUTRAL"` |
| `is_buy` | `true` / `false` / `null` — pass directly to `hyperliquid_trade` |
| `signal_score` | 0–100 (≥60=LONG, ≤40=SHORT, 41–59=NEUTRAL) |
| `limit_entry` | → `price` in `hyperliquid_trade` (0.4% buffer pre-applied) |
| `take_profit` | → `take_profit` in `hyperliquid_trade` |
| `stop_loss` | → `stop_loss` in `hyperliquid_trade` |
| `leverage` | 20 / 30 / 40 → `leverage` in `hyperliquid_trade` (HL max is ×40) |
| `rr_ratio` | Must be ≥ 1.2 to trade |
| `sl_pct_leveraged` | Must be ≤ 0.50 to trade |

Score distance from 50 → leverage: 10–19=×20, 20–29=×30, ≥30=×40 (HyperLiquid BTC max is ×40).

### `hyperliquid_balance` — no parameters

Key output fields:

| Field | Note |
|-------|------|
| `effective_balance_usd` | **Use this** — perp equity OR spot USDC (whichever is non-zero) |
| `perp_account_equity_usd` | 0 on unified accounts with no open positions — **not** an error |
| `spot_usdc_usd` | Spot USDC (used as margin on unified accounts, no transfer needed) |
| `open_positions` | List of `{coin, side, size, entry_price, unrealized_pnl}` |

### `hyperliquid_execute` — no parameters

Autonomous executor used by the trader routine. Reads `btc/signal/latest`, checks
open positions, and places/manages orders in one call. Returns:
`{"action": "trade_placed"|"reversed"|"skip", "reason": "..."}`.

---

### `hyperliquid_trade` — required: `price`

```
hyperliquid_trade(
  is_buy      = <is_buy from analyze>       # boolean: true=LONG, false=SHORT
  price       = <limit_entry from analyze>  # required
  take_profit = <take_profit from analyze>  # required for new positions
  stop_loss   = <stop_loss from analyze>    # required for new positions
  leverage    = <leverage from analyze>     # 20 / 30 / 40 — auto-sizes position
  # size omitted → auto-calculated from effective_balance_usd
)
```

Submits entry GTC limit + TP trigger + SL trigger in one signed batch.

**To close before reversing** (`reduce_only=true`):
```
hyperliquid_trade(
  reduce_only = true
  is_buy      = <opposite of existing side>   # closing LONG → false
  price       = <limit_entry from analyze>
  size        = <size from open_positions>    # required when reduce_only
  # no take_profit / stop_loss needed
)
```

---

## Position Rules

Check `open_positions` from `hyperliquid_balance` for `coin = "BTC"`:

| Open BTC position | New signal | Action |
|-------------------|------------|--------|
| None | LONG or SHORT | Trade if `rr_ratio ≥ 1.2` and `sl_pct_leveraged ≤ 0.50` |
| None | NEUTRAL | **Skip** — no signal |
| Same direction | any | **Skip** — no pyramiding |
| Any | NEUTRAL | **Skip** — let TP/SL handle exit |
| Opposite (reversal) | LONG or SHORT | Decide — see below |

**Reversal — close and reverse** if any:
- `unrealized_pnl ≤ 0` (at loss or breakeven)
- `rr_ratio ≥ 1.5` AND `signal_score ≥ 70`

**Reversal — keep existing** if all:
- `unrealized_pnl > 0` AND signal is weak (`|score − 50| < 15`)
