//! 核心数据类型：K线、订单、成交、持仓。

use serde::{Deserialize, Serialize};

/// K线（与常见交易所 kline 接口字段对齐，snake_case JSON）
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Kline {
    /// 开盘时间（毫秒时间戳）
    pub open_time: u64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
    /// 收盘时间（毫秒时间戳）
    pub close_time: u64,
}

/// 订单方向
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Side {
    Buy,
    Sell,
}

/// 订单类型（MVP 只支持市价单）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OrderKind {
    Market,
}

/// 订单：策略产出的交易指令
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Order {
    pub symbol: String,
    pub side: Side,
    /// 基础资产数量（如 BTC 数量，非 USDT 金额）
    pub quantity: f64,
    pub kind: OrderKind,
}

impl Order {
    pub fn market_buy(symbol: impl Into<String>, quantity: f64) -> Self {
        Self {
            symbol: symbol.into(),
            side: Side::Buy,
            quantity,
            kind: OrderKind::Market,
        }
    }

    pub fn market_sell(symbol: impl Into<String>, quantity: f64) -> Self {
        Self {
            symbol: symbol.into(),
            side: Side::Sell,
            quantity,
            kind: OrderKind::Market,
        }
    }
}

/// 成交回报：订单被执行后的结果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fill {
    pub symbol: String,
    pub side: Side,
    /// 实际成交数量（可能被现金/持仓约束削减）
    pub quantity: f64,
    /// 实际成交价（已含滑点调整）
    pub price: f64,
    /// 手续费（以计价资产支付，正数）
    pub fee: f64,
    /// 成交时间戳（ms）
    pub timestamp: u64,
}

/// 持仓（单一品种）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Position {
    pub symbol: String,
    pub quantity: f64,
    /// 加权平均入场价
    pub avg_entry_price: f64,
}

/// 一次完整的买卖回合（用于胜率统计；手续费已计入）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Trade {
    pub symbol: String,
    pub entry_price: f64,
    pub exit_price: f64,
    pub quantity: f64,
    /// 净盈亏 = 卖出净得 - 入场成本：含卖出手续费与滑点；买入手续费从现金直扣、
    /// 不按笔摊销（总收益率/权益曲线不受影响，仍为严格净利润口径）
    pub pnl: f64,
    pub entry_time: u64,
    pub exit_time: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_kline_serialization() {
        let kline = Kline {
            open_time: 1724932800000,
            open: 60000.0,
            high: 61000.0,
            low: 59500.0,
            close: 60500.0,
            volume: 100.5,
            close_time: 1724936399999,
        };

        let json = serde_json::to_string(&kline).unwrap();
        assert!(json.contains("60000"));
        assert!(json.contains("61000"));
        assert!(json.contains("59500"));
        assert!(json.contains("60500"));

        let deserialized: Kline = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.open_time, kline.open_time);
        assert!((deserialized.close - 60500.0).abs() < 1e-6);
    }

    #[test]
    fn test_kline_with_zero_volume() {
        let kline = Kline {
            open_time: 0,
            open: 0.0,
            high: 0.0,
            low: 0.0,
            close: 0.0,
            volume: 0.0,
            close_time: 0,
        };

        let json = serde_json::to_string(&kline).unwrap();
        let deserialized: Kline = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.volume, 0.0);
    }

    #[test]
    fn test_side_serialization() {
        let buy_json = serde_json::to_string(&Side::Buy).unwrap();
        assert_eq!(buy_json, "\"Buy\"");

        let sell_json = serde_json::to_string(&Side::Sell).unwrap();
        assert_eq!(sell_json, "\"Sell\"");

        let buy: Side = serde_json::from_str("\"Buy\"").unwrap();
        assert_eq!(buy, Side::Buy);

        let sell: Side = serde_json::from_str("\"Sell\"").unwrap();
        assert_eq!(sell, Side::Sell);
    }

    #[test]
    fn test_order_kind_serialization() {
        let market_json = serde_json::to_string(&OrderKind::Market).unwrap();
        assert_eq!(market_json, "\"Market\"");

        let kind: OrderKind = serde_json::from_str("\"Market\"").unwrap();
        assert_eq!(kind, OrderKind::Market);
    }

    #[test]
    fn test_order_market_buy() {
        let order = Order::market_buy("BTCUSDT", 0.5);
        assert_eq!(order.symbol, "BTCUSDT");
        assert_eq!(order.side, Side::Buy);
        assert!((order.quantity - 0.5).abs() < 1e-6);
        assert_eq!(order.kind, OrderKind::Market);
    }

    #[test]
    fn test_order_market_sell() {
        let order = Order::market_sell("ETHUSDT", 2.0);
        assert_eq!(order.symbol, "ETHUSDT");
        assert_eq!(order.side, Side::Sell);
        assert!((order.quantity - 2.0).abs() < 1e-6);
        assert_eq!(order.kind, OrderKind::Market);
    }

    #[test]
    fn test_order_with_string_ref() {
        let symbol = String::from("SOLUSDT");
        let order = Order::market_buy(&symbol, 10.0);
        assert_eq!(order.symbol, "SOLUSDT");
    }

    #[test]
    fn test_order_serialization() {
        let order = Order {
            symbol: "BTCUSDT".to_string(),
            side: Side::Buy,
            quantity: 1.5,
            kind: OrderKind::Market,
        };

        let json = serde_json::to_string(&order).unwrap();
        assert!(json.contains("BTCUSDT"));
        assert!(json.contains("Buy"));
        assert!(json.contains("1.5"));

        let deserialized: Order = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.symbol, "BTCUSDT");
        assert_eq!(deserialized.side, Side::Buy);
    }

    #[test]
    fn test_fill_serialization() {
        let fill = Fill {
            symbol: "ETHUSDT".to_string(),
            side: Side::Sell,
            quantity: 5.0,
            price: 3000.0,
            fee: 15.0,
            timestamp: 1724936400000,
        };

        let json = serde_json::to_string(&fill).unwrap();
        assert!(json.contains("ETHUSDT"));
        assert!(json.contains("Sell"));
        assert!(json.contains("3000"));

        let deserialized: Fill = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.symbol, "ETHUSDT");
        assert!((deserialized.price - 3000.0).abs() < 1e-6);
        assert!((deserialized.fee - 15.0).abs() < 1e-6);
    }

    #[test]
    fn test_fill_with_zero_fee() {
        let fill = Fill {
            symbol: "BTCUSDT".to_string(),
            side: Side::Buy,
            quantity: 0.1,
            price: 60000.0,
            fee: 0.0,
            timestamp: 1724932800000,
        };

        let json = serde_json::to_string(&fill).unwrap();
        let deserialized: Fill = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.fee, 0.0);
    }

    #[test]
    fn test_position_serialization() {
        let position = Position {
            symbol: "BTCUSDT".to_string(),
            quantity: 0.5,
            avg_entry_price: 60000.0,
        };

        let json = serde_json::to_string(&position).unwrap();
        assert!(json.contains("BTCUSDT"));
        assert!(json.contains("0.5"));
        assert!(json.contains("60000"));

        let deserialized: Position = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.symbol, "BTCUSDT");
        assert!((deserialized.quantity - 0.5).abs() < 1e-6);
        assert!((deserialized.avg_entry_price - 60000.0).abs() < 1e-6);
    }

    #[test]
    fn test_position_with_zero_quantity() {
        let position = Position {
            symbol: "ETHUSDT".to_string(),
            quantity: 0.0,
            avg_entry_price: 3000.0,
        };

        let json = serde_json::to_string(&position).unwrap();
        let deserialized: Position = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.quantity, 0.0);
    }

    #[test]
    fn test_trade_serialization() {
        let trade = Trade {
            symbol: "BTCUSDT".to_string(),
            entry_price: 60000.0,
            exit_price: 65000.0,
            quantity: 0.5,
            pnl: 2500.0,
            entry_time: 1724932800000,
            exit_time: 1724936400000,
        };

        let json = serde_json::to_string(&trade).unwrap();
        assert!(json.contains("BTCUSDT"));
        assert!(json.contains("60000"));
        assert!(json.contains("65000"));
        assert!(json.contains("2500"));

        let deserialized: Trade = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.symbol, "BTCUSDT");
        assert!((deserialized.pnl - 2500.0).abs() < 1e-6);
        assert_eq!(deserialized.exit_time - deserialized.entry_time, 3600000);
    }

    #[test]
    fn test_trade_negative_pnl() {
        let trade = Trade {
            symbol: "ETHUSDT".to_string(),
            entry_price: 3000.0,
            exit_price: 2800.0,
            quantity: 10.0,
            pnl: -2000.0,
            entry_time: 1724932800000,
            exit_time: 1724936400000,
        };

        let json = serde_json::to_string(&trade).unwrap();
        assert!(json.contains("-2000"));

        let deserialized: Trade = serde_json::from_str(&json).unwrap();
        assert!((deserialized.pnl + 2000.0).abs() < 1e-6); // -(-2000) = 2000
    }

    #[test]
    fn test_trade_with_fees_included() {
        // 验证 pnl 已经包含了手续费和滑点
        let trade = Trade {
            symbol: "SOLUSDT".to_string(),
            entry_price: 150.0,
            exit_price: 155.0,
            quantity: 100.0,
            pnl: 450.0, // (155-150)*100 - 手续费50 = 450
            entry_time: 1724932800000,
            exit_time: 1724936400000,
        };

        let deserialized: Trade = serde_json::from_str(
            &serde_json::to_string(&trade).unwrap()
        ).unwrap();
        assert!((deserialized.pnl - 450.0).abs() < 1e-6);
    }

    #[test]
    fn test_all_types_debug_traits() {
        // 验证所有类型都实现了 Debug trait
        let kline = Kline {
            open_time: 0,
            open: 0.0,
            high: 0.0,
            low: 0.0,
            close: 0.0,
            volume: 0.0,
            close_time: 0,
        };
        let debug_str = format!("{:?}", kline);
        assert!(debug_str.contains("Kline"));

        let side = Side::Buy;
        let debug_str = format!("{:?}", side);
        assert!(debug_str.contains("Buy"));

        let order = Order::market_buy("BTCUSDT", 1.0);
        let debug_str = format!("{:?}", order);
        assert!(debug_str.contains("Order"));
    }

    #[test]
    fn test_order_clone_and_partial_eq() {
        let order1 = Order::market_buy("BTCUSDT", 1.0);
        let order2 = order1.clone();
        assert_eq!(order1, order2);

        let order3 = Order::market_sell("BTCUSDT", 1.0);
        assert_ne!(order1, order3);
    }

    #[test]
    fn test_kline_clone_and_partial_eq() {
        let kline1 = Kline {
            open_time: 1000,
            open: 100.0,
            high: 110.0,
            low: 90.0,
            close: 105.0,
            volume: 50.0,
            close_time: 2000,
        };
        let kline2 = kline1.clone();
        assert_eq!(kline1, kline2);
    }

    #[test]
    fn test_side_copy_semantics() {
        // Side 是 Copy 类型
        let side1 = Side::Buy;
        let side2 = side1; // 隐式复制
        assert_eq!(side1, side2);
    }

    #[test]
    fn test_order_kind_copy_semantics() {
        // OrderKind 是 Copy 类型
        let kind1 = OrderKind::Market;
        let kind2 = kind1; // 隐式复制
        assert_eq!(kind1, kind2);
    }
}
