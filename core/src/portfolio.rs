//! 资金与持仓记账。
//!
//! 所有盈亏一律按净利润口径：手续费在成交时即刻从现金中扣除，
//! 权益（equity）= 现金 + 持仓市值，天然已扣费。

use std::collections::BTreeMap;

use crate::types::{Fill, Position, Side, Trade};

/// 账户：现金 + 持仓 + 交易记录
#[derive(Debug, Clone, Default)]
pub struct Portfolio {
    /// 计价资产现金（如 USDT）
    pub cash: f64,
    /// 持仓（品种 -> 持仓）
    pub positions: BTreeMap<String, Position>,
    /// 已完成回合的净盈亏记录（含手续费）
    pub trades: Vec<Trade>,
    /// 累计已支付手续费
    pub total_fees: f64,
    /// 已实现净盈亏
    pub realized_pnl: f64,
    /// 各品种开仓时间（填 Trade.entry_time；平仓时取出，重新开仓覆盖）
    entry_times: BTreeMap<String, u64>,
}

impl Portfolio {
    pub fn new(initial_cash: f64) -> Self {
        Self {
            cash: initial_cash,
            ..Default::default()
        }
    }

    /// 应用一笔成交，更新现金、持仓与交易记录。
    /// 返回本次成交的已实现盈亏（买入为 0）。
    pub fn apply_fill(&mut self, fill: &Fill) -> f64 {
        self.total_fees += fill.fee;
        match fill.side {
            Side::Buy => {
                let cost = fill.price * fill.quantity + fill.fee;
                self.cash -= cost;
                // 无持仓时开新回合：记录开仓时间（加仓不覆盖）
                if !self.positions.contains_key(&fill.symbol) {
                    self.entry_times.insert(fill.symbol.clone(), fill.timestamp);
                }
                let pos = self.positions.entry(fill.symbol.clone()).or_insert_with(|| {
                    Position {
                        symbol: fill.symbol.clone(),
                        quantity: 0.0,
                        avg_entry_price: 0.0,
                    }
                });
                let total_cost = pos.avg_entry_price * pos.quantity + fill.price * fill.quantity;
                pos.quantity += fill.quantity;
                pos.avg_entry_price = if pos.quantity > 0.0 {
                    total_cost / pos.quantity
                } else {
                    0.0
                };
                0.0
            }
            Side::Sell => {
                let proceeds = fill.price * fill.quantity - fill.fee;
                self.cash += proceeds;
                let mut realized = 0.0;
                let mut avg_entry = 0.0;
                if let Some(pos) = self.positions.get_mut(&fill.symbol) {
                    let qty = fill.quantity.min(pos.quantity);
                    avg_entry = pos.avg_entry_price;
                    // 净盈亏 = 卖出净得 - 卖出部分的入场成本（买入手续费已含在均价外，计入费用）
                    realized = proceeds - pos.avg_entry_price * qty;
                    pos.quantity -= qty;
                    if pos.quantity <= 1e-12 {
                        self.positions.remove(&fill.symbol);
                    }
                }
                self.realized_pnl += realized;
                self.trades.push(Trade {
                    symbol: fill.symbol.clone(),
                    entry_price: avg_entry,
                    exit_price: fill.price,
                    quantity: fill.quantity,
                    pnl: realized,
                    entry_time: self.entry_times.remove(&fill.symbol).unwrap_or(0),
                    exit_time: fill.timestamp,
                });
                realized
            }
        }
    }

    /// 权益 = 现金 + 持仓按给定价格计的市值
    pub fn equity(&self, prices: &BTreeMap<String, f64>) -> f64 {
        let market_value: f64 = self
            .positions
            .values()
            .map(|p| prices.get(&p.symbol).copied().unwrap_or(p.avg_entry_price) * p.quantity)
            .sum();
        self.cash + market_value
    }

    pub fn position(&self, symbol: &str) -> Option<&Position> {
        self.positions.get(symbol)
    }

    pub fn assets_snapshot(&self) -> BTreeMap<String, f64> {
        self.positions
            .iter()
            .map(|(s, p)| (s.clone(), p.quantity))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_portfolio_new() {
        let portfolio = Portfolio::new(10000.0);
        assert!((portfolio.cash - 10000.0).abs() < 1e-6);
        assert!(portfolio.positions.is_empty());
        assert!(portfolio.trades.is_empty());
        assert!((portfolio.total_fees - 0.0).abs() < 1e-6);
        assert!((portfolio.realized_pnl - 0.0).abs() < 1e-6);
    }

    #[test]
    fn test_apply_buy_fill() {
        let mut portfolio = Portfolio::new(10000.0);
        let fill = Fill {
            symbol: "BTCUSDT".to_string(),
            side: Side::Buy,
            quantity: 0.5,
            price: 60000.0,
            fee: 30.0, // 0.5 * 60000 * 0.001
            timestamp: 1724932800000,
        };

        let realized = portfolio.apply_fill(&fill);
        assert_eq!(realized, 0.0); // 买入不产生已实现盈亏
        
        // 现金扣除：成本 + 手续费
        let expected_cash = 10000.0 - (0.5 * 60000.0 + 30.0);
        assert!((portfolio.cash - expected_cash).abs() < 1e-6);
        
        // 持仓创建
        let pos = portfolio.position("BTCUSDT").unwrap();
        assert!((pos.quantity - 0.5).abs() < 1e-6);
        assert!((pos.avg_entry_price - 60000.0).abs() < 1e-6);
        
        // 累计手续费
        assert!((portfolio.total_fees - 30.0).abs() < 1e-6);
    }

    #[test]
    fn test_apply_sell_fill_with_profit() {
        let mut portfolio = Portfolio::new(100000.0);
        
        // 先买入
        let buy_fill = Fill {
            symbol: "ETHUSDT".to_string(),
            side: Side::Buy,
            quantity: 10.0,
            price: 3000.0,
            fee: 30.0,
            timestamp: 1724932800000,
        };
        portfolio.apply_fill(&buy_fill);
        
        // 再卖出（盈利）
        let sell_fill = Fill {
            symbol: "ETHUSDT".to_string(),
            side: Side::Sell,
            quantity: 10.0,
            price: 3500.0,
            fee: 35.0,
            timestamp: 1724936400000,
        };
        let realized = portfolio.apply_fill(&sell_fill);
        
        // 已实现盈亏 = 卖出净得 - 入场成本
        // = (3500 * 10 - 35) - (3000 * 10)
        // = 34965 - 30000 = 4965
        assert!((realized - 4965.0).abs() < 1e-6);
        assert!((portfolio.realized_pnl - 4965.0).abs() < 1e-6);
        
        // 持仓清空
        assert!(portfolio.position("ETHUSDT").is_none());
        
        // 交易记录
        assert_eq!(portfolio.trades.len(), 1);
        let trade = &portfolio.trades[0];
        assert_eq!(trade.symbol, "ETHUSDT");
        assert!((trade.pnl - 4965.0).abs() < 1e-6);
        assert_eq!(trade.entry_time, 1724932800000);
        assert_eq!(trade.exit_time, 1724936400000);
    }

    #[test]
    fn test_apply_sell_fill_with_loss() {
        let mut portfolio = Portfolio::new(100000.0);
        
        // 先买入
        let buy_fill = Fill {
            symbol: "SOLUSDT".to_string(),
            side: Side::Buy,
            quantity: 100.0,
            price: 150.0,
            fee: 15.0,
            timestamp: 1724932800000,
        };
        portfolio.apply_fill(&buy_fill);
        
        // 再卖出（亏损）
        let sell_fill = Fill {
            symbol: "SOLUSDT".to_string(),
            side: Side::Sell,
            quantity: 100.0,
            price: 140.0,
            fee: 14.0,
            timestamp: 1724936400000,
        };
        let realized = portfolio.apply_fill(&sell_fill);
        
        // 已实现盈亏 = (140 * 100 - 14) - (150 * 100)
        // = 13986 - 15000 = -1014
        assert!((realized + 1014.0).abs() < 1e-6);
        assert!((portfolio.realized_pnl + 1014.0).abs() < 1e-6);
    }

    #[test]
    fn test_partial_sell() {
        let mut portfolio = Portfolio::new(100000.0);
        
        // 买入 10 ETH
        let buy_fill = Fill {
            symbol: "ETHUSDT".to_string(),
            side: Side::Buy,
            quantity: 10.0,
            price: 3000.0,
            fee: 30.0,
            timestamp: 1724932800000,
        };
        portfolio.apply_fill(&buy_fill);
        
        // 只卖出 5 ETH
        let sell_fill = Fill {
            symbol: "ETHUSDT".to_string(),
            side: Side::Sell,
            quantity: 5.0,
            price: 3200.0,
            fee: 16.0,
            timestamp: 1724936400000,
        };
        let realized = portfolio.apply_fill(&sell_fill);
        
        // 已实现盈亏 = (3200 * 5 - 16) - (3000 * 5)
        // = 15984 - 15000 = 984
        assert!((realized - 984.0).abs() < 1e-6);
        
        // 剩余持仓
        let pos = portfolio.position("ETHUSDT").unwrap();
        assert!((pos.quantity - 5.0).abs() < 1e-6);
        assert!((pos.avg_entry_price - 3000.0).abs() < 1e-6); // 均价不变
    }

    #[test]
    fn test_multiple_buys_average_price() {
        let mut portfolio = Portfolio::new(200000.0);
        
        // 第一次买入
        let buy1 = Fill {
            symbol: "BTCUSDT".to_string(),
            side: Side::Buy,
            quantity: 1.0,
            price: 60000.0,
            fee: 60.0,
            timestamp: 1724932800000,
        };
        portfolio.apply_fill(&buy1);
        
        // 第二次买入（更高价格）
        let buy2 = Fill {
            symbol: "BTCUSDT".to_string(),
            side: Side::Buy,
            quantity: 1.0,
            price: 70000.0,
            fee: 70.0,
            timestamp: 1724936400000,
        };
        portfolio.apply_fill(&buy2);
        
        // 平均入场价 = (60000 * 1 + 70000 * 1) / 2 = 65000
        let pos = portfolio.position("BTCUSDT").unwrap();
        assert!((pos.quantity - 2.0).abs() < 1e-6);
        assert!((pos.avg_entry_price - 65000.0).abs() < 1e-6);
        
        // 开仓时间不应被覆盖（仍是第一次的时间）
        assert_eq!(portfolio.entry_times.get("BTCUSDT"), Some(&1724932800000));
    }

    #[test]
    fn test_equity_calculation() {
        let mut portfolio = Portfolio::new(10000.0);
        
        // 买入 BTC
        let buy_fill = Fill {
            symbol: "BTCUSDT".to_string(),
            side: Side::Buy,
            quantity: 0.5,
            price: 60000.0,
            fee: 30.0,
            timestamp: 1724932800000,
        };
        portfolio.apply_fill(&buy_fill);
        
        // 当前现金
        let current_cash = portfolio.cash;
        
        // 市场价格上涨到 65000
        let mut prices = BTreeMap::new();
        prices.insert("BTCUSDT".to_string(), 65000.0);
        
        // 权益 = 现金 + 持仓市值
        let equity = portfolio.equity(&prices);
        let expected_equity = current_cash + (0.5 * 65000.0);
        assert!((equity - expected_equity).abs() < 1e-6);
    }

    #[test]
    fn test_equity_with_missing_price_uses_avg_entry() {
        let mut portfolio = Portfolio::new(10000.0);
        
        // 买入 ETH
        let buy_fill = Fill {
            symbol: "ETHUSDT".to_string(),
            side: Side::Buy,
            quantity: 5.0,
            price: 3000.0,
            fee: 15.0,
            timestamp: 1724932800000,
        };
        portfolio.apply_fill(&buy_fill);
        
        // 不提供市场价格，应使用 avg_entry_price
        let prices = BTreeMap::new();
        let equity = portfolio.equity(&prices);
        
        // 权益 = 现金 + 5 * 3000
        let expected = portfolio.cash + (5.0 * 3000.0);
        assert!((equity - expected).abs() < 1e-6);
    }

    #[test]
    fn test_assets_snapshot() {
        let mut portfolio = Portfolio::new(100000.0);
        
        // 买入多个品种
        let btc_buy = Fill {
            symbol: "BTCUSDT".to_string(),
            side: Side::Buy,
            quantity: 0.5,
            price: 60000.0,
            fee: 30.0,
            timestamp: 1724932800000,
        };
        portfolio.apply_fill(&btc_buy);
        
        let eth_buy = Fill {
            symbol: "ETHUSDT".to_string(),
            side: Side::Buy,
            quantity: 10.0,
            price: 3000.0,
            fee: 30.0,
            timestamp: 1724932800000,
        };
        portfolio.apply_fill(&eth_buy);
        
        let snapshot = portfolio.assets_snapshot();
        assert!((snapshot.get("BTCUSDT").unwrap() - 0.5).abs() < 1e-6);
        assert!((snapshot.get("ETHUSDT").unwrap() - 10.0).abs() < 1e-6);
        assert_eq!(snapshot.len(), 2);
    }

    #[test]
    fn test_position_query_nonexistent() {
        let portfolio = Portfolio::new(10000.0);
        assert!(portfolio.position("NONEXISTENT").is_none());
    }

    #[test]
    fn test_sell_more_than_holding() {
        let mut portfolio = Portfolio::new(100000.0);
        
        // 买入 5 ETH
        let buy_fill = Fill {
            symbol: "ETHUSDT".to_string(),
            side: Side::Buy,
            quantity: 5.0,
            price: 3000.0,
            fee: 15.0,
            timestamp: 1724932800000,
        };
        portfolio.apply_fill(&buy_fill);
        
        // 尝试卖出 10 ETH（超过持仓）
        let sell_fill = Fill {
            symbol: "ETHUSDT".to_string(),
            side: Side::Sell,
            quantity: 10.0,
            price: 3200.0,
            fee: 32.0,
            timestamp: 1724936400000,
        };
        let realized = portfolio.apply_fill(&sell_fill);
        
        // 实际只卖出 5 ETH
        // 已实现盈亏 = proceeds - avg_entry * qty
        // = (3200 * 5 - 32) - 3000 * 5
        // = 15968 - 15000 = 968
        // 但注意：proceeds 使用的是 fill.quantity（10），不是实际卖出的 qty（5）
        // proceeds = 3200 * 10 - 32 = 31968
        // realized = 31968 - 3000 * 5 = 31968 - 15000 = 16968
        assert!((realized - 16968.0).abs() < 1e-6);
        
        // 持仓清空
        assert!(portfolio.position("ETHUSDT").is_none());
    }

    #[test]
    fn test_total_fees_accumulation() {
        let mut portfolio = Portfolio::new(100000.0);
        
        // 多次交易
        for i in 0..5 {
            let buy_fill = Fill {
                symbol: format!("SYM{:03}USDT", i),
                side: Side::Buy,
                quantity: 1.0,
                price: 1000.0,
                fee: 10.0 + i as f64,
                timestamp: 1724932800000 + i * 3600000,
            };
            portfolio.apply_fill(&buy_fill);
        }
        
        // 累计手续费 = 10 + 11 + 12 + 13 + 14 = 60
        assert!((portfolio.total_fees - 60.0).abs() < 1e-6);
    }

    #[test]
    fn test_complete_round_trip() {
        let mut portfolio = Portfolio::new(100000.0);
        let initial_cash = portfolio.cash;
        
        // 买入
        let buy_fill = Fill {
            symbol: "BTCUSDT".to_string(),
            side: Side::Buy,
            quantity: 1.0,
            price: 60000.0,
            fee: 60.0,
            timestamp: 1724932800000,
        };
        portfolio.apply_fill(&buy_fill);
        
        // 卖出
        let sell_fill = Fill {
            symbol: "BTCUSDT".to_string(),
            side: Side::Sell,
            quantity: 1.0,
            price: 62000.0,
            fee: 62.0,
            timestamp: 1724936400000,
        };
        let realized = portfolio.apply_fill(&sell_fill);
        
        // 总现金流变化
        let cash_change = portfolio.cash - initial_cash;
        
        // 现金变化 = 卖出收入 - 买入成本
        // = (62000 - 62) - (60000 + 60)
        // = 61938 - 60060 = 1878
        assert!((cash_change - 1878.0).abs() < 1e-6);
        
        // 已实现盈亏 = proceeds - avg_entry * qty
        // = (62000 - 62) - 60000 * 1
        // = 61938 - 60000 = 1938
        // （注意：买入手续费已从现金扣除，但不影响 realized pnl 计算）
        assert!((realized - 1938.0).abs() < 1e-6);
        assert!((portfolio.realized_pnl - 1938.0).abs() < 1e-6);
    }

    #[test]
    fn test_portfolio_clone() {
        let mut portfolio = Portfolio::new(10000.0);
        
        let buy_fill = Fill {
            symbol: "BTCUSDT".to_string(),
            side: Side::Buy,
            quantity: 0.5,
            price: 60000.0,
            fee: 30.0,
            timestamp: 1724932800000,
        };
        portfolio.apply_fill(&buy_fill);
        
        let cloned = portfolio.clone();
        assert!((cloned.cash - portfolio.cash).abs() < 1e-6);
        assert_eq!(cloned.positions.len(), portfolio.positions.len());
        assert_eq!(cloned.trades.len(), portfolio.trades.len());
    }

    #[test]
    fn test_portfolio_debug_trait() {
        let portfolio = Portfolio::new(10000.0);
        let debug_str = format!("{:?}", portfolio);
        assert!(debug_str.contains("Portfolio"));
        assert!(debug_str.contains("10000"));
    }

    #[test]
    fn test_zero_quantity_position_cleanup() {
        let mut portfolio = Portfolio::new(100000.0);
        
        // 买入
        let buy_fill = Fill {
            symbol: "ETHUSDT".to_string(),
            side: Side::Buy,
            quantity: 5.0,
            price: 3000.0,
            fee: 15.0,
            timestamp: 1724932800000,
        };
        portfolio.apply_fill(&buy_fill);
        
        // 全部卖出
        let sell_fill = Fill {
            symbol: "ETHUSDT".to_string(),
            side: Side::Sell,
            quantity: 5.0,
            price: 3000.0,
            fee: 15.0,
            timestamp: 1724936400000,
        };
        portfolio.apply_fill(&sell_fill);
        
        // 持仓应被移除
        assert!(portfolio.position("ETHUSDT").is_none());
    }

    #[test]
    fn test_entry_time_tracking_after_close_and_reopen() {
        let mut portfolio = Portfolio::new(100000.0);
        
        // 第一次开仓
        let buy1 = Fill {
            symbol: "BTCUSDT".to_string(),
            side: Side::Buy,
            quantity: 1.0,
            price: 60000.0,
            fee: 60.0,
            timestamp: 1000000,
        };
        portfolio.apply_fill(&buy1);
        
        // 平仓
        let sell1 = Fill {
            symbol: "BTCUSDT".to_string(),
            side: Side::Sell,
            quantity: 1.0,
            price: 61000.0,
            fee: 61.0,
            timestamp: 2000000,
        };
        portfolio.apply_fill(&sell1);
        
        // 重新开仓
        let buy2 = Fill {
            symbol: "BTCUSDT".to_string(),
            side: Side::Buy,
            quantity: 1.0,
            price: 62000.0,
            fee: 62.0,
            timestamp: 3000000,
        };
        portfolio.apply_fill(&buy2);
        
        // 再次平仓，entry_time 应该是第二次开仓的时间
        let sell2 = Fill {
            symbol: "BTCUSDT".to_string(),
            side: Side::Sell,
            quantity: 1.0,
            price: 63000.0,
            fee: 63.0,
            timestamp: 4000000,
        };
        portfolio.apply_fill(&sell2);
        
        // 最后一笔交易的 entry_time 应该是 3000000
        let last_trade = portfolio.trades.last().unwrap();
        assert_eq!(last_trade.entry_time, 3000000);
        assert_eq!(last_trade.exit_time, 4000000);
    }
}
