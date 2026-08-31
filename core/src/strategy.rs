//! 策略抽象：Strategy trait + 引擎注入的上下文。

use std::collections::{BTreeMap, VecDeque};

use crate::types::{Kline, Order, Position};

/// 引擎在每根K线收盘后注入给策略的只读上下文
pub struct StrategyContext {
    history: BTreeMap<String, VecDeque<Kline>>,
    positions: BTreeMap<String, Position>,
    cash: f64,
    max_history: usize,
}

impl StrategyContext {
    pub fn new(max_history: usize) -> Self {
        Self {
            history: BTreeMap::new(),
            positions: BTreeMap::new(),
            cash: 0.0,
            max_history,
        }
    }

    /// 引擎专用：追加一根K线到历史窗口
    pub fn push_bar(&mut self, symbol: &str, kline: Kline) {
        let dq = self.history.entry(symbol.to_string()).or_default();
        dq.push_back(kline);
        while dq.len() > self.max_history {
            dq.pop_front();
        }
    }

    /// 引擎专用：每步同步账户快照
    pub fn sync_account(&mut self, positions: BTreeMap<String, Position>, cash: f64) {
        self.positions = positions;
        self.cash = cash;
    }

    /// 某品种最近 n 根K线（含当前bar，旧→新）
    pub fn history(&self, symbol: &str, n: usize) -> Vec<&Kline> {
        self.history
            .get(symbol)
            .map(|dq| dq.iter().rev().take(n).collect::<Vec<_>>().into_iter().rev().collect())
            .unwrap_or_default()
    }

    /// 当前持仓（None 表示空仓）
    pub fn position(&self, symbol: &str) -> Option<&Position> {
        self.positions.get(symbol)
    }

    pub fn positions(&self) -> &BTreeMap<String, Position> {
        &self.positions
    }

    /// 可用计价资产现金
    pub fn cash(&self) -> f64 {
        self.cash
    }
}

/// 策略：每根对齐的K线收盘后收到一次回调，返回订单列表。
///
/// 订单由引擎统一执行（回测按下一根开盘价撮合），策略不直接接触执行器——
/// 这就是引擎能拦截、审计、替换执行方式的原因。
pub trait Strategy {
    fn name(&self) -> &str;

    /// bars: 本时间戳各品种收盘的K线（对齐后）。返回的订单在下一根bar开盘执行。
    fn on_bars(&mut self, ctx: &StrategyContext, bars: &BTreeMap<String, Kline>) -> Vec<Order>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strategy_context_new() {
        let ctx = StrategyContext::new(100);
        assert_eq!(ctx.cash(), 0.0);
        assert!(ctx.positions().is_empty());
        assert!(ctx.history("BTCUSDT", 10).is_empty());
    }

    #[test]
    fn test_push_bar_and_history() {
        let mut ctx = StrategyContext::new(10);
        
        // 添加 5 根K线
        for i in 0..5 {
            let kline = Kline {
                open_time: i * 86400000,
                open: 60000.0 + i as f64 * 100.0,
                high: 61000.0 + i as f64 * 100.0,
                low: 59000.0 + i as f64 * 100.0,
                close: 60500.0 + i as f64 * 100.0,
                volume: 100.0,
                close_time: (i + 1) * 86400000,
            };
            ctx.push_bar("BTCUSDT", kline);
        }
        
        // 查询全部 5 根
        let history = ctx.history("BTCUSDT", 10);
        assert_eq!(history.len(), 5);
        
        // 顺序应该是旧→新
        assert!((history[0].open - 60000.0).abs() < 1e-6);
        assert!((history[4].open - 60400.0).abs() < 1e-6);
    }

    #[test]
    fn test_history_limited_to_n() {
        let mut ctx = StrategyContext::new(100);
        
        // 添加 10 根K线
        for i in 0..10 {
            let kline = Kline {
                open_time: i * 86400000,
                open: 60000.0 + i as f64,
                high: 61000.0,
                low: 59000.0,
                close: 60500.0,
                volume: 100.0,
                close_time: (i + 1) * 86400000,
            };
            ctx.push_bar("BTCUSDT", kline);
        }
        
        // 只查询最近 3 根
        let history = ctx.history("BTCUSDT", 3);
        assert_eq!(history.len(), 3);
        
        // 应该是最后 3 根（索引 7, 8, 9）
        assert!((history[0].open - 60007.0).abs() < 1e-6);
        assert!((history[2].open - 60009.0).abs() < 1e-6);
    }

    #[test]
    fn test_push_bar_exceeds_max_history() {
        let mut ctx = StrategyContext::new(5);
        
        // 添加 10 根K线，超过 max_history
        for i in 0..10 {
            let kline = Kline {
                open_time: i * 86400000,
                open: 60000.0 + i as f64,
                high: 61000.0,
                low: 59000.0,
                close: 60500.0,
                volume: 100.0,
                close_time: (i + 1) * 86400000,
            };
            ctx.push_bar("BTCUSDT", kline);
        }
        
        // 应该只保留最近 5 根
        let history = ctx.history("BTCUSDT", 10);
        assert_eq!(history.len(), 5);
        
        // 应该是索引 5-9
        assert!((history[0].open - 60005.0).abs() < 1e-6);
        assert!((history[4].open - 60009.0).abs() < 1e-6);
    }

    #[test]
    fn test_history_nonexistent_symbol() {
        let ctx = StrategyContext::new(10);
        let history = ctx.history("NONEXISTENT", 10);
        assert!(history.is_empty());
    }

    #[test]
    fn test_sync_account() {
        let mut ctx = StrategyContext::new(10);
        
        let mut positions = BTreeMap::new();
        positions.insert("BTCUSDT".to_string(), Position {
            symbol: "BTCUSDT".to_string(),
            quantity: 0.5,
            avg_entry_price: 60000.0,
        });
        
        ctx.sync_account(positions, 50000.0);
        
        assert!((ctx.cash() - 50000.0).abs() < 1e-6);
        assert_eq!(ctx.positions().len(), 1);
        
        let pos = ctx.position("BTCUSDT").unwrap();
        assert!((pos.quantity - 0.5).abs() < 1e-6);
        assert!((pos.avg_entry_price - 60000.0).abs() < 1e-6);
    }

    #[test]
    fn test_position_query_nonexistent() {
        let ctx = StrategyContext::new(10);
        assert!(ctx.position("BTCUSDT").is_none());
    }

    #[test]
    fn test_positions_reference() {
        let mut ctx = StrategyContext::new(10);
        
        let mut positions = BTreeMap::new();
        positions.insert("BTCUSDT".to_string(), Position {
            symbol: "BTCUSDT".to_string(),
            quantity: 1.0,
            avg_entry_price: 60000.0,
        });
        positions.insert("ETHUSDT".to_string(), Position {
            symbol: "ETHUSDT".to_string(),
            quantity: 10.0,
            avg_entry_price: 3000.0,
        });
        
        ctx.sync_account(positions, 100000.0);
        
        let all_positions = ctx.positions();
        assert_eq!(all_positions.len(), 2);
        assert!(all_positions.contains_key("BTCUSDT"));
        assert!(all_positions.contains_key("ETHUSDT"));
    }

    #[test]
    fn test_multiple_symbols_history() {
        let mut ctx = StrategyContext::new(10);
        
        // BTC
        let btc_kline = Kline {
            open_time: 1724932800000,
            open: 60000.0,
            high: 61000.0,
            low: 59000.0,
            close: 60500.0,
            volume: 100.0,
            close_time: 1724936400000,
        };
        ctx.push_bar("BTCUSDT", btc_kline);
        
        // ETH
        let eth_kline = Kline {
            open_time: 1724932800000,
            open: 3000.0,
            high: 3100.0,
            low: 2900.0,
            close: 3050.0,
            volume: 1000.0,
            close_time: 1724936400000,
        };
        ctx.push_bar("ETHUSDT", eth_kline);
        
        // 分别查询
        let btc_history = ctx.history("BTCUSDT", 10);
        assert_eq!(btc_history.len(), 1);
        assert!((btc_history[0].close - 60500.0).abs() < 1e-6);
        
        let eth_history = ctx.history("ETHUSDT", 10);
        assert_eq!(eth_history.len(), 1);
        assert!((eth_history[0].close - 3050.0).abs() < 1e-6);
    }

    #[test]
    fn test_history_order_preservation() {
        let mut ctx = StrategyContext::new(100);
        
        // 添加 3 根K线，验证顺序保持
        for i in 0..3 {
            let kline = Kline {
                open_time: i * 86400000,
                open: i as f64,
                high: i as f64 + 1.0,
                low: i as f64 - 1.0,
                close: i as f64 + 0.5,
                volume: 100.0,
                close_time: (i + 1) * 86400000,
            };
            ctx.push_bar("TEST", kline);
        }
        
        let history = ctx.history("TEST", 10);
        assert_eq!(history.len(), 3);
        
        // 顺序应该是 0, 1, 2
        for (idx, kline) in history.iter().enumerate() {
            assert!((kline.open - idx as f64).abs() < 1e-6);
        }
    }

    #[test]
    fn test_zero_max_history() {
        let mut ctx = StrategyContext::new(0);
        
        // 添加K线，max_history=0 应该立即丢弃
        let kline = Kline {
            open_time: 0,
            open: 60000.0,
            high: 61000.0,
            low: 59000.0,
            close: 60500.0,
            volume: 100.0,
            close_time: 86400000,
        };
        ctx.push_bar("BTCUSDT", kline);
        
        // 历史记录应该为空（因为 max_history=0）
        let history = ctx.history("BTCUSDT", 10);
        assert_eq!(history.len(), 0);
    }

    #[test]
    fn test_cash_update_via_sync() {
        let mut ctx = StrategyContext::new(10);
        
        // 初始现金为 0
        assert!((ctx.cash() - 0.0).abs() < 1e-6);
        
        // 更新现金
        ctx.sync_account(BTreeMap::new(), 10000.0);
        assert!((ctx.cash() - 10000.0).abs() < 1e-6);
        
        // 再次更新
        ctx.sync_account(BTreeMap::new(), 20000.0);
        assert!((ctx.cash() - 20000.0).abs() < 1e-6);
    }

    #[test]
    fn test_position_overwrite_on_sync() {
        let mut ctx = StrategyContext::new(10);
        
        // 第一次同步持仓
        let mut positions1 = BTreeMap::new();
        positions1.insert("BTCUSDT".to_string(), Position {
            symbol: "BTCUSDT".to_string(),
            quantity: 1.0,
            avg_entry_price: 60000.0,
        });
        ctx.sync_account(positions1, 10000.0);
        
        assert_eq!(ctx.positions().len(), 1);
        
        // 第二次同步，覆盖持仓
        let mut positions2 = BTreeMap::new();
        positions2.insert("ETHUSDT".to_string(), Position {
            symbol: "ETHUSDT".to_string(),
            quantity: 10.0,
            avg_entry_price: 3000.0,
        });
        ctx.sync_account(positions2, 20000.0);
        
        // BTC 持仓应被清除，ETH 持仓应存在
        assert_eq!(ctx.positions().len(), 1);
        assert!(ctx.position("BTCUSDT").is_none());
        assert!(ctx.position("ETHUSDT").is_some());
    }

    #[test]
    fn test_history_with_zero_n() {
        let mut ctx = StrategyContext::new(10);
        
        let kline = Kline {
            open_time: 0,
            open: 60000.0,
            high: 61000.0,
            low: 59000.0,
            close: 60500.0,
            volume: 100.0,
            close_time: 86400000,
        };
        ctx.push_bar("BTCUSDT", kline);
        
        // n=0 应该返回空
        let history = ctx.history("BTCUSDT", 0);
        assert!(history.is_empty());
    }

    #[test]
    fn test_strategy_trait_mock_implementation() {
        // 创建一个简单的 mock 策略用于测试 trait
        struct MockStrategy;
        
        impl Strategy for MockStrategy {
            fn name(&self) -> &str {
                "mock_strategy"
            }
            
            fn on_bars(&mut self, _ctx: &StrategyContext, _bars: &BTreeMap<String, Kline>) -> Vec<Order> {
                vec![]
            }
        }
        
        let strategy = MockStrategy;
        assert_eq!(strategy.name(), "mock_strategy");
    }

    #[test]
    fn test_strategy_context_with_large_history() {
        let mut ctx = StrategyContext::new(1000);
        
        // 添加大量K线
        for i in 0..500 {
            let kline = Kline {
                open_time: i * 86400000,
                open: i as f64,
                high: i as f64 + 1.0,
                low: i as f64 - 1.0,
                close: i as f64 + 0.5,
                volume: 100.0,
                close_time: (i + 1) * 86400000,
            };
            ctx.push_bar("TEST", kline);
        }
        
        let history = ctx.history("TEST", 1000);
        assert_eq!(history.len(), 500);
    }

    #[test]
    fn test_push_bar_updates_same_symbol() {
        let mut ctx = StrategyContext::new(10);
        
        // 连续添加同一品种的K线
        for i in 0..5 {
            ctx.push_bar("BTCUSDT", Kline {
                open_time: i * 86400000,
                open: 60000.0 + i as f64 * 100.0,
                high: 61000.0,
                low: 59000.0,
                close: 60500.0 + i as f64 * 100.0,
                volume: 100.0 + i as f64 * 10.0,
                close_time: (i + 1) * 86400000,
            });
        }
        
        // 查询不同数量的历史
        assert_eq!(ctx.history("BTCUSDT", 2).len(), 2);
        assert_eq!(ctx.history("BTCUSDT", 5).len(), 5);
        assert_eq!(ctx.history("BTCUSDT", 10).len(), 5); // 最多只有 5 根
    }
}
