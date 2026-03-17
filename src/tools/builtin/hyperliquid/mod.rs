//! HyperLiquid automated futures trading tools.
//!
//! Provides two builtin tools:
//! - [`HyperliquidAnalyzeTool`]: fetches BTC multi-timeframe candle data directly from
//!   the HyperLiquid exchange and runs 14 technical indicators to produce a
//!   LONG/SHORT/NEUTRAL signal with entry, take-profit, stop-loss, and leverage recommendation.
//! - [`HyperliquidTradeTool`]: places a GTC limit order on HyperLiquid perpetuals
//!   with EIP-712 signing and the required builder fee tag.
//!
//! ## Environment
//!
//! `HYPERLIQUID_PRIVATE_KEY` — hex-encoded secp256k1 private key (with or without `0x`
//! prefix). Required for order placement; if unset, only analysis is available.
//!
//! `HYPERLIQUID_VAULT_ADDRESS` — (optional) main wallet address when `HYPERLIQUID_PRIVATE_KEY`
//! is an agent (API) wallet key. Leave unset when using the main wallet key directly.

mod analysis;
pub mod indicators;
mod trading;

pub use analysis::HyperliquidAnalyzeTool;
pub use trading::{HyperliquidBalanceTool, HyperliquidTradeTool};

/// Cancel any active (non-terminal) jobs linked to previous runs of a routine.
///
/// Called at startup before rescheduling so stale `Pending`/`InProgress`/`Stuck`
/// jobs from the previous process lifetime are cleaned up.
async fn cancel_active_routine_jobs(
    store: &std::sync::Arc<dyn crate::db::Database>,
    routine_id: uuid::Uuid,
) {
    use crate::context::JobState;

    let runs = match store.list_routine_runs(routine_id, 50).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("Failed to list runs for routine {}: {}", routine_id, e);
            return;
        }
    };

    for run in runs {
        let Some(job_id) = run.job_id else { continue };

        let ctx = match store.get_job(job_id).await {
            Ok(Some(c)) => c,
            Ok(None) => continue,
            Err(e) => {
                tracing::warn!("Failed to fetch job {}: {}", job_id, e);
                continue;
            }
        };

        if !ctx.state.is_active() {
            continue; // already terminal — nothing to do
        }

        if let Err(e) = store
            .update_job_status(job_id, JobState::Cancelled, Some("routine rescheduled at startup"))
            .await
        {
            tracing::warn!("Failed to cancel stale routine job {}: {}", job_id, e);
        } else {
            tracing::debug!("Cancelled stale routine job {}", job_id);
        }
    }
}

/// Seed (or reschedule) the HyperLiquid 15-minute trading routine at startup.
///
/// - **First run**: creates a `full_job` cron routine named `"hyperliquid-btc-15m"`
///   that fires at second 15 of every 15th minute (`15 */15 * * * *`).
/// - **Subsequent startups**: cancels any active jobs from previous runs, then
///   updates `next_fire_at` to the next future fire time so the ticker picks it
///   up immediately without waiting for a stale past timestamp.
///
/// Only active when `HYPERLIQUID_PRIVATE_KEY` is set — if the trade tool isn't
/// registered the routine would only be able to analyze, never place orders.
pub async fn seed_hyperliquid_routine(store: &std::sync::Arc<dyn crate::db::Database>) {
    if std::env::var("HYPERLIQUID_PRIVATE_KEY").is_err() {
        return;
    }

    const ROUTINE_NAME: &str = "hyperliquid-btc-15m";
    const USER_ID: &str = "default";
    const SCHEDULE: &str = "15 */15 * * * *";

    let next_fire = match crate::agent::routine::next_cron_fire(SCHEDULE, None) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!("Failed to compute next fire for HyperLiquid routine: {}", e);
            return;
        }
    };

    // Canonical action — updated on every startup so the prompt stays in sync.
    let canonical_action = crate::agent::routine::RoutineAction::FullJob {
        title: "HyperLiquid BTC 15m trade".to_string(),
        description: "Step 1: call hyperliquid_balance. Note open_positions for any BTC entry: \
            record its side (LONG/SHORT), size, entry_price, and unrealized_pnl. \
            Step 2: call hyperliquid_analyze. Read ALL fields directly from the result. \
            Step 3: apply position rules based on existing BTC position and new signal: \
            \
            (A) No open position — check conditions and trade: \
                only proceed if signal is LONG or SHORT (not NEUTRAL), is_buy is present, \
                sl_pct_leveraged <= 0.40, rr_ratio >= 1.2. \
                Call hyperliquid_trade with is_buy, price=limit_entry, take_profit, \
                stop_loss, leverage from analysis. Omit size (auto-calculated). \
            \
            (B) Open position SAME direction as new signal — SKIP. No pyramiding. \
            \
            (C) Open position OPPOSITE direction (reversal) — decide autonomously: \
                Rule 1 — CLOSE AND REVERSE if the new signal is stronger/better: \
                  - new signal_score distance from 50 > existing implied strength, OR \
                  - existing position has unrealized_pnl <= 0 (at loss or breakeven), OR \
                  - new signal rr_ratio >= 1.5 AND signal_score >= 70. \
                  Action: call hyperliquid_trade with reduce_only=true, \
                    is_buy=opposite of existing side, price=limit_entry from new analysis, \
                    size=existing position size from open_positions. \
                  Then immediately call hyperliquid_trade again with the new direction \
                    (is_buy, price=limit_entry, take_profit, stop_loss, leverage). \
                Rule 2 — KEEP existing position if it is clearly better: \
                  - existing unrealized_pnl > 0 (profitable), AND \
                  - new signal_score is weak (distance from 50 < 15), OR \
                  - existing entry is already inside the new signal TP/SL range. \
                  Action: skip — let the existing TP handle the exit. \
            \
            IMPORTANT — balance: unified account mode. Spot USDC is the trading balance. \
            Do NOT treat perp_account_equity_usd=0 as insufficient funds — use effective_balance_usd."
            .to_string(),
        max_iterations: 10,
        tool_permissions: vec![
            "hyperliquid_analyze".to_string(),
            "hyperliquid_balance".to_string(),
            "hyperliquid_trade".to_string(),
        ],
    };
    let canonical_description = "Analyze BTC perpetuals and place a trade if signal is clear \
        (every 15 min at T+15s). Uses unified account — spot USDC is the trading balance."
        .to_string();

    match store.get_routine_by_name(USER_ID, ROUTINE_NAME).await {
        Ok(Some(existing)) => {
            // Cancel stale jobs from the previous process lifetime.
            cancel_active_routine_jobs(store, existing.id).await;

            // Reschedule and sync prompt/action to latest code on every startup.
            let mut updated = existing;
            updated.next_fire_at = next_fire;
            updated.description = canonical_description;
            updated.action = canonical_action;
            updated.updated_at = chrono::Utc::now();

            match store.update_routine(&updated).await {
                Ok(()) => tracing::info!(
                    "Rescheduled and synced HyperLiquid routine '{}' (next fire: {:?})",
                    ROUTINE_NAME,
                    updated.next_fire_at
                ),
                Err(e) => tracing::warn!("Failed to reschedule HyperLiquid routine: {}", e),
            }
        }
        Ok(None) => {
            // First run — create the routine.
            let routine = crate::agent::routine::Routine {
                id: uuid::Uuid::new_v4(),
                name: ROUTINE_NAME.to_string(),
                description: canonical_description,
                user_id: USER_ID.to_string(),
                enabled: true,
                trigger: crate::agent::routine::Trigger::Cron {
                    schedule: SCHEDULE.to_string(),
                    timezone: None,
                },
                action: canonical_action,
                guardrails: crate::agent::routine::RoutineGuardrails {
                    cooldown: std::time::Duration::from_secs(600),
                    max_concurrent: 1,
                    dedup_window: None,
                },
                notify: crate::agent::routine::NotifyConfig {
                    channel: None,
                    user: USER_ID.to_string(),
                    on_attention: true,
                    on_failure: true,
                    on_success: false,
                },
                last_run_at: None,
                next_fire_at: next_fire,
                run_count: 0,
                consecutive_failures: 0,
                state: serde_json::json!({}),
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            };

            match store.create_routine(&routine).await {
                Ok(()) => tracing::info!(
                    "Seeded HyperLiquid routine '{}' (next fire: {:?})",
                    ROUTINE_NAME,
                    routine.next_fire_at
                ),
                Err(e) => tracing::warn!("Failed to seed HyperLiquid routine: {}", e),
            }
        }
        Err(e) => {
            tracing::warn!("Failed to check for HyperLiquid routine: {}", e);
        }
    }
}
