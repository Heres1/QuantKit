//! 最小可运行示例：内联实现一个均线突破策略，在合成K线上跑回测。
//!
//! 运行：cargo run -p quantkit-examples --bin quickstart
//!
//! 展示 quantkit 的三个核心抽象：
//! - `Strategy`：只写决策逻辑（on_bars 返回订单）
//! - `OrderExecutor`/`FillModel`：撮合与手续费由引擎注入，策略不感知
//! - `run_backtest`：同一引擎，换执行方式即可用于模拟/实盘

use std::collections::BTreeMap;

use quantkit_core::engine::{run_backtest, BacktestConfig};
use quantkit_core::executor::FillModel;
use quantkit_core::strategy::{Strategy, StrategyContext};
use quantkit_core::types::{Kline, Order};

const DAY: u64 = 86_400_000;

/// 均线突破策略：收盘上穿 MA(20) 全仓买入，下穿清仓
struct MaBreakout {
    ma_days: usize,
}

impl Strategy for MaBreakout {
    fn name(&self) -> &str {
        "ma_breakout-example"
    }

    fn on_bars(&mut self, ctx: &StrategyContext, bars: &BTreeMap<String, Kline>) -> Vec<Order> {
        let (sym, k) = match bars.iter().next() {
            Some(v) => v,
            None => return vec![],
        };
        let hist = ctx.history(sym, self.ma_days);
        if hist.len() < self.ma_days {
            return vec![];
        }
        let ma: f64 = hist.iter().map(|k| k.close).sum::<f64>() / self.ma_days as f64;
        let held = ctx.position(sym).map(|p| p.quantity).unwrap_or(0.0);
        if held == 0.0 && k.close > ma {
            // 预留手续费余量；撮合与扣费由执行器完成
            return vec![Order::market_buy(sym.clone(), ctx.cash() * 0.999 / k.close)];
        }
        if held > 0.0 && k.close < ma {
            return vec![Order::market_sell(sym.clone(), held)];
        }
        vec![]
    }
}

fn main() {
    // 合成数据：趋势 + 波动（真实场景请用 app 的 backtest 子命令加载历史K线）
    let mut klines = Vec::new();
    let mut price = 100.0;
    for i in 0..365u64 {
        // 前 120 天下跌，中间 150 天上涨，最后回落：制造可交易的趋势段
        let drift = if i < 120 { -0.002 } else if i < 270 { 0.004 } else { -0.003 };
        let wave = ((i as f64) * 0.35).sin() * 0.006;
        price *= 1.0 + drift + wave;
        klines.push(Kline {
            open_time: i * DAY,
            open: price * 0.998,
            high: price * 1.005,
            low: price * 0.995,
            close: price,
            volume: 1.0,
            close_time: i * DAY + DAY - 1,
        });
    }
    let mut data = BTreeMap::new();
    data.insert("DEMO".to_string(), klines);

    let mut strategy = MaBreakout { ma_days: 20 };
    let config = BacktestConfig {
        initial_cash: 10_000.0,
        model: FillModel::new(0.0, 0.001), // 单边 0.1% 手续费
        max_history: 30,
        ..Default::default() // 默认下一根开盘价撮合（无未来函数）
    };

    let r = run_backtest(&mut strategy, &data, &config).expect("回测失败");
    println!("策略: {}", r.strategy_name);
    println!("  总收益率(净): {:.1}%", r.metrics.total_return_pct);
    println!("  最大回撤:     {:.1}%", r.metrics.max_drawdown_pct);
    println!("  夏普比率:     {:.2}", r.metrics.sharpe_ratio);
    println!("  交易回合:     {} | 胜率 {:.1}%", r.metrics.num_round_trips, r.metrics.win_rate_pct);
    println!("  累计手续费:   {:.2}", r.metrics.total_fees);
}
