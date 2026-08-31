//! 成交模型与执行器。
//!
//! 回测与模拟共用 [`FillModel`]：给定参考价，叠加滑点与手续费得到成交回报。
//! 回测时参考价 = 下一根K线开盘价（由引擎保证，杜绝未来函数）；
//! 模拟/实盘时参考价 = 当前市价。

use std::collections::BTreeMap;

use thiserror::Error;

use crate::types::{Fill, Order, Side};

/// 执行错误
#[derive(Debug, Error)]
pub enum ExecError {
    #[error("缺少 {0} 的参考价格")]
    NoPrice(String),
    #[error("卖出数量超过持仓: {0}")]
    InsufficientAsset(String),
    #[error("交易所错误: {0}")]
    Exchange(String),
}

/// 成交模型：滑点 + 手续费
#[derive(Debug, Clone)]
pub struct FillModel {
    /// 滑点比例（单边，如 0.0005 = 0.05%）
    pub slippage_pct: f64,
    /// 手续费率（单边，如 0.001 = 0.1%）
    pub fee_rate: f64,
}

impl FillModel {
    pub fn new(slippage_pct: f64, fee_rate: f64) -> Self {
        Self {
            slippage_pct,
            fee_rate,
        }
    }

    /// 按参考价撮合：买入价格上浮滑点、卖出下浮滑点，手续费按成交额收取。
    /// 纯函数，便于单元测试。
    pub fn fill(&self, order: &Order, ref_price: f64, timestamp: u64) -> Fill {
        let price = match order.side {
            Side::Buy => ref_price * (1.0 + self.slippage_pct),
            Side::Sell => ref_price * (1.0 - self.slippage_pct),
        };
        let fee = price * order.quantity * self.fee_rate;
        Fill {
            symbol: order.symbol.clone(),
            side: order.side,
            quantity: order.quantity,
            price,
            fee,
            timestamp,
        }
    }
}

/// 执行时的账户视图（用于现金/持仓约束削减）
#[derive(Debug, Clone)]
pub struct ExecutionAccount {
    /// 计价资产现金
    pub cash: f64,
    /// 各品种基础资产持仓数量
    pub assets: BTreeMap<String, f64>,
    /// 当前时间戳（ms）
    pub timestamp: u64,
    /// 不足时是否削减到可执行量（false 则直接报错）
    pub allow_partial: bool,
}

/// 执行器：把订单变成成交回报。
///
/// 回测/模拟用同步的 [`FillModelSyncExecutor`]；
/// 实盘执行器（Phase 4）是异步的（网络调用），因此 trait 本身为异步。
#[async_trait::async_trait]
pub trait OrderExecutor {
    async fn execute(
        &mut self,
        order: &Order,
        ref_price: f64,
        account: &ExecutionAccount,
    ) -> Result<Fill, ExecError>;
}

/// 同步执行器：回测与 dry-run 模拟共用。
///
/// - 买入：现金不足时削减数量（或按配置报错）
/// - 卖出：持仓不足时削减数量（或按配置报错）
pub struct FillModelSyncExecutor {
    pub model: FillModel,
}

impl FillModelSyncExecutor {
    pub fn new(model: FillModel) -> Self {
        Self { model }
    }

    pub fn execute_sync(
        &mut self,
        order: &Order,
        ref_price: f64,
        account: &ExecutionAccount,
    ) -> Result<Fill, ExecError> {
        if ref_price <= 0.0 || !ref_price.is_finite() {
            return Err(ExecError::NoPrice(order.symbol.clone()));
        }
        match order.side {
            Side::Buy => {
                let exec_price = ref_price * (1.0 + self.model.slippage_pct);
                let cost_per_unit = exec_price * (1.0 + self.model.fee_rate);
                let max_affordable = account.cash / cost_per_unit;
                let qty = order.quantity.min(max_affordable);
                if qty <= 0.0 {
                    return Err(ExecError::Exchange(format!(
                        "现金不足: 需要约 {:.2}，可用 {:.2}",
                        order.quantity * cost_per_unit,
                        account.cash
                    )));
                }
                if qty < order.quantity && !account.allow_partial {
                    return Err(ExecError::Exchange("现金不足且不允许部分成交".into()));
                }
                let adjusted = Order { quantity: qty, ..order.clone() };
                Ok(self.model.fill(&adjusted, ref_price, account.timestamp))
            }
            Side::Sell => {
                let held = account
                    .assets
                    .get(&order.symbol)
                    .copied()
                    .unwrap_or(0.0);
                let qty = order.quantity.min(held);
                if qty <= 0.0 {
                    return Err(ExecError::InsufficientAsset(order.symbol.clone()));
                }
                if qty < order.quantity && !account.allow_partial {
                    return Err(ExecError::InsufficientAsset(order.symbol.clone()));
                }
                let adjusted = Order { quantity: qty, ..order.clone() };
                Ok(self.model.fill(&adjusted, ref_price, account.timestamp))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fill_model_new() {
        let model = FillModel::new(0.001, 0.0005);
        assert!((model.slippage_pct - 0.001).abs() < 1e-6);
        assert!((model.fee_rate - 0.0005).abs() < 1e-6);
    }

    #[test]
    fn test_fill_buy_with_slippage_and_fee() {
        let model = FillModel::new(0.001, 0.001); // 0.1% 滑点，0.1% 手续费
        
        let order = Order::market_buy("BTCUSDT", 1.0);
        let fill = model.fill(&order, 60000.0, 1724932800000);
        
        // 买入价格上浮滑点：60000 * (1 + 0.001) = 60060
        assert!((fill.price - 60060.0).abs() < 1e-6);
        
        // 手续费 = 成交价 * 数量 * 费率 = 60060 * 1 * 0.001 = 60.06
        assert!((fill.fee - 60.06).abs() < 1e-6);
        
        assert_eq!(fill.side, Side::Buy);
        assert!((fill.quantity - 1.0).abs() < 1e-6);
        assert_eq!(fill.symbol, "BTCUSDT");
    }

    #[test]
    fn test_fill_sell_with_slippage_and_fee() {
        let model = FillModel::new(0.001, 0.001);
        
        let order = Order::market_sell("ETHUSDT", 10.0);
        let fill = model.fill(&order, 3000.0, 1724932800000);
        
        // 卖出价格下浮滑点：3000 * (1 - 0.001) = 2997
        assert!((fill.price - 2997.0).abs() < 1e-6);
        
        // 手续费 = 2997 * 10 * 0.001 = 29.97
        assert!((fill.fee - 29.97).abs() < 1e-6);
        
        assert_eq!(fill.side, Side::Sell);
    }

    #[test]
    fn test_fill_zero_slippage_and_fee() {
        let model = FillModel::new(0.0, 0.0);
        
        let order = Order::market_buy("TEST", 5.0);
        let fill = model.fill(&order, 100.0, 0);
        
        assert!((fill.price - 100.0).abs() < 1e-6);
        assert!((fill.fee - 0.0).abs() < 1e-6);
    }

    #[test]
    fn test_fill_negative_slippage() {
        // 负滑点（理论上不应该出现，但应能处理）
        let model = FillModel::new(-0.001, 0.0);
        
        let buy_order = Order::market_buy("TEST", 1.0);
        let buy_fill = model.fill(&buy_order, 100.0, 0);
        // 买入价格：100 * (1 - 0.001) = 99.9
        assert!((buy_fill.price - 99.9).abs() < 1e-6);
        
        let sell_order = Order::market_sell("TEST", 1.0);
        let sell_fill = model.fill(&sell_order, 100.0, 0);
        // 卖出价格：100 * (1 + 0.001) = 100.1
        assert!((sell_fill.price - 100.1).abs() < 1e-6);
    }

    #[test]
    fn test_exec_error_variants() {
        // NoPrice
        let err = ExecError::NoPrice("BTCUSDT".to_string());
        let msg = format!("{}", err);
        assert!(msg.contains("BTCUSDT"));
        
        // InsufficientAsset
        let err = ExecError::InsufficientAsset("ETHUSDT".to_string());
        let msg = format!("{}", err);
        assert!(msg.contains("ETHUSDT"));
        
        // Exchange
        let err = ExecError::Exchange("网络超时".to_string());
        let msg = format!("{}", err);
        assert!(msg.contains("网络超时"));
    }

    #[test]
    fn test_execution_account_clone() {
        let mut assets = BTreeMap::new();
        assets.insert("BTCUSDT".to_string(), 1.0);
        
        let account = ExecutionAccount {
            cash: 10000.0,
            assets,
            timestamp: 1724932800000,
            allow_partial: true,
        };
        
        let cloned = account.clone();
        assert!((cloned.cash - 10000.0).abs() < 1e-6);
        assert_eq!(cloned.assets.len(), 1);
        assert!(cloned.allow_partial);
    }

    #[test]
    fn test_fill_model_sync_executor_new() {
        let model = FillModel::new(0.001, 0.001);
        let executor = FillModelSyncExecutor::new(model);
        assert!((executor.model.slippage_pct - 0.001).abs() < 1e-6);
    }

    #[test]
    fn test_execute_sync_buy_with_sufficient_cash() {
        let mut executor = FillModelSyncExecutor::new(FillModel::new(0.0, 0.0));
        
        let account = ExecutionAccount {
            cash: 100000.0,
            assets: BTreeMap::new(),
            timestamp: 1724932800000,
            allow_partial: false,
        };
        
        let order = Order::market_buy("BTCUSDT", 1.0);
        let fill = executor.execute_sync(&order, 60000.0, &account).unwrap();
        
        assert!((fill.price - 60000.0).abs() < 1e-6);
        assert!((fill.quantity - 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_execute_sync_buy_with_insufficient_cash_partial_allowed() {
        let mut executor = FillModelSyncExecutor::new(FillModel::new(0.0, 0.0));
        
        let account = ExecutionAccount {
            cash: 50000.0, // 只够买 0.833 BTC
            assets: BTreeMap::new(),
            timestamp: 1724932800000,
            allow_partial: true,
        };
        
        let order = Order::market_buy("BTCUSDT", 1.0);
        let fill = executor.execute_sync(&order, 60000.0, &account).unwrap();
        
        // 应该被削减到可负担的数量
        assert!(fill.quantity < 1.0);
        assert!((fill.quantity - 50000.0 / 60000.0).abs() < 1e-6);
    }

    #[test]
    fn test_execute_sync_buy_with_insufficient_cash_partial_denied() {
        let mut executor = FillModelSyncExecutor::new(FillModel::new(0.0, 0.0));
        
        let account = ExecutionAccount {
            cash: 50000.0,
            assets: BTreeMap::new(),
            timestamp: 1724932800000,
            allow_partial: false,
        };
        
        let order = Order::market_buy("BTCUSDT", 1.0);
        let result = executor.execute_sync(&order, 60000.0, &account);
        
        assert!(result.is_err());
        match result.unwrap_err() {
            ExecError::Exchange(msg) => assert!(msg.contains("现金不足")),
            _ => panic!("Expected Exchange error"),
        }
    }

    #[test]
    fn test_execute_sync_buy_with_zero_cash() {
        let mut executor = FillModelSyncExecutor::new(FillModel::new(0.0, 0.0));
        
        let account = ExecutionAccount {
            cash: 0.0,
            assets: BTreeMap::new(),
            timestamp: 1724932800000,
            allow_partial: true,
        };
        
        let order = Order::market_buy("BTCUSDT", 1.0);
        let result = executor.execute_sync(&order, 60000.0, &account);
        
        assert!(result.is_err());
    }

    #[test]
    fn test_execute_sync_sell_with_sufficient_holding() {
        let mut executor = FillModelSyncExecutor::new(FillModel::new(0.0, 0.0));
        
        let mut assets = BTreeMap::new();
        assets.insert("BTCUSDT".to_string(), 2.0);
        
        let account = ExecutionAccount {
            cash: 0.0,
            assets,
            timestamp: 1724932800000,
            allow_partial: false,
        };
        
        let order = Order::market_sell("BTCUSDT", 1.0);
        let fill = executor.execute_sync(&order, 60000.0, &account).unwrap();
        
        assert!((fill.quantity - 1.0).abs() < 1e-6);
        assert!((fill.price - 60000.0).abs() < 1e-6);
    }

    #[test]
    fn test_execute_sync_sell_with_insufficient_holding_partial_allowed() {
        let mut executor = FillModelSyncExecutor::new(FillModel::new(0.0, 0.0));
        
        let mut assets = BTreeMap::new();
        assets.insert("BTCUSDT".to_string(), 0.5);
        
        let account = ExecutionAccount {
            cash: 0.0,
            assets,
            timestamp: 1724932800000,
            allow_partial: true,
        };
        
        let order = Order::market_sell("BTCUSDT", 1.0);
        let fill = executor.execute_sync(&order, 60000.0, &account).unwrap();
        
        // 应该被削减到实际持仓
        assert!((fill.quantity - 0.5).abs() < 1e-6);
    }

    #[test]
    fn test_execute_sync_sell_with_insufficient_holding_partial_denied() {
        let mut executor = FillModelSyncExecutor::new(FillModel::new(0.0, 0.0));
        
        let mut assets = BTreeMap::new();
        assets.insert("BTCUSDT".to_string(), 0.5);
        
        let account = ExecutionAccount {
            cash: 0.0,
            assets,
            timestamp: 1724932800000,
            allow_partial: false,
        };
        
        let order = Order::market_sell("BTCUSDT", 1.0);
        let result = executor.execute_sync(&order, 60000.0, &account);
        
        assert!(result.is_err());
        match result.unwrap_err() {
            ExecError::InsufficientAsset(sym) => assert_eq!(sym, "BTCUSDT"),
            _ => panic!("Expected InsufficientAsset error"),
        }
    }

    #[test]
    fn test_execute_sync_sell_with_zero_holding() {
        let mut executor = FillModelSyncExecutor::new(FillModel::new(0.0, 0.0));
        
        let account = ExecutionAccount {
            cash: 0.0,
            assets: BTreeMap::new(),
            timestamp: 1724932800000,
            allow_partial: true,
        };
        
        let order = Order::market_sell("BTCUSDT", 1.0);
        let result = executor.execute_sync(&order, 60000.0, &account);
        
        assert!(result.is_err());
    }

    #[test]
    fn test_execute_sync_invalid_ref_price() {
        let mut executor = FillModelSyncExecutor::new(FillModel::new(0.0, 0.0));
        
        let account = ExecutionAccount {
            cash: 100000.0,
            assets: BTreeMap::new(),
            timestamp: 1724932800000,
            allow_partial: false,
        };
        
        let order = Order::market_buy("BTCUSDT", 1.0);
        
        // 零价格
        let result = executor.execute_sync(&order, 0.0, &account);
        assert!(result.is_err());
        
        // 负价格
        let result = executor.execute_sync(&order, -100.0, &account);
        assert!(result.is_err());
        
        // NaN
        let result = executor.execute_sync(&order, f64::NAN, &account);
        assert!(result.is_err());
        
        // Inf
        let result = executor.execute_sync(&order, f64::INFINITY, &account);
        assert!(result.is_err());
    }

    #[test]
    fn test_execute_sync_with_slippage_and_fee_affects_cost() {
        let mut executor = FillModelSyncExecutor::new(FillModel::new(0.001, 0.001));
        
        // 每份成本 = ref_price * (1 + slippage) * (1 + fee_rate)
        // = 60000 * 1.001 * 1.001 = 60120.12
        // 提供足够的现金买 1 份
        let account = ExecutionAccount {
            cash: 70000.0,
            assets: BTreeMap::new(),
            timestamp: 1724932800000,
            allow_partial: false,
        };
        
        let order = Order::market_buy("BTCUSDT", 1.0);
        let fill = executor.execute_sync(&order, 60000.0, &account).unwrap();
        
        // 成交价 = 60000 * 1.001 = 60060
        assert!((fill.price - 60060.0).abs() < 1e-6);
        
        // 应该能完整成交 1 份
        assert!((fill.quantity - 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_execute_sync_nonexistent_symbol_sell() {
        let mut executor = FillModelSyncExecutor::new(FillModel::new(0.0, 0.0));
        
        let account = ExecutionAccount {
            cash: 0.0,
            assets: BTreeMap::new(), // 没有任何持仓
            timestamp: 1724932800000,
            allow_partial: true,
        };
        
        let order = Order::market_sell("NONEXISTENT", 1.0);
        let result = executor.execute_sync(&order, 100.0, &account);
        
        assert!(result.is_err());
    }

    #[test]
    fn test_fill_model_pure_function() {
        // 验证 fill 是纯函数：相同输入产生相同输出
        let model = FillModel::new(0.001, 0.001);
        let order = Order::market_buy("TEST", 1.0);
        
        let fill1 = model.fill(&order, 100.0, 0);
        let fill2 = model.fill(&order, 100.0, 0);
        
        assert!((fill1.price - fill2.price).abs() < 1e-6);
        assert!((fill1.fee - fill2.fee).abs() < 1e-6);
        assert!((fill1.quantity - fill2.quantity).abs() < 1e-6);
    }

    #[test]
    fn test_execution_account_debug_trait() {
        let account = ExecutionAccount {
            cash: 10000.0,
            assets: BTreeMap::new(),
            timestamp: 0,
            allow_partial: false,
        };
        
        let debug_str = format!("{:?}", account);
        assert!(debug_str.contains("ExecutionAccount"));
    }

    #[test]
    fn test_fill_model_debug_trait() {
        let model = FillModel::new(0.001, 0.001);
        let debug_str = format!("{:?}", model);
        assert!(debug_str.contains("FillModel"));
    }

    #[test]
    fn test_exec_error_debug_trait() {
        let err = ExecError::NoPrice("TEST".to_string());
        let debug_str = format!("{:?}", err);
        assert!(debug_str.contains("NoPrice"));
    }
}
