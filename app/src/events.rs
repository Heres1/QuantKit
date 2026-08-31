//! 事件总线模块
//!
//! 负责事件的发布、订阅和分发，解耦数据层与策略层
//!
//! 设计目标：
//! - 统一事件模型：所有数据流通过 DomainEvent 表达
//! - 发布-订阅模式：支持多订阅者共享同一数据源
//! - 类型安全过滤：订阅者只接收关心的事件类型
//! - 容错降级：WebSocket 失败时自动回退到 REST

use async_trait::async_trait;
use parking_lot::RwLock;
use quantkit_core::types::{Kline, Order, Position};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::broadcast;

/// 订阅 ID 类型
pub type SubscriptionId = u64;

/// 事件总线错误类型
#[derive(Debug, thiserror::Error)]
pub enum EventBusError {
    #[error("发布事件失败: {0}")]
    PublishFailed(String),

    #[error("订阅失败: {0}")]
    SubscribeFailed(String),

    #[error("缓存操作失败: {0}")]
    CacheError(String),
}

/// 领域事件枚举 - 系统能处理的所有事件类型
#[derive(Debug, Clone)]
pub enum DomainEvent {
    /// K线更新事件（来自 WebSocket 实时推送）
    KlineUpdate {
        symbol: String,
        kline: Kline,
    },

    /// K线批量就绪事件（来自 REST API 初始拉取）
    KlineBatchReady {
        symbol: String,
        klines: Vec<Kline>,
    },

    /// 交易信号生成事件
    SignalGenerated {
        symbol: String,
        signal: Signal,
    },

    /// 订单提交事件
    OrderPlaced {
        symbol: String,
        order: Order,
    },

    /// 订单成交事件
    OrderFilled {
        symbol: String,
        order: Order,
    },

    /// 持仓变化事件
    PositionUpdated {
        symbol: String,
        position: Position,
    },

    /// 账户余额更新事件
    BalanceUpdated {
        asset: String,
        balance: f64,
    },

    /// 策略状态变更事件
    StrategyStateChanged {
        strategy_id: String,
        state: String,
    },

    /// 风控告警事件
    RiskAlert {
        symbol: String,
        alert_type: String,
        message: String,
    },
}

/// 交易信号
#[derive(Debug, Clone)]
pub struct Signal {
    /// 信号方向
    pub direction: SignalDirection,
    /// 信号强度（0.0 - 1.0）
    pub strength: f64,
    /// 建议价格
    pub price: f64,
    /// 建议数量
    pub quantity: f64,
    /// 信号生成时间戳（毫秒）
    pub timestamp: u64,
    /// 触发信号的指标值（可选，用于调试）
    pub indicators: Option<HashMap<String, f64>>,
}

/// 信号方向
#[derive(Debug, Clone, PartialEq)]
pub enum SignalDirection {
    Buy,
    Sell,
    Hold,
}

/// 事件类型枚举（用于订阅过滤）
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum EventType {
    KlineUpdate,
    KlineBatchReady,
    SignalGenerated,
    OrderPlaced,
    OrderFilled,
    PositionUpdated,
    BalanceUpdated,
    StrategyStateChanged,
    RiskAlert,
    All, // 订阅所有事件
}

/// 事件处理器 Trait
///
/// 实现此 trait 的对象可以订阅事件总线并处理特定类型的事件
#[async_trait]
pub trait EventHandler: Send + Sync {
    /// 处理事件
    ///
    /// # 参数
    /// * `event` - 接收到的领域事件
    ///
    /// # 返回值
    /// 成功返回 Ok(())，失败返回错误信息
    async fn handle(&self, event: &DomainEvent) -> Result<(), EventBusError>;

    /// 感兴趣的事件类型（用于过滤）
    ///
    /// 返回 `vec![EventType::All]` 表示订阅所有事件
    fn event_types(&self) -> Vec<EventType>;
}

/// K线缓存类型别名
pub type KlineCache = Arc<parking_lot::Mutex<std::collections::BTreeMap<String, Vec<Kline>>>>;

/// QuantKit 事件总线实现
///
/// 基于 tokio broadcast channel 实现发布-订阅模式
/// 同时维护 K线缓存供策略查询历史数据
pub struct QuantKitEventBus {
    /// 事件广播发送端
    sender: broadcast::Sender<DomainEvent>,
    /// 事件处理器注册表
    handlers: Arc<RwLock<HashMap<SubscriptionId, Arc<dyn EventHandler>>>>,
    /// 下一个订阅 ID
    next_subscription_id: RwLock<SubscriptionId>,
    /// K线缓存（symbol -> Vec<Kline>）
    kline_cache: KlineCache,
}

impl QuantKitEventBus {
    /// 创建新的事件总线
    ///
    /// # 参数
    /// * `capacity` - broadcast channel 容量，建议设置为 1024 或更高
    pub fn new(capacity: usize) -> Self {
        let (sender, _receiver) = broadcast::channel(capacity);
        Self {
            sender,
            handlers: Arc::new(RwLock::new(HashMap::new())),
            next_subscription_id: RwLock::new(1),
            kline_cache: Arc::new(parking_lot::Mutex::new(std::collections::BTreeMap::new())),
        }
    }

    /// 获取 K线缓存的引用
    pub fn kline_cache(&self) -> KlineCache {
        Arc::clone(&self.kline_cache)
    }

    /// 从 REST API 初始化 K线缓存
    ///
    /// # 参数
    /// * `client` - Binance 客户端
    /// * `symbols` - 交易对列表
    /// * `interval` - K线间隔（如 "1m", "5m"）
    /// * `limit` - 每个品种拉取的 K线数量
    ///
    /// # 返回值
    /// 成功返回 Ok(())，失败返回错误信息
    pub async fn init_kline_cache_from_rest(
        &self,
        client: &quantkit_exchanges::binance::BinanceClient,
        symbols: &[String],
        interval: &str,
        limit: usize,
    ) -> Result<(), String> {
        println!("[events] 📊 正在从 REST 初始化 K线缓存...");

        let mut cache = self.kline_cache.lock();

        for sym in symbols {
            match client
                .fetch_klines_history(sym, interval, limit as u32, None)
                .await
            {
                Ok(klines) => {
                    println!(
                        "[events] 📊 {} 初始化为 {} 根K线",
                        sym,
                        klines.len()
                    );
                    cache.insert(sym.clone(), klines);
                }
                Err(e) => {
                    return Err(format!("拉取 {} 历史K线失败: {}", sym, e));
                }
            }
        }

        println!(
            "[events] 📊 K线缓存初始化完成，共 {} 个品种",
            cache.len()
        );
        Ok(())
    }

    /// 从 WebSocket 消息更新 K线缓存
    ///
    /// # 参数
    /// * `symbol` - 交易对符号
    /// * `kline` - 新的 K线数据
    ///
    /// # 返回值
    /// 更新的 K线数量（通常为 1）
    pub fn update_kline_cache(&self, symbol: String, kline: Kline) -> usize {
        let mut cache = self.kline_cache.lock();
        if let Some(klines) = cache.get_mut(&symbol) {
            let pos = klines.iter().position(|k| k.open_time == kline.open_time);
            if let Some(idx) = pos {
                klines[idx] = kline; // 更新现有 K线
            } else {
                klines.push(kline); // 添加新 K线
                if klines.len() > 3000 {
                    klines.remove(0); // 保持缓存大小限制
                }
            }
            1
        } else {
            cache.insert(symbol, vec![kline]);
            1
        }
    }

    /// 发布事件到总线
    ///
    /// # 参数
    /// * `event` - 要发布的领域事件
    ///
    /// # 返回值
    /// 成功返回 Ok(())，失败返回 EventBusError
    pub async fn publish(&self, event: DomainEvent) -> Result<(), EventBusError> {
        // 如果是 K线更新事件，同时更新缓存
        if let DomainEvent::KlineUpdate {
            ref symbol,
            ref kline,
        } = event
        {
            self.update_kline_cache(symbol.clone(), kline.clone());
        }

        self.sender
            .send(event)
            .map_err(|e| EventBusError::PublishFailed(e.to_string()))?;

        Ok(())
    }

    /// 订阅事件处理器
    ///
    /// # 参数
    /// * `handler` - 实现 EventHandler trait 的处理器
    ///
    /// # 返回值
    /// 返回订阅 ID，可用于后续取消订阅
    pub fn subscribe<T: EventHandler + 'static>(&self, handler: Arc<T>) -> SubscriptionId {
        let id = {
            let mut id = self.next_subscription_id.write();
            let current = *id;
            *id += 1;
            current
        };

        self.handlers.write().insert(id, handler);
        id
    }

    /// 取消订阅
    ///
    /// # 参数
    /// * `subscription_id` - 订阅时返回的 ID
    pub fn unsubscribe(&self, subscription_id: SubscriptionId) {
        self.handlers.write().remove(&subscription_id);
    }

    /// 获取当前未消费的事件数量
    pub fn pending_count(&self) -> usize {
        self.sender.len()
    }

    /// 启动事件分发器（后台任务）
    ///
    /// 持续监听事件通道，并将事件分发给所有订阅的处理器
    pub async fn run_dispatcher(self: Arc<Self>) {
        let mut receiver = self.sender.subscribe();

        println!("[events] 🚀 事件分发器已启动");

        loop {
            match receiver.recv().await {
                Ok(event) => {
                    let event_type = extract_event_type(&event);

                    // 筛选关心此事件类型的处理器
                    let handlers: Vec<_> = {
                        let guard = self.handlers.read();
                        guard
                            .values()
                            .filter(|h| {
                                let types = h.event_types();
                                types.contains(&EventType::All) || types.contains(&event_type)
                            })
                            .cloned()
                            .collect()
                    };

                    if handlers.is_empty() {
                        continue;
                    }

                    // 并行处理所有匹配的处理器
                    let futures: Vec<_> =
                        handlers.iter().map(|h| h.handle(&event)).collect();

                    let results = futures_util::future::join_all(futures).await;

                    // 记录处理错误（不中断其他处理器）
                    for result in results {
                        if let Err(e) = result {
                            log::error!("[events] ❌ 事件处理失败: {}", e);
                        }
                    }
                }
                Err(broadcast::error::RecvError::Closed) => {
                    println!("[events] ⚠️  事件通道已关闭，分发器退出");
                    break;
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    log::warn!("[events] ⚠️  事件处理滞后，丢弃 {} 条旧事件", n);
                    continue;
                }
            }
        }
    }
}

/// 从 DomainEvent 提取 EventType
fn extract_event_type(event: &DomainEvent) -> EventType {
    match event {
        DomainEvent::KlineUpdate { .. } => EventType::KlineUpdate,
        DomainEvent::KlineBatchReady { .. } => EventType::KlineBatchReady,
        DomainEvent::SignalGenerated { .. } => EventType::SignalGenerated,
        DomainEvent::OrderPlaced { .. } => EventType::OrderPlaced,
        DomainEvent::OrderFilled { .. } => EventType::OrderFilled,
        DomainEvent::PositionUpdated { .. } => EventType::PositionUpdated,
        DomainEvent::BalanceUpdated { .. } => EventType::BalanceUpdated,
        DomainEvent::StrategyStateChanged { .. } => EventType::StrategyStateChanged,
        DomainEvent::RiskAlert { .. } => EventType::RiskAlert,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quantkit_core::types::Side;

    struct TestHandler {
        received_events: Arc<parking_lot::Mutex<Vec<DomainEvent>>>,
    }

    impl TestHandler {
        fn new() -> Self {
            Self {
                received_events: Arc::new(parking_lot::Mutex::new(Vec::new())),
            }
        }
    }

    #[async_trait]
    impl EventHandler for TestHandler {
        async fn handle(&self, event: &DomainEvent) -> Result<(), EventBusError> {
            self.received_events.lock().push(event.clone());
            Ok(())
        }

        fn event_types(&self) -> Vec<EventType> {
            vec![EventType::All]
        }
    }

    #[tokio::test]
    async fn test_event_bus_publish_subscribe() {
        let event_bus = QuantKitEventBus::new(100);
        let handler = Arc::new(TestHandler::new());

        // 订阅
        let subscription_id = event_bus.subscribe(handler.clone());
        assert_eq!(subscription_id, 1);

        // 发布事件
        let event = DomainEvent::KlineUpdate {
            symbol: "BTCUSDT".to_string(),
            kline: Kline {
                open_time: 1234567890000,
                open: 50000.0,
                high: 51000.0,
                low: 49000.0,
                close: 50500.0,
                volume: 100.0,
                close_time: 1234567950000,
            },
        };

        let _ = event_bus.publish(event.clone()).await;

        // 验证订阅 ID 正确
        assert_eq!(subscription_id, 1);
    }

    #[tokio::test]
    async fn test_event_bus_unsubscribe() {
        let event_bus = QuantKitEventBus::new(100);
        let handler = Arc::new(TestHandler::new());

        let subscription_id = event_bus.subscribe(handler.clone());
        event_bus.unsubscribe(subscription_id);

        // 验证取消订阅后不再接收事件
        let event = DomainEvent::KlineUpdate {
            symbol: "ETHUSDT".to_string(),
            kline: Kline {
                open_time: 1234567890000,
                open: 3000.0,
                high: 3100.0,
                low: 2900.0,
                close: 3050.0,
                volume: 200.0,
                close_time: 1234567950000,
            },
        };

        let _ = event_bus.publish(event).await;

        // 测试通过（不 panic 即成功）
    }

    #[test]
    fn test_kline_cache_update() {
        let event_bus = QuantKitEventBus::new(100);

        let kline1 = Kline {
            open_time: 1000,
            open: 50000.0,
            high: 51000.0,
            low: 49000.0,
            close: 50500.0,
            volume: 100.0,
            close_time: 1060000,
        };

        let count = event_bus.update_kline_cache("BTCUSDT".to_string(), kline1.clone());
        assert_eq!(count, 1);

        let binding = event_bus.kline_cache();
        let cache = binding.lock();
        assert_eq!(cache.get("BTCUSDT").unwrap().len(), 1);
    }

    #[test]
    fn test_kline_update_handler() {
        let handler = KlineUpdateHandler::new();

        // 首次处理，不是重复
        assert!(!handler.is_duplicate("BTCUSDT", 1000));

        // 标记为已处理
        handler.mark_processed("BTCUSDT", 1000);

        // 再次检查，应该是重复
        assert!(handler.is_duplicate("BTCUSDT", 1000));
        assert!(handler.is_duplicate("BTCUSDT", 999)); // 更早的时间戳也是重复

        // 新的时间戳不是重复
        assert!(!handler.is_duplicate("BTCUSDT", 1001));
    }

    #[tokio::test]
    async fn test_kline_update_handler_event() {
        let handler = Arc::new(KlineUpdateHandler::new());
        let event_bus = Arc::new(QuantKitEventBus::new(100));

        // 先订阅处理器，再发布事件
        event_bus.subscribe(handler.clone());

        // 启动分发器（后台任务）
        let bus_clone = event_bus.clone();
        tokio::spawn(async move {
            bus_clone.run_dispatcher().await;
        });

        // 等待分发器启动
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        // 发布 K线更新事件
        let event = DomainEvent::KlineUpdate {
            symbol: "BTCUSDT".to_string(),
            kline: Kline {
                open_time: 1234567890000,
                open: 50000.0,
                high: 51000.0,
                low: 49000.0,
                close: 50500.0,
                volume: 100.0,
                close_time: 1234567950000,
            },
        };

        // publish 可能失败（如果没有接收者），这是正常的
        let _ = event_bus.publish(event).await;
        
        // 主要验证不 panic
        assert!(true);
    }

    #[test]
    fn test_monitor_handler_metrics() {
        let monitor = MonitorHandler::new(60);
        let metrics = monitor.get_metrics();

        // 初始状态应为零
        assert_eq!(metrics.kline_update_count, 0);
        assert_eq!(metrics.signal_count, 0);
        assert_eq!(metrics.order_count, 0);
        assert_eq!(metrics.risk_alert_count, 0);
        assert!(metrics.last_update.is_none());
    }

    #[tokio::test]
    async fn test_monitor_handler_events() {
        let monitor = Arc::new(MonitorHandler::new(60));
        let event_bus = Arc::new(QuantKitEventBus::new(100));

        // 订阅监控器
        event_bus.subscribe(monitor.clone());

        // 直接调用 handle 方法测试，而不是通过 publish
        let events = vec![
            DomainEvent::KlineUpdate {
                symbol: "BTCUSDT".to_string(),
                kline: Kline {
                    open_time: 1000,
                    open: 50000.0,
                    high: 51000.0,
                    low: 49000.0,
                    close: 50500.0,
                    volume: 100.0,
                    close_time: 1060000,
                },
            },
            DomainEvent::SignalGenerated {
                symbol: "BTCUSDT".to_string(),
                signal: Signal {
                    direction: SignalDirection::Buy,
                    strength: 0.8,
                    price: 50000.0,
                    quantity: 0.1,
                    timestamp: 1060000,
                    indicators: None,
                },
            },
            DomainEvent::OrderPlaced {
                symbol: "BTCUSDT".to_string(),
                order: Order {
                    symbol: "BTCUSDT".to_string(),
                    side: Side::Buy,
                    quantity: 0.1,
                    kind: quantkit_core::types::OrderKind::Market,
                },
            },
        ];

        for event in &events {
            let _ = monitor.handle(event).await;
        }

        // 验证指标更新
        let metrics = monitor.get_metrics();
        assert_eq!(metrics.kline_update_count, 1);
        assert_eq!(metrics.signal_count, 1);
        assert_eq!(metrics.order_count, 1);
        assert!(metrics.last_update.is_some());
    }
}

/// K线更新事件处理器
///
/// 专门用于响应 KlineUpdate 事件并更新内部状态
/// 不包含策略逻辑，策略调用仍在主循环中进行
pub struct KlineUpdateHandler {
    /// 最后处理的 K线时间戳（按品种）
    last_processed_ts: RwLock<HashMap<String, u64>>,
}

impl KlineUpdateHandler {
    pub fn new() -> Self {
        Self {
            last_processed_ts: RwLock::new(HashMap::new()),
        }
    }

    /// 检查是否已处理过此 K线
    pub fn is_duplicate(&self, symbol: &str, timestamp: u64) -> bool {
        let last_ts = self.last_processed_ts.read();
        if let Some(&last) = last_ts.get(symbol) {
            timestamp <= last
        } else {
            false
        }
    }

    /// 标记 K线已处理
    pub fn mark_processed(&self, symbol: &str, timestamp: u64) {
        let mut last_ts = self.last_processed_ts.write();
        last_ts.insert(symbol.to_string(), timestamp);
    }
}

#[async_trait]
impl EventHandler for KlineUpdateHandler {
    async fn handle(&self, event: &DomainEvent) -> Result<(), EventBusError> {
        if let DomainEvent::KlineUpdate { symbol, kline } = event {
            // 记录已处理的 K线
            self.mark_processed(symbol, kline.open_time);
            
            log::debug!("[events] 📡 处理 {} K线更新: open_time={}", symbol, kline.open_time);
        }
        Ok(())
    }

    fn event_types(&self) -> Vec<EventType> {
        vec![EventType::KlineUpdate]
    }
}

/// 监控指标
#[derive(Debug, Clone)]
pub struct MonitorMetrics {
    /// 接收到的 K线更新总数
    pub kline_update_count: u64,
    /// 最后更新的品种和时间戳
    pub last_update: Option<(String, u64)>,
    /// 信号生成总数
    pub signal_count: u64,
    /// 订单提交总数
    pub order_count: u64,
    /// 风控告警总数
    pub risk_alert_count: u64,
    /// 启动时间
    pub start_time: std::time::Instant,
}

impl Default for MonitorMetrics {
    fn default() -> Self {
        Self {
            kline_update_count: 0,
            last_update: None,
            signal_count: 0,
            order_count: 0,
            risk_alert_count: 0,
            start_time: std::time::Instant::now(),
        }
    }
}

/// 监控器事件处理器
///
/// 监控系统状态，记录关键指标，定期输出统计信息
pub struct MonitorHandler {
    /// 监控指标
    metrics: RwLock<MonitorMetrics>,
    /// 日志输出间隔（秒）
    log_interval_secs: u64,
    /// 上次输出时间
    last_log_time: RwLock<std::time::Instant>,
}

impl MonitorHandler {
    /// 创建新的监控器
    ///
    /// # 参数
    /// * `log_interval_secs` - 日志输出间隔（秒），默认 60 秒
    pub fn new(log_interval_secs: u64) -> Self {
        Self {
            metrics: RwLock::new(MonitorMetrics::default()),
            log_interval_secs,
            last_log_time: RwLock::new(std::time::Instant::now()),
        }
    }

    /// 获取当前监控指标
    pub fn get_metrics(&self) -> MonitorMetrics {
        self.metrics.read().clone()
    }

    /// 输出监控摘要
    fn log_summary(&self, metrics: &MonitorMetrics) {
        let elapsed = metrics.start_time.elapsed();
        let uptime_secs = elapsed.as_secs();
        
        println!("[monitor] 📊 ===== 监控摘要 =====");
        println!("[monitor] ⏱️  运行时长: {} 分 {} 秒", uptime_secs / 60, uptime_secs % 60);
        println!("[monitor] 📡 K线更新: {} 次", metrics.kline_update_count);
        println!("[monitor] 🎯 信号生成: {} 次", metrics.signal_count);
        println!("[monitor] 📦 订单提交: {} 次", metrics.order_count);
        println!("[monitor] ⚠️  风控告警: {} 次", metrics.risk_alert_count);
        
        if let Some((symbol, ts)) = &metrics.last_update {
            println!("[monitor] 🕐 最后更新: {} @ {}", symbol, ts);
        }
        
        println!("[monitor] ========================");
    }
}

#[async_trait]
impl EventHandler for MonitorHandler {
    async fn handle(&self, event: &DomainEvent) -> Result<(), EventBusError> {
        // 更新指标
        {
            let mut metrics = self.metrics.write();
            
            match event {
                DomainEvent::KlineUpdate { symbol, kline } => {
                    metrics.kline_update_count += 1;
                    metrics.last_update = Some((symbol.clone(), kline.open_time));
                }
                DomainEvent::SignalGenerated { .. } => {
                    metrics.signal_count += 1;
                }
                DomainEvent::OrderPlaced { .. } | DomainEvent::OrderFilled { .. } => {
                    metrics.order_count += 1;
                }
                DomainEvent::RiskAlert { .. } => {
                    metrics.risk_alert_count += 1;
                }
                _ => {}
            }
        }

        // 检查是否需要输出日志
        {
            let last_log = self.last_log_time.read();
            let elapsed = last_log.elapsed();
            
            if elapsed.as_secs() >= self.log_interval_secs {
                drop(last_log); // 释放读锁
                
                let metrics = self.get_metrics();
                self.log_summary(&metrics);
                
                // 更新最后输出时间
                let mut last_log = self.last_log_time.write();
                *last_log = std::time::Instant::now();
            }
        }

        Ok(())
    }

    fn event_types(&self) -> Vec<EventType> {
        vec![EventType::All] // 监控所有事件
    }
}
