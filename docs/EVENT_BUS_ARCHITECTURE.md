# QuantKit 事件总线架构

## 概述

QuantKit 事件总线（`QuantKitEventBus`）是一个基于发布-订阅模式的轻量级事件分发系统，用于解耦数据层与策略层，支持多订阅者共享同一数据源。

**设计目标**：
- 统一事件模型：所有数据流通过 `DomainEvent` 表达
- 类型安全过滤：订阅者只接收关心的事件类型
- 容错降级：WebSocket 失败时自动回退到 REST
- 零拷贝缓存：K线数据同时存在于广播通道和内存缓存中

---

## 核心组件

### 1. DomainEvent 枚举

定义系统中所有领域事件的统一数据结构：

```rust
pub enum DomainEvent {
    /// K线更新事件（来自 WebSocket 实时推送）
    KlineUpdate { symbol: String, kline: Kline },

    /// K线批量就绪事件（来自 REST API 初始拉取）
    KlineBatchReady { symbol: String, klines: Vec<Kline> },

    /// 交易信号生成事件
    SignalGenerated { symbol: String, signal: Signal },

    /// 订单提交事件
    OrderPlaced { symbol: String, order: Order },

    /// 订单成交事件
    OrderFilled { symbol: String, order: Order },

    /// 持仓变化事件
    PositionUpdated { symbol: String, position: Position },

    /// 账户余额更新事件
    BalanceUpdated { asset: String, balance: f64 },

    /// 策略状态变更事件
    StrategyStateChanged { strategy_id: String, state: String },

    /// 风控告警事件
    RiskAlert { symbol: String, alert_type: String, message: String },
}
```

### 2. EventHandler Trait

实现此 trait 的对象可以订阅事件总线：

```rust
#[async_trait]
pub trait EventHandler: Send + Sync {
    /// 处理事件
    async fn handle(&self, event: &DomainEvent) -> Result<(), EventBusError>;

    /// 感兴趣的事件类型（用于过滤）
    fn event_types(&self) -> Vec<EventType>;
}
```

**示例实现**：

```rust
struct MyStrategy;

#[async_trait]
impl EventHandler for MyStrategy {
    async fn handle(&self, event: &DomainEvent) -> Result<(), EventBusError> {
        if let DomainEvent::KlineUpdate { symbol, kline } = event {
            println!("收到 {} 的实时K线: {:?}", symbol, kline.close);
            // 执行策略逻辑...
        }
        Ok(())
    }

    fn event_types(&self) -> Vec<EventType> {
        vec![EventType::KlineUpdate] // 只接收 K线更新事件
    }
}
```

### 3. QuantKitEventBus 结构

```rust
pub struct QuantKitEventBus {
    sender: broadcast::Sender<DomainEvent>,
    handlers: Arc<RwLock<HashMap<SubscriptionId, Arc<dyn EventHandler>>>>,
    next_subscription_id: RwLock<SubscriptionId>,
    kline_cache: KlineCache, // Arc<Mutex<BTreeMap<String, Vec<Kline>>>>
}
```

**关键方法**：

| 方法 | 说明 |
|------|------|
| `new(capacity)` | 创建事件总线，设置 channel 容量 |
| `publish(event)` | 发布事件到总线 |
| `subscribe(handler)` | 注册事件处理器，返回订阅 ID |
| `unsubscribe(id)` | 取消订阅 |
| `kline_cache()` | 获取 K线缓存引用 |
| `init_kline_cache_from_rest(...)` | 从 REST API 初始化缓存 |
| `update_kline_cache(symbol, kline)` | 从 WebSocket 更新缓存 |
| `run_dispatcher()` | 启动后台分发器 |

### 4. KlineUpdateHandler

轻量级事件处理器，专门用于响应 `KlineUpdate` 事件：

```rust
pub struct KlineUpdateHandler {
    last_processed_ts: RwLock<HashMap<String, u64>>,
}

impl KlineUpdateHandler {
    pub fn new() -> Self;
    pub fn is_duplicate(&self, symbol: &str, timestamp: u64) -> bool;
    pub fn mark_processed(&self, symbol: &str, timestamp: u64);
}

#[async_trait]
impl EventHandler for KlineUpdateHandler {
    async fn handle(&self, event: &DomainEvent) -> Result<(), EventBusError> {
        if let DomainEvent::KlineUpdate { symbol, kline } = event {
            self.mark_processed(symbol, kline.open_time);
            log::debug!("[events] 📡 处理 {} K线更新", symbol);
        }
        Ok(())
    }

    fn event_types(&self) -> Vec<EventType> {
        vec![EventType::KlineUpdate]
    }
}
```

**使用场景**：
- 监控 K线更新的实时到达情况
- 去重检测：避免同一根 K线被多次处理
- 调试日志：追踪数据流的完整性

**注意**：策略调用仍在主循环中进行，保持同步性和架构稳定性。

---

## 使用流程

### 启动阶段

```rust
// 1. 创建事件总线
let event_bus = Arc::new(QuantKitEventBus::new(1024));

// 2. 初始化 K线缓存（从 REST 拉取历史数据）
event_bus
    .init_kline_cache_from_rest(&client, &symbols, "5m", 2000)
    .await?;

// 3. 注册策略为事件处理器
let strategy = Arc::new(MyStrategy::new());
event_bus.subscribe(strategy.clone());

// 4. 启动后台分发器
tokio::spawn(event_bus.clone().run_dispatcher());
```

### 数据接入阶段

**WebSocket 实时推送**：

```rust
// 后台任务：监听 WebSocket 并发布事件
tokio::spawn(async move {
    while let Ok((symbol, kline)) = ws_rx.recv().await {
        let event = DomainEvent::KlineUpdate {
            symbol: symbol.clone(),
            kline: kline.clone(),
        };
        event_bus.publish(event).await?;
    }
});
```

**REST 批量拉取**：

```rust
// 兜底方案：定期从 REST 拉取
for sym in &symbols {
    let klines = client.fetch_klines_history(sym, "5m", 100, None).await?;
    let event = DomainEvent::KlineBatchReady {
        symbol: sym.clone(),
        klines: klines.clone(),
    };
    event_bus.publish(event).await?;
}
```

### 策略响应阶段

策略作为事件处理器自动响应：

```rust
#[async_trait]
impl EventHandler for MyStrategy {
    async fn handle(&self, event: &DomainEvent) -> Result<(), EventBusError> {
        match event {
            DomainEvent::KlineUpdate { symbol, kline } => {
                // 实时更新指标
                self.update_indicators(symbol, kline);

                // 检查交易信号
                if let Some(signal) = self.calculate_signal(symbol) {
                    let signal_event = DomainEvent::SignalGenerated {
                        symbol: symbol.clone(),
                        signal: signal.clone(),
                    };
                    // 注意：这里不能直接调用 event_bus.publish，需要重新设计
                    // 通常由主循环或专门的信号处理器发布
                }
            }
            _ => {} // 忽略其他事件
        }
        Ok(())
    }

    fn event_types(&self) -> Vec<EventType> {
        vec![EventType::KlineUpdate]
    }
}
```

---

## 性能特性

### 内存占用

| 组件 | 估算大小 |
|------|---------|
| broadcast channel (capacity=1024) | ~512 KB |
| K线缓存 (4品种 × 2000根 × 48字节) | ~384 KB |
| 事件处理器注册表 | <1 KB |
| **总计** | **~900 KB** |

### 延迟对比

| 场景 | REST 轮询 | WebSocket + 事件总线 |
|------|----------|---------------------|
| 数据到达延迟 | 300s（5分钟轮询） | <1s（实时推送） |
| 策略响应延迟 | 300s | <10ms（内存分发） |
| 每日 REST 调用次数 | 3,456 次 | 12 次（仅初始化） |

### CPU 占用

- **空闲时**：事件总线无轮询开销，CPU 占用 ≈ 0%
- **活跃时**：并行分发事件，单个事件处理耗时 <1ms

---

## 容错机制

### 1. WebSocket 断连降级

```rust
if ws_connection_failed() {
    // 自动回退到 REST 轮询
    fallback_to_rest_polling().await;
}
```

### 2. 事件积压处理

当消费者处理速度慢于生产者时，broadcast channel 会丢弃旧事件：

```rust
Err(broadcast::error::RecvError::Lagged(n)) => {
    log::warn!("[events] ⚠️  事件处理滞后，丢弃 {} 条旧事件", n);
    continue;
}
```

### 3. 处理器错误隔离

单个 handler 失败不影响其他订阅者：

```rust
let results = futures_util::future::join_all(futures).await;
for result in results {
    if let Err(e) = result {
        log::error!("[events] ❌ 事件处理失败: {}", e);
        // 继续处理下一个结果
    }
}
```

---

## 与 binance-rust 的对比

| 特性 | QuantKit | binance-rust |
|------|----------|--------------|
| 事件类型数量 | 9 种 | 14 种 |
| 事件元数据 | 无（轻量级） | UUID + 时间戳 + 因果链 |
| 事件溯源 | 不支持 | 支持（StorableEvent） |
| 缓存集成 | 内置 K线缓存 | 分离的缓存模块 |
| 分发方式 | 并行（join_all） | 并行（join_all） |
| 过滤机制 | EventType 枚举 | EventType 枚举 |

**设计取舍**：
- QuantKit 选择更轻量的设计，适合单策略场景
- binance-rust 提供完整的事件溯源能力，适合多策略审计追踪

---

## 未来扩展方向

### 短期（1-2周）

1. **添加事件持久化**：将关键事件写入 SQLite，支持重启后回放
2. **支持事件回放**：从历史记录重建策略状态
3. **增加监控指标**：记录事件吞吐量、处理延迟、丢弃率

### 中期（1个月）

1. **引入事件版本控制**：支持事件结构演进（`version: "1.0"`）
2. **实现事件聚合**：高频事件合并为批量事件（如每秒 100 条 K线更新 → 每秒 1 条聚合事件）
3. **支持分布式部署**：使用 Redis Pub/Sub 替代 tokio broadcast

### 长期（3个月+）

1. **完整 CQRS 架构**：命令查询职责分离，事件驱动的状态机
2. **事件溯源系统**：所有状态变更通过事件重放重建
3. **流式计算引擎**：基于事件流的实时指标计算（类似 Apache Flink）

---

## 最佳实践

### ✅ 推荐做法

1. **细粒度事件类型**：每个业务动作对应一个事件类型
2. **快速处理**：handler 中避免阻塞操作（如网络请求），异步任务用 `tokio::spawn`
3. **明确订阅范围**：只订阅实际需要的事件类型，避免 `EventType::All`
4. **及时取消订阅**：策略停止运行时调用 `unsubscribe`

### ❌ 避免做法

1. **在 handler 中发布新事件**：可能导致循环依赖或无限递归
2. **长时间持有锁**：handler 中不要锁定共享资源超过 10ms
3. **忽略错误**：所有 handler 错误都应记录日志
4. **过度使用事件总线**：简单场景直接用函数调用更高效

---

## 故障排查

### 问题 1：事件未被处理

**症状**：发布事件后，handler 未收到

**排查步骤**：
```rust
// 1. 检查订阅是否成功
let sub_id = event_bus.subscribe(handler.clone());
println!("订阅 ID: {}", sub_id);

// 2. 检查 handler 的 event_types 是否正确
println!("订阅的事件类型: {:?}", handler.event_types());

// 3. 检查分发器是否启动
println!("pending_count: {}", event_bus.pending_count());
```

### 问题 2：事件积压

**症状**：`pending_count()` 持续增长

**解决方案**：
- 增加 channel 容量：`QuantKitEventBus::new(2048)`
- 优化 handler 处理速度（减少 I/O 操作）
- 使用 `try_recv()` 非阻塞读取

### 问题 3：K线缓存不一致

**症状**：缓存中的 K线与交易所实际数据不符

**排查步骤**：
```rust
// 1. 检查 WebSocket 是否正常接收
println!("[ws] 最后更新时间: {:?}", last_ws_timestamp);

// 2. 对比 REST 数据
let rest_klines = client.fetch_klines_history("BTCUSDT", "5m", 1, None).await?;
let cache_klines = event_bus.kline_cache().lock();
println!("REST: {:?}, Cache: {:?}", rest_klines[0], cache_klines["BTCUSDT"].last());
```

---

## 参考资料

- [Tokio Broadcast Channel 文档](https://docs.rs/tokio/latest/tokio/sync/broadcast/index.html)
- [binance-rust 事件总线实现](../binance-rust/src/event_bus.rs)
- [事件驱动架构模式](https://martinfowler.com/articles/201701-event-driven.html)
