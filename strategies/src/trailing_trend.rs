//! 趋势 + 追踪止损策略（单品种）。
//!
//! 规则：
//! - 空仓且「收盘价 > MA(ma_days)」且冷却期已过 -> 市价全仓买入
//! - 持仓期间维护峰值；价格从峰值回撤 >= `trailing_stop_pct` -> 市价清仓
//! - 止损后进入 `cooldown_days` 天冷却（防震荡市反复止损）

use quantkit_core::strategy::{Strategy, StrategyContext};
use quantkit_core::types::{Kline, Order};
use std::collections::BTreeMap;

const MS_PER_DAY: u64 = 86_400_000;

/// 趋势 + 追踪止损策略
pub struct TrailingTrend {
    /// 目标品种
    pub symbol: String,
    /// 趋势过滤均线天数
    pub ma_days: usize,
    /// 追踪止损回撤阈值（0.12 = 12%）
    pub trailing_stop_pct: f64,
    /// 止损后冷却天数
    pub cooldown_days: u64,
    peak: f64,
    last_stop_ts: u64,
}

impl TrailingTrend {
    pub fn new(
        symbol: impl Into<String>,
        ma_days: usize,
        trailing_stop_pct: f64,
        cooldown_days: u64,
    ) -> Self {
        Self {
            symbol: symbol.into(),
            ma_days,
            trailing_stop_pct,
            cooldown_days,
            peak: 0.0,
            last_stop_ts: 0,
        }
    }
}

impl Strategy for TrailingTrend {
    fn name(&self) -> &str {
        "trailing_trend"
    }

    fn on_bars(&mut self, ctx: &StrategyContext, bars: &BTreeMap<String, Kline>) -> Vec<Order> {
        let bar = match bars.get(&self.symbol) {
            Some(b) => b,
            None => return vec![],
        };
        let ts = bar.open_time;
        let hist = ctx.history(&self.symbol, self.ma_days);
        if hist.len() < self.ma_days {
            return vec![];
        }
        let ma: f64 = hist.iter().map(|k| k.close).sum::<f64>() / self.ma_days as f64;

        // 持仓中：追踪止损检查
        if let Some(pos) = ctx.position(&self.symbol) {
            self.peak = self.peak.max(bar.close);
            if self.peak > 0.0 {
                let drawdown = (self.peak - bar.close) / self.peak;
                if drawdown >= self.trailing_stop_pct {
                    self.last_stop_ts = ts;
                    self.peak = 0.0;
                    return vec![Order::market_sell(self.symbol.clone(), pos.quantity)];
                }
            }
            return vec![];
        }

        // 空仓：冷却期检查 + 趋势过滤
        if self.last_stop_ts != 0
            && ts.saturating_sub(self.last_stop_ts) < self.cooldown_days * MS_PER_DAY
        {
            return vec![];
        }
        if bar.close > ma && ctx.cash() > 0.0 {
            self.peak = bar.close;
            return vec![Order::market_buy(
                self.symbol.clone(),
                ctx.cash() * 0.99 / bar.close,
            )];
        }
        vec![]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quantkit_core::engine::{run_backtest, BacktestConfig};
    use quantkit_core::executor::FillModel;

    const DAY: u64 = 86_400_000;

    fn kline(i: u64, open: f64, close: f64) -> Kline {
        Kline {
            open_time: i * DAY,
            open,
            high: open.max(close),
            low: open.min(close),
            close,
            volume: 1.0,
            close_time: i * DAY + 1000,
        }
    }

    #[test]
    fn test_trailing_stop_triggers_on_drawdown() {
        // 入场后峰值 130，跌至 114（回撤 12.3% >= 12%）应触发止损
        let data_close = [100.0, 100.0, 110.0, 120.0, 130.0, 114.0, 114.0, 114.0];
        let mut klines = Vec::new();
        for (i, c) in data_close.iter().enumerate() {
            let open = if i == 0 { *c } else { data_close[i - 1] };
            klines.push(kline(i as u64, open, *c));
        }
        let mut data = BTreeMap::new();
        data.insert("T".to_string(), klines);

        let mut s = TrailingTrend::new("T", 3, 0.12, 5);
        let config = BacktestConfig {
            initial_cash: 10_000.0,
            model: FillModel::new(0.0, 0.001),
            max_history: 50,
        fill_price: quantkit_core::engine::FillPrice::default(),
            ..Default::default()
        };
        let result = run_backtest(&mut s, &data, &config).unwrap();
        assert_eq!(result.metrics.num_round_trips, 1, "应完成一次完整买卖回合");
        // 110 买入、114 止损卖出（开盘价）：毛利为正，净利扣费后仍应为正
        assert!(result.metrics.win_rate_pct > 99.0);
    }

    #[test]
    fn test_cooldown_blocks_reentry() {
        // 止损后 5 天冷却：同段数据内不应二次入场
        let data_close = [100.0, 100.0, 110.0, 120.0, 130.0, 114.0, 120.0, 125.0, 130.0];
        let mut klines = Vec::new();
        for (i, c) in data_close.iter().enumerate() {
            let open = if i == 0 { *c } else { data_close[i - 1] };
            klines.push(kline(i as u64, open, *c));
        }
        let mut data = BTreeMap::new();
        data.insert("T".to_string(), klines);

        let mut s = TrailingTrend::new("T", 3, 0.12, 5);
        let config = BacktestConfig {
            initial_cash: 10_000.0,
            model: FillModel::new(0.0, 0.001),
            max_history: 50,
        fill_price: quantkit_core::engine::FillPrice::default(),
            ..Default::default()
        };
        let result = run_backtest(&mut s, &data, &config).unwrap();
        // 止损发生在 i=5（信号）-> 冷却到 i=10；数据只到 i=8，不应二次入场
        assert_eq!(result.metrics.num_round_trips, 1);
    }
}
