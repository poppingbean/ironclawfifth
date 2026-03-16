//! Pure technical analysis indicator functions.
//!
//! All functions are stateless and operate on slices of `f64` values.
//! Returns `None` when there is insufficient data for the requested period.

use serde::{Deserialize, Serialize};

// ── Data types ────────────────────────────────────────────────────────────────

/// One OHLCV candle as returned by Binance klines.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Candle {
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
}

/// MACD value (line, signal, histogram).
#[derive(Debug, Clone, Serialize)]
pub struct MacdValue {
    pub macd: f64,
    pub signal: f64,
    pub hist: f64,
}

/// Bollinger Bands snapshot.
#[derive(Debug, Clone, Serialize)]
pub struct BollingerValue {
    pub upper: f64,
    pub middle: f64,
    pub lower: f64,
}

/// Stochastic oscillator (%K, %D).
#[derive(Debug, Clone, Serialize)]
pub struct StochValue {
    pub k: f64,
    pub d: f64,
}

/// Ichimoku Cloud snapshot for the latest candle.
#[derive(Debug, Clone, Serialize)]
pub struct IchimokuValue {
    /// Conversion line: (max_high + min_low) / 2 over tenkan_period.
    pub tenkan: f64,
    /// Base line: (max_high + min_low) / 2 over kijun_period.
    pub kijun: f64,
    /// Leading Span A: (tenkan + kijun) / 2.
    pub span_a: f64,
    /// Leading Span B: (max_high + min_low) / 2 over senkou_b_period.
    pub span_b: f64,
    /// True when close is strictly above both span_a and span_b.
    pub price_above_cloud: bool,
    /// True when close is strictly below both span_a and span_b.
    pub price_below_cloud: bool,
    /// True when tenkan crossed above kijun on the most recent candle.
    pub tk_cross_bull: bool,
    /// True when tenkan crossed below kijun on the most recent candle.
    pub tk_cross_bear: bool,
}

/// Fibonacci retracement levels from a recent swing.
#[derive(Debug, Clone, Serialize)]
pub struct FibonacciValue {
    pub swing_high: f64,
    pub swing_low: f64,
    /// 23.6% retracement level (measured down from swing_high).
    pub level_236: f64,
    /// 38.2% retracement level.
    pub level_382: f64,
    /// 50.0% retracement level.
    pub level_500: f64,
    /// 61.8% retracement level.
    pub level_618: f64,
    /// 78.6% retracement level.
    pub level_786: f64,
    /// The nearest fib level as a percentage (e.g. 61.8 means the 61.8% level).
    pub nearest_level_pct: f64,
    /// True if price is above the nearest level (support); false if below (resistance).
    pub price_above_nearest: bool,
}

/// All indicators for one timeframe.
#[derive(Debug, Clone, Serialize)]
pub struct IndicatorSet {
    pub rsi: Option<f64>,
    pub macd: Option<MacdValue>,
    pub bb: Option<BollingerValue>,
    pub ema50: Option<f64>,
    pub ema200: Option<f64>,
    pub atr: Option<f64>,
    pub stoch: Option<StochValue>,
    pub williams_r: Option<f64>,
    pub cci: Option<f64>,
    pub adx: Option<f64>,
    pub obv: f64,
    pub vwap: Option<f64>,
    pub ichimoku: Option<IchimokuValue>,
    pub fibonacci: Option<FibonacciValue>,
}

// ── Helper: extract price series ─────────────────────────────────────────────

fn closes(candles: &[Candle]) -> Vec<f64> {
    candles.iter().map(|c| c.close).collect()
}
fn highs(candles: &[Candle]) -> Vec<f64> {
    candles.iter().map(|c| c.high).collect()
}
fn lows(candles: &[Candle]) -> Vec<f64> {
    candles.iter().map(|c| c.low).collect()
}
fn volumes(candles: &[Candle]) -> Vec<f64> {
    candles.iter().map(|c| c.volume).collect()
}

// ── EMA ───────────────────────────────────────────────────────────────────────

/// Exponential moving average seeded from SMA of the first `period` values.
///
/// Returns `None` when `closes.len() < period`.
pub fn ema(closes: &[f64], period: usize) -> Option<f64> {
    if closes.len() < period || period == 0 {
        return None;
    }
    let k = 2.0 / (period as f64 + 1.0);
    let mut val: f64 = closes[..period].iter().sum::<f64>() / period as f64;
    for &c in &closes[period..] {
        val = c * k + val * (1.0 - k);
    }
    Some(val)
}

/// Full EMA series (same length as `closes`, `None`-padded until seed is available).
fn ema_series(closes: &[f64], period: usize) -> Vec<Option<f64>> {
    if closes.len() < period || period == 0 {
        return vec![None; closes.len()];
    }
    let k = 2.0 / (period as f64 + 1.0);
    let seed: f64 = closes[..period].iter().sum::<f64>() / period as f64;
    let mut result: Vec<Option<f64>> = vec![None; period - 1];
    let mut val = seed;
    result.push(Some(val));
    for &c in &closes[period..] {
        val = c * k + val * (1.0 - k);
        result.push(Some(val));
    }
    result
}

// ── RSI ───────────────────────────────────────────────────────────────────────

/// RSI(period) using Wilder's smoothing.
///
/// Returns `None` when `closes.len() <= period`.
pub fn rsi(closes: &[f64], period: usize) -> Option<f64> {
    if closes.len() <= period || period == 0 {
        return None;
    }
    let mut avg_gain = 0.0_f64;
    let mut avg_loss = 0.0_f64;
    for i in 1..=period {
        let diff = closes[i] - closes[i - 1];
        if diff > 0.0 {
            avg_gain += diff;
        } else {
            avg_loss += -diff;
        }
    }
    avg_gain /= period as f64;
    avg_loss /= period as f64;
    for i in (period + 1)..closes.len() {
        let diff = closes[i] - closes[i - 1];
        let gain = if diff > 0.0 { diff } else { 0.0 };
        let loss = if diff < 0.0 { -diff } else { 0.0 };
        avg_gain = (avg_gain * (period as f64 - 1.0) + gain) / period as f64;
        avg_loss = (avg_loss * (period as f64 - 1.0) + loss) / period as f64;
    }
    if avg_loss == 0.0 {
        return Some(100.0);
    }
    let rs = avg_gain / avg_loss;
    Some(100.0 - 100.0 / (1.0 + rs))
}

// ── MACD ──────────────────────────────────────────────────────────────────────

/// MACD(fast, slow, signal_period).
///
/// Returns `None` when there are insufficient candles for the slow EMA plus
/// the signal smoothing period.
pub fn macd(closes: &[f64], fast: usize, slow: usize, signal_period: usize) -> Option<MacdValue> {
    let fast_series = ema_series(closes, fast);
    let slow_series = ema_series(closes, slow);

    // MACD line = fast EMA - slow EMA; only valid where both are Some
    let macd_line: Vec<Option<f64>> = fast_series
        .iter()
        .zip(slow_series.iter())
        .map(|(f, s)| match (f, s) {
            (Some(fv), Some(sv)) => Some(fv - sv),
            _ => None,
        })
        .collect();

    // Extract the valid (non-None) MACD values for signal EMA calculation
    let valid_macd: Vec<f64> = macd_line.iter().filter_map(|v| *v).collect();
    if valid_macd.len() < signal_period {
        return None;
    }
    let sig = ema(&valid_macd, signal_period)?;
    let last_macd = *valid_macd.last()?;
    Some(MacdValue {
        macd: last_macd,
        signal: sig,
        hist: last_macd - sig,
    })
}

// ── Bollinger Bands ───────────────────────────────────────────────────────────

/// Bollinger Bands(period, num_std_devs).
pub fn bollinger(closes: &[f64], period: usize, num_std: f64) -> Option<BollingerValue> {
    if closes.len() < period || period == 0 {
        return None;
    }
    let window = &closes[closes.len() - period..];
    let mean = window.iter().sum::<f64>() / period as f64;
    let variance = window.iter().map(|&x| (x - mean).powi(2)).sum::<f64>() / period as f64;
    let std_dev = variance.sqrt();
    Some(BollingerValue {
        upper: mean + num_std * std_dev,
        middle: mean,
        lower: mean - num_std * std_dev,
    })
}

// ── ATR ───────────────────────────────────────────────────────────────────────

/// Average True Range(period) using Wilder's smoothing.
pub fn atr(highs: &[f64], lows: &[f64], closes: &[f64], period: usize) -> Option<f64> {
    let n = closes.len();
    if n <= period || period == 0 || highs.len() != n || lows.len() != n {
        return None;
    }
    // True range series
    let tr: Vec<f64> = (1..n)
        .map(|i| {
            let hl = highs[i] - lows[i];
            let hc = (highs[i] - closes[i - 1]).abs();
            let lc = (lows[i] - closes[i - 1]).abs();
            hl.max(hc).max(lc)
        })
        .collect();

    if tr.len() < period {
        return None;
    }

    // Seed from SMA of first period TR values
    let mut atr_val: f64 = tr[..period].iter().sum::<f64>() / period as f64;
    for &t in &tr[period..] {
        atr_val = (atr_val * (period as f64 - 1.0) + t) / period as f64;
    }
    Some(atr_val)
}

// ── Stochastic ────────────────────────────────────────────────────────────────

/// Stochastic oscillator %K(k_period) and %D(d_period smoothing of %K).
pub fn stochastic(
    highs: &[f64],
    lows: &[f64],
    closes: &[f64],
    k_period: usize,
    d_period: usize,
) -> Option<StochValue> {
    let n = closes.len();
    if n < k_period + d_period - 1 || highs.len() != n || lows.len() != n {
        return None;
    }

    // %K series
    let k_series: Vec<f64> = (k_period - 1..n)
        .map(|i| {
            let start = i + 1 - k_period;
            let highest = highs[start..=i]
                .iter()
                .cloned()
                .fold(f64::NEG_INFINITY, f64::max);
            let lowest = lows[start..=i]
                .iter()
                .cloned()
                .fold(f64::INFINITY, f64::min);
            if (highest - lowest).abs() < f64::EPSILON {
                50.0
            } else {
                100.0 * (closes[i] - lowest) / (highest - lowest)
            }
        })
        .collect();

    if k_series.len() < d_period {
        return None;
    }

    let last_k = *k_series.last()?;
    let last_d = k_series[k_series.len() - d_period..].iter().sum::<f64>() / d_period as f64;
    Some(StochValue {
        k: last_k,
        d: last_d,
    })
}

// ── Williams %R ───────────────────────────────────────────────────────────────

/// Williams %R(period). Range: [-100, 0].
pub fn williams_r(highs: &[f64], lows: &[f64], closes: &[f64], period: usize) -> Option<f64> {
    let n = closes.len();
    if n < period || highs.len() != n || lows.len() != n {
        return None;
    }
    let start = n - period;
    let highest = highs[start..].iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let lowest = lows[start..].iter().cloned().fold(f64::INFINITY, f64::min);
    if (highest - lowest).abs() < f64::EPSILON {
        return Some(-50.0);
    }
    Some(-100.0 * (highest - closes[n - 1]) / (highest - lowest))
}

// ── CCI ───────────────────────────────────────────────────────────────────────

/// Commodity Channel Index(period).
pub fn cci(highs: &[f64], lows: &[f64], closes: &[f64], period: usize) -> Option<f64> {
    let n = closes.len();
    if n < period || highs.len() != n || lows.len() != n {
        return None;
    }
    let typical: Vec<f64> = (0..n)
        .map(|i| (highs[i] + lows[i] + closes[i]) / 3.0)
        .collect();
    let window = &typical[n - period..];
    let mean = window.iter().sum::<f64>() / period as f64;
    let mad = window.iter().map(|&x| (x - mean).abs()).sum::<f64>() / period as f64;
    if mad.abs() < f64::EPSILON {
        return Some(0.0);
    }
    Some((typical[n - 1] - mean) / (0.015 * mad))
}

// ── ADX ───────────────────────────────────────────────────────────────────────

/// Average Directional Index(period).
///
/// Computes +DI and -DI via Wilder smoothing, then smooths DX to get ADX.
/// Returns `None` when there are insufficient candles (needs `2*period + 1`).
pub fn adx(highs: &[f64], lows: &[f64], closes: &[f64], period: usize) -> Option<f64> {
    let n = closes.len();
    if n < 2 * period + 1 || highs.len() != n || lows.len() != n || period == 0 {
        return None;
    }

    // Directional movement and true range deltas
    let mut plus_dm_vals: Vec<f64> = Vec::with_capacity(n - 1);
    let mut minus_dm_vals: Vec<f64> = Vec::with_capacity(n - 1);
    let mut tr_vals: Vec<f64> = Vec::with_capacity(n - 1);

    for i in 1..n {
        let up = highs[i] - highs[i - 1];
        let down = lows[i - 1] - lows[i];
        plus_dm_vals.push(if up > down && up > 0.0 { up } else { 0.0 });
        minus_dm_vals.push(if down > up && down > 0.0 { down } else { 0.0 });
        let hl = highs[i] - lows[i];
        let hc = (highs[i] - closes[i - 1]).abs();
        let lc = (lows[i] - closes[i - 1]).abs();
        tr_vals.push(hl.max(hc).max(lc));
    }

    // Wilder seed from first `period` values
    let mut smoothed_plus = plus_dm_vals[..period].iter().sum::<f64>();
    let mut smoothed_minus = minus_dm_vals[..period].iter().sum::<f64>();
    let mut smoothed_tr = tr_vals[..period].iter().sum::<f64>();

    // Collect DX values
    let mut dx_vals: Vec<f64> = Vec::new();

    let di_plus = if smoothed_tr > 0.0 {
        100.0 * smoothed_plus / smoothed_tr
    } else {
        0.0
    };
    let di_minus = if smoothed_tr > 0.0 {
        100.0 * smoothed_minus / smoothed_tr
    } else {
        0.0
    };
    let di_sum = di_plus + di_minus;
    if di_sum > 0.0 {
        dx_vals.push(100.0 * (di_plus - di_minus).abs() / di_sum);
    }

    for i in period..plus_dm_vals.len() {
        smoothed_plus = smoothed_plus - smoothed_plus / period as f64 + plus_dm_vals[i];
        smoothed_minus = smoothed_minus - smoothed_minus / period as f64 + minus_dm_vals[i];
        smoothed_tr = smoothed_tr - smoothed_tr / period as f64 + tr_vals[i];
        let dp = if smoothed_tr > 0.0 {
            100.0 * smoothed_plus / smoothed_tr
        } else {
            0.0
        };
        let dm = if smoothed_tr > 0.0 {
            100.0 * smoothed_minus / smoothed_tr
        } else {
            0.0
        };
        let s = dp + dm;
        if s > 0.0 {
            dx_vals.push(100.0 * (dp - dm).abs() / s);
        } else {
            dx_vals.push(0.0);
        }
    }

    if dx_vals.len() < period {
        return None;
    }

    // Smooth DX → ADX using Wilder
    let mut adx_val: f64 = dx_vals[..period].iter().sum::<f64>() / period as f64;
    for &dx in &dx_vals[period..] {
        adx_val = (adx_val * (period as f64 - 1.0) + dx) / period as f64;
    }
    Some(adx_val)
}

// ── OBV ───────────────────────────────────────────────────────────────────────

/// On-Balance Volume (cumulative). Returns the final cumulative OBV.
pub fn obv(closes: &[f64], volumes: &[f64]) -> f64 {
    if closes.len() != volumes.len() || closes.is_empty() {
        return 0.0;
    }
    let mut total = 0.0_f64;
    for i in 1..closes.len() {
        if closes[i] > closes[i - 1] {
            total += volumes[i];
        } else if closes[i] < closes[i - 1] {
            total -= volumes[i];
        }
    }
    total
}

/// OBV series (full length). Used to detect rising/falling trend.
pub fn obv_series(closes: &[f64], volumes: &[f64]) -> Vec<f64> {
    if closes.len() != volumes.len() || closes.is_empty() {
        return vec![];
    }
    let mut series = vec![0.0_f64; closes.len()];
    for i in 1..closes.len() {
        if closes[i] > closes[i - 1] {
            series[i] = series[i - 1] + volumes[i];
        } else if closes[i] < closes[i - 1] {
            series[i] = series[i - 1] - volumes[i];
        } else {
            series[i] = series[i - 1];
        }
    }
    series
}

// ── VWAP ──────────────────────────────────────────────────────────────────────

/// Volume-Weighted Average Price over the entire candle slice (one "session").
pub fn vwap(highs: &[f64], lows: &[f64], closes: &[f64], volumes: &[f64]) -> Option<f64> {
    let n = closes.len();
    if n == 0 || highs.len() != n || lows.len() != n || volumes.len() != n {
        return None;
    }
    let (num, den) = (0..n).fold((0.0_f64, 0.0_f64), |(num, den), i| {
        let typical = (highs[i] + lows[i] + closes[i]) / 3.0;
        (num + typical * volumes[i], den + volumes[i])
    });
    if den == 0.0 {
        return None;
    }
    Some(num / den)
}

// ── Ichimoku ─────────────────────────────────────────────────────────────────

/// Rolling (high + low) / 2 midpoint over a window ending at index `i`.
fn midpoint_at(highs: &[f64], lows: &[f64], i: usize, period: usize) -> Option<f64> {
    if i + 1 < period {
        return None;
    }
    let start = i + 1 - period;
    let h = highs[start..=i]
        .iter()
        .cloned()
        .fold(f64::NEG_INFINITY, f64::max);
    let l = lows[start..=i]
        .iter()
        .cloned()
        .fold(f64::INFINITY, f64::min);
    Some((h + l) / 2.0)
}

/// Ichimoku Cloud for the most recent candle.
///
/// Standard periods: tenkan=9, kijun=26, senkou_b=52.
pub fn ichimoku(
    highs: &[f64],
    lows: &[f64],
    closes: &[f64],
    tenkan_period: usize,
    kijun_period: usize,
    senkou_b_period: usize,
) -> Option<IchimokuValue> {
    let n = closes.len();
    if n < senkou_b_period.max(kijun_period).max(tenkan_period) + 1
        || highs.len() != n
        || lows.len() != n
    {
        return None;
    }
    let last = n - 1;
    let prev = if last > 0 { last - 1 } else { return None };

    let tenkan = midpoint_at(highs, lows, last, tenkan_period)?;
    let kijun = midpoint_at(highs, lows, last, kijun_period)?;
    let span_a = (tenkan + kijun) / 2.0;
    let span_b = midpoint_at(highs, lows, last, senkou_b_period)?;

    let tenkan_prev = midpoint_at(highs, lows, prev, tenkan_period)?;
    let kijun_prev = midpoint_at(highs, lows, prev, kijun_period)?;

    let close = closes[last];
    let cloud_top = span_a.max(span_b);
    let cloud_bot = span_a.min(span_b);

    Some(IchimokuValue {
        tenkan,
        kijun,
        span_a,
        span_b,
        price_above_cloud: close > cloud_top,
        price_below_cloud: close < cloud_bot,
        tk_cross_bull: tenkan > kijun && tenkan_prev <= kijun_prev,
        tk_cross_bear: tenkan < kijun && tenkan_prev >= kijun_prev,
    })
}

// ── Fibonacci retracement ─────────────────────────────────────────────────────

/// Fibonacci retracement levels based on swing high/low over `lookback` candles.
pub fn fibonacci_levels(
    highs: &[f64],
    lows: &[f64],
    closes: &[f64],
    lookback: usize,
) -> Option<FibonacciValue> {
    let n = closes.len();
    if n < lookback || lookback == 0 || highs.len() != n || lows.len() != n {
        return None;
    }
    let start = n - lookback;
    let swing_high = highs[start..].iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let swing_low = lows[start..].iter().cloned().fold(f64::INFINITY, f64::min);
    let range = swing_high - swing_low;
    if range < f64::EPSILON {
        return None;
    }

    let price = closes[n - 1];
    let level_236 = swing_high - range * 0.236;
    let level_382 = swing_high - range * 0.382;
    let level_500 = swing_high - range * 0.500;
    let level_618 = swing_high - range * 0.618;
    let level_786 = swing_high - range * 0.786;

    // Find nearest fib level within 1% of current price
    let mut nearest_pct = 0.0_f64;
    let mut nearest_dist = f64::INFINITY;
    let levels_list = [
        (23.6_f64, level_236),
        (38.2, level_382),
        (50.0, level_500),
        (61.8, level_618),
        (78.6, level_786),
    ];
    for (pct, level) in &levels_list {
        let dist = (price - level).abs();
        if dist < nearest_dist {
            nearest_dist = dist;
            nearest_pct = *pct;
        }
    }

    Some(FibonacciValue {
        swing_high,
        swing_low,
        level_236,
        level_382,
        level_500,
        level_618,
        level_786,
        nearest_level_pct: nearest_pct,
        price_above_nearest: price
            >= levels_list
                .iter()
                .find(|(p, _)| (*p - nearest_pct).abs() < 0.01)
                .map(|(_, l)| *l)
                .unwrap_or(price),
    })
}

// ── compute_all ───────────────────────────────────────────────────────────────

/// Compute all 14 indicators for a candle slice.
pub fn compute_all(candles: &[Candle]) -> IndicatorSet {
    let c = closes(candles);
    let h = highs(candles);
    let l = lows(candles);
    let v = volumes(candles);
    let obv_s = obv_series(&c, &v);

    // OBV rising: compare first and last quarter averages
    let obv_val = obv_s.last().copied().unwrap_or(0.0);

    IndicatorSet {
        rsi: rsi(&c, 14),
        macd: macd(&c, 12, 26, 9),
        bb: bollinger(&c, 20, 2.0),
        ema50: ema(&c, 50),
        ema200: ema(&c, 200),
        atr: atr(&h, &l, &c, 14),
        stoch: stochastic(&h, &l, &c, 14, 3),
        williams_r: williams_r(&h, &l, &c, 14),
        cci: cci(&h, &l, &c, 20),
        adx: adx(&h, &l, &c, 14),
        obv: obv_val,
        vwap: vwap(&h, &l, &c, &v),
        ichimoku: ichimoku(&h, &l, &c, 9, 26, 52),
        fibonacci: fibonacci_levels(&h, &l, &c, 50),
    }
}

/// Returns true if OBV is trending upward (last half average > first half average).
pub fn obv_rising(candles: &[Candle]) -> bool {
    let c = closes(candles);
    let v = volumes(candles);
    let series = obv_series(&c, &v);
    if series.len() < 2 {
        return false;
    }
    let mid = series.len() / 2;
    let first_avg = series[..mid].iter().sum::<f64>() / mid as f64;
    let second_avg = series[mid..].iter().sum::<f64>() / (series.len() - mid) as f64;
    second_avg > first_avg
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn candle(c: f64) -> Candle {
        Candle {
            open: c,
            high: c + 10.0,
            low: c - 10.0,
            close: c,
            volume: 100.0,
        }
    }

    fn trending_up(n: usize) -> Vec<Candle> {
        (0..n).map(|i| candle(100.0 + i as f64 * 5.0)).collect()
    }

    #[test]
    fn test_ema_basic() {
        // 5 closes all at 10.0 → EMA(5) should equal 10.0
        let closes = vec![10.0; 10];
        assert!((ema(&closes, 5).unwrap() - 10.0).abs() < 1e-9);
    }

    #[test]
    fn test_ema_insufficient() {
        assert!(ema(&[1.0, 2.0], 5).is_none());
    }

    #[test]
    fn test_rsi_all_gains() {
        // Monotonically increasing prices → RSI should be 100
        let closes: Vec<f64> = (0..30).map(|i| 100.0 + i as f64).collect();
        let r = rsi(&closes, 14).unwrap();
        assert!((r - 100.0).abs() < 0.001);
    }

    #[test]
    fn test_rsi_all_losses() {
        // Monotonically decreasing prices → RSI should be 0
        let closes: Vec<f64> = (0..30).map(|i| 100.0 - i as f64).collect();
        let r = rsi(&closes, 14).unwrap();
        assert!(r < 1.0, "rsi was {r}");
    }

    #[test]
    fn test_rsi_insufficient() {
        assert!(rsi(&[1.0; 14], 14).is_none());
    }

    #[test]
    fn test_macd_returns_value() {
        let candles = trending_up(200);
        let c: Vec<f64> = candles.iter().map(|c| c.close).collect();
        assert!(macd(&c, 12, 26, 9).is_some());
    }

    #[test]
    fn test_bollinger_uniform() {
        // All prices equal → std_dev = 0, so upper = lower = middle = price
        let closes = vec![50.0; 25];
        let bb = bollinger(&closes, 20, 2.0).unwrap();
        assert!((bb.upper - 50.0).abs() < 1e-9);
        assert!((bb.lower - 50.0).abs() < 1e-9);
        assert!((bb.middle - 50.0).abs() < 1e-9);
    }

    #[test]
    fn test_atr_basic() {
        let candles = trending_up(30);
        let h: Vec<f64> = candles.iter().map(|c| c.high).collect();
        let l: Vec<f64> = candles.iter().map(|c| c.low).collect();
        let c: Vec<f64> = candles.iter().map(|c| c.close).collect();
        let a = atr(&h, &l, &c, 14);
        assert!(a.is_some());
        assert!(a.unwrap() > 0.0);
    }

    #[test]
    fn test_stochastic_oversold() {
        // Prices dropping hard → %K near 0
        let n = 25;
        let closes: Vec<f64> = (0..n).map(|i| 100.0 - i as f64 * 3.0).collect();
        let highs: Vec<f64> = closes.iter().map(|&c| c + 1.0).collect();
        let lows: Vec<f64> = closes.iter().map(|&c| c - 1.0).collect();
        let s = stochastic(&highs, &lows, &closes, 14, 3).unwrap();
        assert!(s.k < 10.0, "stoch k={}", s.k);
    }

    #[test]
    fn test_williams_r_range() {
        let candles = trending_up(20);
        let h: Vec<f64> = candles.iter().map(|c| c.high).collect();
        let l: Vec<f64> = candles.iter().map(|c| c.low).collect();
        let c: Vec<f64> = candles.iter().map(|c| c.close).collect();
        let wr = williams_r(&h, &l, &c, 14).unwrap();
        assert!((-100.0..=0.0).contains(&wr), "williams_r={wr}");
    }

    #[test]
    fn test_cci_flat() {
        let closes = vec![50.0; 25];
        let highs = vec![55.0; 25];
        let lows = vec![45.0; 25];
        let c = cci(&highs, &lows, &closes, 20).unwrap();
        // Flat data → CCI near 0
        assert!(c.abs() < 0.01, "cci={c}");
    }

    #[test]
    fn test_adx_trending() {
        let candles = trending_up(100);
        let h: Vec<f64> = candles.iter().map(|c| c.high).collect();
        let l: Vec<f64> = candles.iter().map(|c| c.low).collect();
        let c: Vec<f64> = candles.iter().map(|c| c.close).collect();
        let a = adx(&h, &l, &c, 14).unwrap();
        assert!(a > 25.0, "adx={a} (should indicate strong trend)");
    }

    #[test]
    fn test_obv_alternating() {
        let closes = vec![10.0, 11.0, 10.5, 11.5, 11.0];
        let volumes = vec![100.0; 5];
        let series = obv_series(&closes, &volumes);
        // Up: +100, Down: -100, Up: +100, Down: -100
        assert_eq!(series[4], 0.0);
    }

    #[test]
    fn test_vwap_uniform() {
        let h = vec![11.0; 5];
        let l = vec![9.0; 5];
        let c = vec![10.0; 5];
        let v = vec![100.0; 5];
        // Typical = (11+9+10)/3 = 10 → VWAP = 10
        let vw = vwap(&h, &l, &c, &v).unwrap();
        assert!((vw - 10.0).abs() < 1e-9);
    }

    #[test]
    fn test_ichimoku_above_cloud() {
        // Create strongly rising prices — tenkan > kijun > span_b, close well above cloud
        let candles: Vec<Candle> = (0..60).map(|i| candle(1000.0 + i as f64 * 100.0)).collect();
        let h: Vec<f64> = candles.iter().map(|c| c.high).collect();
        let l: Vec<f64> = candles.iter().map(|c| c.low).collect();
        let c: Vec<f64> = candles.iter().map(|c| c.close).collect();
        let ichi = ichimoku(&h, &l, &c, 9, 26, 52).unwrap();
        assert!(ichi.price_above_cloud);
        assert!(!ichi.price_below_cloud);
    }

    #[test]
    fn test_fibonacci_levels() {
        // Swing high=100, swing low=0 → level_618 = 100 - 100*0.618 = 38.2
        let highs = [vec![0.0_f64; 49], vec![100.0]].concat();
        let lows = [vec![0.0_f64; 49], vec![0.0]].concat();
        let closes = vec![50.0; 50]; // price at 50, between 38.2 and 61.8
        let fib = fibonacci_levels(&highs, &lows, &closes, 50).unwrap();
        assert!((fib.swing_high - 100.0).abs() < 1e-9);
        assert!((fib.swing_low - 0.0).abs() < 1e-9);
        assert!((fib.level_618 - 38.2).abs() < 0.01);
        assert!((fib.level_382 - 61.8).abs() < 0.01);
    }

    #[test]
    fn test_compute_all_runs() {
        let candles = trending_up(250);
        let ind = compute_all(&candles);
        assert!(ind.rsi.is_some());
        assert!(ind.macd.is_some());
        assert!(ind.ema50.is_some());
        assert!(ind.ema200.is_some());
        assert!(ind.atr.is_some());
        assert!(ind.ichimoku.is_some());
        assert!(ind.fibonacci.is_some());
    }
}
