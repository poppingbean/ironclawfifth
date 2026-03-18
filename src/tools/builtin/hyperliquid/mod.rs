//! HyperLiquid automated futures trading tools.
//!
//! Provides three builtin tools:
//! - [`HyperliquidAnalyzeTool`]: fetches BTC multi-timeframe candle data directly from
//!   the HyperLiquid exchange and runs 14 technical indicators to produce a
//!   LONG/SHORT/NEUTRAL signal with entry, take-profit, stop-loss, and leverage recommendation.
//! - [`HyperliquidTradeTool`]: places a GTC limit order on HyperLiquid perpetuals
//!   with EIP-712 signing and the required builder fee tag.
//! - [`HyperliquidExecuteTool`]: autonomous single-call executor — reads the stored signal,
//!   checks open positions, and places/manages orders with no LLM decision-making.
//!
//! ## Routines
//!
//! Routines are created manually via `ironclaw routine create`. Two recommended routines:
//!
//! - `hyperliquid-btc-15m` (schedule `15 */15 * * * *`): calls `hyperliquid_analyze` only.
//! - `hyperliquid-btc-trader` (schedule `15 3,18,33,48 * * * *`): calls `hyperliquid_execute` only.
//!
//! ## Environment
//!
//! `HYPERLIQUID_PRIVATE_KEY` — hex-encoded secp256k1 private key (with or without `0x`
//! prefix). Required for order placement; if unset, only analysis is available.
//!
//! `HYPERLIQUID_VAULT_ADDRESS` — (optional) main wallet address when `HYPERLIQUID_PRIVATE_KEY`
//! is an agent (API) wallet key. Leave unset when using the main wallet key directly.

mod analysis;
mod execute;
pub mod indicators;
mod trading;

pub use analysis::HyperliquidAnalyzeTool;
pub use execute::HyperliquidExecuteTool;
pub use trading::{HyperliquidBalanceTool, HyperliquidTradeTool};
