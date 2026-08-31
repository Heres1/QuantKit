//! 交易所能力抽象（接口先行，实现后补）。

use async_trait::async_trait;

use quantkit_core::executor::ExecError;
use quantkit_core::types::{Fill, Kline, Order};

/// 行情源：拉取K线与最新价
#[async_trait]
pub trait MarketData {
    async fn fetch_klines(
        &self,
        symbol: &str,
        interval: &str,
        limit: u32,
    ) -> Result<Vec<Kline>, ExecError>;

    async fn fetch_last_price(&self, symbol: &str) -> Result<f64, ExecError>;
}

/// 交易通道：下单
#[async_trait]
pub trait Broker {
    /// 下单。`client_order_id`：幂等 ID（Binance clientOrderId）。
    /// 网络超时后客户端凭此 ID 查询恢复真实成交结果，防止重复下单；
    /// 同一笔订单重试必须复用同一 ID（交易所据此去重）。
    async fn place_order(
        &mut self,
        order: &Order,
        client_order_id: Option<&str>,
    ) -> Result<Fill, ExecError>;
}
