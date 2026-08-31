//! 自定义策略模板：双均线金叉（ma_cross）。
//!
//! 本文件既是一个可直接回测/实盘的策略，也是**手写策略的参考模板**。
//! 策略逻辑刻意保持最简：快线上穿慢线买入、下穿卖出，方便你在此基础上
//! 替换信号、加过滤器或风控。
//!
//! # 手写策略三步注册（详见 README「自定义策略指南」）
//! 1. 在 `strategies/src/` 新建文件并实现 [`Strategy`] trait（照抄本文件结构）
//! 2. 在 `strategies/src/lib.rs` 添加 `pub mod 文件名;`
//! 3. 在 `app/src/lib.rs` 的 `build_strategy` 中加一个匹配分支，
//!    并在 `app/src/config.rs` 增加所需参数字段（带 `#[serde(default)]`）
//!
//! # 关键约定（避免未来函数）
//! - `on_bars` 收到的是**已收盘**的当根 bar；返回的订单由引擎在**下一根开盘**撮合
//!   （默认 `next_open`），因此信号只能用 `ctx.history` 与 `bars` 中的已收盘数据
//! - 只返回订单，不直接改持仓/现金；持仓查询用 `ctx.position(symbols)`
//! - 私有字段（如下方的 `prev_fast_above`）跨 bar 保存状态，重启后由
//!   dry-run/回测的历史重放自动重建，无需手动持久化

use std::collections::{BTreeMap, HashSet};

use quantkit_core::strategy::{Strategy, StrategyContext};
use quantkit_core::types::{Kline, Order};

/// 双均线金叉策略（多品种独立信号）
pub struct MaCross {
    /// 目标品种：每个品种独立评估金叉/死叉
    pub symbols: Vec<String>,
    /// 快线周期
    pub fast_days: usize,
    /// 慢线周期（必须 > 快线）
    pub slow_days: usize,
    /// 上一根 bar 快线在慢线上方的品种集合（用于识别「交叉」而非「状态」）
    prev_fast_above: HashSet<String>,
}

impl MaCross {
    pub fn new(symbols: Vec<String>, fast_days: usize, slow_days: usize) -> Self {
        Self {
            symbols,
            fast_days: fast_days.max(1),
            slow_days: slow_days.max(2),
            prev_fast_above: HashSet::new(),
        }
    }
}

impl Strategy for MaCross {
    fn name(&self) -> &str {
        "ma_cross"
    }

    fn on_bars(&mut self, ctx: &StrategyContext, bars: &BTreeMap<String, Kline>) -> Vec<Order> {
        let mut orders = Vec::new();
        // 同 bar 多个金叉时按未入场品种数均分现金（快照口径，足够模板使用）
        let pending: usize = self
            .symbols
            .iter()
            .filter(|s| ctx.position(s).is_none())
            .count()
            .max(1);
        for sym in &self.symbols {
            let Some(bar) = bars.get(sym) else {
                continue;
            };
            // ctx.history 返回最近 n 根（含当根）；不足慢线周期则跳过
            let hist = ctx.history(sym, self.slow_days);
            if hist.len() < self.slow_days {
                continue;
            }
            let fast_ma: f64 =
                hist.iter().rev().take(self.fast_days).map(|k| k.close).sum::<f64>()
                    / self.fast_days as f64;
            let slow_ma: f64 = hist.iter().map(|k| k.close).sum::<f64>() / self.slow_days as f64;
            let fast_above = fast_ma > slow_ma;
            let was_above = self.prev_fast_above.contains(sym);

            if fast_above && !was_above {
                // 金叉：无持仓才买入，现金按未入场品种均分
                if ctx.position(sym).is_none() && ctx.cash() > 0.0 && bar.close > 0.0 {
                    let budget = ctx.cash() * 0.99 / pending as f64;
                    orders.push(Order::market_buy(sym.clone(), budget / bar.close));
                }
            } else if !fast_above && was_above {
                // 死叉：有持仓则全部卖出
                if let Some(pos) = ctx.position(sym) {
                    orders.push(Order::market_sell(sym.clone(), pos.quantity));
                }
            }

            if fast_above {
                self.prev_fast_above.insert(sym.clone());
            } else {
                self.prev_fast_above.remove(sym);
            }
        }
        orders
    }
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

    #[test]
    fn test_golden_cross_buy_and_dead_cross_sell() {
        // 先涨（金叉买入）后跌（死叉卖出），应完成一次完整回合且期末空仓
        let closes = [
            100.0, 100.0, 102.0, 105.0, 109.0, 112.0, // 上行：快线上穿慢线
            108.0, 105.0, 101.0, 98.0, 96.0, 96.0, // 下行：快线下穿慢线
        ];
        let ks: Vec<Kline> = closes
            .iter()
            .enumerate()
            .map(|(i, &c)| kline(i as u64, c))
            .collect();
        let mut data = BTreeMap::new();
        data.insert("T".to_string(), ks);

        let mut s = MaCross::new(vec!["T".into()], 2, 4);
        let config = BacktestConfig {
            initial_cash: 10_000.0,
            model: FillModel::new(0.0, 0.001),
            max_history: 50,
            fill_price: quantkit_core::engine::FillPrice::default(),
            ..Default::default()
        };
        let result = run_backtest(&mut s, &data, &config).unwrap();
        assert!(
            result.metrics.num_round_trips >= 1,
            "应至少完成一次金叉买入 + 死叉卖出: {}",
            result.metrics.num_round_trips
        );
        assert!(result.final_positions.is_empty(), "死叉后应空仓");
    }

    #[test]
    fn test_no_signal_before_warmup() {
        // 数据不足慢线周期：不应产生任何订单
        let ks: Vec<Kline> = (0..3).map(|i| kline(i, 100.0 + i as f64)).collect();
        let mut data = BTreeMap::new();
        data.insert("T".to_string(), ks);

        let mut s = MaCross::new(vec!["T".into()], 2, 4);
        let config = BacktestConfig {
            initial_cash: 10_000.0,
            model: FillModel::new(0.0, 0.0),
            max_history: 50,
            fill_price: quantkit_core::engine::FillPrice::default(),
            ..Default::default()
        };
        let result = run_backtest(&mut s, &data, &config).unwrap();
        assert!(result.final_positions.is_empty(), "热身期内不应建仓");
    }
}
