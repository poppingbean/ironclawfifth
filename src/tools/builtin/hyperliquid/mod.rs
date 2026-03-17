//! HyperLiquid automated futures trading tools.
//!
//! Provides two builtin tools:
//! - [`HyperliquidAnalyzeTool`]: fetches BTC multi-timeframe candle data directly from
//!   the HyperLiquid exchange and runs 14 technical indicators to produce a
//!   LONG/SHORT/NEUTRAL signal with entry, take-profit, stop-loss, and leverage recommendation.
//! - [`HyperliquidTradeTool`]: places a GTC limit order on HyperLiquid perpetuals
//!   with EIP-712 signing and the required builder fee tag.
//!
//! ## Routines
//!
//! Two complementary cron routines are seeded at startup:
//!
//! - `hyperliquid-btc-15m` (T+15s): runs `hyperliquid_analyze` and writes the
//!   signal to workspace memory at `btc/signal/latest`.
//! - `hyperliquid-btc-trader` (T+45s): reads the stored signal, checks open
//!   positions via `hyperliquid_balance`, and places or manages orders.
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
            continue;
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

/// Seed or sync a single routine. Creates on first run; updates prompt and reschedules on
/// subsequent startups so the context always reflects the latest code.
async fn seed_or_sync_routine(
    store: &std::sync::Arc<dyn crate::db::Database>,
    name: &str,
    description: &str,
    schedule: &str,
    action: crate::agent::routine::RoutineAction,
    cooldown_secs: u64,
) {
    let user_id = "default";

    let next_fire = match crate::agent::routine::next_cron_fire(schedule, None) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!("Failed to compute next fire for routine '{}': {}", name, e);
            return;
        }
    };

    match store.get_routine_by_name(user_id, name).await {
        Ok(Some(existing)) => {
            cancel_active_routine_jobs(store, existing.id).await;

            let mut updated = existing;
            updated.next_fire_at = next_fire;
            updated.description = description.to_string();
            updated.action = action;
            updated.updated_at = chrono::Utc::now();

            match store.update_routine(&updated).await {
                Ok(()) => tracing::info!("Synced routine '{}' (next: {:?})", name, updated.next_fire_at),
                Err(e) => tracing::warn!("Failed to sync routine '{}': {}", name, e),
            }
        }
        Ok(None) => {
            let routine = crate::agent::routine::Routine {
                id: uuid::Uuid::new_v4(),
                name: name.to_string(),
                description: description.to_string(),
                user_id: user_id.to_string(),
                enabled: true,
                trigger: crate::agent::routine::Trigger::Cron {
                    schedule: schedule.to_string(),
                    timezone: None,
                },
                action,
                guardrails: crate::agent::routine::RoutineGuardrails {
                    cooldown: std::time::Duration::from_secs(cooldown_secs),
                    max_concurrent: 1,
                    dedup_window: None,
                },
                notify: crate::agent::routine::NotifyConfig {
                    channel: None,
                    user: user_id.to_string(),
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
                Ok(()) => tracing::info!("Seeded routine '{}' (next: {:?})", name, routine.next_fire_at),
                Err(e) => tracing::warn!("Failed to seed routine '{}': {}", name, e),
            }
        }
        Err(e) => tracing::warn!("Failed to check routine '{}': {}", name, e),
    }
}

/// Seed the two HyperLiquid cron routines at startup.
///
/// **Routine 1 — `hyperliquid-btc-15m`** (fires at T+15s every 15 min):
///   Runs `hyperliquid_analyze` and writes the full signal JSON to workspace
///   memory at path `btc/signal/latest`. Simple, fast, no trading logic.
///
/// **Routine 2 — `hyperliquid-btc-trader`** (fires at T+45s every 15 min):
///   Reads the stored signal, checks open BTC positions via `hyperliquid_balance`,
///   and places or manages orders based on position state and signal quality.
///   Fires 30 seconds after Routine 1 so the signal is always fresh.
pub async fn seed_hyperliquid_routine(store: &std::sync::Arc<dyn crate::db::Database>) {
    if std::env::var("HYPERLIQUID_PRIVATE_KEY").is_err() {
        return;
    }

    // ── Routine 1: Analyze & store signal ────────────────────────────────────
    seed_or_sync_routine(
        store,
        "hyperliquid-btc-15m",
        "Fetch BTC multi-timeframe signal from HyperLiquid and store it in memory (every 15 min at T+15s).",
        "15 */15 * * * *",
        crate::agent::routine::RoutineAction::FullJob {
            title: "HyperLiquid BTC signal analysis".to_string(),
            description: "Call hyperliquid_analyze (no parameters). \
                Then call memory_write with path='btc/signal/latest' and the FULL \
                JSON result as the content. Do nothing else."
                .to_string(),
            max_iterations: 3,
            tool_permissions: vec![
                "hyperliquid_analyze".to_string(),
                "memory_write".to_string(),
            ],
        },
        600,
    )
    .await;

    // ── Routine 2: Read signal & trade ────────────────────────────────────────
    seed_or_sync_routine(
        store,
        "hyperliquid-btc-trader",
        "Read latest BTC signal from memory and place order if conditions are met (every 15 min at T+45s).",
        "45 */15 * * * *",
        crate::agent::routine::RoutineAction::FullJob {
            title: "HyperLiquid BTC order placement".to_string(),
            description: "1. Call memory_read with path='btc/signal/latest'. \
                Parse the signal fields: signal, is_buy, signal_score, limit_entry, \
                take_profit, stop_loss, leverage, rr_ratio, sl_pct_leveraged. \
                2. Call hyperliquid_balance. Note open_positions for coin=BTC \
                (record side, size, entry_price, unrealized_pnl). \
                Also note effective_balance_usd — this is the correct trading balance \
                even when perp_account_equity_usd is 0 (unified account). \
                3. Act based on position state: \
                NO open BTC position: trade if signal=LONG or SHORT AND \
                  sl_pct_leveraged<=0.50 AND rr_ratio>=1.2. \
                  Call hyperliquid_trade(is_buy, price=limit_entry, take_profit, \
                  stop_loss, leverage). Omit size (auto-calculated). \
                SAME direction as open position: skip. \
                OPPOSITE direction (reversal): \
                  CLOSE AND REVERSE if unrealized_pnl<=0, OR \
                    (rr_ratio>=1.5 AND signal_score>=70). \
                    Call hyperliquid_trade(reduce_only=true, is_buy=opposite_of_existing, \
                    price=limit_entry, size=existing_size). \
                    Then call hyperliquid_trade(is_buy, price=limit_entry, \
                    take_profit, stop_loss, leverage). \
                  KEEP existing if unrealized_pnl>0 AND signal is weak (score dist <15)."
                .to_string(),
            max_iterations: 6,
            tool_permissions: vec![
                "memory_read".to_string(),
                "hyperliquid_balance".to_string(),
                "hyperliquid_trade".to_string(),
            ],
        },
        600,
    )
    .await;
}
