//! 回测指标：年化、最大回撤、Sharpe、胜率、profit factor、盈亏比、Calmar、暴露率。
//!
//! 权益曲线本身已含手续费扣减，回合盈亏也是成交即扣，所有指标均为净利润口径。

use serde::{Deserialize, Serialize};

use crate::interval::{periods_per_year_from_timestamps, DAYS_PER_YEAR};
use crate::types::Trade;

/// 权益曲线点
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EquityPoint {
    pub timestamp: u64,
    pub equity: f64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BacktestMetrics {
    /// 总收益率（%，净利润口径）
    pub total_return_pct: f64,
    /// 年化收益率（%）
    pub annualized_return_pct: f64,
    /// 最大回撤（%，正数）
    pub max_drawdown_pct: f64,
    /// 夏普比率（日化收益年化）
    pub sharpe_ratio: f64,
    /// 胜率（%，按回合计）
    pub win_rate_pct: f64,
    /// 完成的买卖回合数
    pub num_round_trips: usize,
    /// 累计手续费
    pub total_fees: f64,
    /// profit factor = 盈利回合净利润之和 / 亏损回合净亏之和；无亏损回合时 None（无穷大）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profit_factor: Option<f64>,
    /// 盈亏比 = 平均单笔盈利 / 平均单笔亏损（绝对值）；无亏损或无盈利回合时 None
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payoff_ratio: Option<f64>,
    /// Calmar = 年化收益率 / 最大回撤；回撤为 0 时无定义
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calmar_ratio: Option<f64>,
    /// 暴露率（%）：已平仓回合持仓时间并集 / 评估区间长度（未平仓持仓不计入）
    pub exposure_pct: f64,
    /// Sortino = 平均收益 / 下行标准差（年化）：只惩罚亏损波动，无下行波动时无定义
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sortino_ratio: Option<f64>,
    /// 年化波动率（%）：逐根收益标准差按周期年化
    #[serde(default)]
    pub annualized_volatility_pct: f64,
    /// 最长回撤持续天数：自权益峰值起到重新收复该峰值（未收复则算到期末）的最长天数
    #[serde(default)]
    pub max_drawdown_duration_days: f64,
}

const MS_PER_DAY: f64 = 86_400_000.0;

/// 从权益曲线与交易记录计算指标。
/// 夏普/Sortino/波动率的年化周期数由曲线时间戳自适应推断（见
/// [`periods_per_year_from_timestamps`]），因此 1h/4h 等短周期回测口径同样正确；
/// 日线曲线推断结果恰为 365，与历史口径完全一致。
/// 年化/夏普的计算区间从首次动用资金起算：策略热身期空仓、收益恒为 0，
/// 计入会稀释年化与夏普（与对标回测系统“曲线从首个有效评估日起”的口径一致）。
pub fn compute_metrics(
    initial_cash: f64,
    curve: &[EquityPoint],
    trades: &[Trade],
    total_fees: f64,
) -> BacktestMetrics {
    let final_equity = curve.last().map(|p| p.equity).unwrap_or(initial_cash);
    let total_return_pct = if initial_cash > 0.0 {
        (final_equity / initial_cash - 1.0) * 100.0
    } else {
        0.0
    };

    // 裁掉前置空仓平坦段（保留其前一点作为首笔收益的基准）
    let eps = initial_cash.abs() * 1e-9 + 1e-12;
    let first_active = curve
        .iter()
        .position(|p| (p.equity - initial_cash).abs() > eps)
        .unwrap_or(0);
    let active_curve = if first_active > 0 {
        &curve[first_active - 1..]
    } else {
        curve
    };

    let annualized_return_pct = match (active_curve.first(), active_curve.last()) {
        (Some(first), Some(last)) if last.timestamp > first.timestamp && initial_cash > 0.0 => {
            let days = (last.timestamp - first.timestamp) as f64 / MS_PER_DAY;
            let years = days / DAYS_PER_YEAR;
            if years > 1e-9 {
                ((final_equity / initial_cash).powf(1.0 / years) - 1.0) * 100.0
            } else {
                0.0
            }
        }
        _ => 0.0,
    };

    let mut peak = f64::MIN;
    let mut max_dd = 0.0_f64;
    for p in curve {
        peak = peak.max(p.equity);
        if peak > 0.0 {
            max_dd = max_dd.max((peak - p.equity) / peak);
        }
    }

    let RiskRatios {
        sharpe_ratio,
        sortino_ratio,
        annualized_volatility_pct,
    } = risk_ratios_from_curve(active_curve);
    let max_drawdown_duration_days = max_drawdown_duration_days(curve);

    let wins = trades.iter().filter(|t| t.pnl > 0.0).count();
    let win_rate_pct = if trades.is_empty() {
        0.0
    } else {
        wins as f64 / trades.len() as f64 * 100.0
    };

    // 回合级风险指标（均为净利润口径，盈亏已扣手续费）
    let gross_win: f64 = trades.iter().filter(|t| t.pnl > 0.0).map(|t| t.pnl).sum::<f64>() + 0.0;
    let gross_loss: f64 = trades.iter().filter(|t| t.pnl < 0.0).map(|t| -t.pnl).sum();
    let losses = trades.iter().filter(|t| t.pnl < 0.0).count();
    let profit_factor = if gross_loss > 1e-12 {
        Some(gross_win / gross_loss)
    } else {
        None // 无亏损回合：profit factor 为无穷大，以 None 表达
    };
    let payoff_ratio = if gross_loss > 1e-12 && wins > 0 {
        Some((gross_win / wins as f64) / (gross_loss / losses as f64))
    } else {
        None
    };
    let calmar_ratio = if max_dd > 1e-12 {
        Some(annualized_return_pct / (max_dd * 100.0))
    } else {
        None
    };
    let exposure_pct = exposure_from_trades(active_curve, trades);

    BacktestMetrics {
        total_return_pct,
        annualized_return_pct,
        max_drawdown_pct: max_dd * 100.0,
        sharpe_ratio,
        win_rate_pct,
        num_round_trips: trades.len(),
        total_fees,
        profit_factor,
        payoff_ratio,
        calmar_ratio,
        exposure_pct,
        sortino_ratio,
        annualized_volatility_pct,
        max_drawdown_duration_days,
    }
}

/// 暴露率：将每个已平仓回合的 [entry_time, exit_time] 合并为不重叠区间，
/// 总持仓时长 / 评估区间（active_curve 首尾跨度）时长 × 100。
fn exposure_from_trades(curve: &[EquityPoint], trades: &[Trade]) -> f64 {
    let (span_start, span_end) = match (curve.first(), curve.last()) {
        (Some(a), Some(b)) if b.timestamp > a.timestamp => (a.timestamp, b.timestamp),
        _ => return 0.0,
    };
    let mut iv: Vec<(u64, u64)> = trades
        .iter()
        .map(|t| (t.entry_time.min(t.exit_time), t.exit_time.max(t.entry_time)))
        .collect();
    if iv.is_empty() {
        return 0.0;
    }
    iv.sort_unstable();
    let mut held = 0u64;
    let (mut cur_start, mut cur_end) = iv[0];
    for &(s, e) in &iv[1..] {
        if s <= cur_end {
            cur_end = cur_end.max(e);
        } else {
            held += cur_end - cur_start;
            cur_start = s;
            cur_end = e;
        }
    }
    held += cur_end - cur_start;
    (held as f64 / (span_end - span_start) as f64 * 100.0).min(100.0)
}

/// 权益曲线的逐点日收益序列（过滤非正权益点），供夏普/β/相关性复用
fn returns_from_curve(curve: &[EquityPoint]) -> Vec<f64> {
    curve
        .windows(2)
        .filter(|w| w[0].equity > 0.0)
        .map(|w| w[1].equity / w[0].equity - 1.0)
        .collect()
}

/// 波动类风险指标（共用同一条收益序列与同一个年化周期数）
struct RiskRatios {
    sharpe_ratio: f64,
    sortino_ratio: Option<f64>,
    annualized_volatility_pct: f64,
}

/// 夏普 / Sortino / 年化波动率。
///
/// 年化因子取 sqrt(每年周期数)，周期数由曲线时间戳推断：日线得 365，
/// 4h 得 365×6。硬编码 365 会让短周期回测的夏普被系统性高估。
fn risk_ratios_from_curve(curve: &[EquityPoint]) -> RiskRatios {
    let returns = returns_from_curve(curve);
    if returns.len() < 2 {
        return RiskRatios {
            sharpe_ratio: 0.0,
            sortino_ratio: None,
            annualized_volatility_pct: 0.0,
        };
    }
    let timestamps: Vec<u64> = curve.iter().map(|p| p.timestamp).collect();
    let annualize = periods_per_year_from_timestamps(&timestamps).sqrt();

    let mean = returns.iter().sum::<f64>() / returns.len() as f64;
    let std = variance(&returns, mean).sqrt();
    let sharpe_ratio = if std < 1e-12 { 0.0 } else { mean / std * annualize };

    // 下行标准差：只对负收益取平方，分母仍为全样本数（Sortino 标准定义）
    let downside_sq: f64 = returns.iter().filter(|r| **r < 0.0).map(|r| r * r).sum();
    let downside_std = (downside_sq / returns.len() as f64).sqrt();
    let sortino_ratio = if downside_std > 1e-12 {
        Some(mean / downside_std * annualize)
    } else {
        None // 无亏损周期：下行风险为 0，Sortino 无穷大
    };

    RiskRatios {
        sharpe_ratio,
        sortino_ratio,
        annualized_volatility_pct: std * annualize * 100.0,
    }
}

/// 最长回撤持续天数：权益创新高后到重新收复该高点之间的最长跨度。
/// 期末仍未收复时，算到曲线最后一点（未修复的回撤对投资者同样是持续痛苦）。
fn max_drawdown_duration_days(curve: &[EquityPoint]) -> f64 {
    let mut peak = f64::MIN;
    let mut peak_ts = match curve.first() {
        Some(p) => p.timestamp,
        None => return 0.0,
    };
    let mut underwater = false;
    let mut longest_ms = 0u64;
    for p in curve {
        if p.equity >= peak {
            // 收复峰值：整段水下时长在此刻才完整可知
            if underwater {
                longest_ms = longest_ms.max(p.timestamp.saturating_sub(peak_ts));
                underwater = false;
            }
            peak = p.equity;
            peak_ts = p.timestamp;
        } else {
            // 仍在水下：持续结算，保证期末未收复的尾段也被计入
            underwater = true;
            longest_ms = longest_ms.max(p.timestamp.saturating_sub(peak_ts));
        }
    }
    longest_ms as f64 / MS_PER_DAY
}

/// 样本方差（n-1 无偏估计）
fn variance(xs: &[f64], mean: f64) -> f64 {
    if xs.len() < 2 {
        return 0.0;
    }
    xs.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (xs.len() - 1) as f64
}

/// 策略相对基准的统计量（两条权益曲线逐点配对，按较短者截断）：
/// β = cov(r_s, r_b)/var(r_b)，相关系数，信息比率 = 年化超额 / 年化跟踪误差。
/// 基准收益方差为 0（横盘）时 β/相关系数无定义；超额恒为 0 时 IR 无定义。
pub fn relative_stats(strategy: &[EquityPoint], benchmark: &[EquityPoint]) -> RelativeStats {
    let n = strategy.len().min(benchmark.len());
    let rs = returns_from_curve(&strategy[..n]);
    let rb = returns_from_curve(&benchmark[..n]);
    let m = rs.len().min(rb.len());
    if m < 2 {
        return RelativeStats::default();
    }
    let (rs, rb) = (&rs[..m], &rb[..m]);
    let ms = rs.iter().sum::<f64>() / m as f64;
    let mb = rb.iter().sum::<f64>() / m as f64;
    let var_b = variance(rb, mb);
    let cov: f64 = rs
        .iter()
        .zip(rb)
        .map(|(a, b)| (a - ms) * (b - mb))
        .sum::<f64>()
        / (m - 1) as f64;
    let var_s = variance(rs, ms);
    let beta = if var_b > 1e-18 { Some(cov / var_b) } else { None };
    let correlation = if var_b > 1e-18 && var_s > 1e-18 {
        Some(cov / (var_s.sqrt() * var_b.sqrt()))
    } else {
        None
    };
    // 超额收益序列的均值/标准差 -> 信息比率（年化因子随曲线周期自适应）
    let ex: Vec<f64> = rs.iter().zip(rb).map(|(a, b)| a - b).collect();
    let me = ex.iter().sum::<f64>() / m as f64;
    let se = variance(&ex, me).sqrt();
    let timestamps: Vec<u64> = strategy[..n].iter().map(|p| p.timestamp).collect();
    let annualize = periods_per_year_from_timestamps(&timestamps).sqrt();
    let information_ratio = if se > 1e-12 {
        Some(me / se * annualize)
    } else {
        None
    };
    RelativeStats { beta, correlation, information_ratio }
}

/// 策略相对基准的统计量（超额年化由调用方用两侧年化相减得出）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RelativeStats {
    /// β：相对基准的系统性暴露；基准横盘时无定义
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub beta: Option<f64>,
    /// 日收益 Pearson 相关系数
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation: Option<f64>,
    /// 信息比率 = 年化超额收益 / 年化跟踪误差；超额恒为 0 时无定义
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub information_ratio: Option<f64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pt(day: f64, equity: f64) -> EquityPoint {
        EquityPoint {
            timestamp: (day * MS_PER_DAY) as u64,
            equity,
        }
    }

    fn trade(pnl: f64, entry_day: f64, exit_day: f64) -> Trade {
        Trade {
            symbol: "BTCUSDT".into(),
            entry_price: 100.0,
            exit_price: 100.0,
            quantity: 1.0,
            pnl,
            entry_time: (entry_day * MS_PER_DAY) as u64,
            exit_time: (exit_day * MS_PER_DAY) as u64,
        }
    }

    #[test]
    fn test_trade_risk_metrics() {
        // 两赢 +300/+100，一亏 -200：PF=2.0，盈亏比=200/200=1.0，胜率 66.7%
        let curve = vec![pt(0.0, 100.0), pt(10.0, 100.0)];
        let trades = vec![
            trade(300.0, 2.0, 4.0),
            trade(100.0, 6.0, 8.0),
            trade(-200.0, 8.0, 9.0),
        ];
        let m = compute_metrics(100.0, &curve, &trades, 0.0);
        assert!((m.profit_factor.unwrap() - 2.0).abs() < 1e-9);
        assert!((m.payoff_ratio.unwrap() - 1.0).abs() < 1e-9);
        assert!((m.win_rate_pct - 200.0 / 3.0).abs() < 1e-6);
        // 持仓并集 [2,4]+[6,9] = 5 天 / 10 天 = 50%
        assert!((m.exposure_pct - 50.0).abs() < 1e-9);
    }

    #[test]
    fn test_no_loss_profit_factor_none() {
        let curve = vec![pt(0.0, 100.0), pt(5.0, 120.0)];
        let trades = vec![trade(50.0, 1.0, 3.0)];
        let m = compute_metrics(100.0, &curve, &trades, 0.0);
        assert!(m.profit_factor.is_none());
        assert!(m.payoff_ratio.is_none());
    }

    #[test]
    fn test_calmar_ratio() {
        // 一年：100 → 120 → 60 → 110，年化 10%，最大回撤 50% → Calmar 0.2
        let curve = vec![pt(0.0, 100.0), pt(182.0, 120.0), pt(200.0, 60.0), pt(365.0, 110.0)];
        let m = compute_metrics(100.0, &curve, &[], 0.0);
        assert!((m.max_drawdown_pct - 50.0).abs() < 1e-9);
        assert!((m.annualized_return_pct - 10.0).abs() < 0.1);
        assert!((m.calmar_ratio.unwrap() - 0.2).abs() < 1e-3);
    }

    #[test]
    fn test_zero_drawdown_calmar_none() {
        let curve = vec![pt(0.0, 100.0), pt(10.0, 110.0)];
        let m = compute_metrics(100.0, &curve, &[], 0.0);
        assert!(m.calmar_ratio.is_none());
        assert_eq!(m.exposure_pct, 0.0);
    }

    fn curve_from(values: &[f64]) -> Vec<EquityPoint> {
        values
            .iter()
            .enumerate()
            .map(|(i, v)| pt(i as f64, *v))
            .collect()
    }

    #[test]
    fn test_relative_stats_leverage() {
        // 策略日收益 = 2 × 基准日收益：beta=2，相关系数=1，IR 有定义且为正
        let bench = curve_from(&[100.0, 101.0, 99.99, 100.9899, 99.980001]);
        let strat = curve_from(&[100.0, 102.0, 99.96, 101.9592, 99.920016]);
        let r = relative_stats(&strat, &bench);
        assert!((r.beta.unwrap() - 2.0).abs() < 1e-9);
        assert!((r.correlation.unwrap() - 1.0).abs() < 1e-9);
        assert!(r.information_ratio.unwrap() > 0.0);
    }

    #[test]
    fn test_relative_stats_identical() {
        // 与基准完全一致：beta=1、corr=1、超额恒 0 -> IR 无定义
        let bench = curve_from(&[100.0, 103.0, 98.0, 105.0]);
        let r = relative_stats(&bench, &bench);
        assert!((r.beta.unwrap() - 1.0).abs() < 1e-9);
        assert!((r.correlation.unwrap() - 1.0).abs() < 1e-9);
        assert!(r.information_ratio.is_none());
    }

    #[test]
    fn test_relative_stats_flat_benchmark() {
        // 基准横盘（方差 0）：beta/相关系数无定义；样本不足时全部缺省
        let bench = curve_from(&[100.0, 100.0, 100.0]);
        let strat = curve_from(&[100.0, 101.0, 102.0]);
        let r = relative_stats(&strat, &bench);
        assert!(r.beta.is_none());
        assert!(r.correlation.is_none());
        let tiny = relative_stats(&strat[..1], &bench);
        assert!(tiny.beta.is_none() && tiny.information_ratio.is_none());
    }

    /// 按指定毫秒间隔构造权益曲线（用于验证周期自适应年化）
    fn curve_spaced(values: &[f64], step_ms: u64) -> Vec<EquityPoint> {
        values
            .iter()
            .enumerate()
            .map(|(i, v)| EquityPoint {
                timestamp: i as u64 * step_ms,
                equity: *v,
            })
            .collect()
    }

    #[test]
    fn test_sharpe_annualization_follows_bar_interval() {
        use crate::interval::Interval;
        let values = [100.0, 101.0, 100.5, 102.0, 101.0, 103.0, 102.5];

        // 日线：年化因子必须仍是 sqrt(365)，与历史口径完全一致
        let daily = curve_spaced(&values, Interval::D1.ms());
        let m_daily = compute_metrics(100.0, &daily, &[], 0.0);
        let returns: Vec<f64> = values.windows(2).map(|w| w[1] / w[0] - 1.0).collect();
        let mean = returns.iter().sum::<f64>() / returns.len() as f64;
        let std = variance(&returns, mean).sqrt();
        let expected_daily = mean / std * 365.0_f64.sqrt();
        assert!(
            (m_daily.sharpe_ratio - expected_daily).abs() < 1e-9,
            "日线夏普口径不得改变: {} vs {}",
            m_daily.sharpe_ratio,
            expected_daily
        );

        // 同一条收益序列改为 4h 间隔：一年 6 倍周期数，夏普按 sqrt(6) 放大
        let four_hour = curve_spaced(&values, Interval::H4.ms());
        let m_4h = compute_metrics(100.0, &four_hour, &[], 0.0);
        assert!(
            (m_4h.sharpe_ratio - expected_daily * 6.0_f64.sqrt()).abs() < 1e-9,
            "4h 夏普应按 sqrt(6) 年化: {}",
            m_4h.sharpe_ratio
        );
    }

    #[test]
    fn test_sortino_and_volatility() {
        // 只涨不跌：无下行波动，Sortino 无定义
        let up = curve_from(&[100.0, 101.0, 102.0, 103.0]);
        let m_up = compute_metrics(100.0, &up, &[], 0.0);
        assert!(m_up.sortino_ratio.is_none());
        assert!(m_up.annualized_volatility_pct > 0.0);

        // 有回撤：下行标准差 < 总标准差，故 Sortino > 夏普
        let mixed = curve_from(&[100.0, 104.0, 103.0, 108.0, 107.0, 112.0]);
        let m = compute_metrics(100.0, &mixed, &[], 0.0);
        let sortino = m.sortino_ratio.expect("存在亏损周期，Sortino 应有定义");
        assert!(
            sortino > m.sharpe_ratio,
            "Sortino({sortino}) 应高于 Sharpe({})",
            m.sharpe_ratio
        );
    }

    #[test]
    fn test_max_drawdown_duration() {
        // 第 1 天见峰 120，第 2 天跌到 90，第 6 天才收复 -> 水下 5 天；
        // 之后第 7 天新高 130，第 8 天回落且期末未收复 -> 水下 2 天。取最长 5 天。
        let curve = vec![
            pt(0.0, 100.0),
            pt(1.0, 120.0),
            pt(2.0, 90.0),
            pt(6.0, 120.0),
            pt(7.0, 130.0),
            pt(9.0, 125.0),
        ];
        let m = compute_metrics(100.0, &curve, &[], 0.0);
        assert!(
            (m.max_drawdown_duration_days - 5.0).abs() < 1e-9,
            "最长水下天数={}",
            m.max_drawdown_duration_days
        );

        // 单调上涨：从不水下
        let up = curve_from(&[100.0, 110.0, 120.0]);
        assert_eq!(
            compute_metrics(100.0, &up, &[], 0.0).max_drawdown_duration_days,
            0.0
        );
    }

    #[test]
    fn test_compute_metrics_empty_curve() {
        let curve = vec![];
        let m = compute_metrics(10000.0, &curve, &[], 0.0);
        
        assert!((m.total_return_pct - 0.0).abs() < 1e-6);
        assert!((m.annualized_return_pct - 0.0).abs() < 1e-6);
        assert_eq!(m.num_round_trips, 0);
        assert_eq!(m.win_rate_pct, 0.0);
    }

    #[test]
    fn test_compute_metrics_single_point() {
        let curve = vec![pt(0.0, 10000.0)];
        let m = compute_metrics(10000.0, &curve, &[], 0.0);
        
        assert!((m.total_return_pct - 0.0).abs() < 1e-6);
        assert!((m.annualized_return_pct - 0.0).abs() < 1e-6);
        assert!((m.sharpe_ratio - 0.0).abs() < 1e-6);
    }

    #[test]
    fn test_compute_metrics_zero_initial_cash() {
        let curve = vec![pt(0.0, 0.0), pt(10.0, 100.0)];
        let m = compute_metrics(0.0, &curve, &[], 0.0);
        
        assert!((m.total_return_pct - 0.0).abs() < 1e-6);
        assert!((m.annualized_return_pct - 0.0).abs() < 1e-6);
    }

    #[test]
    fn test_exposure_from_trades_overlapping() {
        // 重叠持仓区间应合并
        let curve = vec![pt(0.0, 100.0), pt(10.0, 110.0)];
        let trades = vec![
            trade(100.0, 1.0, 5.0),
            trade(100.0, 3.0, 7.0), // 与上一笔重叠
            trade(100.0, 6.0, 9.0), // 与上一笔部分重叠
        ];
        let m = compute_metrics(100.0, &curve, &trades, 0.0);
        
        // 合并后区间 [1, 9] = 8 天 / 10 天 = 80%
        assert!((m.exposure_pct - 80.0).abs() < 1e-6);
    }

    #[test]
    fn test_returns_from_curve_with_zero_equity() {
        // 权益为零的点应被过滤（作为窗口的起点时）
        let curve = vec![
            pt(0.0, 100.0),
            pt(1.0, 0.0),  // 零权益
            pt(2.0, 110.0),
        ];
        let returns = returns_from_curve(&curve);
        
        // windows(2) 产生两个窗口：[100, 0] 和 [0, 110]
        // [100, 0]: w[0]=100 > 0，保留，返回 0/100 - 1 = -1.0
        // [0, 110]: w[0]=0 不大于 0，过滤
        assert_eq!(returns.len(), 1);
        assert!((returns[0] + 1.0).abs() < 1e-6); // -100% 收益
    }

    #[test]
    fn test_risk_ratios_from_curve_insufficient_data() {
        // 少于 2 个点应返回默认值
        let curve = vec![pt(0.0, 100.0)];
        let ratios = risk_ratios_from_curve(&curve);
        
        assert!((ratios.sharpe_ratio - 0.0).abs() < 1e-6);
        assert!(ratios.sortino_ratio.is_none());
        assert!((ratios.annualized_volatility_pct - 0.0).abs() < 1e-6);
    }

    #[test]
    fn test_variance_single_value() {
        // 单值方差应为 0
        let xs = vec![100.0];
        let var = variance(&xs, 100.0);
        assert!((var - 0.0).abs() < 1e-6);
    }

    #[test]
    fn test_relative_stats_insufficient_data() {
        // 样本不足应返回默认值
        let strat = vec![pt(0.0, 100.0)];
        let bench = vec![pt(0.0, 100.0)];
        let r = relative_stats(&strat, &bench);
        
        assert!(r.beta.is_none());
        assert!(r.correlation.is_none());
        assert!(r.information_ratio.is_none());
    }

    #[test]
    fn test_equity_point_serialization() {
        let point = EquityPoint {
            timestamp: 1724932800000,
            equity: 10500.0,
        };
        
        let json = serde_json::to_string(&point).unwrap();
        assert!(json.contains("1724932800000"));
        assert!(json.contains("10500"));
        
        let deserialized: EquityPoint = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.timestamp, point.timestamp);
        assert!((deserialized.equity - 10500.0).abs() < 1e-6);
    }

    #[test]
    fn test_backtest_metrics_serialization() {
        let metrics = BacktestMetrics {
            total_return_pct: 10.0,
            annualized_return_pct: 12.0,
            max_drawdown_pct: 5.0,
            sharpe_ratio: 1.5,
            win_rate_pct: 60.0,
            num_round_trips: 10,
            total_fees: 50.0,
            profit_factor: Some(2.0),
            payoff_ratio: Some(1.5),
            calmar_ratio: Some(2.4),
            exposure_pct: 70.0,
            sortino_ratio: Some(2.0),
            annualized_volatility_pct: 15.0,
            max_drawdown_duration_days: 10.0,
        };
        
        let json = serde_json::to_string(&metrics).unwrap();
        assert!(json.contains("10.0"));
        assert!(json.contains("2.0")); // profit_factor
        
        let deserialized: BacktestMetrics = serde_json::from_str(&json).unwrap();
        assert!((deserialized.total_return_pct - 10.0).abs() < 1e-6);
        assert_eq!(deserialized.profit_factor, Some(2.0));
    }

    #[test]
    fn test_backtest_metrics_serialization_skips_none() {
        let metrics = BacktestMetrics {
            total_return_pct: 5.0,
            annualized_return_pct: 6.0,
            max_drawdown_pct: 0.0,
            sharpe_ratio: 0.0,
            win_rate_pct: 0.0,
            num_round_trips: 0,
            total_fees: 0.0,
            profit_factor: None,
            payoff_ratio: None,
            calmar_ratio: None,
            exposure_pct: 0.0,
            sortino_ratio: None,
            annualized_volatility_pct: 0.0,
            max_drawdown_duration_days: 0.0,
        };
        
        let json = serde_json::to_string(&metrics).unwrap();
        // None 值应被跳过
        assert!(!json.contains("profit_factor"));
        assert!(!json.contains("payoff_ratio"));
        assert!(!json.contains("calmar_ratio"));
        assert!(!json.contains("sortino_ratio"));
    }

    #[test]
    fn test_relative_stats_serialization() {
        let stats = RelativeStats {
            beta: Some(1.2),
            correlation: Some(0.8),
            information_ratio: Some(0.5),
        };
        
        let json = serde_json::to_string(&stats).unwrap();
        assert!(json.contains("1.2"));
        assert!(json.contains("0.8"));
        
        let deserialized: RelativeStats = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.beta, Some(1.2));
    }

    #[test]
    fn test_pt_helper_function() {
        let point = pt(5.0, 10500.0);
        assert_eq!(point.timestamp, (5.0 * MS_PER_DAY) as u64);
        assert!((point.equity - 10500.0).abs() < 1e-6);
    }

    #[test]
    fn test_trade_helper_function() {
        let t = trade(500.0, 1.0, 5.0);
        assert_eq!(t.symbol, "BTCUSDT");
        assert!((t.pnl - 500.0).abs() < 1e-6);
        assert_eq!(t.entry_time, (1.0 * MS_PER_DAY) as u64);
        assert_eq!(t.exit_time, (5.0 * MS_PER_DAY) as u64);
    }

    #[test]
    fn test_curve_from_helper() {
        let curve = curve_from(&[100.0, 105.0, 110.0]);
        assert_eq!(curve.len(), 3);
        assert_eq!(curve[0].timestamp, 0);
        assert_eq!(curve[1].timestamp, (1.0 * MS_PER_DAY) as u64);
    }

    #[test]
    fn test_curve_spaced_helper() {
        let curve = curve_spaced(&[100.0, 105.0, 110.0], 3600000); // 1小时间隔
        assert_eq!(curve.len(), 3);
        assert_eq!(curve[0].timestamp, 0);
        assert_eq!(curve[1].timestamp, 3600000);
        assert_eq!(curve[2].timestamp, 7200000);
    }

    #[test]
    fn test_exposure_from_trades_empty() {
        let curve = vec![pt(0.0, 100.0), pt(10.0, 110.0)];
        let exposure = exposure_from_trades(&curve, &[]);
        assert!((exposure - 0.0).abs() < 1e-6);
    }

    #[test]
    fn test_exposure_from_trades_single_point_curve() {
        let curve = vec![pt(0.0, 100.0)];
        let trades = vec![trade(100.0, 0.0, 5.0)];
        let exposure = exposure_from_trades(&curve, &trades);
        assert!((exposure - 0.0).abs() < 1e-6);
    }

    #[test]
    fn test_compute_metrics_negative_return() {
        let curve = vec![pt(0.0, 10000.0), pt(365.0, 8000.0)];
        let m = compute_metrics(10000.0, &curve, &[], 0.0);
        
        assert!((m.total_return_pct + 20.0).abs() < 1e-6);
        assert!(m.annualized_return_pct < 0.0);
    }

    #[test]
    fn test_compute_metrics_with_fees_only() {
        // 只有手续费亏损，无交易
        let curve = vec![pt(0.0, 10000.0), pt(10.0, 9900.0)];
        let m = compute_metrics(10000.0, &curve, &[], 100.0);
        
        assert!((m.total_fees - 100.0).abs() < 1e-6);
        assert!((m.total_return_pct + 1.0).abs() < 1e-6);
    }
}
