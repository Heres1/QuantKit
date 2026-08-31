//! dry-run：真实行情 + 模拟撮合。
//!
//! 实现方式（确定性重放）：
//! 1. 每轮从 Binance 拉取各品种配置周期K线，丢弃未收盘的当前bar
//! 2. 用已收盘bar序列对**全新**策略实例做完整重放
//!    （`SameBarClose` 口径：收盘确认信号、按该收盘价模拟即时成交）
//! 3. 重放结果是行情与参数的纯函数：重启后状态自动重建，天然不丢失；
//!    本地快照用于审计与差量比对，新出现的回合即“本轮模拟成交”，
//!    打日志后原子持久化（临时文件 + rename）

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use quantkit_core::engine::{run_backtest, BacktestConfig, FillPrice};
use quantkit_core::executor::FillModel;
use quantkit_core::interval::Interval;
use quantkit_core::types::{Kline, Position, Trade};
use quantkit_exchanges::binance::{now_ms, BinanceClient};

use crate::config::AppConfig;

/// dry-run 状态快照（原子写入磁盘）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DryRunState {
    pub strategy: String,
    pub initial_cash: f64,
    pub fee_rate: f64,
    /// 已重放到的最后一根（已收盘）bar 时间戳
    pub last_bar_ts: u64,
    pub cash: f64,
    pub equity: f64,
    pub positions: Vec<Position>,
    /// 已平仓回合（净盈亏口径）
    pub trades: Vec<Trade>,
    pub updated_at_ms: u64,
}

/// 运行 dry-run 循环（阻塞直到进程被终止）
pub async fn run(cfg: &AppConfig, interval: Interval) {
    let state_path = PathBuf::from(&cfg.state_file);
    let client = BinanceClient::public();
    let mut prev: Option<DryRunState> = load_state(&state_path);
    if let Some(p) = &prev {
        println!(
            "[dry-run] 恢复状态: 权益 {:.2} | 持仓 {} 个 | 历史回合 {} 笔",
            p.equity,
            p.positions.len(),
            p.trades.len()
        );
    } else {
        println!("[dry-run] 无历史状态，从初始资金 {:.2} 开始", cfg.initial_cash);
    }
    println!(
        "[dry-run] 品种 {:?} | 周期 {} | 轮询 {}s | 状态文件 {}",
        cfg.symbols, interval.as_str(), cfg.poll_secs, cfg.state_file
    );
    println!(
        "[dry-run] 手续费率 {:.4}（配置值；默认 0.00075 = Binance 现货 0.1% 启用 BNB 抵扣，未开启则设 0.001）",
        cfg.fee_rate
    );

    loop {
        match run_cycle(cfg, interval, &client, &state_path, &mut prev).await {
            Ok(_) => {}
            Err(e) => eprintln!("[dry-run] 本轮失败（下轮重试）: {e}"),
        }
        tokio::time::sleep(Duration::from_secs(cfg.poll_secs)).await;
    }
}

/// 单轮：拉数据 -> 重放 -> 差量日志 -> 原子保存
async fn run_cycle(
    cfg: &AppConfig,
    interval: Interval,
    client: &BinanceClient,
    state_path: &Path,
    prev: &mut Option<DryRunState>,
) -> Result<(), String> {
    // 1) 拉取全量历史K线（向前分页，上限 2000 根）：
    // 调仓评估相位锚定历史起点，等价于"从历史起一直运行的进程"，
    // 避免窗口起点依赖造成的信号滞后
    let mut data: BTreeMap<String, Vec<Kline>> = BTreeMap::new();
    let now = now_ms();
    for sym in &cfg.symbols {
        let mut ks = client
            .fetch_klines_history(sym, interval.as_str(), 2000, None)
            .await
            .map_err(|e| format!("拉取 {sym} K线失败: {e}"))?;
        // 丢弃未收盘的当前bar：只在收盘确认后决策（杜绝盘中假信号）
        ks.retain(|k| k.close_time <= now);
        data.insert(sym.clone(), ks);
    }

    // 2) 确定性重放：全新策略实例，内部状态由历史数据重建（重启无损）
    let mut strategy = crate::build_strategy(cfg, interval);
    let bt = BacktestConfig {
        initial_cash: cfg.initial_cash,
        model: FillModel::new(cfg.slippage_pct, cfg.fee_rate),
        max_history: crate::history_window(cfg, interval),
        fill_price: FillPrice::SameBarClose,
        circuit_breaker_pct: cfg.circuit_breaker_pct,
        circuit_breaker_cooldown_ms: cfg.circuit_breaker_cooldown_days * 86_400_000,
        // 回测/模拟/实盘均从第一根K线就开始交易，无热身段
        signal_start_ts: 0,
    };
    let r = run_backtest(strategy.as_mut(), &data, &bt).map_err(|e| format!("重放失败: {e}"))?;
    let last_bar_ts = r.equity_curve.last().map(|p| p.timestamp).unwrap_or(0);

    // 与上轮快照同bar -> 无新收盘数据，跳过
    if prev.as_ref().is_some_and(|p| p.last_bar_ts == last_bar_ts) {
        return Ok(());
    }

    // 3) 差量比对：新回合 = 本轮新增模拟成交（逐笔可追溯）
    let prev_n = prev.as_ref().map(|p| p.trades.len()).unwrap_or(0);
    for t in r.trades.iter().skip(prev_n) {
        println!(
            "[dry-run] 模拟成交 {} {} 入{:.6} -> 出{:.6} 量{:.6} 净盈亏 {:+.2}",
            t.exit_time, t.symbol, t.entry_price, t.exit_price, t.quantity, t.pnl
        );
    }

    // 4) 快照 + 原子持久化
    let state = DryRunState {
        strategy: r.strategy_name.clone(),
        initial_cash: cfg.initial_cash,
        fee_rate: cfg.fee_rate,
        last_bar_ts,
        cash: r.final_cash,
        equity: r.equity_curve.last().map(|p| p.equity).unwrap_or(cfg.initial_cash),
        positions: r.final_positions.clone(),
        trades: r.trades.clone(),
        updated_at_ms: now_ms(),
    };
    save_state_atomic(state_path, &state).map_err(|e| format!("状态保存失败: {e}"))?;

    println!(
        "[dry-run] bar {} | 权益 {:.2} ({:+.2}%) | 现金 {:.2} | 持仓 {:?} | 累计回合 {}",
        last_bar_ts,
        state.equity,
        (state.equity / cfg.initial_cash - 1.0) * 100.0,
        state.cash,
        state
            .positions
            .iter()
            .map(|p| format!("{} {:.6}", p.symbol, p.quantity))
            .collect::<Vec<_>>(),
        state.trades.len()
    );
    *prev = Some(state);
    Ok(())
}

fn load_state(path: &Path) -> Option<DryRunState> {
    let s = std::fs::read_to_string(path).ok()?;
    match serde_json::from_str(&s) {
        Ok(st) => Some(st),
        Err(e) => {
            eprintln!("[dry-run] 状态文件损坏({e})，将从初始资金重建");
            None
        }
    }
}

/// 原子写：先写临时文件再 rename，避免半写状态
fn save_state_atomic(path: &Path, state: &DryRunState) -> Result<(), std::io::Error> {
    let json = serde_json::to_string_pretty(state).expect("状态序列化");
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_state_roundtrip_and_atomic_write() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("quantkit_dryrun_test_{}.json", std::process::id()));
        let st = DryRunState {
            strategy: "momentum_rotation".into(),
            initial_cash: 10_000.0,
            fee_rate: 0.0005,
            last_bar_ts: 1_700_000_000_000,
            cash: 123.45,
            equity: 10_123.45,
            positions: vec![Position {
                symbol: "BTCUSDT".into(),
                quantity: 0.05,
                avg_entry_price: 60_000.0,
            }],
            trades: vec![],
            updated_at_ms: 1_700_000_001_000,
        };
        save_state_atomic(&path, &st).unwrap();
        let loaded = load_state(&path).expect("应能读回");
        assert_eq!(loaded.equity, st.equity);
        assert_eq!(loaded.positions[0].symbol, "BTCUSDT");
        assert_eq!(loaded.last_bar_ts, st.last_bar_ts);
        // 临时文件不应残留
        assert!(!path.with_extension("json.tmp").exists());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_dryrun_state_serialization() {
        let state = DryRunState {
            strategy: "trend_following".to_string(),
            initial_cash: 50000.0,
            fee_rate: 0.001,
            last_bar_ts: 1724932800000,
            cash: 45000.0,
            equity: 52000.0,
            positions: vec![
                Position {
                    symbol: "ETHUSDT".to_string(),
                    quantity: 10.0,
                    avg_entry_price: 3000.0,
                },
                Position {
                    symbol: "SOLUSDT".to_string(),
                    quantity: 100.0,
                    avg_entry_price: 150.0,
                },
            ],
            trades: vec![],
            updated_at_ms: 1724936400000,
        };

        let json = serde_json::to_string(&state).unwrap();
        assert!(json.contains("trend_following"));
        assert!(json.contains("ETHUSDT"));
        assert!(json.contains("SOLUSDT"));

        let deserialized: DryRunState = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.strategy, state.strategy);
        assert_eq!(deserialized.positions.len(), 2);
        assert!((deserialized.equity - 52000.0).abs() < 1e-6);
    }

    #[test]
    fn test_load_nonexistent_state() {
        let path = PathBuf::from("/tmp/nonexistent_dryrun_state.json");
        let loaded = load_state(&path);
        assert!(loaded.is_none());
    }

    #[test]
    fn test_load_corrupted_state() {
        let temp_dir = std::env::temp_dir();
        let path = temp_dir.join(format!("corrupt_dryrun_{}.json", std::process::id()));

        std::fs::write(&path, "this is not valid json").unwrap();

        let loaded = load_state(&path);
        assert!(loaded.is_none());

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_save_empty_positions() {
        let temp_dir = std::env::temp_dir();
        let path = temp_dir.join(format!("empty_pos_dryrun_{}.json", std::process::id()));

        let state = DryRunState {
            strategy: "ma_cross".to_string(),
            initial_cash: 10000.0,
            fee_rate: 0.00075,
            last_bar_ts: 0,
            cash: 10000.0,
            equity: 10000.0,
            positions: vec![],
            trades: vec![],
            updated_at_ms: 0,
        };

        save_state_atomic(&path, &state).unwrap();
        let loaded = load_state(&path).unwrap();

        assert!(loaded.positions.is_empty());
        assert!((loaded.cash - 10000.0).abs() < 1e-6);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_save_many_trades() {
        let temp_dir = std::env::temp_dir();
        let path = temp_dir.join(format!("many_trades_dryrun_{}.json", std::process::id()));

        let mut trades = Vec::new();
        for i in 0..100 {
            trades.push(Trade {
                symbol: if i % 2 == 0 { "BTCUSDT".to_string() } else { "ETHUSDT".to_string() },
                entry_time: 1724932800000 + i as u64 * 86400000,
                exit_time: 1724932800000 + (i as u64 + 1) * 86400000,
                entry_price: 60000.0 + i as f64 * 100.0,
                exit_price: 61000.0 + i as f64 * 100.0,
                quantity: 0.1,
                pnl: 100.0,
            });
        }

        let state = DryRunState {
            strategy: "momentum".to_string(),
            initial_cash: 10000.0,
            fee_rate: 0.001,
            last_bar_ts: 1724932800000,
            cash: 5000.0,
            equity: 15000.0,
            positions: vec![],
            trades,
            updated_at_ms: 1724936400000,
        };

        save_state_atomic(&path, &state).unwrap();
        let loaded = load_state(&path).unwrap();

        assert_eq!(loaded.trades.len(), 100);
        assert!((loaded.equity - 15000.0).abs() < 1e-6);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_save_overwrite_existing() {
        let temp_dir = std::env::temp_dir();
        let path = temp_dir.join(format!("overwrite_dryrun_{}.json", std::process::id()));

        // 第一次保存
        let state1 = DryRunState {
            strategy: "v1".to_string(),
            initial_cash: 10000.0,
            fee_rate: 0.001,
            last_bar_ts: 1000,
            cash: 10000.0,
            equity: 10000.0,
            positions: vec![],
            trades: vec![],
            updated_at_ms: 1000,
        };
        save_state_atomic(&path, &state1).unwrap();

        // 第二次保存（覆盖）
        let state2 = DryRunState {
            strategy: "v2".to_string(),
            initial_cash: 10000.0,
            fee_rate: 0.001,
            last_bar_ts: 2000,
            cash: 12000.0,
            equity: 12000.0,
            positions: vec![],
            trades: vec![],
            updated_at_ms: 2000,
        };
        save_state_atomic(&path, &state2).unwrap();

        // 验证是最新状态
        let loaded = load_state(&path).unwrap();
        assert_eq!(loaded.strategy, "v2");
        assert!((loaded.equity - 12000.0).abs() < 1e-6);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_dryrun_state_with_zero_equity() {
        let state = DryRunState {
            strategy: "test".to_string(),
            initial_cash: 0.0,
            fee_rate: 0.0,
            last_bar_ts: 0,
            cash: 0.0,
            equity: 0.0,
            positions: vec![],
            trades: vec![],
            updated_at_ms: 0,
        };

        let json = serde_json::to_string(&state).unwrap();
        let deserialized: DryRunState = serde_json::from_str(&json).unwrap();

        assert!((deserialized.equity - 0.0).abs() < 1e-6);
        assert!((deserialized.cash - 0.0).abs() < 1e-6);
    }

    #[test]
    fn test_dryrun_state_negative_pnl_simulation() {
        // 模拟亏损场景
        let state = DryRunState {
            strategy: "mean_reversion".to_string(),
            initial_cash: 10000.0,
            fee_rate: 0.001,
            last_bar_ts: 1724932800000,
            cash: 8000.0,
            equity: 8500.0, // 亏损 15%
            positions: vec![],
            trades: vec![
                Trade {
                    symbol: "BTCUSDT".to_string(),
                    entry_time: 1724846400000,
                    exit_time: 1724932800000,
                    entry_price: 65000.0,
                    exit_price: 60000.0,
                    quantity: 0.1,
                    pnl: -500.0,
                },
            ],
            updated_at_ms: 1724936400000,
        };

        let json = serde_json::to_string(&state).unwrap();
        let deserialized: DryRunState = serde_json::from_str(&json).unwrap();

        assert!((deserialized.equity - 8500.0).abs() < 1e-6);
        assert_eq!(deserialized.trades.len(), 1);
        assert!((deserialized.trades[0].pnl - (-500.0)).abs() < 1e-6);
    }

    #[test]
    fn test_dryrun_state_multiple_positions_same_symbol() {
        // 虽然实际策略不会这样，但数据结构应该支持
        let state = DryRunState {
            strategy: "test".to_string(),
            initial_cash: 10000.0,
            fee_rate: 0.001,
            last_bar_ts: 1724932800000,
            cash: 5000.0,
            equity: 10000.0,
            positions: vec![
                Position {
                    symbol: "BTCUSDT".to_string(),
                    quantity: 0.05,
                    avg_entry_price: 60000.0,
                },
                Position {
                    symbol: "BTCUSDT".to_string(), // 同一品种
                    quantity: 0.03,
                    avg_entry_price: 61000.0,
                },
            ],
            trades: vec![],
            updated_at_ms: 1724936400000,
        };

        let json = serde_json::to_string(&state).unwrap();
        let deserialized: DryRunState = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.positions.len(), 2);
    }

    #[test]
    fn test_save_state_large_json() {
        // 测试大 JSON 序列化性能
        let temp_dir = std::env::temp_dir();
        let path = temp_dir.join(format!("large_dryrun_{}.json", std::process::id()));

        let mut positions = Vec::new();
        for i in 0..50 {
            positions.push(Position {
                symbol: format!("SYM{:03}USDT", i),
                quantity: 100.0 + i as f64,
                avg_entry_price: 10.0 + i as f64,
            });
        }

        let state = DryRunState {
            strategy: "diversified".to_string(),
            initial_cash: 100000.0,
            fee_rate: 0.001,
            last_bar_ts: 1724932800000,
            cash: 50000.0,
            equity: 120000.0,
            positions,
            trades: vec![],
            updated_at_ms: 1724936400000,
        };

        save_state_atomic(&path, &state).unwrap();
        let loaded = load_state(&path).unwrap();

        assert_eq!(loaded.positions.len(), 50);
        assert_eq!(loaded.positions[0].symbol, "SYM000USDT");
        assert_eq!(loaded.positions[49].symbol, "SYM049USDT");

        std::fs::remove_file(&path).ok();
    }
}
