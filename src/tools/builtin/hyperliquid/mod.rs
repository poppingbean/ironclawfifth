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
//! - `hyperliquid-btc-trader` (T+3 min): reads the stored signal, checks open
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
/// **Routine 1 — `hyperliquid-btc-15m`** (fires at second 15 of minutes 0,15,30,45):
///   Runs `hyperliquid_analyze` and writes the full signal JSON to workspace
///   memory at path `btc/signal/latest`. Simple, fast, no trading logic.
///
/// **Routine 2 — `hyperliquid-btc-trader`** (fires at second 15 of minutes 3,18,33,48):
///   Reads the stored signal, checks open BTC positions via `hyperliquid_balance`,
///   and places or manages orders based on position state and signal quality.
///   Fires 3 minutes after Routine 1 so the signal is always fresh.
pub async fn seed_hyperliquid_routine(store: &std::sync::Arc<dyn crate::db::Database>) {
    if std::env::var("HYPERLIQUID_PRIVATE_KEY").is_err() {
        return;
    }

    // ── Routine 1: Analyze & store signal ────────────────────────────────────
    seed_or_sync_routine(
        store,
        "hyperliquid-btc-15m",
        "Fetch BTC multi-timeframe signal from HyperLiquid and store to memory (every 15 min at T+15s).",
        "15 */15 * * * *",
        crate::agent::routine::RoutineAction::FullJob {
            title: "HyperLiquid BTC signal analysis".to_string(),
            description: "\
                FIRST ACTION: call hyperliquid_analyze now (no parameters). \
                Do not call any other tool first. Do not call memory_write before hyperliquid_analyze returns.\n\
                \n\
                After hyperliquid_analyze returns its JSON result, immediately call memory_write with:\n\
                  path    = 'btc/signal/latest'\n\
                  content = the EXACT raw JSON string returned by hyperliquid_analyze\n\
                Do not summarize, paraphrase, or modify the JSON. Copy it verbatim.\n\
                Do not write to any other path (not daily/, not logs/, nothing else).\n\
                \n\
                Those are the only two tool calls for this job. Stop after memory_write succeeds."
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
        "Read latest BTC signal from memory and place order if conditions are met (every 15 min at T+3min15s).",
        "15 3,18,33,48 * * * *",
        crate::agent::routine::RoutineAction::FullJob {
            title: "HyperLiquid BTC order placement".to_string(),
            description: "\
                Goal: read the stored signal and manage BTC perpetual positions on HyperLiquid.\n\
                \n\
                Step 1: call memory_read with path='btc/signal/latest'.\n\
                If the result has found=false or content=null, stop immediately —\n\
                the analysis routine has not run yet. Do not call any other tool.\n\
                Otherwise parse the content and extract:\n\
                signal, is_buy, signal_score, limit_entry, take_profit,\n\
                stop_loss, leverage, rr_ratio, sl_pct_leveraged.\n\
                \n\
                Step 2: call hyperliquid_balance (no parameters).\n\
                Key fields:\n\
                  effective_balance_usd    — USE THIS for all balance decisions.\n\
                                            On unified accounts perp_account_equity_usd\n\
                                            may be 0 even with funds — that is NOT an error.\n\
                  open_positions           — list of {coin, side, size, entry_price, unrealized_pnl}\n\
                Check for any entry with coin=BTC.\n\
                \n\
                Step 3: act based on the BTC position state:\n\
                \n\
                A) NO open BTC position:\n\
                   Trade only if ALL: signal=LONG or SHORT, sl_pct_leveraged<=0.50, rr_ratio>=1.2.\n\
                   Call hyperliquid_trade(\n\
                     is_buy=<is_buy from signal>, price=<limit_entry>,\n\
                     take_profit=<take_profit>, stop_loss=<stop_loss>, leverage=<leverage>\n\
                   ). Omit size — auto-calculated from effective_balance_usd.\n\
                \n\
                B) Open position SAME direction as signal: skip, do nothing.\n\
                   (e.g. existing LONG and signal=LONG — no pyramiding at high leverage.)\n\
                \n\
                C) Open position OPPOSITE direction (reversal signal):\n\
                   CLOSE AND REVERSE if any condition is true:\n\
                     - unrealized_pnl <= 0 (at loss or breakeven)\n\
                     - rr_ratio >= 1.5 AND signal_score >= 70\n\
                   To close: call hyperliquid_trade(\n\
                     reduce_only=true,\n\
                     is_buy=<opposite of existing side>,  # closing LONG->false, SHORT->true\n\
                     price=<limit_entry from signal>,\n\
                     size=<size from open_positions>\n\
                   ).\n\
                   Then open new: call hyperliquid_trade(\n\
                     is_buy=<is_buy from signal>, price=<limit_entry>,\n\
                     take_profit=<take_profit>, stop_loss=<stop_loss>, leverage=<leverage>\n\
                   ).\n\
                   KEEP existing (skip) if: unrealized_pnl > 0\n\
                     AND signal_score distance from 50 < 15 (weak signal)."
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
