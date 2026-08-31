//! 数据提供者抽象层：统一 REST 轮询与 WebSocket 实时数据源。
//!
//! 设计目标：
//! - 保持现有轮询逻辑不变（向后兼容）
//! - 可选接入 WebSocket 作为加速通道（Phase 1）
//! - 通过 trait 抽象，便于未来扩展其他交易所

use quantkit_core::types::Kline;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;

/// 数据提供者错误
#[derive(Debug, thiserror::Error)]
pub enum DataProviderError {
    #[error("网络错误: {0}")]
    Network(String),
    #[error("解析失败: {0}")]
    Parse(String),
    #[error("限流: {0}")]
    RateLimit(String),
}

/// K线缓存：按品种存储最近的 K线列表
pub type KlineCache = Arc<Mutex<BTreeMap<String, Vec<Kline>>>>;

/// 混合数据提供者：REST 为主，WebSocket 可选加速
///
/// Phase 2 实现：
/// - WebSocket 实时更新内存中的 K线缓存
/// - run_cycle 直接从缓存读取，无需每次拉取 REST
pub struct HybridDataProvider {
    ws_receiver: Option<broadcast::Receiver<(String, Kline)>>,
    kline_cache: KlineCache,
}

impl HybridDataProvider {
    /// 创建仅使用 REST 的数据提供者
    pub fn new_rest_only() -> Self {
        Self {
            ws_receiver: None,
            kline_cache: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    /// 创建带 WebSocket 加速的数据提供者
    pub fn new_with_ws(
        symbols: &[String],
        interval: &str,
    ) -> Self {
        // 启动 WebSocket 客户端（后台任务）
        let receiver = start_ws_background(symbols, interval);

        // 初始化空的 K线缓存
        let mut cache = BTreeMap::new();
        for sym in symbols {
            cache.insert(sym.clone(), Vec::new());
        }

        Self {
            ws_receiver: Some(receiver),
            kline_cache: Arc::new(Mutex::new(cache)),
        }
    }

    /// 获取 K线缓存的引用（用于 run_cycle 读取）
    pub fn get_kline_cache(&self) -> KlineCache {
        self.kline_cache.clone()
    }

    /// 初始化缓存：从 REST 拉取历史数据填充
    pub async fn init_cache_from_rest(
        &self,
        client: &quantkit_exchanges::binance::BinanceClient,
        symbols: &[String],
        interval: &str,
        limit: usize,
    ) -> Result<(), String> {
        println!("[live] [ws] 正在从 REST 初始化 K线缓存...");
        
        let mut cache = self.kline_cache.lock().map_err(|e| format!("缓存锁定失败: {}", e))?;
        
        for sym in symbols {
            match client.fetch_klines_history(sym, interval, limit as u32, None).await {
                Ok(klines) => {
                    println!("[live] [ws] {} 初始化为 {} 根K线", sym, klines.len());
                    cache.insert(sym.clone(), klines);
                }
                Err(e) => {
                    eprintln!("[live] [ws] {} 初始化失败: {}", sym, e);
                    return Err(format!("拉取 {} 历史K线失败: {}", sym, e));
                }
            }
        }
        
        println!("[live] [ws] K线缓存初始化完成，共 {} 个品种", cache.len());
        Ok(())
    }

    /// 从 WebSocket 更新缓存（在主循环中调用）
    pub fn update_cache_from_ws(&mut self) -> usize {
        let mut count = 0;
        if let Some(rx) = &mut self.ws_receiver {
            loop {
                match rx.try_recv() {
                    Ok((sym, kline)) => {
                        count += 1;
                        if let Ok(mut cache) = self.kline_cache.lock() {
                            if let Some(klines) = cache.get_mut(&sym) {
                                // 查找是否已存在该 K线（通过 open_time）
                                let pos = klines.iter().position(|k| k.open_time == kline.open_time);
                                if let Some(idx) = pos {
                                    // 更新现有 K线
                                    klines[idx] = kline;
                                } else {
                                    // 添加新 K线
                                    klines.push(kline);
                                    // 保持缓存大小合理（最多保留 3000 根）
                                    if klines.len() > 3000 {
                                        klines.remove(0);
                                    }
                                }
                            }
                        }
                    }
                    Err(_) => break, // 通道为空
                }
            }
        }
        count
    }

    /// 尝试从 WebSocket 接收最新数据（非阻塞，仅用于日志）
    pub fn try_recv_latest(&mut self) -> Option<(String, Kline)> {
        if let Some(rx) = &mut self.ws_receiver {
            loop {
                match rx.try_recv() {
                    Ok(msg) => {
                        // 持续读取直到拿到最新的
                        if rx.is_empty() {
                            return Some(msg);
                        }
                    }
                    Err(_) => return None, // 通道为空或已关闭
                }
            }
        }
        None
    }
}

/// 在后台启动 WebSocket 客户端（真实连接 Binance）
fn start_ws_background(
    symbols: &[String],
    interval: &str,
) -> broadcast::Receiver<(String, Kline)> {
    use quantkit_exchanges::binance_ws;
    
    println!("[live] [ws] 正在启动后台 WebSocket 任务...");
    
    // quick_start_ws 已经在后台 spawn 了任务，直接返回 receiver
    let rx = tokio::task::block_in_place(|| {
        let rt = tokio::runtime::Handle::current();
        let symbols_vec = symbols.to_vec();
        let interval_str = interval.to_string();
        
        println!("[live] [ws] 调用 quick_start_ws: {:?} @ {}", symbols_vec, interval_str);
        
        // block_on 等待 quick_start_ws 返回 receiver
        rt.block_on(async move {
            binance_ws::quick_start_ws(&symbols_vec, &interval_str).await
        })
    });
    
    println!("[live] [ws] 后台 WebSocket 任务已启动，receiver 已创建");
    
    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_data_provider_error_display() {
        // 测试错误类型的 Display 实现
        let net_err = DataProviderError::Network("connection timeout".to_string());
        assert!(net_err.to_string().contains("connection timeout"));
        
        let parse_err = DataProviderError::Parse("invalid JSON".to_string());
        assert!(parse_err.to_string().contains("invalid JSON"));
        
        let rate_err = DataProviderError::RateLimit("too many requests".to_string());
        assert!(rate_err.to_string().contains("too many requests"));
    }

    #[test]
    fn test_kline_cache_type_alias() {
        // 验证 KlineCache 类型别名正确工作
        let cache: KlineCache = Arc::new(Mutex::new(BTreeMap::new()));
        
        // 可以正常锁定和访问
        let guard = cache.lock().unwrap();
        assert!(guard.is_empty());
        drop(guard);
        
        // 可以插入数据
        let mut guard = cache.lock().unwrap();
        guard.insert("BTCUSDT".to_string(), Vec::new());
        assert_eq!(guard.len(), 1);
    }

    #[test]
    fn test_hybrid_provider_new_rest_only() {
        // 测试仅 REST 模式的创建
        let provider = HybridDataProvider::new_rest_only();
        
        // ws_receiver 应该为 None
        assert!(provider.ws_receiver.is_none());
        
        // kline_cache 应该为空
        let cache = provider.kline_cache.lock().unwrap();
        assert!(cache.is_empty());
    }

    #[test]
    fn test_get_kline_cache_clone() {
        // 测试 get_kline_cache 返回克隆的 Arc
        let provider = HybridDataProvider::new_rest_only();
        
        let cache1 = provider.get_kline_cache();
        let cache2 = provider.get_kline_cache();
        
        // 两个 Arc 应该指向同一个 Mutex
        assert!(Arc::ptr_eq(&cache1, &cache2));
        
        // 修改一个会影响另一个
        {
            let mut guard = cache1.lock().unwrap();
            guard.insert("TEST".to_string(), Vec::new());
        }
        
        let guard = cache2.lock().unwrap();
        assert!(guard.contains_key("TEST"));
    }

    #[test]
    fn test_update_cache_from_ws_no_receiver() {
        // 没有 WebSocket 接收器时，update_cache_from_ws 应返回 0
        let mut provider = HybridDataProvider::new_rest_only();
        let count = provider.update_cache_from_ws();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_try_recv_latest_no_receiver() {
        // 没有 WebSocket 接收器时，try_recv_latest 应返回 None
        let mut provider = HybridDataProvider::new_rest_only();
        let result = provider.try_recv_latest();
        assert!(result.is_none());
    }

    #[test]
    fn test_kline_cache_insert_and_update() {
        // 测试缓存的基本操作
        let cache: KlineCache = Arc::new(Mutex::new(BTreeMap::new()));
        
        // 创建一个测试 K线
        let kline = Kline {
            open_time: 1724932800000,
            close_time: 1724936399999,
            open: 60000.0,
            high: 61000.0,
            low: 59500.0,
            close: 60500.0,
            volume: 100.5,
        };
        
        // 插入到缓存
        {
            let mut guard = cache.lock().unwrap();
            guard.insert("BTCUSDT".to_string(), vec![kline.clone()]);
        }
        
        // 验证插入
        {
            let guard = cache.lock().unwrap();
            let klines = guard.get("BTCUSDT").unwrap();
            assert_eq!(klines.len(), 1);
            assert_eq!(klines[0].open_time, 1724932800000);
        }
    }

    #[test]
    fn test_kline_cache_multiple_symbols() {
        // 测试多品种缓存
        let cache: KlineCache = Arc::new(Mutex::new(BTreeMap::new()));
        
        {
            let mut guard = cache.lock().unwrap();
            guard.insert("BTCUSDT".to_string(), Vec::new());
            guard.insert("ETHUSDT".to_string(), Vec::new());
            guard.insert("SOLUSDT".to_string(), Vec::new());
        }
        
        let guard = cache.lock().unwrap();
        assert_eq!(guard.len(), 3);
        assert!(guard.contains_key("BTCUSDT"));
        assert!(guard.contains_key("ETHUSDT"));
        assert!(guard.contains_key("SOLUSDT"));
    }

    #[test]
    fn test_kline_cache_update_existing() {
        // 测试更新现有 K线
        let cache: KlineCache = Arc::new(Mutex::new(BTreeMap::new()));
        
        let kline1 = Kline {
            open_time: 1724932800000,
            close_time: 1724936399999,
            open: 60000.0,
            high: 61000.0,
            low: 59500.0,
            close: 60500.0,
            volume: 100.0,
        };
        
        {
            let mut guard = cache.lock().unwrap();
            guard.insert("BTCUSDT".to_string(), vec![kline1]);
        }
        
        // 更新同一时间戳的 K线（模拟价格变动）
        let kline2 = Kline {
            open_time: 1724932800000, // 相同时间戳
            close_time: 1724936399999,
            open: 60000.0,
            high: 61500.0, // 最高价变化
            low: 59500.0,
            close: 61000.0, // 收盘价变化
            volume: 150.0,
        };
        
        {
            let mut guard = cache.lock().unwrap();
            if let Some(klines) = guard.get_mut("BTCUSDT") {
                let pos = klines.iter().position(|k| k.open_time == kline2.open_time);
                if let Some(idx) = pos {
                    klines[idx] = kline2;
                }
            }
        }
        
        // 验证更新
        let guard = cache.lock().unwrap();
        let klines = guard.get("BTCUSDT").unwrap();
        assert_eq!(klines.len(), 1);
        assert!((klines[0].high - 61500.0).abs() < 1e-6);
        assert!((klines[0].close - 61000.0).abs() < 1e-6);
    }

    #[test]
    fn test_kline_cache_append_new() {
        // 测试追加新 K线
        let cache: KlineCache = Arc::new(Mutex::new(BTreeMap::new()));
        
        let kline1 = Kline {
            open_time: 1724932800000,
            close_time: 1724936399999,
            open: 60000.0,
            high: 61000.0,
            low: 59500.0,
            close: 60500.0,
            volume: 100.0,
        };
        
        let kline2 = Kline {
            open_time: 1724936400000, // 新的时间戳
            close_time: 1724939999999,
            open: 60500.0,
            high: 61500.0,
            low: 60000.0,
            close: 61000.0,
            volume: 120.0,
        };
        
        {
            let mut guard = cache.lock().unwrap();
            let klines = guard.entry("BTCUSDT".to_string()).or_insert_with(Vec::new);
            klines.push(kline1);
            klines.push(kline2);
        }
        
        let guard = cache.lock().unwrap();
        let klines = guard.get("BTCUSDT").unwrap();
        assert_eq!(klines.len(), 2);
        assert_eq!(klines[0].open_time, 1724932800000);
        assert_eq!(klines[1].open_time, 1724936400000);
    }

    #[test]
    fn test_kline_cache_size_limit_simulation() {
        // 模拟缓存大小限制逻辑（最多 3000 根）
        let cache: KlineCache = Arc::new(Mutex::new(BTreeMap::new()));
        
        // 插入 3001 根 K线
        {
            let mut guard = cache.lock().unwrap();
            let klines = guard.entry("BTCUSDT".to_string()).or_insert_with(Vec::new);
            
            for i in 0..3001 {
                klines.push(Kline {
                    open_time: i * 86400000, // 每天一根
                    close_time: (i + 1) * 86400000 - 1,
                    open: 60000.0 + i as f64,
                    high: 61000.0 + i as f64,
                    low: 59500.0 + i as f64,
                    close: 60500.0 + i as f64,
                    volume: 100.0,
                });
            }
            
            // 模拟 update_cache_from_ws 中的大小限制逻辑
            if klines.len() > 3000 {
                klines.remove(0);
            }
        }
        
        let guard = cache.lock().unwrap();
        let klines = guard.get("BTCUSDT").unwrap();
        assert_eq!(klines.len(), 3000);
        // 最旧的已被删除
        assert_eq!(klines[0].open_time, 1 * 86400000);
    }

    #[test]
    fn test_kline_cache_empty_symbol_list() {
        // 测试空品种列表的缓存初始化
        let cache: KlineCache = Arc::new(Mutex::new(BTreeMap::new()));
        let symbols: Vec<String> = vec![];
        
        for sym in &symbols {
            cache.lock().unwrap().insert(sym.clone(), Vec::new());
        }
        
        let guard = cache.lock().unwrap();
        assert!(guard.is_empty());
    }

    #[test]
    fn test_btreemap_ordering() {
        // 验证 BTreeMap 按键排序的特性
        let mut cache: BTreeMap<String, Vec<Kline>> = BTreeMap::new();
        
        cache.insert("SOLUSDT".to_string(), Vec::new());
        cache.insert("BTCUSDT".to_string(), Vec::new());
        cache.insert("ETHUSDT".to_string(), Vec::new());
        
        let keys: Vec<&String> = cache.keys().collect();
        // BTreeMap 应该按字母顺序排序
        assert_eq!(keys[0], "BTCUSDT");
        assert_eq!(keys[1], "ETHUSDT");
        assert_eq!(keys[2], "SOLUSDT");
    }
}
