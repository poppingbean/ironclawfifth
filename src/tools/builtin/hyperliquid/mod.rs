//! HyperLiquid automated futures trading tools.
//!
//! Provides two builtin tools:
//! - [`HyperliquidAnalyzeTool`]: fetches BTCUSDT multi-timeframe data from Binance Futures
//!   and runs 14 technical indicators to produce a LONG/SHORT/NEUTRAL signal with
//!   entry, take-profit, stop-loss, and leverage recommendation.
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
pub use trading::HyperliquidTradeTool;

/// Seed the HyperLiquid 15-minute trading routine if it does not already exist.
///
/// Creates a `full_job` cron routine named `"hyperliquid-btc-15m"` that fires at
/// second 15 of every 15th minute (`15 */15 * * * *`). Only seeded when
/// `HYPERLIQUID_PRIVATE_KEY` is present — if the trade tool isn't registered,
/// the routine would only be able to analyze but never place orders.
///
/// Idempotent: if the routine already exists, this is a no-op.
pub async fn seed_hyperliquid_routine(store: &std::sync::Arc<dyn crate::db::Database>) {
    if std::env::var("HYPERLIQUID_PRIVATE_KEY").is_err() {
        return;
    }

    const ROUTINE_NAME: &str = "hyperliquid-btc-15m";
    const USER_ID: &str = "default";

    match store.get_routine_by_name(USER_ID, ROUTINE_NAME).await {
        Ok(Some(_)) => return, // already exists
        Err(e) => {
            tracing::warn!("Failed to check for HyperLiquid routine: {}", e);
            return;
        }
        Ok(None) => {} // proceed to create
    }

    let schedule = "15 */15 * * * *";
    let next_fire = match crate::agent::routine::next_cron_fire(schedule, None) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!("Failed to compute next fire for HyperLiquid routine: {}", e);
            return;
        }
    };

    let routine = crate::agent::routine::Routine {
        id: uuid::Uuid::new_v4(),
        name: ROUTINE_NAME.to_string(),
        description: "Analyze BTC perpetuals and place a trade if signal is clear (every 15 min at T+15s)".to_string(),
        user_id: USER_ID.to_string(),
        enabled: true,
        trigger: crate::agent::routine::Trigger::Cron {
            schedule: schedule.to_string(),
            timezone: None,
        },
        action: crate::agent::routine::RoutineAction::FullJob {
            title: "HyperLiquid BTC 15m trade".to_string(),
            description: "Run hyperliquid_analyze. If signal is LONG or SHORT (not NEUTRAL), \
                verify sl_pct_leveraged ≤ 0.40 and rr_ratio ≥ 1.2, then call hyperliquid_trade \
                with is_buy, price (limit_entry), take_profit, stop_loss, and leverage from the \
                analysis output. Do not trade on NEUTRAL signals."
                .to_string(),
            max_iterations: 10,
            tool_permissions: vec![
                "hyperliquid_analyze".to_string(),
                "hyperliquid_trade".to_string(),
            ],
        },
        guardrails: crate::agent::routine::RoutineGuardrails {
            cooldown: std::time::Duration::from_secs(600), // 10 min cooldown between fires
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
