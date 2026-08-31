//! 基准对比：基准品种买入持有曲线 + 策略相对基准的统计量。
//!
//! 基准曲线与策略权益曲线逐点时间对齐（取 open_time ≤ 权益点时间戳的最近收盘），
//! 与策略同初始资金归一，指标沿用净利润口径的 compute_metrics（无交易、无手续费）。

use quantkit_core::metrics::{compute_metrics, relative_stats, BacktestMetrics, EquityPoint};
use quantkit_core::types::Kline;
use serde::Serialize;

/// 基准对比报告（随回测结果一并输出）
#[derive(Debug, Clone, Serialize)]
pub struct BenchmarkReport {
    pub symbol: String,
    /// 基准买入持有的指标（同初始资金、净利润口径）
    pub metrics: BacktestMetrics,
    /// 年化超额收益（%）= 策略年化 - 基准年化
    pub excess_annualized_pct: f64,
    /// β：基准横盘时无定义
    #[serde(skip_serializing_if = "Option::is_none")]
    pub beta: Option<f64>,
    /// 日收益相关系数
    #[serde(skip_serializing_if = "Option::is_none")]
    pub correlation: Option<f64>,
    /// 信息比率
    #[serde(skip_serializing_if = "Option::is_none")]
    pub information_ratio: Option<f64>,
    /// 与策略权益曲线对齐后的基准权益曲线
    pub curve: Vec<EquityPoint>,
}

/// 构建基准报告；窗口内无重叠数据时返回 None（调用方提示后继续）
pub fn build_report(
    symbol: &str,
    klines: &[Kline],
    strategy_curve: &[EquityPoint],
    strategy_metrics: &BacktestMetrics,
    initial_cash: f64,
) -> Option<BenchmarkReport> {
    if klines.is_empty() || strategy_curve.is_empty() {
        return None;
    }
    let curve = benchmark_curve(klines, strategy_curve, initial_cash);
    if curve.len() < 2 {
        return None;
    }
    let metrics = compute_metrics(initial_cash, &curve, &[], 0.0);
    let rel = relative_stats(strategy_curve, &curve);
    Some(BenchmarkReport {
        symbol: symbol.to_string(),
        excess_annualized_pct: strategy_metrics.annualized_return_pct
            - metrics.annualized_return_pct,
        beta: rel.beta,
        correlation: rel.correlation,
        information_ratio: rel.information_ratio,
        metrics,
        curve,
    })
}

/// 基准买入持有权益曲线：对每个策略权益点取最近已收盘K线的收盘价，
/// 以首个对齐点为基准归一到初始资金。早于基准首根K线的权益点跳过。
fn benchmark_curve(
    klines: &[Kline],
    strategy_curve: &[EquityPoint],
    initial_cash: f64,
) -> Vec<EquityPoint> {
    let mut out = Vec::with_capacity(strategy_curve.len());
    let mut ki = 0usize;
    let mut base: Option<f64> = None;
    for p in strategy_curve {
        while ki + 1 < klines.len() && klines[ki + 1].open_time <= p.timestamp {
            ki += 1;
        }
        if klines[ki].open_time > p.timestamp {
            continue;
        }
        let close = klines[ki].close;
        let b = *base.get_or_insert(close);
        out.push(EquityPoint {
            timestamp: p.timestamp,
            equity: initial_cash * close / b,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use quantkit_core::types::Kline;

    fn k(day: u64, close: f64) -> Kline {
        Kline {
            open_time: day * 86_400_000,
            open: close,
            high: close,
            low: close,
            close,
            volume: 1.0,
            close_time: (day + 1) * 86_400_000 - 1,
        }
    }

    fn pt(day: u64, equity: f64) -> EquityPoint {
        EquityPoint { timestamp: day * 86_400_000, equity }
    }

    #[test]
    fn test_benchmark_curve_alignment() {
        // 基准价格 100 -> 120 -> 90；权益点在第 0/1/2 天
        // 注意：open_time <= t 的最近收盘 —— 第 0 天取首根（基准），第 1 天取第 1 根
        // 10.0 点在第 0 天之前 -> 跳过；0.5 天也早于第 1 根开盘，仍用第 0 根
        // 这里用整数天构造：点(1,?), (2,?), (3,?) 对应基准收盘 100/120/90
        let ks = vec![k(1, 100.0), k(2, 120.0), k(3, 90.0)];
        let strat = vec![pt(0, 1000.0), pt(1, 1000.0), pt(2, 1100.0), pt(3, 1050.0)];
        let c = benchmark_curve(&ks, &strat, 1000.0);
        assert_eq!(c.len(), 3); // t=0 早于基准首根，跳过
        assert!((c[0].equity - 1000.0).abs() < 1e-9);
        assert!((c[1].equity - 1200.0).abs() < 1e-9);
        assert!((c[2].equity - 900.0).abs() < 1e-9);
    }

    #[test]
    fn test_build_report_none_when_no_overlap() {
        let ks = vec![k(10, 100.0), k(11, 110.0)];
        let strat = vec![pt(0, 100.0), pt(1, 105.0)];
        let m = compute_metrics(100.0, &strat, &[], 0.0);
        assert!(build_report("BTCUSDT", &ks, &strat, &m, 100.0).is_none());
    }

    #[test]
    fn test_build_report_excess() {
        // 一年期：基准 100 -> 110（+10%），策略 100 -> 130（+30%）
        let ks = vec![k(0, 100.0), k(365, 110.0)];
        let strat = vec![pt(0, 100.0), pt(365, 130.0)];
        let sm = compute_metrics(100.0, &strat, &[], 0.0);
        let r = build_report("BTCUSDT", &ks, &strat, &sm, 100.0).unwrap();
        assert!(r.excess_annualized_pct > 19.0 && r.excess_annualized_pct < 21.0);
        // 两点曲线日收益样本仅 1 个 -> beta/相关系数无定义
        assert!(r.beta.is_none());
        assert!((r.metrics.total_return_pct - 10.0).abs() < 1e-9);
    }
}
