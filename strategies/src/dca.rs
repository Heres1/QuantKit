//! 定投策略（dca）：按固定周期、固定金额买入，可选「低于趋势线时加倍」的智能加码。
//!
//! 面向绝大多数加密货币投资者最常用、也最不容易做错的策略：不预测方向，
//! 用时间分散成本。策略只买不卖——定投的收益来自长期持有，
//! 卖出时机应由投资者自己决定（或叠加引擎层的组合回撤熔断）。
//!
//! 与网格/动量的区别：定投不依赖任何行情判断，因此没有「假信号」风险；
//! 代价是熊市中会一路买入，回撤完全等于标的自身回撤。
//!
//! # 智能加码（`ma_days` > 0 时启用）
//! 收盘价低于 `ma_days` 均线时，本期买入金额 × `dip_multiplier`。
//! 依据：价格低于长期趋势线时单位成本更低，加大投入可摊低均价。
//! 阈值就是均线本身，没有额外参数——少一个参数就少一个需要调优的自由度。

use std::collections::BTreeMap;

use quantkit_core::strategy::{Strategy, StrategyContext};
use quantkit_core::types::{Kline, Order};

const MS_PER_DAY: u64 = 86_400_000;

/// 定投策略（多品种：每个品种独立按同一节奏定投）
pub struct Dca {
    /// 目标品种：每期对每个品种各买入 `amount`
    pub symbols: Vec<String>,
    /// 每期每品种买入金额（计价资产，如 USDT）
    pub amount: f64,
    /// 定投间隔（天）：墙钟时间，因此换 K 线周期不改变定投节奏
    pub interval_days: u64,
    /// 智能加码均线周期（根数，0 = 关闭，恒定投固定金额）
    pub ma_periods: usize,
    /// 低于均线时的加码倍数（<=1 视为不加码）
    pub dip_multiplier: f64,
    /// 各品种上次买入的 bar 时间戳（跨 bar 状态，历史重放可完整重建）
    last_buy_ts: BTreeMap<String, u64>,
}

impl Dca {
    pub fn new(
        symbols: Vec<String>,
        amount: f64,
        interval_days: u64,
        ma_periods: usize,
        dip_multiplier: f64,
    ) -> Self {
        Self {
            symbols,
            amount: amount.max(0.0),
            // 0 天间隔会导致每根 bar 都买入，等同一次性全仓，违背定投本意
            interval_days: interval_days.max(1),
            ma_periods,
            dip_multiplier: dip_multiplier.max(1.0),
            last_buy_ts: BTreeMap::new(),
        }
    }
}

impl Strategy for Dca {
    fn name(&self) -> &str {
        "dca"
    }

    fn on_bars(&mut self, ctx: &StrategyContext, bars: &BTreeMap<String, Kline>) -> Vec<Order> {
        let mut orders = Vec::new();
        let interval_ms = self.interval_days * MS_PER_DAY;

        for sym in &self.symbols {
            let Some(bar) = bars.get(sym) else {
                continue;
            };
            if bar.close <= 0.0 || self.amount <= 0.0 {
                continue;
            }
            // 首次见到该品种即买入第一期；之后按墙钟间隔推进
            let due = match self.last_buy_ts.get(sym) {
                Some(last) => bar.open_time.saturating_sub(*last) >= interval_ms,
                None => true,
            };
            if !due {
                continue;
            }

            // 智能加码：收盘价低于均线则本期加倍。历史不足均线周期时按常规金额买入，
            // 而不是跳过——定投不该因为热身期而漏掉前几期。
            let mut amount = self.amount;
            if self.ma_periods > 0 && self.dip_multiplier > 1.0 {
                let hist = ctx.history(sym, self.ma_periods);
                if hist.len() >= self.ma_periods {
                    let ma = hist.iter().map(|k| k.close).sum::<f64>() / hist.len() as f64;
                    if bar.close < ma {
                        amount *= self.dip_multiplier;
                    }
                }
            }

            // 现金不足时按剩余现金买入（引擎也会钳制，这里提前收敛避免下发无效大单）
            let amount = amount.min(ctx.cash());
            if amount <= 0.0 {
                continue;
            }
            orders.push(Order::market_buy(sym.clone(), amount / bar.close));
            self.last_buy_ts.insert(sym.clone(), bar.open_time);
        }
        orders
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quantkit_core::engine::{run_backtest, BacktestConfig, FillPrice};
    use quantkit_core::executor::FillModel;

    fn kline(i: u64, close: f64) -> Kline {
        Kline {
            open_time: i * MS_PER_DAY,
            open: close,
            high: close,
            low: close,
            close,
            volume: 1.0,
            close_time: i * MS_PER_DAY + 1000,
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

    fn flat_data(bars: u64, close: f64) -> BTreeMap<String, Vec<Kline>> {
        let ks: Vec<Kline> = (0..bars).map(|i| kline(i, close)).collect();
        let mut data = BTreeMap::new();
        data.insert("T".to_string(), ks);
        data
    }

    #[test]
    fn test_buys_on_schedule() {
        // 30 根日线、每 7 天定投 100：应在第 0/7/14/21/28 天各买一次 = 5 期
        let data = flat_data(30, 100.0);
        let mut s = Dca::new(vec!["T".into()], 100.0, 7, 0, 1.0);
        let r = run_backtest(&mut s, &data, &config()).unwrap();
        let pos = r.final_positions.first().expect("应持有仓位");
        // 恒定价 100、无手续费：5 期 × 100 USDT = 5 份
        assert!(
            (pos.quantity - 5.0).abs() < 1e-9,
            "应买入 5 期共 5 份，实际 {}",
            pos.quantity
        );
        assert_eq!(r.metrics.num_round_trips, 0, "定投只买不卖，不应有平仓回合");
    }

    #[test]
    fn test_dip_multiplier_buys_more_below_ma() {
        // 前 10 天高价 200 拉高均线，后 10 天跌到 100（低于 MA10）：
        // 跌破均线后的期数应按 2 倍金额买入
        let mut closes = vec![200.0; 10];
        closes.extend(vec![100.0; 10]);
        let ks: Vec<Kline> = closes
            .iter()
            .enumerate()
            .map(|(i, &c)| kline(i as u64, c))
            .collect();
        let mut data = BTreeMap::new();
        data.insert("T".to_string(), ks);

        let mut plain = Dca::new(vec!["T".into()], 100.0, 5, 0, 1.0);
        let plain_qty = run_backtest(&mut plain, &data, &config())
            .unwrap()
            .final_positions
            .first()
            .unwrap()
            .quantity;

        let mut smart = Dca::new(vec!["T".into()], 100.0, 5, 10, 2.0);
        let smart_qty = run_backtest(&mut smart, &data, &config())
            .unwrap()
            .final_positions
            .first()
            .unwrap()
            .quantity;

        assert!(
            smart_qty > plain_qty,
            "低于均线加码应买到更多份额: smart={smart_qty} plain={plain_qty}"
        );
    }

    #[test]
    fn test_stops_when_cash_exhausted() {
        // 现金 10000、每期 3000：买满后不应再下单，也不应产生负现金
        let data = flat_data(50, 100.0);
        let mut s = Dca::new(vec!["T".into()], 3000.0, 1, 0, 1.0);
        let r = run_backtest(&mut s, &data, &config()).unwrap();
        assert!(r.final_cash >= -1e-9, "现金不应为负: {}", r.final_cash);
        let pos = r.final_positions.first().unwrap();
        assert!(
            (pos.quantity - 100.0).abs() < 1e-6,
            "10000 USDT 应买满 100 份，实际 {}",
            pos.quantity
        );
    }

    #[test]
    fn test_interval_days_floor_of_one() {
        // 间隔 0 会退化成每根 bar 全仓买入，构造函数必须钳到 1 天
        let s = Dca::new(vec!["T".into()], 100.0, 0, 0, 1.0);
        assert_eq!(s.interval_days, 1);
    }
}
