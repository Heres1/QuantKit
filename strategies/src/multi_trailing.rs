//! 多品种趋势组合：每个品种一个独立的追踪止损子系统，买入按等分预算下单。
//!
//! 与单品种 [`crate::trailing_trend::TrailingTrend`] 的区别：
//! - 各品种独立维护峰值/冷却，互不干扰；
//! - 买入不再全仓，而是 `现金 / 当前空仓品种数`（近似等权切片：所有品种空仓时
//!   每个分到 1/N；已有持仓的品种不占现金预算）；
//! - 卖出直通（全额清掉各自持仓），且排在买单之前，保证同 bar 止损回笼的现金
//!   在顺序执行时可用。

use quantkit_core::strategy::{Strategy, StrategyContext};
use quantkit_core::types::{Kline, Order, Side};
use std::collections::BTreeMap;

use crate::trailing_trend::TrailingTrend;

/// 多品种趋势组合
pub struct MultiTrailingTrend {
    subs: Vec<TrailingTrend>,
}

impl MultiTrailingTrend {
    pub fn new(subs: Vec<TrailingTrend>) -> Self {
        Self { subs }
    }

    pub fn symbols(&self) -> Vec<&str> {
        self.subs.iter().map(|s| s.symbol.as_str()).collect()
    }
}

impl Strategy for MultiTrailingTrend {
    fn name(&self) -> &str {
        "multi_trailing_trend"
    }

    fn on_bars(&mut self, ctx: &StrategyContext, bars: &BTreeMap<String, Kline>) -> Vec<Order> {
        // 以本 bar 开始时的持仓快照判定空仓集合（同 bar 内的止损不影响预算划分）
        let flat: Vec<&str> = self
            .subs
            .iter()
            .map(|s| s.symbol.as_str())
            .filter(|sym| ctx.position(sym).is_none())
            .collect();
        let budget = if flat.is_empty() {
            0.0
        } else {
            ctx.cash() / flat.len() as f64
        };

        let mut raw: Vec<Order> = Vec::new();
        for s in self.subs.iter_mut() {
            raw.extend(s.on_bars(ctx, bars));
        }

        let (sells, buys): (Vec<Order>, Vec<Order>) =
            raw.into_iter().partition(|o| o.side == Side::Sell);
        let mut out = sells;
        for o in buys {
            let price = bars.get(&o.symbol).map(|b| b.close).unwrap_or(0.0);
            let qty = if price > 0.0 { budget * 0.99 / price } else { 0.0 };
            if qty > 0.0 {
                out.push(Order::market_buy(o.symbol, qty));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quantkit_core::types::Position;

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

    /// 构造带 n 根历史 + 当前 bar 的 ctx/bars；close_fn(i) 给出各品种价格
    fn setup(
        symbols: &[&str],
        cash: f64,
        positions: BTreeMap<String, Position>,
        closes: &BTreeMap<String, Vec<f64>>,
    ) -> (StrategyContext, BTreeMap<String, Kline>) {
        let n = closes.values().next().map(|v| v.len()).unwrap_or(0);
        let mut ctx = StrategyContext::new(100);
        for i in 0..n {
            for sym in symbols {
                ctx.push_bar(sym, kline(i as u64, closes[*sym][i]));
            }
        }
        ctx.sync_account(positions, cash);
        let bars = symbols
            .iter()
            .map(|sym| (sym.to_string(), kline((n - 1) as u64, closes[*sym][n - 1])))
            .collect();
        (ctx, bars)
    }

    fn multi(symbols: &[&str], trail: f64) -> MultiTrailingTrend {
        MultiTrailingTrend::new(
            symbols
                .iter()
                .map(|s| TrailingTrend::new(*s, 2, trail, 3))
                .collect(),
        )
    }

    #[test]
    fn test_equal_budget_split_on_entry() {
        // 两品种都站上 MA -> 各分一半现金
        let mut closes = BTreeMap::new();
        closes.insert("A".to_string(), vec![100.0, 110.0]);
        closes.insert("B".to_string(), vec![100.0, 220.0]);
        let (ctx, bars) = setup(&["A", "B"], 10_000.0, BTreeMap::new(), &closes);
        let mut s = multi(&["A", "B"], 0.12);

        let orders = s.on_bars(&ctx, &bars);
        assert_eq!(orders.len(), 2);
        for o in &orders {
            assert_eq!(o.side, Side::Buy);
            let expect = 5_000.0 * 0.99 / bars[&o.symbol].close;
            assert!((o.quantity - expect).abs() < 1e-9, "{}", o.symbol);
        }
    }

    #[test]
    fn test_holding_symbol_gets_no_budget() {
        // A 已持仓：预算只分给空仓的 B（全部现金）
        let mut closes = BTreeMap::new();
        closes.insert("A".to_string(), vec![100.0, 110.0]);
        closes.insert("B".to_string(), vec![100.0, 200.0]);
        let mut positions = BTreeMap::new();
        positions.insert(
            "A".to_string(),
            Position {
                symbol: "A".into(),
                quantity: 10.0,
                avg_entry_price: 105.0,
            },
        );
        let (ctx, bars) = setup(&["A", "B"], 6_000.0, positions, &closes);
        let mut s = multi(&["A", "B"], 0.12);

        let orders = s.on_bars(&ctx, &bars);
        assert_eq!(orders.len(), 1);
        assert_eq!(orders[0].symbol, "B");
        let expect = 6_000.0 * 0.99 / 200.0;
        assert!((orders[0].quantity - expect).abs() < 1e-9);
    }

    /// 逐 bar 推进：返回 (ctx, bars) 序列中的一步（持仓/现金可逐步变化）
    fn step(
        symbols: &[&str],
        cash: f64,
        positions: BTreeMap<String, Position>,
        closes: &BTreeMap<String, Vec<f64>>,
        upto: usize,
    ) -> (StrategyContext, BTreeMap<String, Kline>) {
        let mut ctx = StrategyContext::new(100);
        for i in 0..=upto {
            for sym in symbols {
                ctx.push_bar(sym, kline(i as u64, closes[*sym][i]));
            }
        }
        ctx.sync_account(positions, cash);
        let bars = symbols
            .iter()
            .map(|sym| (sym.to_string(), kline(upto as u64, closes[*sym][upto])))
            .collect();
        (ctx, bars)
    }

    #[test]
    fn test_stop_sell_passes_through_and_orders_sell_first() {
        // A 峰值 130，随后回撤超阈值 -> 全额卖出；B 同 bar 买入；卖单排在买单前
        let mut closes = BTreeMap::new();
        closes.insert("A".to_string(), vec![100.0, 130.0, 110.0]);
        closes.insert("B".to_string(), vec![100.0, 100.0, 120.0]);
        let mut s = multi(&["A", "B"], 0.12);
        let mut positions = BTreeMap::new();
        positions.insert(
            "A".to_string(),
            Position {
                symbol: "A".into(),
                quantity: 2.5,
                avg_entry_price: 100.0,
            },
        );

        // bar1：持仓中，峰值累计到 130，无动作（B 收盘价==均线不买）
        let (c1, b1) = step(&["A", "B"], 4_000.0, positions.clone(), &closes, 1);
        assert!(s.on_bars(&c1, &b1).is_empty());

        // bar2：A 回撤 15.4% 触发止损；B 站上均线，用全部现金买入（A 在 bar 开始仍持仓）
        let (c2, b2) = step(&["A", "B"], 4_000.0, positions, &closes, 2);
        let orders = s.on_bars(&c2, &b2);
        assert_eq!(orders.len(), 2);
        assert_eq!(orders[0].side, Side::Sell, "卖单必须在前");
        assert_eq!(orders[0].symbol, "A");
        assert!((orders[0].quantity - 2.5).abs() < 1e-9, "卖出全部持仓");
        assert_eq!(orders[1].side, Side::Buy);
        assert_eq!(orders[1].symbol, "B");
        let expect = 4_000.0 * 0.99 / 120.0;
        assert!((orders[1].quantity - expect).abs() < 1e-9);
    }

    #[test]
    fn test_cooldown_independent_between_symbols() {
        // A 止损后进冷却：价格回到均线上也不入场；B 不受影响继续交易
        let mut closes = BTreeMap::new();
        closes.insert("A".to_string(), vec![100.0, 130.0, 110.0, 140.0]);
        closes.insert("B".to_string(), vec![100.0, 100.0, 100.0, 130.0]);
        let mut s = multi(&["A", "B"], 0.12);
        let mut positions = BTreeMap::new();
        positions.insert(
            "A".to_string(),
            Position {
                symbol: "A".into(),
                quantity: 1.0,
                avg_entry_price: 100.0,
            },
        );

        // bar1：峰值累计到 130
        let (c1, b1) = step(&["A", "B"], 4_000.0, positions.clone(), &closes, 1);
        assert!(s.on_bars(&c1, &b1).is_empty());

        // bar2：A 止损；B 平盘不买
        let (c2, b2) = step(&["A", "B"], 4_000.0, positions, &closes, 2);
        let o2 = s.on_bars(&c2, &b2);
        assert_eq!(o2.len(), 1);
        assert_eq!(o2[0].side, Side::Sell);
        assert_eq!(o2[0].symbol, "A");

        // bar3：A 已空仓且站上均线，但冷却（3 天）未过不入场；B 站上均线买入
        let (c3, b3) = step(&["A", "B"], 4_000.0, BTreeMap::new(), &closes, 3);
        let o3 = s.on_bars(&c3, &b3);
        assert_eq!(o3.len(), 1, "A 冷却中不应再买入");
        assert_eq!(o3[0].symbol, "B");
        assert_eq!(o3[0].side, Side::Buy);
    }
}
