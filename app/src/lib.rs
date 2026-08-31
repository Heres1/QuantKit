//! quantkit 应用层库：配置 / 数据 / dry-run / live（供 CLI 与 WebUI 复用）

pub mod benchmark;
pub mod config;
pub mod data;
pub mod data_provider;
pub mod dryrun;
pub mod events;
pub mod live;
pub mod logging;
pub mod notify;
pub mod optimize;
pub mod risk;

use config::AppConfig;
use quantkit_core::engine::BacktestConfig;
use quantkit_core::executor::FillModel;
use quantkit_core::interval::Interval;
use quantkit_core::strategy::Strategy;
use quantkit_strategies::dca::Dca;
use quantkit_strategies::grid::Grid;
use quantkit_strategies::ma_cross::MaCross;
use quantkit_strategies::momentum_rotation::MomentumRotation;
use quantkit_strategies::multi_trailing::MultiTrailingTrend;
use quantkit_strategies::trailing_trend::TrailingTrend;

/// 按配置构建策略实例（momentum 动量轮动 / trend 趋势追踪 / ma_cross 双均线模板 /
/// grid 智能网格 / dca 定投）
///
/// 参数语义分三类，多周期下的处理方式不同：
/// - 以「天」表达的回看窗口（`momentum_days` / `ma_days` / `regime_ma_days`）：
///   按 `interval` 换算为根数，使换周期后信号覆盖的真实时间跨度不变
///   （90 日动量在 4h 上是 540 根，仍是 90 天的动量，只是评估更频繁）
/// - 以「天」表达的时间间隔（`rebalance_days` / `cooldown_days`）：
///   策略内部按墙钟毫秒计算，本身跨周期正确，不换算
/// - `ma_cross_fast` / `ma_cross_slow`：按图表惯例即为「根」（4h 上的 MA10 = 10 根 4h），不换算
pub fn build_strategy(cfg: &AppConfig, interval: Interval) -> Box<dyn Strategy> {
    match cfg.strategy.as_str() {
        "trend" => {
            let ma_bars = interval.days_to_bars(cfg.ma_days);
            let symbols = if cfg.symbols.is_empty() {
                vec!["BTCUSDT".to_string()]
            } else {
                cfg.symbols.clone()
            };
            let trail_for = |s: &str| {
                cfg.trailing_stop_by_symbol
                    .get(s)
                    .copied()
                    .unwrap_or(cfg.trailing_stop_pct)
            };
            if symbols.len() == 1 {
                Box::new(TrailingTrend::new(
                    symbols[0].clone(),
                    ma_bars,
                    trail_for(&symbols[0]),
                    cfg.cooldown_days,
                ))
            } else {
                Box::new(MultiTrailingTrend::new(
                    symbols
                        .iter()
                        .map(|s| TrailingTrend::new(s.clone(), ma_bars, trail_for(s), cfg.cooldown_days))
                        .collect(),
                ))
            }
        }
        "ma_cross" => Box::new(MaCross::new(
            cfg.symbols.clone(),
            cfg.ma_cross_fast,
            cfg.ma_cross_slow,
        )),
        "grid" => Box::new(Grid::new(
            cfg.symbols.clone(),
            cfg.grid_levels,
            interval.days_to_bars(cfg.grid_lookback_days),
            cfg.grid_stop_loss_pct,
            cfg.grid_budget_per_symbol,
        )),
        "dca" => Box::new(Dca::new(
            cfg.symbols.clone(),
            cfg.dca_amount,
            cfg.dca_interval_days,
            interval.days_to_bars(cfg.dca_ma_days),
            cfg.dca_dip_multiplier,
        )),
        _ => Box::new(MomentumRotation::new_extended(
            interval.days_to_bars(cfg.momentum_days),
            interval.days_to_bars(cfg.ma_days),
            cfg.rebalance_days,
            if cfg.trailing_stop_enabled { cfg.trailing_stop_pct } else { 0.0 },
            cfg.top_n,
            interval.days_to_bars(cfg.regime_ma_days),
            cfg.regime_min_breadth,
        )),
    }
}

/// 策略历史窗口需求（单位：根）：动量/均线/市场状态均线/双均线慢线/
/// 网格区间回看/定投趋势均线，取最大者（+10 余量）
///
/// 必须在换算成根数之后取最大值：4h 下 90 日动量需要 540 根历史，
/// 若仍按 90 根开窗，策略会因历史不足而永不触发。
pub fn history_window(cfg: &AppConfig, interval: Interval) -> usize {
    interval
        .days_to_bars(cfg.momentum_days)
        .max(interval.days_to_bars(cfg.ma_days))
        .max(interval.days_to_bars(cfg.regime_ma_days))
        .max(if cfg.strategy == "ma_cross" { cfg.ma_cross_slow } else { 0 })
        .max(if cfg.strategy == "grid" {
            interval.days_to_bars(cfg.grid_lookback_days)
        } else {
            0
        })
        .max(if cfg.strategy == "dca" {
            interval.days_to_bars(cfg.dca_ma_days)
        } else {
            0
        })
        + 10
}

/// 由 AppConfig 生成回测配置（含组合回撤熔断风控）
pub fn backtest_config(cfg: &AppConfig, interval: Interval) -> BacktestConfig {
    BacktestConfig {
        initial_cash: cfg.initial_cash,
        model: FillModel::new(cfg.slippage_pct, cfg.fee_rate),
        max_history: history_window(cfg, interval),
        fill_price: if cfg.fill_mode == "same_close" {
            quantkit_core::engine::FillPrice::SameBarClose
        } else {
            quantkit_core::engine::FillPrice::NextBarOpen
        },
        circuit_breaker_pct: cfg.circuit_breaker_pct,
        circuit_breaker_cooldown_ms: cfg.circuit_breaker_cooldown_days * 86_400_000,
        // 回测/模拟/实盘均从第一根K线就开始交易，无热身段
        signal_start_ts: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> AppConfig {
        let mut cfg = AppConfig::default();
        cfg.strategy = "momentum".to_string();
        cfg.symbols = vec!["BTCUSDT".to_string(), "ETHUSDT".to_string()];
        cfg.interval = "1d".to_string();
        cfg.initial_cash = 10000.0;
        cfg.slippage_pct = 0.001;
        cfg.fee_rate = 0.001;
        cfg.momentum_days = 90;
        cfg.ma_days = 30;
        cfg.regime_ma_days = 200;
        cfg.rebalance_days = 7;
        cfg.cooldown_days = 3;
        cfg.trailing_stop_enabled = true;
        cfg.trailing_stop_pct = 0.05;
        cfg.top_n = 3;
        cfg.regime_min_breadth = 0.6;
        cfg.ma_cross_fast = 10;
        cfg.ma_cross_slow = 30;
        cfg.grid_levels = 10;
        cfg.grid_lookback_days = 30;
        cfg.grid_stop_loss_pct = 0.02;
        cfg.grid_budget_per_symbol = 1000.0;
        cfg.dca_amount = 100.0;
        cfg.dca_interval_days = 7;
        cfg.dca_ma_days = 200;
        cfg.dca_dip_multiplier = 1.5;
        cfg.fill_mode = "next_open".to_string();
        cfg.circuit_breaker_pct = 0.1;
        cfg.circuit_breaker_cooldown_days = 30;
        cfg.poll_secs = 60;
        cfg.state_file = "/tmp/state.json".to_string();
        cfg
    }

    #[test]
    fn test_build_strategy_momentum() {
        let cfg = test_config();
        let interval = Interval::D1;
        let strategy = build_strategy(&cfg, interval);
        
        assert_eq!(strategy.name(), "momentum_rotation");
    }

    #[test]
    fn test_build_strategy_trend() {
        // 多品种 -> 组合策略
        let mut cfg = test_config();
        cfg.strategy = "trend".to_string();
        let strategy = build_strategy(&cfg, Interval::H4);
        assert_eq!(strategy.name(), "multi_trailing_trend");

        // 单品种 -> 裸追踪止损（保持原行为）
        cfg.symbols = vec!["BTCUSDT".to_string()];
        let strategy = build_strategy(&cfg, Interval::H4);
        assert_eq!(strategy.name(), "trailing_trend");
    }

    #[test]
    fn test_build_strategy_ma_cross() {
        let mut cfg = test_config();
        cfg.strategy = "ma_cross".to_string();
        
        let interval = Interval::D1;
        let strategy = build_strategy(&cfg, interval);
        
        assert_eq!(strategy.name(), "ma_cross");
    }

    #[test]
    fn test_build_strategy_grid() {
        let mut cfg = test_config();
        cfg.strategy = "grid".to_string();
        
        let interval = Interval::H1;
        let strategy = build_strategy(&cfg, interval);
        
        assert_eq!(strategy.name(), "grid");
    }

    #[test]
    fn test_build_strategy_dca() {
        let mut cfg = test_config();
        cfg.strategy = "dca".to_string();
        
        let interval = Interval::D1;
        let strategy = build_strategy(&cfg, interval);
        
        assert_eq!(strategy.name(), "dca");
    }

    #[test]
    fn test_build_strategy_unknown_defaults_to_momentum() {
        let mut cfg = test_config();
        cfg.strategy = "unknown_strategy".to_string();
        
        let interval = Interval::D1;
        let strategy = build_strategy(&cfg, interval);
        
        // 未知策略应回退到动量轮动
        assert_eq!(strategy.name(), "momentum_rotation");
    }

    #[test]
    fn test_build_strategy_with_empty_symbols() {
        let mut cfg = test_config();
        cfg.strategy = "trend".to_string();
        cfg.symbols = vec![];
        
        let interval = Interval::D1;
        let strategy = build_strategy(&cfg, interval);
        
        // trend 策略应使用默认 BTCUSDT
        assert_eq!(strategy.name(), "trailing_trend");
    }

    #[test]
    fn test_history_window_momentum_daily() {
        let cfg = test_config();
        let interval = Interval::D1;
        
        let window = history_window(&cfg, interval);
        
        // momentum_days=90, ma_days=30, regime_ma_days=200
        // max(90, 30, 200) + 10 = 210
        assert_eq!(window, 210);
    }

    #[test]
    fn test_history_window_momentum_4h() {
        let cfg = test_config();
        let interval = Interval::H4;
        
        let window = history_window(&cfg, interval);
        
        // 4h: bars_per_day = 6
        // momentum: 90 * 6 = 540
        // ma: 30 * 6 = 180
        // regime_ma: 200 * 6 = 1200
        // max(540, 180, 1200) + 10 = 1210
        assert_eq!(window, 1210);
    }

    #[test]
    fn test_history_window_ma_cross() {
        let mut cfg = test_config();
        cfg.strategy = "ma_cross".to_string();
        cfg.ma_cross_slow = 50;
        
        let interval = Interval::D1;
        let window = history_window(&cfg, interval);
        
        // ma_cross_slow=50, regime_ma_days=200
        // max(90, 30, 200, 50) + 10 = 210
        assert_eq!(window, 210);
    }

    #[test]
    fn test_history_window_grid() {
        let mut cfg = test_config();
        cfg.strategy = "grid".to_string();
        cfg.grid_lookback_days = 60;
        
        let interval = Interval::H4;
        let window = history_window(&cfg, interval);
        
        // grid_lookback: 60 * 6 = 360
        // regime_ma: 200 * 6 = 1200
        // max(540, 180, 1200, 360) + 10 = 1210
        assert_eq!(window, 1210);
    }

    #[test]
    fn test_history_window_dca() {
        let mut cfg = test_config();
        cfg.strategy = "dca".to_string();
        cfg.dca_ma_days = 100;
        
        let interval = Interval::D1;
        let window = history_window(&cfg, interval);
        
        // dca_ma: 100
        // regime_ma: 200
        // max(90, 30, 200, 100) + 10 = 210
        assert_eq!(window, 210);
    }

    #[test]
    fn test_backtest_config_next_bar_open() {
        let cfg = test_config();
        let interval = Interval::D1;
        
        let bt_cfg = backtest_config(&cfg, interval);
        
        assert!((bt_cfg.initial_cash - 10000.0).abs() < 1e-6);
        assert!((bt_cfg.model.slippage_pct - 0.001).abs() < 1e-6);
        assert!((bt_cfg.model.fee_rate - 0.001).abs() < 1e-6);
        assert_eq!(bt_cfg.fill_price, quantkit_core::engine::FillPrice::NextBarOpen);
        assert!((bt_cfg.circuit_breaker_pct - 0.1).abs() < 1e-6);
        assert_eq!(bt_cfg.circuit_breaker_cooldown_ms, 30 * 86_400_000);
        assert_eq!(bt_cfg.signal_start_ts, 0);
    }

    #[test]
    fn test_backtest_config_same_bar_close() {
        let mut cfg = test_config();
        cfg.fill_mode = "same_close".to_string();
        
        let interval = Interval::H4;
        let bt_cfg = backtest_config(&cfg, interval);
        
        assert_eq!(bt_cfg.fill_price, quantkit_core::engine::FillPrice::SameBarClose);
    }

    #[test]
    fn test_backtest_config_circuit_breaker_disabled() {
        let mut cfg = test_config();
        cfg.circuit_breaker_pct = 0.0;
        cfg.circuit_breaker_cooldown_days = 0; // 同时设置冷却天数为 0
        
        let interval = Interval::D1;
        let bt_cfg = backtest_config(&cfg, interval);
        
        assert!((bt_cfg.circuit_breaker_pct - 0.0).abs() < 1e-6);
        assert_eq!(bt_cfg.circuit_breaker_cooldown_ms, 0);
    }

    #[test]
    fn test_backtest_config_trailing_stop_disabled() {
        let mut cfg = test_config();
        cfg.trailing_stop_enabled = false;
        
        let interval = Interval::D1;
        let strategy = build_strategy(&cfg, interval);
        
        // 动量轮动策略应能正常创建，只是 trailing_stop_pct=0
        assert_eq!(strategy.name(), "momentum_rotation");
    }

    #[test]
    fn test_build_strategy_different_intervals() {
        let cfg = test_config();
        
        // 测试不同周期下的策略构建
        for interval in &[Interval::M5, Interval::H1, Interval::D1, Interval::W1] {
            let strategy = build_strategy(&cfg, *interval);
            assert_eq!(strategy.name(), "momentum_rotation");
        }
    }

    #[test]
    fn test_history_window_weekly_interval() {
        let cfg = test_config();
        let interval = Interval::W1;
        
        let window = history_window(&cfg, interval);
        
        // W1: bars_per_day = 1/7
        // momentum: round(90 / 7) = 13
        // ma: round(30 / 7) = 4
        // regime_ma: round(200 / 7) = 29
        // max(13, 4, 29) + 10 = 39
        assert_eq!(window, 39);
    }

    #[test]
    fn test_backtest_config_max_history_matches_history_window() {
        let cfg = test_config();
        let interval = Interval::H4;
        
        let bt_cfg = backtest_config(&cfg, interval);
        let expected_window = history_window(&cfg, interval);
        
        assert_eq!(bt_cfg.max_history, expected_window);
    }

    #[test]
    fn test_build_strategy_momentum_with_trailing_stop_disabled() {
        let mut cfg = test_config();
        cfg.trailing_stop_enabled = false;
        
        let interval = Interval::D1;
        let strategy = build_strategy(&cfg, interval);
        
        // 应正常创建，trailing_stop_pct 传 0.0
        assert_eq!(strategy.name(), "momentum_rotation");
    }

    #[test]
    fn test_config_field_accessibility() {
        let cfg = test_config();
        
        // 验证所有配置字段都可访问
        assert_eq!(cfg.strategy, "momentum");
        assert_eq!(cfg.symbols.len(), 2);
        assert!((cfg.initial_cash - 10000.0).abs() < 1e-6);
        assert_eq!(cfg.momentum_days, 90);
        assert_eq!(cfg.ma_cross_fast, 10);
        assert_eq!(cfg.grid_levels, 10);
        assert!((cfg.dca_amount - 100.0).abs() < 1e-6);
    }
}
