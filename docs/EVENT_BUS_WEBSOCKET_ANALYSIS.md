# QuantKit 引入事件总线和 WebSocket 可行性分析

## 1. 当前架构痛点分析

### 1.1 数据获取方式（轮询模式）

**现状**:
```rust
// app/src/live.rs - run_cycle()
async fn run_cycle(...) -> Result<(), String> {
    // 每 poll_secs (默认300秒) 执行一次：
    for sym in &cfg.symbols {
        let ks = client.fetch_klines_history(sym, "1d", 2000, None).await?;
        data.insert(sym.clone(), ks);
    }
    
    // 找到最新对齐的时间戳
    let last_bar_ts = data.values()...max().unwrap_or(0);
    
    // 同bar不重复决策
    if st.last_bar_ts == last_bar_ts {
        return Ok(());  // 静默跳过
    }
}
```

**问题**:
- ❌ **延迟高**: 即使 bar 已收盘，也要等下一轮轮询（最多等待 poll_secs）
- ❌ **带宽浪费**: 每次全量拉取 2000 根K线，大部分是重复数据
- ❌ **无法响应突发事件**: 盘中价格剧烈波动时，系统毫不知情
- ❌ **多品种不同步**: 各品种K线到达时间不一致，需要对齐逻辑

### 1.2 策略执行流程

**现状**:
```
轮询触发 → 拉取全量K线 → 重放回测引擎 → 生成目标持仓 → 差量调仓
   ↑                                                              |
   └──────────────────── sleep(poll_secs) ←──────────────────────┘
```

**特点**:
- ✅ 简单可靠，易于调试
- ✅ 天然无竞态条件
- ❌ 被动等待，实时性差
- ❌ 无法支持日内策略（分钟级信号）

---

## 2. 引入 WebSocket 的价值评估

### 2.1 收益分析

#### 📈 实时性提升

| 指标 | 轮询模式 | WebSocket 模式 | 提升 |
|------|----------|----------------|------|
| **K线更新延迟** | 最多 poll_secs (300s) | < 1s | **300x** |
| **成交通知** | 下次轮询发现 | 实时推送 | **即时** |
| **余额变化** | 下次轮询发现 | 实时推送 | **即时** |
| **网络带宽** | 每次 ~500KB (全量) | 每次 ~100B (增量) | **5000x** |

#### 🎯 新能力解锁

1. **日内策略支持**
   - 可以基于 1分钟/5分钟/15分钟 K线交易
   - 当前架构只适合日线/小时线

2. **止损止盈实时监控**
   - WebSocket 推送订单成交后立即检查
   - 无需等待下一轮轮询

3. **异常行情告警**
   - 价格突然暴跌/暴涨时立即通知
   - 自动触发风控措施

4. **多品种联动交易**
   - 实时监控相关性品种的价格变化
   - 套利策略、配对交易成为可能

### 2.2 成本分析

#### ⚠️ 复杂度增加

**代码层面**:
- 需要维护 WebSocket 连接池
- 处理断线重连逻辑
- 消息解析和验证
- 并发控制（多个订阅流）

**运维层面**:
- 需要监控连接健康状态
- 处理 Binance WebSocket 限频
- 日志量大幅增加

#### ⚠️ 可靠性挑战

**WebSocket 固有问题**:
- 连接可能随时断开
- 消息可能乱序或丢失
- 需要心跳保活机制
- 重连后需要同步状态

**对比轮询**:
- 轮询天然容错（失败重试即可）
- WebSocket 需要复杂的恢复逻辑

---

## 3. 引入事件总线的价值评估

### 3.1 收益分析

#### 🔗 模块解耦

**现状（紧耦合）**:
```rust
// live.rs 中直接调用所有逻辑
async fn run_cycle() {
    let data = fetch_klines().await;     // 数据获取
    let targets = run_backtest(...);     // 策略计算
    execute_orders(targets).await;       // 订单执行
    save_state().await;                  // 状态持久化
}
```

**引入事件总线后**:
```rust
// 各模块独立订阅事件
MarketDataService:    订阅 WebSocket → 发布 KlineCompleted
StrategyHandler:      订阅 KlineCompleted → 发布 TradingSignal
OrderExecutionService: 订阅 TradingSignal → 发布 OrderFilled
StatePersistence:     订阅 OrderFilled → 写入 JSON
```

**优势**:
- ✅ 模块间零依赖，可独立测试
- ✅ 新增功能只需注册 Handler
- ✅ 支持动态启用/禁用模块

#### 📊 可观测性增强

**事件溯源能力**:
```rust
StorableEvent {
    sequence: 12345,
    stream_id: "momentum_strategy",
    event: DomainEvent::TradingSignal(TradingSignalEvent {
        symbol: "SOLUSDT".to_string(),
        side: Side::Buy,
        reason: "90日动量排名第1".to_string(),
        timestamp: Utc::now(),
    }),
}
```

**价值**:
- ✅ 完整审计轨迹（为什么买这个品种？）
- ✅ 事后复盘（回放事件流重现决策过程）
- ✅ 合规要求（金融监管需要）

#### 🧪 测试友好

**模拟事件流**:
```rust
#[tokio::test]
async fn test_strategy_on_kline_completed() {
    let event_bus = TestEventBus::new();
    let strategy = MomentumRotation::new(...);
    
    // 注入历史K线事件
    for kline in historical_klines {
        event_bus.publish(DomainEvent::KlineCompleted(...)).await;
    }
    
    // 验证是否发出正确的交易信号
    let signals = event_bus.collect::<TradingSignalEvent>().await;
    assert_eq!(signals[0].symbol, "SOLUSDT");
}
```

### 3.2 成本分析

#### ⚠️ 学习曲线

**开发者需要理解**:
- broadcast channel 语义
- 事件版本兼容性
- 异步事件处理顺序
- 背压（backpressure）处理

#### ⚠️ 调试难度

**问题排查**:
- 事件流难以追踪（哪个 Handler 发布了什么？）
- 竞态条件更难复现
- 需要专门的事件可视化工具

#### ⚠️ 性能开销

**内存占用**:
- 事件队列缓存（broadcast channel capacity）
- Handler 注册表（DashMap）
- 事件序列化/反序列化

**CPU 开销**:
- 事件分发（遍历所有订阅者）
- 克隆事件（broadcast 语义）

---

## 4. 技术方案设计

### 4.1 渐进式引入方案（推荐）

#### Phase 1: 可选 WebSocket 数据源（2周）

**目标**: 保持现有轮询逻辑不变，添加 WebSocket 作为可选加速通道

**架构**:
```
┌─────────────────────────────────────────┐
│         DataProvider (trait)            │
├──────────────────┬──────────────────────┤
│ PollingProvider  │ WebSocketProvider    │
│ (现有逻辑)       │ (新增)               │
└──────────────────┴──────────────────────┘
           │
           ▼
    Strategy Engine (不变)
```

**实现**:
```rust
// exchanges/src/binance_ws.rs
pub struct BinanceWebSocketClient {
    url: String,
    subscriptions: Vec<WsSubscription>,
}

impl BinanceWebSocketClient {
    pub async fn subscribe_klines(&self, symbol: &str, interval: &str) 
        -> broadcast::Receiver<Kline> 
    {
        // 连接到 wss://stream.binance.com:9443/ws
        // 订阅 {symbol}@kline_{interval}
        // 返回 Receiver 供上层消费
    }
}

// app/src/data_provider.rs
pub trait DataProvider {
    async fn get_latest_klines(&self, symbol: &str, limit: usize) 
        -> Result<Vec<Kline>>;
}

pub struct HybridDataProvider {
    polling: PollingProvider,      // fallback
    websocket: Option<WebSocketProvider>,  // optional
}

impl DataProvider for HybridDataProvider {
    async fn get_latest_klines(...) -> Result<Vec<Kline>> {
        if let Some(ws) = &self.websocket {
            ws.get_cached_klines(symbol, limit).await
        } else {
            self.polling.get_latest_klines(symbol, limit).await
        }
    }
}
```

**配置**:
```toml
# quantkit.toml
[data]
mode = "websocket"  # "polling" | "websocket" | "hybrid"
websocket_url = "wss://stream.binance.com:9443/ws"
reconnect_interval_ms = 5000
```

**收益**:
- ✅ 实时性提升，风险可控
- ✅ 可随时切回轮询模式
- ✅ 不影响现有策略逻辑

---

#### Phase 2: 轻量级事件总线（1周）

**目标**: 在核心模块间引入简单的事件分发，不追求完整的事件溯源

**设计**:
```rust
// core/src/event_bus.rs
use tokio::sync::broadcast;

#[derive(Debug, Clone)]
pub enum QuantEvent {
    KlineUpdated { symbol: String, kline: Kline },
    SignalGenerated { symbol: String, signal: Signal },
    OrderExecuted { fill: Fill },
    EquitySnapshot { snap: EquitySnap },
}

pub struct EventBus {
    sender: broadcast::Sender<QuantEvent>,
}

impl EventBus {
    pub fn new(capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity);
        Self { sender }
    }
    
    pub fn publish(&self, event: QuantEvent) {
        let _ = self.sender.send(event);  // 忽略无订阅者的情况
    }
    
    pub fn subscribe(&self) -> broadcast::Receiver<QuantEvent> {
        self.sender.subscribe()
    }
}
```

**使用示例**:
```rust
// app/src/live.rs
let event_bus = EventBus::new(1024);

// 数据层发布事件
for kline in &klines {
    event_bus.publish(QuantEvent::KlineUpdated {
        symbol: sym.clone(),
        kline: kline.clone(),
    });
}

// 监控层订阅事件（异步后台任务）
let monitor_bus = event_bus.clone();
tokio::spawn(async move {
    let mut rx = monitor_bus.subscribe();
    while let Ok(event) = rx.recv().await {
        match event {
            QuantEvent::OrderExecuted { fill } => {
                log::info!("🟢 成交: {} {} @ {}", fill.symbol, fill.quantity, fill.price);
            }
            _ => {}
        }
    }
});
```

**特点**:
- ✅ 极简实现（< 100行代码）
- ✅ 利用 tokio broadcast（零额外依赖）
- ✅ 不强制事件版本管理
- ❌ 无持久化（重启丢失）
- ❌ 无事件回放能力

---

#### Phase 3: 完整事件溯源（可选，2-3月）

**目标**: 借鉴 binance-rust，实现完整的事件存储和回放

**架构**:
```rust
// core/src/event_store.rs
pub struct EventStore {
    db: sled::Db,  // 嵌入式KV数据库
    sequence: AtomicU64,
}

impl EventStore {
    pub fn append(&self, stream_id: &str, event: DomainEvent) -> Result<u64> {
        let seq = self.sequence.fetch_add(1, Ordering::SeqCst);
        let key = format!("{}:{:020}", stream_id, seq);
        let value = serde_json::to_vec(&event)?;
        self.db.insert(key, value)?;
        Ok(seq)
    }
    
    pub fn replay(&self, stream_id: &str, from_seq: u64) -> Result<Vec<DomainEvent>> {
        // 从 sled 中按前缀扫描
    }
}
```

**价值**:
- ✅ 完整的审计轨迹
- ✅ 状态重建（从事件回放）
- ✅ 合规要求满足

**成本**:
- ❌ 需要引入持久化存储（sled/rocksdb）
- ❌ 事件版本迁移复杂
- ❌ 对当前项目可能过度设计

---

### 4.2 激进式重构方案（不推荐）

**思路**: 完全借鉴 binance-rust，重写整个架构为事件驱动

**工作量**:
- 重写 `core/engine.rs` 为事件处理器
- 拆分 `app/live.rs` 为多个服务
- 引入完整的领域事件定义
- 重新设计策略接口

**风险**:
- ❌ 破坏现有回测/实盘一致性
- ❌ 大量回归测试工作
- ❌ 学习曲线陡峭

**结论**: **不建议**，除非项目战略转型为高频交易平台

---

## 5. 实施建议

### 5.1 短期（1-2周）✅ 强烈推荐

**实施 Phase 1 + Phase 2**:

1. **添加 WebSocket 客户端**
   ```rust
   // exchanges/src/binance_ws.rs (~300行)
   - 连接管理
   - 自动重连
   - K线订阅解析
   ```

2. **实现混合数据提供者**
   ```rust
   // app/src/data_provider.rs (~150行)
   - DataProvider trait
   - HybridDataProvider 实现
   - 配置切换逻辑
   ```

3. **引入轻量级事件总线**
   ```rust
   // core/src/event_bus.rs (~80行)
   - QuantEvent 枚举
   - broadcast::Sender/Receiver 封装
   ```

4. **集成到实盘通道**
   ```rust
   // app/src/live.rs (修改 ~50行)
   - 使用 DataProvider 替代直接调用 client
   - 发布关键事件（成交、权益快照）
   ```

**预期收益**:
- ✅ K线更新延迟从 300s → <1s
- ✅ 带宽节省 99%+
- ✅ 支持分钟级策略
- ✅ 模块解耦，易于扩展

**风险控制**:
- ✅ 保留轮询作为 fallback
- ✅ 配置开关控制启用
- ✅ 不影响现有回测逻辑

---

### 5.2 中期（1-2月）🤔 视需求决定

**如果 Phase 1+2 运行良好，考虑**:

5. **增强事件类型**
   - 添加更多细粒度事件（订单簿更新、资金费率等）
   - 支持事件过滤和路由

6. **改进风控**
   - 借鉴 binance-rust 的多层检查
   - 实时风险监控（订阅 OrderExecuted 事件）

7. **策略热加载**
   - 运行时注册新策略 Handler
   - 无需重启服务

---

### 5.3 长期（3-6月）❓ 谨慎评估

**仅在以下场景考虑 Phase 3**:

- 需要满足金融合规审计要求
- 计划商业化运营（需要完整追溯）
- 团队规模扩大（需要严格模块边界）

否则，当前的轻量级方案已足够。

---

## 6. 技术选型对比

### 6.1 WebSocket 库

| 库 | 优势 | 劣势 | 推荐度 |
|----|------|------|--------|
| **tokio-tungstenite** | 成熟稳定，binance-rust 在用 | API 较底层 | ⭐⭐⭐⭐⭐ |
| **async-tungstenite** | 更现代的 async 支持 | 生态较小 | ⭐⭐⭐ |
| **ws** | 简单易用 | 不支持 rustls | ⭐⭐ |

**推荐**: `tokio-tungstenite`（与 binance-rust 一致，降低学习成本）

---

### 6.2 事件总线实现

| 方案 | 优势 | 劣势 | 推荐度 |
|------|------|------|--------|
| **tokio::broadcast** | 零依赖，性能好 | 无持久化 | ⭐⭐⭐⭐⭐ (Phase 2) |
| **dashmap + channel** | 灵活，可定制 | 需要自己实现订阅管理 | ⭐⭐⭐ |
| **完整事件溯源** | 审计能力强 | 复杂度高 | ⭐⭐ (Phase 3) |

**推荐**: 先用 `tokio::broadcast`，后续按需升级

---

### 6.3 持久化存储（Phase 3）

| 数据库 | 优势 | 劣势 | 推荐度 |
|--------|------|------|--------|
| **sled** | 纯 Rust，嵌入式 | 社区较小 | ⭐⭐⭐⭐ |
| **rocksdb** | 成熟稳定 | C++ 依赖 | ⭐⭐⭐ |
| **SQLite** | 通用性强 | 写入性能一般 | ⭐⭐⭐ |

**推荐**: `sled`（纯 Rust，零配置）

---

## 7. 风险评估

### 7.1 技术风险

| 风险 | 概率 | 影响 | 缓解措施 |
|------|------|------|----------|
| **WebSocket 频繁断线** | 中 | 中 | 自动重连 + 指数退避 |
| **消息乱序/丢失** | 低 | 高 | 序列号校验 + 定期全量同步 |
| **broadcast channel 背压** | 中 | 中 | 设置合理 capacity + 监控丢包率 |
| **内存泄漏** | 低 | 高 | 定期压力测试 + valgrind |

---

### 7.2 业务风险

| 风险 | 概率 | 影响 | 缓解措施 |
|------|------|------|----------|
| **策略逻辑被破坏** | 低 | 高 | 保留轮询 fallback + 充分回归测试 |
| **实盘稳定性下降** | 中 | 高 | 灰度发布（先模拟盘验证） |
| **维护成本上升** | 中 | 中 | 完善文档 + 监控告警 |

---

## 8. 成本效益分析

### 8.1 开发成本估算

| 阶段 | 工作量 | 人员 | 时间 |
|------|--------|------|------|
| Phase 1 (WebSocket) | 300行代码 | 1人 | 1周 |
| Phase 2 (事件总线) | 150行代码 | 1人 | 1周 |
| 集成测试 | - | 1人 | 1周 |
| **总计** | **~500行** | **1人** | **3周** |

### 8.2 预期收益

| 收益项 | 量化指标 |
|--------|----------|
| **实时性提升** | 延迟降低 300x (300s → 1s) |
| **带宽节省** | 99%+ (全量 → 增量) |
| **新策略支持** | 从日线扩展到分钟线 |
| **代码质量** | 模块解耦，易测试 |
| **用户体验** | Web UI 实时更新 |

### 8.3 ROI 评估

**投入**: 3周开发时间  
**产出**: 
- 实时性提升 300倍
- 支持更多策略类型
- 代码可维护性提升

**结论**: **强烈推荐**，ROI 非常高

---

## 9. 最终建议

### ✅ 推荐行动

1. **立即实施 Phase 1 + Phase 2**（3周）
   - 风险可控，收益明显
   - 保持向后兼容
   - 为未来扩展打下基础

2. **保留轮询作为 fallback**
   - 配置开关控制
   - 默认使用 WebSocket
   - 失败时自动降级

3. **充分测试后再上实盘**
   - 先在模拟盘运行 1-2 周
   - 监控连接稳定性和消息完整性
   - 对比轮询模式的结果一致性

### ❌ 不推荐行动

1. **不要完全重写为事件驱动**
   - 破坏现有架构优势
   - ROI 太低

2. **不要过早引入完整事件溯源**
   - 复杂度远超当前需求
   - 等有明确合规要求再说

3. **不要同时改动太多**
   - 渐进式引入，逐步验证
   - 每一步都可回退

---

## 10. 下一步行动

如果同意此方案，建议按以下顺序实施：

1. **Week 1**: 实现 `BinanceWebSocketClient`
2. **Week 2**: 实现 `DataProvider` trait + `HybridDataProvider`
3. **Week 3**: 引入轻量级 `EventBus` + 集成测试
4. **Week 4**: 模拟盘验证 + 监控优化
5. **Week 5**: 实盘灰度发布

是否需要我开始实施 Phase 1（WebSocket 客户端）？
