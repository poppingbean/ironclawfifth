//! HyperLiquid analysis tool — fetches BTCUSDT multi-timeframe OHLCV from
//! Binance Futures and runs 14 technical indicators to produce a trading signal.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::Utc;
use serde::Serialize;

use crate::context::JobContext;
use crate::tools::tool::{ApprovalRequirement, Tool, ToolError, ToolOutput, ToolRateLimitConfig};
use crate::workspace::Workspace;

use super::indicators::{self, Candle, IndicatorSet};

// ── Constants ─────────────────────────────────────────────────────────────────

const BINANCE_KLINES: &str = "https://fapi.binance.com/fapi/v1/klines";
const SYMBOL: &str = "BTCUSDT";
const CANDLE_LIMIT: u32 = 500;

/// Timeframe identifiers matching Binance interval strings.
#[derive(Debug, Clone, Copy)]
enum Tf {
    M15,
    H1,
    H4,
}

impl Tf {
    fn as_str(self) -> &'static str {
        match self {
            Self::M15 => "15m",
            Self::H1 => "1h",
            Self::H4 => "4h",
        }
    }
}

// ── Output types ──────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct TfSnapshot {
    score: f64,
    indicators: IndicatorSet,
}

#[derive(Debug, Serialize)]
struct AnalysisOutput {
    signal: &'static str,
    /// Ready-to-pass boolean for `hyperliquid_trade`: true = LONG, false = SHORT.
    /// null for NEUTRAL signals — do NOT call hyperliquid_trade when this is null.
    is_buy: Option<bool>,
    signal_score: f64,
    /// Raw signal entry price (current close of the 4h candle).
    signal_entry: f64,
    /// Limit order price with 0.4% buffer applied.
    limit_entry: f64,
    take_profit: f64,
    stop_loss: f64,
    leverage: u32,
    /// Stop-loss distance × leverage / entry, as a fraction (e.g. 0.12 = 12 %).
    sl_pct_leveraged: f64,
    rr_ratio: f64,
    /// 4h ATR — used for trend context only.
    atr_4h: f64,
    /// 1h ATR — used for SL/TP sizing.
    atr_1h: f64,
    timeframes: TimeframeScores,
    computed_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    neutral_reason: Option<&'static str>,
}

#[derive(Debug, Serialize)]
struct TimeframeScores {
    #[serde(rename = "15m")]
    m15: TfSnapshot,
    #[serde(rename = "1h")]
    h1: TfSnapshot,
    #[serde(rename = "4h")]
    h4: TfSnapshot,
}

// ── Tool struct ───────────────────────────────────────────────────────────────

/// Fetches BTCUSDT OHLCV from Binance Futures and produces a trading signal
/// using 14 technical indicators across 15m / 1h / 4h timeframes.
///
/// When a `Workspace` is provided, each signal is appended to
/// `hyperliquid/signal-history.md` for historical review.
pub struct HyperliquidAnalyzeTool {
    client: reqwest::Client,
    workspace: Option<Arc<Workspace>>,
}

impl HyperliquidAnalyzeTool {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap_or_default();
        Self { client, workspace: None }
    }

    pub fn with_workspace(mut self, workspace: Arc<Workspace>) -> Self {
        self.workspace = Some(workspace);
        self
    }
}

impl Default for HyperliquidAnalyzeTool {
    fn default() -> Self {
        Self::new()
    }
}

// ── Binance fetch ─────────────────────────────────────────────────────────────

/// Fetch candles from Binance Futures klines endpoint.
async fn fetch_candles(
    client: &reqwest::Client,
    tf: Tf,
) -> Result<Vec<Candle>, ToolError> {
    let url = format!(
        "{}?symbol={}&interval={}&limit={}",
        BINANCE_KLINES,
        SYMBOL,
        tf.as_str(),
        CANDLE_LIMIT,
    );

    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| ToolError::ExternalService(format!("Binance request failed ({tf:?}): {e}")))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(ToolError::ExternalService(format!(
            "Binance returned {status} for {tf:?}: {body}"
        )));
    }

    // Binance klines: each element is an array
    // [0]=open_time [1]=open [2]=high [3]=low [4]=close [5]=volume ...
    let raw: Vec<serde_json::Value> = resp
        .json()
        .await
        .map_err(|e| ToolError::ExternalService(format!("Binance parse error ({tf:?}): {e}")))?;

    let candles: Vec<Candle> = raw
        .iter()
        .filter_map(|row| {
            let arr = row.as_array()?;
            let parse = |i: usize| -> Option<f64> {
                arr.get(i)?.as_str()?.parse().ok()
            };
            Some(Candle {
                open: parse(1)?,
                high: parse(2)?,
                low: parse(3)?,
                close: parse(4)?,
                volume: parse(5)?,
            })
        })
        .collect();

    if candles.is_empty() {
        return Err(ToolError::ExternalService(format!(
            "Binance returned empty klines for {tf:?}"
        )));
    }
    Ok(candles)
}

// ── Signal scoring ────────────────────────────────────────────────────────────

/// Score a single timeframe's indicator set against the current close price.
///
/// Returns a value in [0.0, 100.0] where:
/// - ~100 = strongly bullish (LONG)
/// - ~50  = neutral
/// - ~0   = strongly bearish (SHORT)
fn score_indicators(ind: &IndicatorSet, close: f64, obv_up: bool) -> f64 {
    let mut pts: f64 = 0.0;

    // RSI (max ±10)
    if let Some(r) = ind.rsi {
        pts += if r < 30.0 {
            10.0
        } else if r < 45.0 {
            5.0
        } else if r > 70.0 {
            -10.0
        } else if r > 55.0 {
            -5.0
        } else {
            0.0
        };
    }

    // MACD histogram (max ±10)
    if let Some(ref m) = ind.macd {
        if m.hist > 0.0 {
            pts += if m.hist.abs() > m.signal.abs() * 0.1 {
                10.0
            } else {
                5.0
            };
        } else {
            pts += if m.hist.abs() > m.signal.abs() * 0.1 {
                -10.0
            } else {
                -5.0
            };
        }
    }

    // Bollinger Bands %B (max ±8)
    if let Some(ref bb) = ind.bb {
        let range = bb.upper - bb.lower;
        if range > 0.0 {
            let pct_b = (close - bb.lower) / range * 100.0;
            pts += if pct_b < 0.0 {
                8.0
            } else if pct_b < 20.0 {
                4.0
            } else if pct_b > 100.0 {
                -8.0
            } else if pct_b > 80.0 {
                -4.0
            } else {
                0.0
            };
        }
    }

    // EMA50 vs EMA200 — golden/death cross regime (max ±8)
    if let (Some(e50), Some(e200)) = (ind.ema50, ind.ema200) {
        pts += if e50 > e200 { 8.0 } else { -8.0 };
    }

    // Close vs EMA50 (max ±6)
    if let Some(e50) = ind.ema50 {
        pts += if close > e50 { 6.0 } else { -6.0 };
    }

    // Stochastic %K (max ±8)
    if let Some(ref s) = ind.stoch {
        pts += if s.k < 20.0 {
            8.0
        } else if s.k < 30.0 {
            4.0
        } else if s.k > 80.0 {
            -8.0
        } else if s.k > 70.0 {
            -4.0
        } else {
            0.0
        };
    }

    // Williams %R (max ±6)
    if let Some(wr) = ind.williams_r {
        pts += if wr < -80.0 {
            6.0
        } else if wr < -60.0 {
            3.0
        } else if wr > -20.0 {
            -6.0
        } else if wr > -40.0 {
            -3.0
        } else {
            0.0
        };
    }

    // CCI (max ±6)
    if let Some(c) = ind.cci {
        pts += if c < -100.0 {
            6.0
        } else if c < -50.0 {
            3.0
        } else if c > 100.0 {
            -6.0
        } else if c > 50.0 {
            -3.0
        } else {
            0.0
        };
    }

    // OBV direction (max ±4)
    pts += if obv_up { 4.0 } else { -4.0 };

    // VWAP vs close (max ±4)
    if let Some(vw) = ind.vwap {
        pts += if close > vw { 4.0 } else { -4.0 };
    }

    // Ichimoku (max ±10)
    if let Some(ref ichi) = ind.ichimoku {
        if ichi.price_above_cloud {
            pts += 8.0;
        } else if ichi.price_below_cloud {
            pts -= 8.0;
        }
        if ichi.tk_cross_bull {
            pts += 4.0;
        } else if ichi.tk_cross_bear {
            pts -= 4.0;
        }
    }

    // Fibonacci proximity (max ±8)
    // Only score when price is within 1% of a key fib level (38.2 / 50.0 / 61.8)
    if let Some(ref fib) = ind.fibonacci {
        let key_levels = [38.2_f64, 50.0, 61.8];
        if key_levels
            .iter()
            .any(|&l| (fib.nearest_level_pct - l).abs() < 0.1)
        {
            // Nearest level is a major fib level
            let nearest_price = fib.swing_high - (fib.swing_high - fib.swing_low) * fib.nearest_level_pct / 100.0;
            let proximity_pct = (close - nearest_price).abs() / close * 100.0;
            if proximity_pct <= 1.0 {
                pts += if fib.price_above_nearest { 8.0 } else { -8.0 };
            }
        }
    }

    // ADX — multiplier, not direct vote
    let adx_mult = match ind.adx.map(|a| a as u32) {
        Some(a) if a >= 25 => 1.2,
        Some(a) if a < 15 => 0.8,
        _ => 1.0,
    };
    pts *= adx_mult;

    // Normalize: raw max ≈ 106 (88 × 1.2)
    ((pts / 106.0) * 50.0 + 50.0).clamp(0.0, 100.0)
}

// ── Leverage / signal helpers ─────────────────────────────────────────────────

/// Map signal score to leverage using symmetric distance from 50.
///
/// | Distance from 50 | LONG score | SHORT score | Leverage |
/// |------------------|------------|-------------|----------|
/// | ≥ 30             | ≥ 80       | ≤ 20        | ×100     |
/// | 20–29            | 70–79      | 21–30       | ×75      |
/// | 10–19            | 60–69      | 31–40       | ×50      |
/// | < 10             | —          | —           | ×25 (neutral-adjacent) |
fn leverage_from_score(score: f64) -> u32 {
    let dist = (score - 50.0).abs();
    if dist >= 30.0 {
        100
    } else if dist >= 20.0 {
        75
    } else {
        50 // dist 10-19 (signal band but below x75 threshold)
    }
}

/// Derive signal direction from final score.
/// NEUTRAL band is 41..=59 (distance < 10 from 50).
fn signal_from_score(score: f64) -> Option<bool> {
    if score >= 60.0 {
        Some(true) // LONG
    } else if score <= 40.0 {
        Some(false) // SHORT
    } else {
        None // NEUTRAL
    }
}

// ── Tool implementation ───────────────────────────────────────────────────────

#[async_trait]
impl Tool for HyperliquidAnalyzeTool {
    fn name(&self) -> &str {
        "hyperliquid_analyze"
    }

    fn description(&self) -> &str {
        "Fetches BTCUSDT multi-timeframe OHLCV from Binance Futures (15m, 1h, 4h), \
        computes 14 technical indicators per timeframe (RSI, MACD, Bollinger Bands, \
        EMA50/200, ATR, Stochastic, Williams %R, CCI, ADX, OBV, VWAP, Ichimoku, \
        Fibonacci retracements), and returns a LONG/SHORT/NEUTRAL signal with \
        entry price, take-profit, stop-loss, and leverage recommendation (×50/×75/×100). \
        4h timeframe drives trend direction (weighted ×0.45); 1h ATR sizes the SL/TP. \
        No parameters required — always analyzes BTCUSDT."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    fn requires_approval(&self, _params: &serde_json::Value) -> ApprovalRequirement {
        ApprovalRequirement::Never
    }

    fn requires_sanitization(&self) -> bool {
        true
    }

    fn execution_timeout(&self) -> Duration {
        Duration::from_secs(45)
    }

    fn rate_limit_config(&self) -> Option<ToolRateLimitConfig> {
        Some(ToolRateLimitConfig::new(10, 100))
    }

    async fn execute(
        &self,
        _params: serde_json::Value,
        _ctx: &JobContext,
    ) -> Result<ToolOutput, ToolError> {
        let start = Instant::now();

        // Fetch all three timeframes in parallel
        let (r15, r1h, r4h) = tokio::join!(
            fetch_candles(&self.client, Tf::M15),
            fetch_candles(&self.client, Tf::H1),
            fetch_candles(&self.client, Tf::H4),
        );
        let candles_15m = r15?;
        let candles_1h = r1h?;
        let candles_4h = r4h?;

        // Compute indicators
        let ind_15m = indicators::compute_all(&candles_15m);
        let ind_1h = indicators::compute_all(&candles_1h);
        let ind_4h = indicators::compute_all(&candles_4h);

        let obv_15m = indicators::obv_rising(&candles_15m);
        let obv_1h = indicators::obv_rising(&candles_1h);
        let obv_4h = indicators::obv_rising(&candles_4h);

        let close_4h = candles_4h
            .last()
            .ok_or_else(|| ToolError::ExternalService("No 4h candles".to_string()))?
            .close;

        // Per-timeframe scores
        let s15 = score_indicators(&ind_15m, candles_15m.last().map(|c| c.close).unwrap_or(close_4h), obv_15m);
        let s1h = score_indicators(&ind_1h, candles_1h.last().map(|c| c.close).unwrap_or(close_4h), obv_1h);
        let s4h = score_indicators(&ind_4h, close_4h, obv_4h);

        // Multi-timeframe weighted score
        let final_score = s15 * 0.20 + s1h * 0.35 + s4h * 0.45;

        let is_long_opt = signal_from_score(final_score);

        // 4h ATR for trend context; 1h ATR for SL/TP sizing (tighter, more responsive)
        let atr_4h = ind_4h.atr.unwrap_or(close_4h * 0.005); // fallback: 0.5% of price
        let atr_1h = ind_1h.atr.unwrap_or(close_4h * 0.003); // fallback: 0.3% of price

        let signal_entry = close_4h;
        let leverage = if is_long_opt.is_some() {
            leverage_from_score(final_score)
        } else {
            0
        };

        if is_long_opt.is_none() {
            // NEUTRAL — return early with no trade
            let output = AnalysisOutput {
                signal: "NEUTRAL",
                is_buy: None,
                signal_score: final_score,
                signal_entry,
                limit_entry: signal_entry,
                take_profit: signal_entry,
                stop_loss: signal_entry,
                leverage: 0,
                sl_pct_leveraged: 0.0,
                rr_ratio: 0.0,
                atr_4h,
                atr_1h,
                timeframes: TimeframeScores {
                    m15: TfSnapshot { score: s15, indicators: ind_15m },
                    h1: TfSnapshot { score: s1h, indicators: ind_1h },
                    h4: TfSnapshot { score: s4h, indicators: ind_4h },
                },
                computed_at: Utc::now().to_rfc3339(),
                neutral_reason: Some("Score in neutral band (41–59): no trade recommended"),
            };
            return Ok(ToolOutput::success(
                serde_json::to_value(output).unwrap_or_default(),
                start.elapsed(),
            ));
        }

        let is_long = is_long_opt.unwrap();

        // SL / TP sized from 1h ATR (more responsive than 4h)
        let mut sl_distance = atr_1h * 1.5;
        let mut tp_distance = sl_distance * 1.5; // 1.5R

        // Enforce SL ≤ 40% of margin after leverage
        let max_sl_distance = signal_entry * 0.40 / leverage as f64;
        let neutral_reason: Option<&'static str>;
        if sl_distance > max_sl_distance {
            // Cap SL to the 40% limit
            if max_sl_distance < atr_1h * 0.5 {
                // SL would be too tight to be meaningful — go NEUTRAL
                let output = AnalysisOutput {
                    signal: "NEUTRAL",
                    is_buy: None,
                    signal_score: final_score,
                    signal_entry,
                    limit_entry: signal_entry,
                    take_profit: signal_entry,
                    stop_loss: signal_entry,
                    leverage: 0,
                    sl_pct_leveraged: 0.0,
                    rr_ratio: 0.0,
                    atr_4h,
                    atr_1h,
                    timeframes: TimeframeScores {
                        m15: TfSnapshot { score: s15, indicators: ind_15m },
                        h1: TfSnapshot { score: s1h, indicators: ind_1h },
                        h4: TfSnapshot { score: s4h, indicators: ind_4h },
                    },
                    computed_at: Utc::now().to_rfc3339(),
                    neutral_reason: Some("40% SL constraint cannot be met without an impractically tight stop"),
                };
                return Ok(ToolOutput::success(
                    serde_json::to_value(output).unwrap_or_default(),
                    start.elapsed(),
                ));
            }
            sl_distance = max_sl_distance;
            tp_distance = sl_distance * 1.5;
            neutral_reason = None;
        } else {
            neutral_reason = None;
        }

        // Enforce TP:SL ≥ 1.2R
        if tp_distance / sl_distance < 1.2 {
            tp_distance = sl_distance * 1.2;
        }

        // Directional prices
        let (stop_loss, take_profit) = if is_long {
            (signal_entry - sl_distance, signal_entry + tp_distance)
        } else {
            (signal_entry + sl_distance, signal_entry - tp_distance)
        };

        // Entry buffer: 0.4% better than signal entry
        // (derived from the requirement: 10%/×25, 20%/×50, 30%/×75 all = 0.4%)
        let limit_entry = if is_long {
            signal_entry * (1.0 - 0.004)
        } else {
            signal_entry * (1.0 + 0.004)
        };

        let sl_pct_leveraged = (sl_distance / signal_entry) * leverage as f64;
        let rr_ratio = tp_distance / sl_distance;

        let output = AnalysisOutput {
            signal: if is_long { "LONG" } else { "SHORT" },
            is_buy: Some(is_long),
            signal_score: (final_score * 100.0).round() / 100.0,
            signal_entry,
            limit_entry: (limit_entry * 10.0).round() / 10.0, // 0.1 USD tick
            take_profit: (take_profit * 10.0).round() / 10.0,
            stop_loss: (stop_loss * 10.0).round() / 10.0,
            leverage,
            sl_pct_leveraged: (sl_pct_leveraged * 1000.0).round() / 1000.0,
            rr_ratio: (rr_ratio * 100.0).round() / 100.0,
            atr_4h,
            atr_1h,
            timeframes: TimeframeScores {
                m15: TfSnapshot { score: s15, indicators: ind_15m },
                h1: TfSnapshot { score: s1h, indicators: ind_1h },
                h4: TfSnapshot { score: s4h, indicators: ind_4h },
            },
            computed_at: Utc::now().to_rfc3339(),
            neutral_reason,
        };

        let output_value = serde_json::to_value(&output).map_err(|e| {
            ToolError::ExecutionFailed(format!("Failed to serialize analysis: {e}"))
        })?;

        // Append signal to workspace history file (fire-and-forget).
        if let Some(ref ws) = self.workspace {
            let entry = format!(
                "\n## {}\n\
                - **Signal**: {} (score {:.1})\n\
                - **Entry**: ${:.1}  |  Limit: ${:.1}\n\
                - **TP**: ${:.1}  |  **SL**: ${:.1}\n\
                - **Leverage**: ×{}  |  RR: {:.2}  |  SL%: {:.1}%\n\
                - **ATR 1h**: ${:.1}  |  **ATR 4h**: ${:.1}\n\
                - **Scores** — 15m: {:.1}  1h: {:.1}  4h: {:.1}\n",
                output.computed_at,
                output.signal,
                output.signal_score,
                output.signal_entry,
                output.limit_entry,
                output.take_profit,
                output.stop_loss,
                output.leverage,
                output.rr_ratio,
                output.sl_pct_leveraged * 100.0,
                output.atr_1h,
                output.atr_4h,
                output.timeframes.m15.score,
                output.timeframes.h1.score,
                output.timeframes.h4.score,
            );
            let ws = Arc::clone(ws);
            tokio::spawn(async move {
                if let Err(e) = ws.append("hyperliquid/signal-history.md", &entry).await {
                    tracing::warn!("Failed to write HyperLiquid signal history: {}", e);
                }
            });
        }

        Ok(ToolOutput::success(output_value, start.elapsed()))
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::builtin::hyperliquid::indicators::IndicatorSet;

    fn neutral_ind() -> IndicatorSet {
        IndicatorSet {
            rsi: Some(50.0),
            macd: None,
            bb: None,
            ema50: None,
            ema200: None,
            atr: Some(500.0),
            stoch: None,
            williams_r: Some(-50.0),
            cci: Some(0.0),
            adx: Some(20.0),
            obv: 0.0,
            vwap: None,
            ichimoku: None,
            fibonacci: None,
        }
    }

    #[test]
    fn test_leverage_from_score_boundaries() {
        // Symmetric: distance from 50 determines tier.
        // dist ≥ 30 → ×100, dist ≥ 20 → ×75, dist 10-19 → ×50
        assert_eq!(leverage_from_score(20.0), 100); // dist 30 → ×100
        assert_eq!(leverage_from_score(19.9), 100); // dist 30.1 → ×100
        assert_eq!(leverage_from_score(21.0), 75);  // dist 29 → ×75
        assert_eq!(leverage_from_score(30.0), 75);  // dist 20 → ×75
        assert_eq!(leverage_from_score(31.0), 50);  // dist 19 → ×50
        assert_eq!(leverage_from_score(40.0), 50);  // dist 10 → ×50
        assert_eq!(leverage_from_score(60.0), 50);  // dist 10 → ×50
        assert_eq!(leverage_from_score(69.0), 50);  // dist 19 → ×50
        assert_eq!(leverage_from_score(70.0), 75);  // dist 20 → ×75
        assert_eq!(leverage_from_score(79.9), 75);  // dist 29.9 → ×75
        assert_eq!(leverage_from_score(80.0), 100); // dist 30 → ×100
        assert_eq!(leverage_from_score(100.0), 100);
    }

    #[test]
    fn test_signal_from_score_bands() {
        assert_eq!(signal_from_score(60.0), Some(true));
        assert_eq!(signal_from_score(100.0), Some(true));
        assert_eq!(signal_from_score(40.0), Some(false));
        assert_eq!(signal_from_score(0.0), Some(false));
        assert_eq!(signal_from_score(50.0), None);
        assert_eq!(signal_from_score(41.0), None);
        assert_eq!(signal_from_score(59.0), None);
    }

    #[test]
    fn test_entry_buffer_long() {
        let entry = 95_000.0_f64;
        let limit = entry * (1.0 - 0.004);
        assert!((limit - 94_620.0).abs() < 1.0);
    }

    #[test]
    fn test_entry_buffer_short() {
        let entry = 95_000.0_f64;
        let limit = entry * (1.0 + 0.004);
        assert!((limit - 95_380.0).abs() < 1.0);
    }

    #[test]
    fn test_score_neutral_indicators() {
        let ind = neutral_ind();
        let score = score_indicators(&ind, 50_000.0, true);
        // RSI=50 → 0, Williams=-50 → 0, CCI=0 → 0, OBV up → +4
        // ADX=20 → mult 1.0; no EMA, MACD, BB, Stoch, VWAP, Ichimoku, Fib
        // raw pts ≈ 4.0 → score = (4/106)*50 + 50 ≈ 51.9
        assert!(score > 49.0 && score < 54.0, "score={score}");
    }

    #[test]
    fn test_score_strong_long() {
        let ind = IndicatorSet {
            rsi: Some(25.0),         // +10
            macd: Some(crate::tools::builtin::hyperliquid::indicators::MacdValue {
                macd: 1.0,
                signal: 0.5,
                hist: 0.5, // hist > signal*0.1 → +10
            }),
            bb: None,
            ema50: Some(49_000.0),  // e50 < e200 would be negative, but e50 > close is negative…
            ema200: Some(48_000.0), // e50 > e200 → +8
            atr: Some(500.0),
            stoch: Some(crate::tools::builtin::hyperliquid::indicators::StochValue {
                k: 15.0, // +8
                d: 15.0,
            }),
            williams_r: Some(-85.0), // +6
            cci: Some(-120.0),       // +6
            adx: Some(35.0),         // mult 1.2
            obv: 1_000_000.0,
            vwap: Some(49_000.0), // close > vwap → +4
            ichimoku: None,
            fibonacci: None,
        };
        // close = 50_000, e50=49_000 (close > e50 → +6), obv_up=true → +4
        let score = score_indicators(&ind, 50_000.0, true);
        assert!(score > 75.0, "score={score} (expected strong LONG)");
    }

    #[test]
    fn test_tool_metadata() {
        let tool = HyperliquidAnalyzeTool::new();
        assert_eq!(tool.name(), "hyperliquid_analyze");
        assert_eq!(
            tool.requires_approval(&serde_json::json!({})),
            ApprovalRequirement::Never
        );
    }
}
