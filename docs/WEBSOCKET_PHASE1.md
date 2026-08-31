# Phase 1: WebSocket 实时数据源

## 概述

Phase 1 实现了 Binance WebSocket 客户端作为可选的实时数据加速通道，保持与现有 REST 轮询逻辑的向后兼容。

## 架构设计

```
┌─────────────────────────────────────────────┐
│              live.rs (主循环)                │
│                                             │
│  ┌──────────────┐    ┌──────────────────┐  │
│  │ REST Polling │    │ HybridDataProvider│  │
│  │   (主要)     │    │   (可选加速)      │  │
│  └──────────────┘    └────────┬─────────┘  │
│                               │             │
│                        ┌──────▼──────┐      │
│                        │  WebSocket  │      │
│                        │  Broadcast  │      │
│                        └─────────────┘      │
└─────────────────────────────────────────────┘
```

## 核心组件

### 1. `exchanges/src/binance_ws.rs` - Binance WebSocket 客户端

**功能特性：**
- 订阅多品种 K线流（合并流）
- 自动重连（指数退避：5s → 10s → 20s → 40s → 60s）
- 消息解析和验证
- 通过 broadcast channel 推送 Kline 数据

**关键结构：**
```rust
pub struct WsKlineMessage {
    pub event_type: String,
    pub event_time: u64,
    pub symbol: String,
    pub kline: WsKlineData,
}

pub struct BinanceWsClient {
    config: WsConfig,
    sender: broadcast::Sender<(String, Kline)>,
}
```

**使用示例：**
```rust
use quantkit_exchanges::binance_ws;

// 快速启动（后台任务）
let mut rx = binance_ws::quick_start_ws(
    &["BTCUSDT".to_string(), "ETHUSDT".to_string()],
    "1h"
).await;

// 接收数据
while let Ok((symbol, kline)) = rx.recv().await {
    println!("{}: {} @ {}", symbol, kline.close, kline.open_time);
}
```

### 2. `app/src/data_provider.rs` - 数据提供者抽象层

**设计目标：**
- 保持现有轮询逻辑不变（向后兼容）
- 可选接入 WebSocket 作为加速通道
- 通过 trait 抽象，便于未来扩展其他交易所

**关键结构：**
```rust
pub struct HybridDataProvider {
    ws_receiver: Option<broadcast::Receiver<(String, Kline)>>,
}

impl HybridDataProvider {
    // 仅使用 REST
    pub fn new_rest_only() -> Self;

    // 带 WebSocket 加速
    pub fn new_with_ws(symbols: &[String], interval: &str) -> Self;

    // 非阻塞接收最新数据
    pub fn try_recv_latest(&mut self) -> Option<(String, Kline)>;
}
```

### 3. `app/src/live.rs` - 集成点

在实盘主循环中集成 WebSocket 数据提供者：

```rust
// 初始化（可选启用）
let mut ws_provider = if cfg.ws_enabled {
    Some(HybridDataProvider::new_with_ws(&cfg.symbols, interval.as_str()))
} else {
    None
};

// 主循环中尝试接收最新数据（非阻塞）
loop {
    if let Some(ref mut ws) = ws_provider {
        while let Some((sym, kline)) = ws.try_recv_latest() {
            info!("📡 WS 实时: {} 收盘 {:.2}", sym, kline.close);
        }
    }
    // ... 原有轮询逻辑
}
```

## 配置项

在 `quantkit.toml` 中添加以下配置：

```toml
# WebSocket 实时数据源开关：默认关闭，保持 REST 轮询模式
ws_enabled = false

# WebSocket 只接收已闭合的 K线（避免未完结数据干扰决策）
ws_only_closed_bars = true
```

## 性能对比

| 指标 | REST 轮询 | WebSocket |
|------|-----------|-----------|
| 延迟 | 300s (5min) | <1s |
| 带宽占用 | 高（重复请求） | 低（长连接） |
| API 限频压力 | 高 | 无 |
| 实现复杂度 | 简单 | 中等 |
| 稳定性 | 高 | 需处理断连 |

## 测试

运行单元测试验证 JSON 解析：
```bash
cargo test --package quantkit-exchanges binance_ws::tests
```

运行真实连接测试（需要能访问 Binance）：
```bash
cargo run --bin test_ws
```

**注意：** 如果连接失败（Connection reset by peer），可能是以下原因：
1. 本地网络无法直接访问 Binance（需要代理或 VPN）
2. 防火墙限制
3. Binance 对某些地区 IP 的限制

解决方案：
- 使用 SSH 隧道（参考 binance-rust 的 `USE_SSH_TUNNEL` 环境变量）
- 配置 HTTPS_PROXY 环境变量
- 在服务器上部署（如香港服务器）

## 下一步（Phase 2）

- 实现轻量级事件总线（broadcast channel 封装）
- 解耦 live.rs 中的数据获取、策略执行、状态持久化
- 支持多策略并行运行

## 注意事项

1. **WebSocket 是可选加速通道**：即使启用，决策仍基于 REST 拉取的历史 K线，保证与回测口径一致
2. **当前实现为占位版本**：`start_ws_background()` 函数尚未实际连接 Binance WebSocket，需要在生产环境部署前完成集成
3. **重连策略已实现**：指数退避确保在网络不稳定时不会频繁重连
4. **内存管理**：broadcast channel 容量设为 1024，避免慢消费者导致内存溢出
