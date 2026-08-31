//! 日级动量轮动策略（二期升级：Top-N 分散 + 市场状态过滤）。
//!
//! 规则：
//! - 每 `rebalance_interval_days` 天评估一次：
//!   1. 市场状态过滤（若启用）：池内「收盘价 > MA(regime_ma_days)」的品种占比
//!      低于 `regime_min_breadth` 时判为熊市 —— 清仓全部持仓、不开新仓
//!   2. 动量排名：计算各品种 `momentum_days` 日动量（close / close_N天前 - 1），
//!      仅保留动量 > 0 且「收盘价 > MA(ma_days)」的品种，取动量最高的
//!      `top_n` 个为目标（top_n = 1 即老版集中轮动）
//!   3. 差量调仓：退出目标集合的品种全部卖出；已在目标中的持仓保持不动
//!      （降低换手与手续费）；卖出回笼现金 + 现有现金等额分配给新进入品种
//! - 持仓逐品种追踪止损：从各自峰值回撤 >= `trailing_stop_pct` 立即清仓该品种，
//!   不设冻结期：若当天恰逢调仓评估且该品种仍在目标集合，按新信号立即重入
//! - 换仓时先卖后买（引擎保证卖单先于买单撮合）

use std::collections::{BTreeMap, BTreeSet, HashMap};

use quantkit_core::strategy::{Strategy, StrategyContext};
use quantkit_core::types::{Kline, Order};

const MS_PER_DAY: u64 = 86_400_000;

/// 日级动量轮动策略
pub struct MomentumRotation {
    /// 动量回看天数
    pub momentum_days: usize,
    /// 趋势过滤均线天数
    pub ma_days: usize,
    /// 调仓间隔（天）
    pub rebalance_interval_days: u64,
    /// 追踪止损回撤阈值（<=0 表示禁用）
    pub trailing_stop_pct: f64,
    /// 持仓品种数：1 = 集中轮动；>1 = 动量前 N 等额分散
    pub top_n: usize,
    /// 市场状态过滤均线天数（0 = 禁用）
    pub regime_ma_days: usize,
    /// 熊市阈值：池内站上该均线的品种占比低于此值判为熊市（0-1）
    pub regime_min_breadth: f64,
    last_rebalance_ts: u64,
    bars_seen: u64,
    /// 各持仓品种的追踪止损峰值
    peaks: HashMap<String, f64>,
}

impl MomentumRotation {
    /// 老版集中模式（top_n=1、市场状态过滤禁用），保持向后兼容
    pub fn new(
        momentum_days: usize,
        ma_days: usize,
        rebalance_interval_days: u64,
        trailing_stop_pct: f64,
    ) -> Self {
        Self::new_extended(momentum_days, ma_days, rebalance_interval_days, trailing_stop_pct, 1, 0, 0.0)
    }

    /// 完整构造：含 Top-N 分散与市场状态过滤
    #[allow(clippy::too_many_arguments)]
    pub fn new_extended(
        momentum_days: usize,
        ma_days: usize,
        rebalance_interval_days: u64,
        trailing_stop_pct: f64,
        top_n: usize,
        regime_ma_days: usize,
        regime_min_breadth: f64,
    ) -> Self {
        Self {
            momentum_days,
            ma_days,
            rebalance_interval_days,
            trailing_stop_pct,
            top_n: top_n.max(1),
            regime_ma_days,
            regime_min_breadth,
            last_rebalance_ts: 0,
            bars_seen: 0,
            peaks: HashMap::new(),
        }
    }
}

impl Strategy for MomentumRotation {
    fn name(&self) -> &str {
        "momentum_rotation"
    }

    fn on_bars(&mut self, ctx: &StrategyContext, bars: &BTreeMap<String, Kline>) -> Vec<Order> {
        let ts = bars.values().next().map(|k| k.open_time).unwrap_or(0);
        if ts == 0 {
            return vec![];
        }
        self.bars_seen += 1;

        let mut orders = Vec::new();
        let mut stopped: BTreeSet<String> = BTreeSet::new();

        // === 追踪止损检查（每根bar执行、逐品种，不等调仓日）===
        if self.trailing_stop_pct > 0.0 {
            let held: Vec<String> = ctx.positions().keys().cloned().collect();
            for sym in held {
                let Some(bar) = bars.get(&sym) else { continue };
                let peak = self.peaks.entry(sym.clone()).or_insert(bar.close);
                *peak = peak.max(bar.close);
                let peak_v = *peak;
                if peak_v > 0.0 {
                    let drawdown = (peak_v - bar.close) / peak_v;
                    if drawdown >= self.trailing_stop_pct {
                        let qty = ctx.position(&sym).map(|p| p.quantity).unwrap_or(0.0);
                        orders.push(Order::market_sell(sym.clone(), qty));
                        self.peaks.remove(&sym);
                        stopped.insert(sym);
                        // 不设冻结期：继续本bar的调仓评估（与老系统一致）
                    }
                }
            }
        }

        // === 调仓评估（周期性）===
        // 热身期 = max(动量回看, 均线窗口-1, 市场状态均线-1) 根bar；
        // 计时锚点为热身期结束的那根bar，且该bar立即做首次评估，
        // 之后每 interval 推进一次。热身按 bar 计数而非时间戳差：
        // K线 open_time 并非严格等间隔；周期到期即推进计时器，无论是否换仓
        let regime_warmup = if self.regime_ma_days > 0 {
            self.regime_ma_days - 1
        } else {
            0
        };
        let warmup = self
            .momentum_days
            .max(self.ma_days.saturating_sub(1))
            .max(regime_warmup) as u64;
        if self.bars_seen <= warmup {
            return orders;
        }
        if self.last_rebalance_ts == 0 {
            self.last_rebalance_ts = ts;
        } else if ts.saturating_sub(self.last_rebalance_ts)
            < self.rebalance_interval_days * MS_PER_DAY
        {
            return orders;
        } else {
            self.last_rebalance_ts = ts;
        }

        // === 市场状态过滤 + 动量排名 ===
        let targets: Vec<String> =
            if regime_bearish(ctx, bars, self.regime_ma_days, self.regime_min_breadth) {
                Vec::new() // 熊市：目标为空 -> 清仓持有、不开新仓
            } else {
                pick_top_n(ctx, bars, self.momentum_days, self.ma_days, self.top_n)
            };

        // 当前持仓（不含刚止损卖出的品种）
        let held: Vec<String> = ctx
            .positions()
            .keys()
            .filter(|s| !stopped.contains(*s))
            .cloned()
            .collect();

        // 退出目标集合的持仓全部卖出。购买力补偿：ctx 里的现金是成交前
        // 快照，需加上所有卖出将回笼的市值（须在持仓被移走前计算）
        let mut sell_value = 0.0;
        for sym in &held {
            if !targets.contains(sym) {
                if let Some(pos) = ctx.position(sym) {
                    orders.push(Order::market_sell(sym.clone(), pos.quantity));
                    let exit = bars.get(sym).map(|k| k.close).unwrap_or(0.0);
                    sell_value += pos.quantity * exit;
                }
                self.peaks.remove(sym);
            }
        }
        for sym in &stopped {
            let exit = bars.get(sym).map(|k| k.close).unwrap_or(0.0);
            if let Some(pos) = ctx.position(sym) {
                sell_value += pos.quantity * exit;
            }
        }

        // 新进入品种等额买入（差量调仓：已持有且仍在目标中的不动，降低换手）
        let entries: Vec<&String> = targets.iter().filter(|t| !held.contains(t)).collect();
        if !entries.is_empty() {
            let buying_power = ctx.cash() + sell_value;
            let per = buying_power / entries.len() as f64;
            if per > 0.0 {
                for t in entries {
                    if let Some(price) = bars.get(t).map(|k| k.close).filter(|p| *p > 0.0) {
                        orders.push(Order::market_buy(t.clone(), per / price));
                        // 峰值重置为入场参考价，避免旧峰值导致刚入场即触发止损
                        self.peaks.insert(t.clone(), price);
                    }
                }
            }
        }
        orders
    }
}

/// 市场状态过滤（池内广度）：统计站上 `regime_ma_days` 日均线的品种占比，
/// 低于 `min_breadth` 判为熊市。无足量样本时不拦截（宁可不做错杀）。
fn regime_bearish(
    ctx: &StrategyContext,
    bars: &BTreeMap<String, Kline>,
    regime_ma_days: usize,
    min_breadth: f64,
) -> bool {
    if regime_ma_days == 0 || min_breadth <= 0.0 {
        return false;
    }
    let mut total = 0usize;
    let mut above = 0usize;
    for sym in bars.keys() {
        let hist = ctx.history(sym, regime_ma_days);
        if hist.len() < regime_ma_days {
            continue;
        }
        let ma: f64 = hist.iter().map(|k| k.close).sum::<f64>() / regime_ma_days as f64;
        total += 1;
        if hist.last().unwrap().close > ma {
            above += 1;
        }
    }
    if total == 0 {
        return false;
    }
    (above as f64) / (total as f64) < min_breadth
}

/// 动量排名 + 均线过滤：返回动量最高的前 `n` 个品种（动量降序）
fn pick_top_n(
    ctx: &StrategyContext,
    bars: &BTreeMap<String, Kline>,
    momentum_days: usize,
    ma_days: usize,
    n: usize,
) -> Vec<String> {
    if n == 0 {
        return Vec::new();
    }
    // 历史窗口需同时满足动量回看与均线窗口（对齐老系统：
    // start_idx = max(lb, ma_n-1) 保证两个窗口都完整后才评估）
    let need = (momentum_days + 1).max(ma_days);
    let mut cands: Vec<(String, f64)> = Vec::new();
    for sym in bars.keys() {
        let hist = ctx.history(sym, need);
        if hist.len() < need {
            continue;
        }
        let close_now = hist.last().unwrap().close;
        // 窗口长度按动量与均线的较大者取，动量基准须固定回看 momentum_days 根
        let close_ago = hist[hist.len() - 1 - momentum_days].close;
        if close_ago <= 0.0 {
            continue;
        }
        let momentum = close_now / close_ago - 1.0;
        if momentum <= 0.0 {
            continue;
        }
        // MA 过滤：收盘价必须在均线上方
        let m = ma_days.min(hist.len());
        let ma: f64 = hist.iter().rev().take(m).map(|k| k.close).sum::<f64>() / m as f64;
        if close_now <= ma {
            continue;
        }
        cands.push((sym.clone(), momentum));
    }
    cands.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    cands.into_iter().take(n).map(|(s, _)| s).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use quantkit_core::engine::{run_backtest, BacktestConfig};
    use quantkit_core::executor::FillModel;

    const DAY: u64 = 86_400_000;

    fn kline(i: u64, close: f64) -> Kline {
        Kline {
            open_time: i * DAY,
            open: close,
            high: close,
            low: close,
            close,
            volume: 1.0,
            close_time: i * DAY + 1000,
        }
    }

    fn config(initial_cash: f64, fee: f64) -> BacktestConfig {
        BacktestConfig {
            initial_cash,
            model: FillModel::new(0.0, fee),
            max_history: 400,
            fill_price: quantkit_core::engine::FillPrice::default(),
            ..Default::default()
        }
    }

    #[test]
    fn test_rotation_into_stronger_momentum() {
        // AAA 横盘（动量=0 被排除），BBB 单边上涨 -> 应买入 BBB 并持有
        let mut aaa = Vec::new();
        let mut bbb = Vec::new();
        for i in 0..30 {
            aaa.push(kline(i, 100.0));
            bbb.push(kline(i, 100.0 + i as f64));
        }
        let mut data = BTreeMap::new();
        data.insert("AAA".to_string(), aaa);
        data.insert("BBB".to_string(), bbb);

        let mut s = MomentumRotation::new(5, 3, 1, 0.0);
        let result = run_backtest(&mut s, &data, &config(10_000.0, 0.0)).unwrap();
        // 建仓后随 BBB 上涨：期末权益应显著高于初始资金
        let last = result.equity_curve.last().unwrap();
        assert!(last.equity > 10_100.0, "equity={}", last.equity);
        // 未发生卖出（无换仓、无空仓信号）
        assert_eq!(result.metrics.num_round_trips, 0);
    }

    #[test]
    fn test_no_winner_stays_in_cash() {
        // 所有品种下跌（动量<0）-> 始终空仓，权益不变
        let mut aaa = Vec::new();
        for i in 0..30 {
            aaa.push(kline(i, 100.0 - i as f64 * 0.5));
        }
        let mut data = BTreeMap::new();
        data.insert("AAA".to_string(), aaa);

        let mut s = MomentumRotation::new(5, 3, 1, 0.0);
        let result = run_backtest(&mut s, &data, &config(10_000.0, 0.001)).unwrap();
        let last = result.equity_curve.last().unwrap();
        assert!((last.equity - 10_000.0).abs() < 1e-9, "应始终空仓");
        assert_eq!(result.metrics.total_fees, 0.0);
    }

    #[test]
    fn test_trailing_stop_and_reentry_resets_peak() {
        // 涨到峰值120 -> 跌破12%止损 -> 新信号重入后峰值必须重置，
        // 否则旧峰值120会让 118 再次触发止损（(120-118)/120=1.7% 不触发，
        // 但 105 重入场景会：旧峰值下 (120-105)/120=12.5% 立即止损）
        let mut klines = Vec::new();
        for i in 0..20 {
            klines.push(kline(i, 100.0 + i as f64)); // 0..19: 100->119
        }
        klines.push(kline(20, 100.0)); // 峰值119回撤16% -> 止损
        klines.push(kline(21, 98.0));
        for (j, c) in [110.0, 112.0, 111.0, 113.0, 112.0].iter().enumerate() {
            klines.push(kline(22 + j as u64, *c));
        }
        let mut data = BTreeMap::new();
        data.insert("AAA".to_string(), klines);

        // 调仓间隔1天：止损后的新信号可立即重入（无冻结期）
        let mut s = MomentumRotation::new(5, 3, 1, 0.12);
        let result = run_backtest(&mut s, &data, &config(10_000.0, 0.0)).unwrap();
        // 至少一次完整回合（止损卖出）
        assert!(
            result.metrics.num_round_trips >= 1,
            "trades={}",
            result.metrics.num_round_trips
        );
        // 重入后稳定持有：期末仍有持仓且权益随 112 计
        let last = result.equity_curve.last().unwrap();
        assert!(last.equity > 9_000.0, "equity={}", last.equity);
    }

    #[test]
    fn test_top_n_holds_multiple_symbols_equally() {
        // 三品种涨幅分明（动量排名 AAA>BBB>CCC 无并列），top_n=2 ->
        // 应持有动量前两名 AAA/BBB 且资金近似均分，期末权益随上涨增长。
        // 注：涨幅必须与初始价错开（同涨 1 元时初始价低者动量反而更高）
        let mut data = BTreeMap::new();
        for (sym, step) in [("AAA", 3.0), ("BBB", 2.0), ("CCC", 1.0)] {
            let mut ks = Vec::new();
            for i in 0..30 {
                ks.push(kline(i, 100.0 + i as f64 * step));
            }
            data.insert(sym.to_string(), ks);
        }
        let mut s = MomentumRotation::new_extended(5, 3, 1, 0.0, 2, 0, 0.0);
        let result = run_backtest(&mut s, &data, &config(10_000.0, 0.0)).unwrap();
        assert_eq!(result.final_positions.len(), 2, "应持有 2 个品种");
        let mut syms: Vec<&str> = result.final_positions.iter().map(|p| p.symbol.as_str()).collect();
        syms.sort();
        assert_eq!(syms, vec!["AAA", "BBB"], "应持有动量前两名");
        // 等权校验：入场市值差距 < 6%（信号按收盘等额分配，次根开盘撮合价
        // 不同会产生固有偏差，非逻辑错误）
        let vals: Vec<f64> = result
            .final_positions
            .iter()
            .map(|p| p.quantity * p.avg_entry_price)
            .collect();
        let (lo, hi) = (vals[0].min(vals[1]), vals[0].max(vals[1]));
        assert!((hi - lo) / hi < 0.06, "市值应近似均分: {:?}", vals);
        let last = result.equity_curve.last().unwrap();
        assert!(last.equity > 10_100.0, "equity={}", last.equity);
    }

    #[test]
    fn test_regime_filter_blocks_entry_in_bear_market() {
        // 长期下跌后小幅反弹：短期动量为正、过短均线过滤，
        // 但两品种都远低于 20 日均线（广度=0 < 0.5）-> 判熊市不开仓
        let mut data = BTreeMap::new();
        for sym in ["AAA", "BBB"] {
            let mut ks = Vec::new();
            for i in 0..20 {
                ks.push(kline(i, 100.0 - i as f64)); // 100 -> 81
            }
            for (j, c) in [82.0, 83.0, 84.0].iter().enumerate() {
                ks.push(kline(20 + j as u64, *c)); // 小幅反弹：3日动量>0
            }
            data.insert(sym.to_string(), ks);
        }
        // regime_ma=20：MA20 ≈ 90 > 收盘 84 -> 全部位于均线下方
        let mut s = MomentumRotation::new_extended(3, 2, 1, 0.0, 2, 20, 0.5);
        let result = run_backtest(&mut s, &data, &config(10_000.0, 0.0)).unwrap();
        assert!(result.final_positions.is_empty(), "熊市应空仓");
        let last = result.equity_curve.last().unwrap();
        assert!((last.equity - 10_000.0).abs() < 1e-9, "equity={}", last.equity);
    }

    #[test]
    fn test_regime_filter_exits_existing_position() {
        // 先牛市建仓（两品种齐涨，广度=1），随后双双跌破长均线（广度=0）
        // -> 下一次调仓应清仓离场
        let mut data = BTreeMap::new();
        for sym in ["AAA", "BBB"] {
            let mut ks = Vec::new();
            for i in 0..25 {
                ks.push(kline(i, 100.0 + i as f64)); // 齐涨：建仓
            }
            for (j, c) in [105.0, 104.0, 103.0, 102.0, 101.0].iter().enumerate() {
                // 跌破 20 日均线且动量转负
                ks.push(kline(25 + j as u64, *c));
            }
            data.insert(sym.to_string(), ks);
        }
        let mut s = MomentumRotation::new_extended(5, 3, 1, 0.0, 2, 20, 0.5);
        let result = run_backtest(&mut s, &data, &config(10_000.0, 0.0)).unwrap();
        assert!(result.final_positions.is_empty(), "转熊后应清仓");
        assert!(result.metrics.num_round_trips >= 1, "应存在完整回合");
    }

    #[test]
    fn test_trailing_stop_is_per_symbol() {
        // top_n=2 池内两品种均持有；AAA 暴跌触发逐品种止损，
        // 且 5 日动量已转负落选目标（不重入），BBB 持续新高留在目标中。
        // 注：两品种同涨 1 元时动量并列（同比例），此处同涨幅不影响：
        // top_n=2 全池持有，建仓后只验证止损行为。
        let mut aaa = Vec::new();
        let mut bbb = Vec::new();
        for i in 0..20 {
            aaa.push(kline(i, 100.0 + i as f64)); // 齐涨建仓：100->119，峰值119
            bbb.push(kline(i, 100.0 + i as f64));
        }
        aaa.push(kline(20, 100.0)); // AAA 自峰值119回撤16% -> 当bar止损；5日动量 100/114-1 < 0 落选，
        bbb.push(kline(20, 120.0)); // 不会重入；BBB 新高 120，动量为正留在目标中。
        aaa.push(kline(21, 100.0)); // 再补一根：止损卖单在次根开盘成交（尾部订单不撮合会作废）
        bbb.push(kline(21, 121.0));
        let mut data = BTreeMap::new();
        data.insert("AAA".to_string(), aaa);
        data.insert("BBB".to_string(), bbb);

        let mut s = MomentumRotation::new_extended(5, 3, 1, 0.12, 2, 0, 0.0);
        let result = run_backtest(&mut s, &data, &config(10_000.0, 0.0)).unwrap();
        assert_eq!(result.final_positions.len(), 1, "止损后应只剩 BBB");
        assert_eq!(result.final_positions[0].symbol, "BBB");
        assert!(result.metrics.num_round_trips >= 1);
    }
}
