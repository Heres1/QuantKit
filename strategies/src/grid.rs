//! 智能网格策略（grid）：区间内低买高卖，区间由历史行情自动推导。
//!
//! 网格是现货零售最常用的机器人类型：把一段价格区间等分成若干格，
//! 价格每下穿一格买入一份、每上穿一格卖出一份，靠震荡反复吃价差。
//!
//! # 「智能」体现在区间自适应
//! 手工网格要求用户自己填上下界，填错就立刻失效。本策略用最近
//! `lookback_periods` 根K线的最低价/最高价推导区间，并在价格离开区间时
//! 按当前历史重建——即「根据当前价格在历史中的位置动态调整网格」。
//!
//! # 必须正视的风险：单边下跌
//! 网格在震荡市稳定获利，在单边下跌中会一路买入、越买越亏——这是网格
//! 唯一的致命失效模式。因此提供 `stop_loss_pct`：价格跌破区间下界
//! 该比例即清仓并停止本轮网格，等价格重回区间再以新区间开始。
//! 默认启用（0.15），关闭需显式设为 0。
//!
//! # 无未来函数
//! 区间只用已收盘K线的最高/最低价推导，格位判定只用当根收盘价，
//! 订单由引擎在下一根开盘撮合。

use std::collections::BTreeMap;

use quantkit_core::strategy::{Strategy, StrategyContext};
use quantkit_core::types::{Kline, Order};

/// 单品种的网格运行状态
#[derive(Debug, Clone)]
struct GridState {
    /// 区间下界
    lower: f64,
    /// 区间上界
    upper: f64,
    /// 每格基础资产数量（区间建立时按每格预算 / 中位价确定）
    qty_per_grid: f64,
    /// 上一根收盘价所处的格位序号
    last_level: usize,
    /// 已因跌破止损线停止：等价格重回区间上方再重建
    stopped: bool,
}

/// 智能网格策略
pub struct Grid {
    /// 目标品种：每个品种独立一套网格与预算
    pub symbols: Vec<String>,
    /// 网格格数（区间等分数）
    pub levels: usize,
    /// 区间推导回看根数
    pub lookback_periods: usize,
    /// 跌破区间下界多少比例即清仓停止（0 = 关闭止损）
    pub stop_loss_pct: f64,
    /// 每品种网格总预算（计价资产）；0 = 首根bar按品种数均分当时全部现金
    pub budget_per_symbol: f64,
    /// 各品种网格状态（跨 bar，历史重放可重建）
    state: BTreeMap<String, GridState>,
}

impl Grid {
    pub fn new(
        symbols: Vec<String>,
        levels: usize,
        lookback_periods: usize,
        stop_loss_pct: f64,
        budget_per_symbol: f64,
    ) -> Self {
        Self {
            symbols,
            // 少于 2 格无法形成买卖对
            levels: levels.max(2),
            lookback_periods: lookback_periods.max(2),
            stop_loss_pct: stop_loss_pct.max(0.0),
            budget_per_symbol: budget_per_symbol.max(0.0),
            state: BTreeMap::new(),
        }
    }

    /// 由最近 `lookback_periods` 根的最高/最低价建立网格区间。
    /// 区间过窄（高低点几乎相等）时返回 None——此时开网格只会来回付手续费。
    fn build_state(&self, hist: &[&Kline], budget: f64, close: f64) -> Option<GridState> {
        let lower = hist.iter().map(|k| k.low).fold(f64::MAX, f64::min);
        let upper = hist.iter().map(|k| k.high).fold(f64::MIN, f64::max);
        if !(lower > 0.0 && upper > lower) {
            return None;
        }
        // 区间宽度不足 1% 视为无波动，不值得布网
        if (upper - lower) / lower < 0.01 {
            return None;
        }
        let mid = (upper + lower) / 2.0;
        let qty_per_grid = budget / self.levels as f64 / mid;
        // 每格数量必须是有效正数（排除 0、负数与 NaN）
        if !(qty_per_grid.is_finite() && qty_per_grid > 0.0) {
            return None;
        }
        Some(GridState {
            lower,
            upper,
            qty_per_grid,
            last_level: level_of(close, lower, upper, self.levels),
            stopped: false,
        })
    }
}

/// 收盘价所处格位：0 = 最低格，levels-1 = 最高格（区间外钳制到两端）
fn level_of(price: f64, lower: f64, upper: f64, levels: usize) -> usize {
    if price <= lower {
        return 0;
    }
    if price >= upper {
        return levels - 1;
    }
    let step = (upper - lower) / levels as f64;
    (((price - lower) / step) as usize).min(levels - 1)
}

impl Strategy for Grid {
    fn name(&self) -> &str {
        "grid"
    }

    fn on_bars(&mut self, ctx: &StrategyContext, bars: &BTreeMap<String, Kline>) -> Vec<Order> {
        let mut orders = Vec::new();
        for sym in &self.symbols {
            let Some(bar) = bars.get(sym) else {
                continue;
            };
            if bar.close <= 0.0 {
                continue;
            }
            let held = ctx.position(sym).map(|p| p.quantity).unwrap_or(0.0);

            // 1) 止损：跌破区间下界指定比例，清仓并停止本轮网格
            if let Some(st) = self.state.get_mut(sym) {
                if !st.stopped && self.stop_loss_pct > 0.0 {
                    let stop_price = st.lower * (1.0 - self.stop_loss_pct);
                    if bar.close < stop_price {
                        st.stopped = true;
                        if held > 0.0 {
                            orders.push(Order::market_sell(sym.clone(), held));
                        }
                        continue;
                    }
                }
            }

            // 2) 建立或重建区间。向上/向下刻意不对称：
            // 向上突破说明网格已吃完这一段，按更高的历史重新布网是合理的；
            // 向下跌破**绝不能**重建——否则下界会跟着新低一路下移，
            // 止损线（下界的固定比例）永远追不上价格，单边下跌就止不住。
            // 跌破后格位自然钳在最低格、不再产生新买单，交由止损线决定清仓。
            let needs_rebuild = match self.state.get(sym) {
                None => true,
                Some(st) => st.stopped && bar.close > st.lower || bar.close > st.upper,
            };
            if needs_rebuild {
                let hist = ctx.history(sym, self.lookback_periods);
                if hist.len() < self.lookback_periods {
                    continue; // 热身期：历史不足无法推导区间
                }
                let budget = if self.budget_per_symbol > 0.0 {
                    self.budget_per_symbol
                } else {
                    // 未指定预算：按品种数均分当时现金（留 1% 缓冲吸收滑点/手续费）
                    ctx.cash() * 0.99 / self.symbols.len().max(1) as f64
                };
                match self.build_state(&hist, budget, bar.close) {
                    Some(st) => {
                        self.state.insert(sym.clone(), st);
                    }
                    None => continue,
                }
                // 重建当根只确定格位基准，不立即交易——避免用刚推导出的区间
                // 在同一根bar上产生一笔方向随机的开仓
                continue;
            }

            let Some(st) = self.state.get_mut(sym) else {
                continue;
            };
            if st.stopped {
                continue;
            }

            // 3) 格位变化即触发交易：跨几格就做几份，一次补齐
            let level = level_of(bar.close, st.lower, st.upper, self.levels);
            if level < st.last_level {
                let crossed = (st.last_level - level) as f64;
                let qty = st.qty_per_grid * crossed;
                let cost = qty * bar.close;
                // 现金不足则按可用现金买入，避免下发必然被削减的大单
                let qty = if cost > ctx.cash() && bar.close > 0.0 {
                    ctx.cash() / bar.close
                } else {
                    qty
                };
                if qty > 0.0 {
                    orders.push(Order::market_buy(sym.clone(), qty));
                }
            } else if level > st.last_level && held > 0.0 {
                let crossed = (level - st.last_level) as f64;
                let qty = (st.qty_per_grid * crossed).min(held);
                if qty > 0.0 {
                    orders.push(Order::market_sell(sym.clone(), qty));
                }
            }
            st.last_level = level;
        }
        orders
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quantkit_core::engine::{run_backtest, BacktestConfig, FillPrice};
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

    fn config() -> BacktestConfig {
        BacktestConfig {
            initial_cash: 10_000.0,
            model: FillModel::new(0.0, 0.0),
            max_history: 400,
            fill_price: FillPrice::SameBarClose,
            ..Default::default()
        }
    }

    fn data_of(closes: &[f64]) -> BTreeMap<String, Vec<Kline>> {
        let ks: Vec<Kline> = closes
            .iter()
            .enumerate()
            .map(|(i, &c)| kline(i as u64, c))
            .collect();
        let mut data = BTreeMap::new();
        data.insert("T".to_string(), ks);
        data
    }

    #[test]
    fn test_level_of_boundaries() {
        // 区间 [100,200] 分 10 格：每格 10。边界与越界都必须落在合法格位内
        assert_eq!(level_of(100.0, 100.0, 200.0, 10), 0);
        assert_eq!(level_of(105.0, 100.0, 200.0, 10), 0);
        assert_eq!(level_of(115.0, 100.0, 200.0, 10), 1);
        assert_eq!(level_of(200.0, 100.0, 200.0, 10), 9);
        assert_eq!(level_of(50.0, 100.0, 200.0, 10), 0, "跌破下界钳到最低格");
        assert_eq!(level_of(999.0, 100.0, 200.0, 10), 9, "突破上界钳到最高格");
    }

    #[test]
    fn test_oscillation_generates_round_trips() {
        // 先给一段震荡历史建立区间，再反复上下穿格：应产生完整的买卖回合
        let mut closes: Vec<f64> = Vec::new();
        for _ in 0..3 {
            closes.extend([100.0, 120.0, 100.0, 120.0]);
        }
        // 建立区间后继续震荡触发买卖
        for _ in 0..6 {
            closes.extend([102.0, 118.0]);
        }
        let mut s = Grid::new(vec!["T".into()], 4, 12, 0.0, 5_000.0);
        let r = run_backtest(&mut s, &data_of(&closes), &config()).unwrap();
        assert!(
            r.metrics.num_round_trips >= 1,
            "震荡行情应产生网格回合，实际 {}",
            r.metrics.num_round_trips
        );
    }

    #[test]
    fn test_stop_loss_liquidates_on_breakdown() {
        // 建立 [100,120] 区间后单边跌到 70：止损须清仓，且不再继续买入
        let mut closes: Vec<f64> = Vec::new();
        for _ in 0..3 {
            closes.extend([100.0, 120.0, 100.0, 120.0]);
        }
        closes.extend([110.0, 100.0, 95.0, 88.0, 80.0, 75.0, 70.0, 70.0, 70.0]);

        let mut s = Grid::new(vec!["T".into()], 4, 12, 0.15, 5_000.0);
        let r = run_backtest(&mut s, &data_of(&closes), &config()).unwrap();
        assert!(
            r.final_positions.is_empty(),
            "跌破止损线应清仓，实际仍持有 {:?}",
            r.final_positions
        );
    }

    #[test]
    fn test_no_grid_when_range_too_narrow() {
        // 几乎无波动：不应布网、不应交易（否则只是反复付手续费）
        let closes: Vec<f64> = (0..40).map(|i| 100.0 + (i % 2) as f64 * 0.1).collect();
        let mut s = Grid::new(vec!["T".into()], 10, 20, 0.0, 5_000.0);
        let r = run_backtest(&mut s, &data_of(&closes), &config()).unwrap();
        assert_eq!(r.metrics.num_round_trips, 0, "窄幅区间不应布网交易");
        assert!(r.final_positions.is_empty());
    }

    #[test]
    fn test_warmup_produces_no_orders() {
        // 历史不足回看根数：不得下单
        let closes: Vec<f64> = vec![100.0, 110.0, 105.0];
        let mut s = Grid::new(vec!["T".into()], 4, 20, 0.0, 5_000.0);
        let r = run_backtest(&mut s, &data_of(&closes), &config()).unwrap();
        assert!(r.final_positions.is_empty(), "热身期不应建仓");
    }
}
