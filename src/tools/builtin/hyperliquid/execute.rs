//! `hyperliquid_execute` — single-call autonomous trade executor.
//!
//! Reads the latest BTC signal from `btc/signal/latest` in the workspace,
//! checks open positions via [`HyperliquidBalanceTool`], applies the position
//! rules, and places or manages orders via [`HyperliquidTradeTool`] — all in
//! one tool call with no LLM decision-making required.
//!
//! Used by the `hyperliquid-btc-trader` routine so the routine prompt can be
//! a single line: "Call hyperliquid_execute. Done."

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;

use crate::context::JobContext;
use crate::tools::tool::{ApprovalRequirement, Tool, ToolError, ToolOutput, ToolRateLimitConfig};
use crate::workspace::Workspace;

use super::trading::{HyperliquidBalanceTool, HyperliquidTradeTool};

/// Autonomous BTC trade executor.
///
/// Reads signal → checks positions → places order. Zero LLM steps required.
pub struct HyperliquidExecuteTool {
    trade_tool: HyperliquidTradeTool,
    balance_tool: HyperliquidBalanceTool,
    workspace: Arc<Workspace>,
}

impl HyperliquidExecuteTool {
    pub fn new(
        private_key: String,
        vault_address: Option<String>,
        workspace: Arc<Workspace>,
    ) -> Self {
        Self {
            trade_tool: HyperliquidTradeTool::new(
                private_key.clone(),
                vault_address.clone(),
            ),
            balance_tool: HyperliquidBalanceTool::new(private_key, vault_address),
            workspace,
        }
    }
}

// ── Tool implementation ───────────────────────────────────────────────────────

#[async_trait]
impl Tool for HyperliquidExecuteTool {
    fn name(&self) -> &str {
        "hyperliquid_execute"
    }

    fn description(&self) -> &str {
        "Autonomous BTC trade executor. Reads the latest signal from workspace \
        memory (written by hyperliquid_analyze), checks open positions, applies \
        position rules, and places or manages orders — all in one call. \
        No parameters required. Used by the trader routine."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {}, "required": [] })
    }

    fn requires_approval(&self, _params: &serde_json::Value) -> ApprovalRequirement {
        ApprovalRequirement::Never
    }

    fn requires_sanitization(&self) -> bool {
        true
    }

    fn execution_timeout(&self) -> Duration {
        Duration::from_secs(60)
    }

    fn rate_limit_config(&self) -> Option<ToolRateLimitConfig> {
        Some(ToolRateLimitConfig::new(4, 60))
    }

    async fn execute(
        &self,
        _params: serde_json::Value,
        ctx: &JobContext,
    ) -> Result<ToolOutput, ToolError> {
        let start = Instant::now();

        // ── Step 1: Read signal ───────────────────────────────────────────────
        let signal_doc = match self.workspace.read("btc/signal/latest").await {
            Ok(doc) => doc,
            Err(_) => {
                return Ok(ToolOutput::success(
                    serde_json::json!({
                        "action": "skip",
                        "reason": "No signal available — hyperliquid_analyze has not run yet."
                    }),
                    start.elapsed(),
                ));
            }
        };

        let signal: serde_json::Value =
            serde_json::from_str(&signal_doc.content).map_err(|e| {
                ToolError::ExecutionFailed(format!("Failed to parse stored signal: {e}"))
            })?;

        let sig_name = signal
            .get("signal")
            .and_then(|v| v.as_str())
            .unwrap_or("NEUTRAL");

        // Skip if neutral.
        if sig_name == "NEUTRAL" {
            return Ok(ToolOutput::success(
                serde_json::json!({ "action": "skip", "reason": "Signal is NEUTRAL" }),
                start.elapsed(),
            ));
        }

        let is_buy = match signal.get("is_buy").and_then(|v| v.as_bool()) {
            Some(b) => b,
            None => {
                return Ok(ToolOutput::success(
                    serde_json::json!({ "action": "skip", "reason": "is_buy is null (NEUTRAL)" }),
                    start.elapsed(),
                ));
            }
        };

        let limit_entry = signal
            .get("limit_entry")
            .and_then(|v| v.as_f64())
            .ok_or_else(|| ToolError::ExecutionFailed("Signal missing limit_entry".to_string()))?;
        let take_profit = signal
            .get("take_profit")
            .and_then(|v| v.as_f64())
            .ok_or_else(|| ToolError::ExecutionFailed("Signal missing take_profit".to_string()))?;
        let stop_loss = signal
            .get("stop_loss")
            .and_then(|v| v.as_f64())
            .ok_or_else(|| ToolError::ExecutionFailed("Signal missing stop_loss".to_string()))?;
        let leverage = signal
            .get("leverage")
            .and_then(|v| v.as_u64())
            .unwrap_or(20) as u32;
        let rr_ratio = signal
            .get("rr_ratio")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        let sl_pct = signal
            .get("sl_pct_leveraged")
            .and_then(|v| v.as_f64())
            .unwrap_or(1.0);
        let signal_score = signal
            .get("signal_score")
            .and_then(|v| v.as_f64())
            .unwrap_or(50.0);

        // Gate checks.
        if sl_pct > 0.50 {
            return Ok(ToolOutput::success(
                serde_json::json!({
                    "action": "skip",
                    "reason": format!("sl_pct_leveraged {sl_pct:.3} exceeds 0.50 limit")
                }),
                start.elapsed(),
            ));
        }
        if rr_ratio < 1.2 {
            return Ok(ToolOutput::success(
                serde_json::json!({
                    "action": "skip",
                    "reason": format!("rr_ratio {rr_ratio:.2} below 1.2 minimum")
                }),
                start.elapsed(),
            ));
        }

        // ── Step 2: Check open positions ──────────────────────────────────────
        let balance_out = self
            .balance_tool
            .execute(serde_json::json!({}), ctx)
            .await?;

        let open_positions = balance_out
            .result
            .get("open_positions")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        // Find open BTC position.
        let btc_pos = open_positions.iter().find(|p| {
            p.get("coin")
                .and_then(|v| v.as_str())
                .map(|c| c.eq_ignore_ascii_case("BTC"))
                .unwrap_or(false)
        });

        // ── Step 3: Apply position rules ──────────────────────────────────────
        let new_signal_is_long = is_buy;

        match btc_pos {
            None => {
                // No existing position — trade if conditions met (already checked above).
                let trade_params = serde_json::json!({
                    "is_buy": is_buy,
                    "price": limit_entry,
                    "take_profit": take_profit,
                    "stop_loss": stop_loss,
                    "leverage": leverage,
                });
                let trade_out = self.trade_tool.execute(trade_params, ctx).await?;
                Ok(ToolOutput::success(
                    serde_json::json!({
                        "action": "trade_placed",
                        "signal": sig_name,
                        "trade": trade_out.result
                    }),
                    start.elapsed(),
                ))
            }

            Some(pos) => {
                let existing_is_long = pos
                    .get("side")
                    .and_then(|v| v.as_str())
                    .map(|s| s == "LONG")
                    .unwrap_or(false);
                let unrealized_pnl = pos
                    .get("unrealized_pnl")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);
                let existing_size = pos
                    .get("size")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);

                if existing_is_long == new_signal_is_long {
                    // Same direction — no pyramiding.
                    return Ok(ToolOutput::success(
                        serde_json::json!({
                            "action": "skip",
                            "reason": "Open position already in same direction — no pyramiding"
                        }),
                        start.elapsed(),
                    ));
                }

                // Opposite direction (reversal).
                let score_dist = (signal_score - 50.0).abs();
                let should_reverse = unrealized_pnl <= 0.0
                    || (rr_ratio >= 1.5 && signal_score >= 70.0);
                let should_keep =
                    unrealized_pnl > 0.0 && score_dist < 15.0;

                if should_keep && !should_reverse {
                    return Ok(ToolOutput::success(
                        serde_json::json!({
                            "action": "skip",
                            "reason": format!(
                                "Keeping existing position: profitable (pnl={unrealized_pnl:.2}) \
                                and new signal is weak (score dist={score_dist:.1})"
                            )
                        }),
                        start.elapsed(),
                    ));
                }

                if !should_reverse {
                    return Ok(ToolOutput::success(
                        serde_json::json!({
                            "action": "skip",
                            "reason": "Reversal conditions not met"
                        }),
                        start.elapsed(),
                    ));
                }

                // Close existing then open new.
                let close_is_buy = !existing_is_long; // closing LONG → sell, closing SHORT → buy
                let close_params = serde_json::json!({
                    "reduce_only": true,
                    "is_buy": close_is_buy,
                    "price": limit_entry,
                    "size": existing_size,
                });
                let close_out = self.trade_tool.execute(close_params, ctx).await?;

                let open_params = serde_json::json!({
                    "is_buy": is_buy,
                    "price": limit_entry,
                    "take_profit": take_profit,
                    "stop_loss": stop_loss,
                    "leverage": leverage,
                });
                let open_out = self.trade_tool.execute(open_params, ctx).await?;

                Ok(ToolOutput::success(
                    serde_json::json!({
                        "action": "reversed",
                        "signal": sig_name,
                        "close": close_out.result,
                        "open": open_out.result
                    }),
                    start.elapsed(),
                ))
            }
        }
    }
}
