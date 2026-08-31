//! Binance WebSocket 客户端（Phase 1）。
//!
//! 功能：
//! - 订阅多品种 K线流（合并流）
//! - 自动重连（指数退避）
//! - 消息解析和验证
//! - 通过 broadcast channel 推送 Kline 数据
//!
//! 设计原则：
//! - 保持与现有 REST 客户端一致的风格
//! - 简单可靠，优先保证稳定性
//! - 作为轮询的可选加速通道，不破坏现有逻辑

use std::time::Duration;

use futures_util::StreamExt;
use serde::Deserialize;
use tokio::sync::broadcast;
use tokio::time::{sleep, timeout};
use tokio_tungstenite::{connect_async, tungstenite::Message};

use quantkit_core::types::Kline;

/// WebSocket 错误
#[derive(Debug, thiserror::Error)]
pub enum WsError {
    #[error("连接失败: {0}")]
    Connection(String),
    #[error("消息解析失败: {0}")]
    Parse(String),
    #[error("超时: {0}")]
    Timeout(String),
}

/// K线 WebSocket 消息结构（单个流格式）
#[derive(Debug, Deserialize, Clone)]
pub struct WsKlineMessage {
    /// 事件类型 (kline)
    #[serde(rename = "e")]
    pub event_type: String,
    /// 事件时间
    #[serde(rename = "E")]
    pub event_time: u64,
    /// 交易对符号
    #[serde(rename = "s")]
    pub symbol: String,
    /// K线数据
    #[serde(rename = "k")]
    pub kline: WsKlineData,
}

/// 合并流消息包装器（combined stream format）
#[derive(Debug, Deserialize, Clone)]
pub struct WsCombinedStreamMessage {
    /// 流名称 (如 "btcusdt@kline_1m")
    pub stream: String,
    /// 实际数据
    pub data: WsKlineMessage,
}

impl WsCombinedStreamMessage {
    /// 转换为 WsKlineMessage
    pub fn into_kline_message(self) -> WsKlineMessage {
        self.data
    }
}

/// WebSocket K线数据结构
#[derive(Debug, Deserialize, Clone)]
pub struct WsKlineData {
    /// K线开始时间
    #[serde(rename = "t")]
    pub open_time: u64,
    /// K线结束时间
    #[serde(rename = "T")]
    pub close_time: u64,
    /// 开盘价
    #[serde(rename = "o")]
    pub open: String,
    /// 最高价
    #[serde(rename = "h")]
    pub high: String,
    /// 最低价
    #[serde(rename = "l")]
    pub low: String,
    /// 收盘价
    #[serde(rename = "c")]
    pub close: String,
    /// 成交量
    #[serde(rename = "v")]
    pub volume: String,
    /// K线是否已完结
    #[serde(rename = "x")]
    pub is_closed: bool,
}

impl WsKlineData {
    /// 转换为 quantkit-core 的 Kline 类型
    pub fn to_kline(&self) -> Result<Kline, WsError> {
        Ok(Kline {
            open_time: self.open_time,
            close_time: self.close_time,
            open: self.open.parse::<f64>().map_err(|e| WsError::Parse(format!("open: {}", e)))?,
            high: self.high.parse::<f64>().map_err(|e| WsError::Parse(format!("high: {}", e)))?,
            low: self.low.parse::<f64>().map_err(|e| WsError::Parse(format!("low: {}", e)))?,
            close: self.close.parse::<f64>().map_err(|e| WsError::Parse(format!("close: {}", e)))?,
            volume: self.volume.parse::<f64>().map_err(|e| WsError::Parse(format!("volume: {}", e)))?,
        })
    }
}

/// WebSocket 客户端配置
#[derive(Debug, Clone)]
pub struct WsConfig {
    /// 重连初始间隔（秒）
    pub reconnect_interval_secs: u64,
    /// 最大重连间隔（秒）
    pub max_reconnect_interval_secs: u64,
    /// 连接超时（秒）
    pub connect_timeout_secs: u64,
    /// 是否只接收已闭合的 K线
    pub only_closed_bars: bool,
}

impl Default for WsConfig {
    fn default() -> Self {
        Self {
            reconnect_interval_secs: 5,
            max_reconnect_interval_secs: 60,
            connect_timeout_secs: 30,
            only_closed_bars: true,
        }
    }
}

/// Binance WebSocket 客户端
///
/// 订阅多品种 K线流，通过 broadcast channel 推送数据。
/// 支持自动重连（指数退避：5s → 10s → 20s → 40s → 60s）。
pub struct BinanceWsClient {
    config: WsConfig,
    sender: broadcast::Sender<(String, Kline)>,  // (symbol, kline)
}

impl BinanceWsClient {
    /// 创建新的 WebSocket 客户端
    ///
    /// # Arguments
    /// * `config` - 客户端配置
    /// * `buffer_size` - broadcast channel 缓冲区大小（建议 1024）
    pub fn new(config: WsConfig, buffer_size: usize) -> Self {
        let (sender, _) = broadcast::channel(buffer_size);
        Self { config, sender }
    }

    /// 获取订阅者（用于消费 K线数据）
    pub fn subscribe(&self) -> broadcast::Receiver<(String, Kline)> {
        self.sender.subscribe()
    }

    /// 启动 WebSocket 连接并持续运行（带自动重连）
    ///
    /// # Arguments
    /// * `symbols` - 要订阅的交易对列表（如 ["BTCUSDT", "ETHUSDT"]）
    /// * `interval` - K线周期（如 "1m", "5m", "1h", "1d"）
    pub async fn run(&self, symbols: &[String], interval: &str) -> Result<(), WsError> {
        log::info!(
            "[ws] 启动 WebSocket: 品种 {:?}, 周期 {}",
            symbols,
            interval
        );

        let streams: Vec<String> = symbols
            .iter()
            .map(|s| format!("{}@kline_{}", s.to_lowercase(), interval))
            .collect();

        // 构建合并流 URL
        let ws_url = if streams.len() == 1 {
            format!("wss://stream.binance.com:9443/ws/{}", streams[0])
        } else {
            format!(
                "wss://stream.binance.com:9443/stream?streams={}",
                streams.join("/")
            )
        };

        log::info!("[ws] 连接地址: {}", ws_url);

        let mut retry_interval = self.config.reconnect_interval_secs;

        loop {
            match self.connect_and_run(&ws_url).await {
                Ok(_) => {
                    log::warn!("[ws] 连接关闭，{}秒后重连", self.config.reconnect_interval_secs);
                    retry_interval = self.config.reconnect_interval_secs;
                    sleep(Duration::from_secs(self.config.reconnect_interval_secs)).await;
                }
                Err(e) => {
                    log::error!("[ws] 连接失败: {}，{}秒后重连", e, retry_interval);
                    sleep(Duration::from_secs(retry_interval)).await;
                    // 指数退避
                    retry_interval = (retry_interval * 2)
                        .min(self.config.max_reconnect_interval_secs);
                }
            }
        }
    }

    /// 连接 WebSocket 并处理消息流
    async fn connect_and_run(&self, ws_url: &str) -> Result<(), WsError> {
        log::info!("[ws] 正在连接...");

        // 带超时的连接
        let connect_future = connect_async(ws_url);
        let (ws_stream, _) = timeout(
            Duration::from_secs(self.config.connect_timeout_secs),
            connect_future,
        )
        .await
        .map_err(|_| WsError::Timeout(format!("连接超时 ({}s)", self.config.connect_timeout_secs)))?
        .map_err(|e| WsError::Connection(format!("{}", e)))?;

        log::info!("[ws] 连接成功，开始接收数据");

        let (_, mut read) = ws_stream.split();

        while let Some(msg_result) = read.next().await {
            match msg_result {
                Ok(msg) => {
                    if let Err(e) = self.handle_message(msg).await {
                        log::warn!("[ws] 消息处理失败: {}", e);
                    }
                }
                Err(e) => {
                    return Err(WsError::Connection(format!("读取失败: {}", e)));
                }
            }
        }

        Err(WsError::Connection("流结束".to_string()))
    }

    /// 处理单条 WebSocket 消息
    async fn handle_message(&self, msg: Message) -> Result<(), WsError> {
        match msg {
            Message::Text(text) => {
                // 尝试解析为合并流格式，失败则尝试单个流格式
                let ws_msg = if let Ok(combined) = serde_json::from_str::<WsCombinedStreamMessage>(&text) {
                    combined.into_kline_message()
                } else {
                    serde_json::from_str::<WsKlineMessage>(&text)
                        .map_err(|e| WsError::Parse(format!("JSON 解析失败: {}, text={}", e, text)))?
                };

                // 检查事件类型
                if ws_msg.event_type != "kline" {
                    return Ok(());
                }

                // 如果只接收已闭合的 K线，跳过未完结的
                if self.config.only_closed_bars && !ws_msg.kline.is_closed {
                    return Ok(());
                }

                // 转换为 Kline
                let kline = ws_msg.kline.to_kline()?;

                // 推送到 broadcast channel
                let symbol = ws_msg.symbol.clone();
                let _ = self.sender.send((symbol.clone(), kline.clone()));

                // 日志（仅首次连接时输出，避免刷屏）
                log::debug!(
                    "[ws] {} {} 收盘 {:.2}",
                    symbol,
                    chrono::DateTime::from_timestamp_millis(kline.open_time as i64)
                        .map(|dt| dt.format("%H:%M:%S").to_string())
                        .unwrap_or_default(),
                    kline.close
                );

                Ok(())
            }
            Message::Ping(_) => {
                // 自动回复 pong（tungstenite 会自动处理）
                Ok(())
            }
            Message::Close(frame) => {
                log::warn!("[ws] 收到关闭帧: {:?}", frame);
                Err(WsError::Connection("服务器主动关闭".to_string()))
            }
            _ => Ok(()),
        }
    }
}

/// 便捷函数：快速启动 WebSocket 客户端
///
/// # Example
/// ```no_run
/// use quantkit_exchanges::binance_ws::{quick_start_ws, WsConfig};
///
/// #[tokio::main]
/// async fn main() {
///     let symbols = vec!["BTCUSDT".to_string(), "ETHUSDT".to_string()];
///     let mut rx = quick_start_ws(&symbols, "1d").await;
///
///     while let Ok((symbol, kline)) = rx.recv().await {
///         println!("{}: {}", symbol, kline.close);
///     }
/// }
/// ```
pub async fn quick_start_ws(
    symbols: &[String],
    interval: &str,
) -> broadcast::Receiver<(String, Kline)> {
    let config = WsConfig::default();
    let client = BinanceWsClient::new(config, 1024);
    let rx = client.subscribe();

    let symbols_vec = symbols.to_vec();
    let interval_str = interval.to_string();

    tokio::spawn(async move {
        let _ = client.run(&symbols_vec, &interval_str).await;
    });

    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_ws_kline_message() {
        let json = r#"{
            "e": "kline",
            "E": 1724932800000,
            "s": "BTCUSDT",
            "k": {
                "t": 1724932800000,
                "T": 1724936399999,
                "o": "60000.00",
                "h": "61000.00",
                "l": "59500.00",
                "c": "60500.00",
                "v": "100.5",
                "x": true
            }
        }"#;

        let msg: WsKlineMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.event_type, "kline");
        assert_eq!(msg.symbol, "BTCUSDT");
        assert_eq!(msg.kline.open, "60000.00");
        assert!(msg.kline.is_closed);

        let kline = msg.kline.to_kline().unwrap();
        assert!((kline.open - 60000.0).abs() < 1e-6);
        assert!((kline.close - 60500.0).abs() < 1e-6);
    }

    #[test]
    fn test_parse_combined_stream_message() {
        // 合并流格式（带 stream 字段）
        let json = r#"{
            "stream": "btcusdt@kline_1m",
            "data": {
                "e": "kline",
                "E": 1724932800000,
                "s": "BTCUSDT",
                "k": {
                    "t": 1724932800000,
                    "T": 1724936399999,
                    "o": "60000.00",
                    "h": "61000.00",
                    "l": "59500.00",
                    "c": "60500.00",
                    "v": "100.5",
                    "x": true
                }
            }
        }"#;

        let msg: WsCombinedStreamMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.stream, "btcusdt@kline_1m");
        
        let kline_msg = msg.into_kline_message();
        assert_eq!(kline_msg.symbol, "BTCUSDT");
        assert_eq!(kline_msg.kline.close, "60500.00");
    }

    #[test]
    fn test_ws_kline_data_to_kline() {
        let ws_data = WsKlineData {
            open_time: 1724932800000,
            close_time: 1724936399999,
            open: "60000.50".to_string(),
            high: "61000.75".to_string(),
            low: "59500.25".to_string(),
            close: "60500.00".to_string(),
            volume: "1234.567".to_string(),
            is_closed: true,
        };

        let kline = ws_data.to_kline().unwrap();
        assert_eq!(kline.open_time, 1724932800000);
        assert_eq!(kline.close_time, 1724936399999);
        assert!((kline.open - 60000.5).abs() < 1e-6);
        assert!((kline.high - 61000.75).abs() < 1e-6);
        assert!((kline.low - 59500.25).abs() < 1e-6);
        assert!((kline.close - 60500.0).abs() < 1e-6);
        assert!((kline.volume - 1234.567).abs() < 1e-6);
    }

    #[test]
    fn test_ws_kline_invalid_numbers() {
        // 无效数字应返回解析错误
        let ws_data = WsKlineData {
            open_time: 1724932800000,
            close_time: 1724936399999,
            open: "invalid".to_string(),
            high: "61000.00".to_string(),
            low: "59500.00".to_string(),
            close: "60500.00".to_string(),
            volume: "100.0".to_string(),
            is_closed: true,
        };

        let result = ws_data.to_kline();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("open"));
    }

    #[test]
    fn test_ws_kline_all_fields_invalid() {
        // 所有字段都无效
        let ws_data = WsKlineData {
            open_time: 0,
            close_time: 0,
            open: "abc".to_string(),
            high: "def".to_string(),
            low: "ghi".to_string(),
            close: "jkl".to_string(),
            volume: "xyz".to_string(),
            is_closed: false,
        };

        let result = ws_data.to_kline();
        assert!(result.is_err());
    }

    #[test]
    fn test_ws_config_default() {
        let config = WsConfig::default();
        assert_eq!(config.reconnect_interval_secs, 5);
        assert_eq!(config.max_reconnect_interval_secs, 60);
        assert_eq!(config.connect_timeout_secs, 30);
        assert!(config.only_closed_bars);
    }

    #[test]
    fn test_ws_config_custom() {
        let config = WsConfig {
            reconnect_interval_secs: 10,
            max_reconnect_interval_secs: 120,
            connect_timeout_secs: 60,
            only_closed_bars: false,
        };
        assert_eq!(config.reconnect_interval_secs, 10);
        assert!(!config.only_closed_bars);
    }

    #[test]
    fn test_build_stream_url_single() {
        // 单个品种使用单流 URL
        let symbols = vec!["BTCUSDT".to_string()];
        let interval = "1m";
        
        let streams: Vec<String> = symbols
            .iter()
            .map(|s| format!("{}@kline_{}", s.to_lowercase(), interval))
            .collect();
        
        let ws_url = if streams.len() == 1 {
            format!("wss://stream.binance.com:9443/ws/{}", streams[0])
        } else {
            format!(
                "wss://stream.binance.com:9443/stream?streams={}",
                streams.join("/")
            )
        };
        
        assert_eq!(ws_url, "wss://stream.binance.com:9443/ws/btcusdt@kline_1m");
    }

    #[test]
    fn test_build_stream_url_multiple() {
        // 多个品种使用合并流 URL
        let symbols = vec!["BTCUSDT".to_string(), "ETHUSDT".to_string()];
        let interval = "5m";
        
        let streams: Vec<String> = symbols
            .iter()
            .map(|s| format!("{}@kline_{}", s.to_lowercase(), interval))
            .collect();
        
        let ws_url = if streams.len() == 1 {
            format!("wss://stream.binance.com:9443/ws/{}", streams[0])
        } else {
            format!(
                "wss://stream.binance.com:9443/stream?streams={}",
                streams.join("/")
            )
        };
        
        assert!(ws_url.contains("btcusdt@kline_5m"));
        assert!(ws_url.contains("ethusdt@kline_5m"));
        assert!(ws_url.contains("/stream?streams="));
    }

    #[test]
    fn test_ws_error_display() {
        let conn_err = WsError::Connection("timeout".to_string());
        assert!(conn_err.to_string().contains("timeout"));
        
        let parse_err = WsError::Parse("invalid JSON".to_string());
        assert!(parse_err.to_string().contains("invalid JSON"));
        
        let timeout_err = WsError::Timeout("30s".to_string());
        assert!(timeout_err.to_string().contains("30s"));
    }

    #[test]
    fn test_parse_non_kline_event() {
        // 非 K线事件（如 trade）应能解析但不处理
        let json = r#"{
            "e": "trade",
            "E": 1724932800000,
            "s": "BTCUSDT",
            "k": {
                "t": 1724932800000,
                "T": 1724936399999,
                "o": "60000.00",
                "h": "61000.00",
                "l": "59500.00",
                "c": "60500.00",
                "v": "100.5",
                "x": true
            }
        }"#;

        let msg: WsKlineMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.event_type, "trade");
        assert_ne!(msg.event_type, "kline");
    }

    #[test]
    fn test_ws_kline_zero_values() {
        // 零值应能正常解析
        let ws_data = WsKlineData {
            open_time: 0,
            close_time: 0,
            open: "0".to_string(),
            high: "0".to_string(),
            low: "0".to_string(),
            close: "0".to_string(),
            volume: "0".to_string(),
            is_closed: false,
        };

        let kline = ws_data.to_kline().unwrap();
        assert_eq!(kline.open_time, 0);
        assert!((kline.open - 0.0).abs() < 1e-6);
        assert!((kline.volume - 0.0).abs() < 1e-6);
    }

    #[test]
    fn test_ws_kline_scientific_notation() {
        // 科学计数法应能正确解析
        let ws_data = WsKlineData {
            open_time: 1724932800000,
            close_time: 1724936399999,
            open: "1e5".to_string(),
            high: "1.1e5".to_string(),
            low: "9.5e4".to_string(),
            close: "1.05e5".to_string(),
            volume: "1.234e3".to_string(),
            is_closed: true,
        };

        let kline = ws_data.to_kline().unwrap();
        assert!((kline.open - 100000.0).abs() < 1e-6);
        assert!((kline.high - 110000.0).abs() < 1e-6);
        assert!((kline.low - 95000.0).abs() < 1e-6);
        assert!((kline.close - 105000.0).abs() < 1e-6);
    }
}
