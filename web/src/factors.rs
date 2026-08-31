//! 多因子模块：因子计算、截面打分、相关性矩阵、因子 IC 验证。
//!
//! 单因子（全部基于本地日线）：
//! - 动量 20/60/120 日、30 日年化波动、趋势（相对 50/200 日均线偏离）、
//!   RSI(14)、量能比（5日均量/30日均量）、距 120 日高点回撤、30 日日均成交额
//! - 资金流向：CMF(20) 蔡金资金流、`flow20` 近 20 日符号化成交额（净主动买卖代理）
//!
//! 综合得分：截面 z 标准化（截断 ±3）后加权求和，
//! 动量 30% + 趋势 20% + 资金流 15% - 波动 15% + 量能 10% - 回撤 10%。
//!
//! 相关性矩阵：品种两两日收益率 Pearson 相关（近 N 个交易日）。
//!
//! 因子 IC：对每个评估日，计算因子截面值与未来 `horizon` 个交易日
//! 收益的 Pearson 相关（IC），输出 IC 均值/IR/胜率时间序列。

use quantkit_core::types::Kline;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

const W_MOM: f64 = 0.30;
const W_TREND: f64 = 0.20;
const W_FLOW: f64 = 0.15;
const W_VOL: f64 = 0.15;
const W_VRATIO: f64 = 0.10;
const W_DD: f64 = 0.10;
const Z_CLIP: f64 = 3.0;

/// 参与截面打分的最小样本数（需覆盖 200 日均线窗口）
pub const MIN_BARS: usize = 210;

/// 支持 IC 验证的因子名单（rsi14 仅诊断用、不参与打分）
pub const FACTORS: [&str; 12] = [
    "mom20",
    "mom60",
    "mom120",
    "vol_ann",
    "rsi14",
    "ma50_dev",
    "ma200_dev",
    "vol_ratio",
    "dd_from_high",
    "avg_quote_volume",
    "cmf20",
    "flow20",
];

#[derive(Debug, Clone, Serialize)]
pub struct FactorRow {
    pub symbol: String,
    pub price: f64,
    pub mom20: f64,
    pub mom60: f64,
    pub mom120: f64,
    /// 30 日年化波动
    pub vol_ann: f64,
    pub rsi14: f64,
    /// 收盘价相对 50/200 日均线偏离（趋势强度）
    pub ma50_dev: f64,
    pub ma200_dev: f64,
    /// 5 日均量 / 30 日均量
    pub vol_ratio: f64,
    /// 距 120 日高点回撤
    pub dd_from_high: f64,
    /// 30 日日均成交额（USDT）
    pub avg_quote_volume: f64,
    /// 蔡金资金流 [-1, 1]，>0 表示资金净流入
    pub cmf20: f64,
    /// 近 20 日符号化成交额（涨日计正、跌日计负），净资金流代理
    pub flow20: f64,
    pub bars: usize,
    pub score: f64,
    pub rank: usize,
}

/// IC 验证报告
#[derive(Debug, Clone, Serialize)]
pub struct IcReport {
    pub factor: String,
    pub horizon: usize,
    pub step: usize,
    pub n: usize,
    pub ic_mean: f64,
    pub ic_std: f64,
    pub icir: f64,
    pub hit_rate: f64,
    /// (评估日时间戳, IC)
    pub series: Vec<(u64, f64)>,
}

// ---------- 单因子计算（按索引位置，供截面打分与 IC 复用） ----------

/// 计算指定索引处的单因子值；样本不足或因子名未知返回 None
pub fn factor_at(ks: &[Kline], i: usize, name: &str) -> Option<f64> {
    if i >= ks.len() || i < 200 {
        return None;
    }
    let c = ks[i].close;
    if !c.is_finite() || c <= 0.0 {
        return None;
    }
    match name {
        "mom20" => Some(c / ks[i - 20].close - 1.0),
        "mom60" => Some(c / ks[i - 60].close - 1.0),
        "mom120" => Some(c / ks[i - 120].close - 1.0),
        "vol_ann" => Some(ann_vol(ks, i, 30)),
        "rsi14" => Some(rsi(ks, i, 14)),
        "ma50_dev" => Some(c / ma_close(ks, i, 50) - 1.0),
        "ma200_dev" => Some(c / ma_close(ks, i, 200) - 1.0),
        "vol_ratio" => {
            let den = ma_vol(ks, i, 30);
            if den <= 0.0 {
                None
            } else {
                Some(ma_vol(ks, i, 5) / den)
            }
        }
        "dd_from_high" => {
            let hi = ks[i.saturating_sub(119)..=i]
                .iter()
                .map(|k| k.high)
                .fold(0.0_f64, f64::max);
            if hi <= 0.0 {
                None
            } else {
                Some(1.0 - c / hi)
            }
        }
        "avg_quote_volume" => Some(
            ks[i - 29..=i]
                .iter()
                .map(|k| k.close * k.volume)
                .sum::<f64>()
                / 30.0,
        ),
        "cmf20" => Some(cmf(ks, i, 20)),
        "flow20" => Some(flow(ks, i, 20)),
        _ => None,
    }
}

fn ma_close(ks: &[Kline], i: usize, n: usize) -> f64 {
    ks[i + 1 - n..=i].iter().map(|k| k.close).sum::<f64>() / n as f64
}

fn ma_vol(ks: &[Kline], i: usize, n: usize) -> f64 {
    ks[i + 1 - n..=i].iter().map(|k| k.volume).sum::<f64>() / n as f64
}

fn ann_vol(ks: &[Kline], i: usize, n: usize) -> f64 {
    let mut rets = Vec::with_capacity(n);
    for j in i + 1 - n..=i {
        if ks[j - 1].close > 0.0 {
            rets.push(ks[j].close / ks[j - 1].close - 1.0);
        }
    }
    if rets.len() < 2 {
        return 0.0;
    }
    let mean = rets.iter().sum::<f64>() / rets.len() as f64;
    let var = rets.iter().map(|r| (r - mean) * (r - mean)).sum::<f64>() / rets.len() as f64;
    var.sqrt() * 365f64.sqrt()
}

fn rsi(ks: &[Kline], i: usize, n: usize) -> f64 {
    let (mut gain, mut loss) = (0.0_f64, 0.0_f64);
    for j in i + 1 - n..=i {
        let d = ks[j].close - ks[j - 1].close;
        if d > 0.0 {
            gain += d;
        } else {
            loss -= d;
        }
    }
    if gain + loss <= 1e-12 {
        return 50.0;
    }
    gain / (gain + loss) * 100.0
}

/// 蔡金资金流：Σ(MFM×成交量)/Σ成交量，MFM 衡量收盘在当日振幅中的位置
fn cmf(ks: &[Kline], i: usize, n: usize) -> f64 {
    let (mut num, mut den) = (0.0_f64, 0.0_f64);
    for k in &ks[i + 1 - n..=i] {
        let range = k.high - k.low;
        let mfm = if range > 0.0 {
            ((k.close - k.low) - (k.high - k.close)) / range
        } else {
            0.0
        };
        num += mfm * k.volume;
        den += k.volume;
    }
    if den <= 0.0 {
        0.0
    } else {
        num / den
    }
}

/// 净资金流代理：近 n 日成交额按当日涨跌符号加总（用相邻两根窗口取前日收盘）
fn flow(ks: &[Kline], i: usize, n: usize) -> f64 {
    let mut sum = 0.0;
    for w in ks[i - n..=i].windows(2) {
        let sign = if w[1].close > w[0].close {
            1.0
        } else if w[1].close < w[0].close {
            -1.0
        } else {
            0.0
        };
        sum += sign * w[1].close * w[1].volume;
    }
    sum
}

// ---------- 截面打分 ----------

/// 计算单品种全部原始因子；样本不足或最新收盘非法返回 None
pub fn compute_raw(symbol: &str, ks: &[Kline]) -> Option<FactorRow> {
    let n = ks.len();
    if n < MIN_BARS {
        return None;
    }
    let last = ks.last()?;
    if !last.close.is_finite() || last.close <= 0.0 {
        return None;
    }
    let i = n - 1;
    let get = |name: &str| factor_at(ks, i, name).unwrap_or(f64::NAN);
    Some(FactorRow {
        symbol: symbol.to_string(),
        price: last.close,
        mom20: get("mom20"),
        mom60: get("mom60"),
        mom120: get("mom120"),
        vol_ann: get("vol_ann"),
        rsi14: get("rsi14"),
        ma50_dev: get("ma50_dev"),
        ma200_dev: get("ma200_dev"),
        vol_ratio: get("vol_ratio"),
        dd_from_high: get("dd_from_high"),
        avg_quote_volume: get("avg_quote_volume"),
        cmf20: get("cmf20"),
        flow20: get("flow20"),
        bars: n,
        score: 0.0,
        rank: 0,
    })
}

/// 截面 z 标准化加权打分并按得分排序赋名次
pub fn finalize_scores(rows: &mut [FactorRow]) {
    if rows.is_empty() {
        return;
    }
    let z = |get: fn(&FactorRow) -> f64| -> Vec<f64> {
        let v: Vec<f64> = rows.iter().map(get).collect();
        zscores(&v)
    };
    let z_mom20 = z(|r| r.mom20);
    let z_mom60 = z(|r| r.mom60);
    let z_mom120 = z(|r| r.mom120);
    let z_vol = z(|r| r.vol_ann);
    let z_ma50 = z(|r| r.ma50_dev);
    let z_ma200 = z(|r| r.ma200_dev);
    let z_vratio = z(|r| r.vol_ratio);
    let z_dd = z(|r| r.dd_from_high);
    let z_flow = z(|r| r.flow20);
    for (i, row) in rows.iter_mut().enumerate() {
        let momentum = (z_mom20[i] + z_mom60[i] + z_mom120[i]) / 3.0;
        let trend = (z_ma50[i] + z_ma200[i]) / 2.0;
        row.score = W_MOM * momentum
            + W_TREND * trend
            + W_FLOW * z_flow[i]
            - W_VOL * z_vol[i]
            + W_VRATIO * z_vratio[i]
            - W_DD * z_dd[i];
    }
    rows.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    for (i, row) in rows.iter_mut().enumerate() {
        row.rank = i + 1;
    }
}

fn zscores(v: &[f64]) -> Vec<f64> {
    let n = v.len() as f64;
    let mean = v.iter().sum::<f64>() / n;
    let var = v.iter().map(|x| (x - mean) * (x - mean)).sum::<f64>() / n;
    let std = var.sqrt();
    if std < 1e-12 {
        return vec![0.0; v.len()];
    }
    v.iter().map(|x| ((x - mean) / std).clamp(-Z_CLIP, Z_CLIP)).collect()
}

// ---------- 相关性矩阵 ----------

/// 品种两两日收益率 Pearson 相关（近 `days` 个交易日，按日期对齐）
pub fn correlation_matrix(
    data: &[(String, Vec<Kline>)],
    days: usize,
) -> (Vec<String>, Vec<Vec<f64>>, usize) {
    // 每品种取最近 days+1 根，构造日期 -> 日收益率
    let mut series: Vec<(String, BTreeMap<u64, f64>)> = data
        .iter()
        .filter(|(_, ks)| ks.len() >= 2)
        .map(|(sym, ks)| {
            let ks = &ks[ks.len().saturating_sub(days + 1)..];
            let mut m = BTreeMap::new();
            for w in ks.windows(2) {
                if w[0].close > 0.0 {
                    m.insert(w[1].open_time, w[1].close / w[0].close - 1.0);
                }
            }
            (sym.clone(), m)
        })
        .collect();
    series.sort_by(|a, b| a.0.cmp(&b.0));
    let syms: Vec<String> = series.iter().map(|(s, _)| s.clone()).collect();
    let len = syms.len();
    let mut mat = vec![vec![0.0; len]; len];
    let mut used = 0usize;
    for i in 0..len {
        mat[i][i] = 1.0;
        for j in i + 1..len {
            let (mut xs, mut ys) = (Vec::new(), Vec::new());
            for (t, r) in &series[i].1 {
                if let Some(r2) = series[j].1.get(t) {
                    xs.push(*r);
                    ys.push(*r2);
                }
            }
            used = used.max(xs.len());
            let v = if xs.len() >= 10 { pearson(&xs, &ys) } else { f64::NAN };
            mat[i][j] = v;
            mat[j][i] = v;
        }
    }
    (syms, mat, used)
}

// ---------- 因子 IC 验证 ----------

/// 滚动截面 IC：每个评估日计算因子值与未来 `horizon` 个交易日收益的
/// Pearson 相关；评估日取所有品种的公共交易日、每隔 `step` 天一个。
pub fn factor_ic(
    data: &[(String, Vec<Kline>)],
    factor: &str,
    horizon: usize,
    step: usize,
) -> Option<IcReport> {
    if !FACTORS.contains(&factor) || horizon == 0 || data.len() < 4 {
        return None;
    }
    let step = step.max(1);
    // 每品种：日期 -> 索引
    let idx: Vec<BTreeMap<u64, usize>> = data
        .iter()
        .map(|(_, ks)| {
            ks.iter()
                .enumerate()
                .map(|(i, k)| (k.open_time, i))
                .collect()
        })
        .collect();
    // 全品种公共交易日（升序）
    let mut common: Vec<u64> = data[0].1.iter().map(|k| k.open_time).collect();
    for (_, ks) in data.iter().skip(1) {
        let set: BTreeSet<u64> = ks.iter().map(|k| k.open_time).collect();
        common.retain(|t| set.contains(t));
    }
    let mut series: Vec<(u64, f64)> = Vec::new();
    let mut p = 0usize;
    while p + horizon < common.len() {
        let (t, th) = (common[p], common[p + horizon]);
        let (mut xs, mut ys) = (Vec::new(), Vec::new());
        for ((_, ks), im) in data.iter().zip(&idx) {
            let (Some(&i), Some(&ih)) = (im.get(&t), im.get(&th)) else {
                continue;
            };
            let Some(fv) = factor_at(ks, i, factor) else {
                continue;
            };
            let c0 = ks[i].close;
            let c1 = ks[ih].close;
            if c0 <= 0.0 || !c1.is_finite() {
                continue;
            }
            xs.push(fv);
            ys.push(c1 / c0 - 1.0);
        }
        if xs.len() >= 4 {
            let ic = pearson(&xs, &ys);
            if ic.is_finite() {
                series.push((t, ic));
            }
        }
        p += step;
    }
    if series.is_empty() {
        return None;
    }
    let ics: Vec<f64> = series.iter().map(|(_, v)| *v).collect();
    let n = ics.len() as f64;
    let mean = ics.iter().sum::<f64>() / n;
    let var = ics.iter().map(|v| (v - mean) * (v - mean)).sum::<f64>() / n;
    let std = var.sqrt();
    let hit = ics.iter().filter(|v| **v > 0.0).count() as f64 / n;
    Some(IcReport {
        factor: factor.to_string(),
        horizon,
        step,
        n: series.len(),
        ic_mean: mean,
        ic_std: std,
        icir: if std > 1e-12 { mean / std } else { 0.0 },
        hit_rate: hit,
        series,
    })
}

fn pearson(xs: &[f64], ys: &[f64]) -> f64 {
    let n = xs.len().min(ys.len());
    if n < 3 {
        return f64::NAN;
    }
    let mx = xs[..n].iter().sum::<f64>() / n as f64;
    let my = ys[..n].iter().sum::<f64>() / n as f64;
    let (mut sxy, mut sxx, mut syy) = (0.0, 0.0, 0.0);
    for i in 0..n {
        let dx = xs[i] - mx;
        let dy = ys[i] - my;
        sxy += dx * dy;
        sxx += dx * dx;
        syy += dy * dy;
    }
    if sxx <= 1e-15 || syy <= 1e-15 {
        return f64::NAN;
    }
    sxy / (sxx.sqrt() * syy.sqrt())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: u64 = 86_400_000;

    fn series_from(closes: &[f64]) -> Vec<Kline> {
        closes
            .iter()
            .enumerate()
            .map(|(i, &c)| Kline {
                open_time: i as u64 * DAY,
                close_time: i as u64 * DAY + DAY - 1,
                open: c,
                high: c,
                low: c,
                close: c,
                volume: 100.0,
            })
            .collect()
    }

    /// 生成 n 根日K：按方向线性漂移，收盘贴近当日极值（上涨贴近高点、下跌贴近低点），
    /// 使 CMF 等位置型因子方向明确；每 20 根 3 根小幅回调避免零波动。
    fn trend_series(n: usize, start: f64, daily: f64) -> Vec<Kline> {
        let mut out = Vec::with_capacity(n);
        let mut c = start;
        for i in 0..n {
            let zigzag = if i % 20 >= 17 { -daily * 0.3 } else { daily };
            c = (c + zigzag).max(0.01);
            let range = daily.abs().max(0.01) * 0.5;
            // 上涨收盘位于当日上半区、下跌位于下半区（回调日同样如此，
            // 回调只体现在收盘涨跌上），保证 CMF 等位置型因子方向稳定
            let (high, low) = if daily > 0.0 {
                (c + range * 0.4, c - range)
            } else {
                (c + range, c - range * 0.4)
            };
            out.push(Kline {
                open_time: i as u64 * DAY,
                close_time: i as u64 * DAY + DAY - 1,
                open: (high + low) / 2.0,
                high,
                low,
                close: c,
                volume: 100.0,
            });
        }
        out
    }

    #[test]
    fn test_insufficient_bars_are_skipped() {
        assert!(compute_raw("AAA", &series_from(&[1.0; 100])).is_none());
        assert!(compute_raw("AAA", &series_from(&[1.0; 209])).is_none());
        assert!(compute_raw("AAA", &series_from(&[1.0; 210])).is_some());
    }

    #[test]
    fn test_gain_market_have_sane_factors() {
        let ks = trend_series(250, 100.0, 1.0);
        let row = compute_raw("UP", &ks).unwrap();
        assert!(row.mom20 > 0.0 && row.mom120 > 0.0);
        assert!(row.ma50_dev > 0.0 && row.ma200_dev > 0.0);
        assert!(row.rsi14 > 90.0, "单边上涨 RSI 应接近满值: {}", row.rsi14);
        assert!(row.dd_from_high < 0.05, "接近新高，回撤应很小");
        assert!((row.vol_ratio - 1.0).abs() < 1e-9, "等量时量能比应为 1");
        assert!(row.avg_quote_volume > 0.0);
        assert!(row.cmf20 > 0.0, "上涨收盘贴近高点，CMF 应为正");
        assert!(row.flow20 > 0.0, "上涨行情净资金流应为正");
    }

    #[test]
    fn test_down_market_factors() {
        let ks = trend_series(250, 400.0, -1.0);
        let row = compute_raw("DOWN", &ks).unwrap();
        assert!(row.mom60 < 0.0 && row.ma200_dev < 0.0);
        assert!(row.dd_from_high > 0.05, "持续下跌应有明显回撤");
        assert!(row.rsi14 < 10.0, "单边下跌 RSI 应接近 0: {}", row.rsi14);
        assert!(row.cmf20 < 0.0, "下跌收盘贴近低点，CMF 应为负");
        assert!(row.flow20 < 0.0, "下跌行情净资金流应为负");
    }

    #[test]
    fn test_ranking_favors_strong_trend() {
        let mut rows = vec![
            compute_raw("UP", &trend_series(250, 100.0, 1.0)).unwrap(),
            compute_raw("FLAT", &series_from(&vec![100.0; 250])).unwrap(),
            compute_raw("DOWN", &trend_series(250, 400.0, -1.0)).unwrap(),
        ];
        finalize_scores(&mut rows);
        assert_eq!(rows[0].symbol, "UP");
        assert_eq!(rows[0].rank, 1);
        assert_eq!(rows[2].symbol, "DOWN");
        assert_eq!(rows[2].rank, 3);
    }

    #[test]
    fn test_zscores_no_panic_on_zero_variance() {
        let z = zscores(&[1.0, 1.0, 1.0]);
        assert_eq!(z, vec![0.0; 3]);
    }

    fn pair_series(n: usize, b_slope: f64) -> Vec<(String, Vec<Kline>)> {
        let a = trend_series(n, 100.0, 1.0);
        // BBB 以 AAA 收盘为基准镜像构造，收益率近似互为相反数 -> 强负相关；
        // 同向时直接复用同形状序列 -> 相关接近 1（幅度差异不影响线性相关）
        let b = if b_slope > 0.0 {
            trend_series(n, 100.0, b_slope)
        } else {
            let mut ks = Vec::with_capacity(n);
            for (i, k) in a.iter().enumerate() {
                let c = 400.0 - k.close;
                ks.push(Kline {
                    open_time: i as u64 * DAY,
                    close_time: i as u64 * DAY + DAY - 1,
                    open: c,
                    high: c.max(k.high),
                    low: c.min(k.low),
                    close: c,
                    volume: 100.0,
                });
            }
            ks
        };
        vec![("AAA".to_string(), a), ("BBB".to_string(), b)]
    }

    #[test]
    fn test_correlation_perfect_and_anti() {
        // 同向走势 -> 相关接近 1
        let (syms, mat, used) = correlation_matrix(&pair_series(120, 1.0), 60);
        assert_eq!(syms.len(), 2);
        assert!(used >= 55, "公共交易日应接近请求窗口: {}", used);
        assert!(mat[0][1] > 0.95, "同向应高度相关: {}", mat[0][1]);
        // 反向走势 -> 强负相关
        let (_, mat2, _) = correlation_matrix(&pair_series(120, -1.0), 60);
        assert!(mat2[0][1] < -0.9, "反向应强负相关: {}", mat2[0][1]);
        // 对角线恒为 1
        assert!((mat[0][0] - 1.0).abs() < 1e-12);
    }

    #[test]
    fn test_factor_ic_positive_for_predictive_factor() {
        // 5 个品种以不同斜率稳定上涨：动量与未来收益同序 -> IC 应显著为正
        let mut data = Vec::new();
        for (k, sym) in ["A", "B", "C", "D", "E"].iter().enumerate() {
            data.push((
                sym.to_string(),
                trend_series(260, 100.0, 0.5 + k as f64 * 0.5),
            ));
        }
        let report = factor_ic(&data, "mom20", 10, 5).expect("应能计算 IC");
        assert!(report.n >= 5, "样本数不足: {}", report.n);
        assert!(report.ic_mean > 0.8, "预测性因子 IC 应显著为正: {}", report.ic_mean);
        assert!(report.icir > 1.0);
        assert!(report.hit_rate > 0.8);
        // 未知因子应被拒绝
        assert!(factor_ic(&data, "not_a_factor", 10, 5).is_none());
    }
}
