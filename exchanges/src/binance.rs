//! Binance 现货 REST 客户端（从零实现）。
//!
//! 设计要点：
//! - 签名：参数经 [`BTreeMap`] 排序后拼接 query，HMAC-SHA256(secret) 取 hex
//! - 时间：每个签名请求带 `timestamp`（本机毫秒）与 `recvWindow`
//! - 限频：[`RateLimiter`] 用 [`AtomicU64`] 记录“最早可发起时刻”，
//!   CAS 保证并发下请求间隔不小于 `min_interval`
//! - 重试：仅网络层失败与 5xx 重试（指数退避）；4xx 业务错误一律不重试
//! - 精度：[`round_step`] 按 stepSize 向下取整数量，避免交易所拒单

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use hmac::{Hmac, Mac};
use serde::Serialize;
use serde_json::Value;
use sha2::Sha256;

use quantkit_core::executor::ExecError;
use quantkit_core::types::{Fill, Kline, Order, Side};

use crate::traits::{Broker, MarketData};

type HmacSha256 = Hmac<Sha256>;

/// 默认时间窗口容差（毫秒）
pub const DEFAULT_RECV_WINDOW: u64 = 5_000;

/// 客户端错误
#[derive(Debug, thiserror::Error)]
pub enum BinanceError {
    #[error("网络错误: {0}")]
    Network(String),
    #[error("交易所业务错误(code={code}): {msg}")]
    Api { code: i64, msg: String },
    #[error("响应解析失败: {0}")]
    Parse(String),
}

impl From<BinanceError> for ExecError {
    fn from(e: BinanceError) -> Self {
        ExecError::Exchange(e.to_string())
    }
}

/// 简单固定间隔限频器：并发安全，保证相邻请求间隔 >= min_interval。
///
/// 内部用 [`AtomicU64`] 存“最早允许发起请求的毫秒时刻”，
/// 多线程同时 acquire 时通过 CAS 依次排队。
pub struct RateLimiter {
    earliest_ms: AtomicU64,
    min_interval_ms: u64,
}

impl RateLimiter {
    pub fn new(min_interval: Duration) -> Self {
        Self {
            earliest_ms: AtomicU64::new(0),
            min_interval_ms: min_interval.as_millis() as u64,
        }
    }

    /// 等待直到获得执行槽位。返回实际等待后的发起时刻（ms）。
    pub async fn acquire(&self) -> u64 {
        loop {
            let now = now_ms();
            let prev = self.earliest_ms.load(Ordering::Relaxed);
            let slot = now.max(prev);
            match self.earliest_ms.compare_exchange_weak(
                prev,
                slot + self.min_interval_ms,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    if slot > now {
                        tokio::time::sleep(Duration::from_millis(slot - now)).await;
                    }
                    return slot;
                }
                Err(_) => continue,
            }
        }
    }
}

/// 本机当前毫秒时间戳
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 参数排序后拼接为 query string（BTreeMap 保证 key 升序，签名确定性）
pub fn build_query(params: &BTreeMap<String, String>) -> String {
    params
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

/// HMAC-SHA256 签名（hex 小写）。纯函数，便于单元测试。
pub fn sign(secret: &str, payload: &str) -> String {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
        .expect("HMAC 接受任意长度密钥");
    mac.update(payload.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// 数量按 stepSize 向下取整（交易所只接受步长整数倍）。
///
/// 步长按十进制解析为 (整数部分, 小数位数)，转整数运算后向下取整，
/// 规避 0.29/0.01 = 28.999… 之类的浮点误差导致少一步长。
pub fn round_step(qty: f64, step: f64) -> f64 {
    if step <= 0.0 || !qty.is_finite() {
        return qty;
    }
    let step_str = format!("{step}");
    let decimals = step_str.split('.').nth(1).map(|d| d.len()).unwrap_or(0);
    let scale = 10f64.powi(decimals as i32);
    // 步长本身按十进制定义是 scale 的整倍数（0.001*1000=1），直接取整安全
    let step_units = (step * scale).round() as i64;
    if step_units <= 0 {
        return qty;
    }
    // 先加相对容差再向下取整：消除 0.29*100=28.999… 的二进制表示噪声，
    // 同时保持“不足一步长舍去”的交易所语义（1.9999/步长1 仍取 1）
    let scaled = qty * scale;
    let qty_units = (scaled + scaled.abs() * 1e-9 + 1e-9).floor() as i64;
    let stepped = qty_units / step_units * step_units;
    stepped as f64 / scale
}

/// 24h 行情快照（全市场 ticker，WebUI 市场总览用）
#[derive(Debug, Clone, Serialize)]
pub struct TickerQuote {
    pub symbol: String,
    /// 最新成交价
    pub last_price: f64,
    /// 24h 涨跌幅（%，正为上涨）
    pub price_change_pct: f64,
    /// 24h 计价资产成交额（如 USDT）
    pub quote_volume: f64,
    /// 24h 最高价（FULL/MINI 均提供；缺失为 0）
    pub high_price: f64,
    /// 24h 最低价
    pub low_price: f64,
}

/// 解析 /api/v3/ticker/24hr 全市场响应（纯函数，可单测）。
/// 兼容 FULL（带 priceChangePercent）与 MINI（type=MINI，响应体小一半，
/// 无涨跌幅字段，由 (最新价-开盘价)/开盘价 计算）。缺 symbol 则跳过该行。
pub fn parse_ticker_24h(v: &Value) -> Result<Vec<TickerQuote>, BinanceError> {
    let arr = v
        .as_array()
        .ok_or_else(|| BinanceError::Parse("ticker/24hr 响应不是数组".into()))?;
    let mut out = Vec::with_capacity(arr.len());
    for row in arr {
        let symbol = match row.get("symbol").and_then(|s| s.as_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        let num = |k: &str| -> f64 {
            row.get(k)
                .and_then(|x| x.as_str())
                .and_then(|s| s.parse().ok())
                .unwrap_or(0.0)
        };
        let last_price = num("lastPrice");
        // FULL 直接给涨跌幅；MINI 需由开盘价自行折算（两者口径一致，均为 24h 滚动窗口）
        let price_change_pct = row
            .get("priceChangePercent")
            .and_then(|x| x.as_str())
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or_else(|| {
                let open = num("openPrice");
                if open > 0.0 {
                    (last_price - open) / open * 100.0
                } else {
                    0.0
                }
            });
        out.push(TickerQuote {
            symbol,
            last_price,
            price_change_pct,
            quote_volume: num("quoteVolume"),
            high_price: num("highPrice"),
            low_price: num("lowPrice"),
        });
    }
    Ok(out)
}

/// Binance 现货客户端
pub struct BinanceClient {
    http: reqwest::Client,
    /// 大响应专用客户端（全市场 ticker ~1MB）：受限网络下传输缓慢，超时需远大于常规接口
    http_bulk: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
    api_secret: Option<String>,
    recv_window: u64,
    rate: RateLimiter,
    max_retries: u32,
    /// 公共行情镜像兜底：主域不可达时自动切换（显式指定 BINANCE_BASE_URL 则不启用）
    kline_fallback_url: Option<String>,
    /// 镜像粘性：一旦切换成功，后续公共行情直连镜像，避免每页重蹈主域超时
    fallback_active: std::sync::atomic::AtomicBool,
}

impl BinanceClient {
    /// 公共行情客户端（无密钥，用于 dry-run 拉取K线/价格）
    pub fn public() -> Self {
        Self::with_credentials(None, None)
    }

    /// 带密钥客户端。密钥只应从环境变量传入，不落配置文件。
    ///
    ///  环境变量可覆盖默认域名（如部分网络环境下
    /// 主域不可达，可切换到官方公共行情镜像）。
    pub fn with_credentials(api_key: Option<String>, api_secret: Option<String>) -> Self {
        let custom = std::env::var("BINANCE_BASE_URL").ok().filter(|s| !s.is_empty());
        let base_url = custom.clone().unwrap_or_else(|| "https://api.binance.com".into());
        // 主域不可达（部分本地网络）时公共行情自动切官方镜像；用户显式指定域名则尊重其选择；
        // BINANCE_NO_FALLBACK=1 可强制关闭
        let kline_fallback_url = if custom.is_none()
            && std::env::var("BINANCE_NO_FALLBACK").as_deref() != Ok("1")
        {
            Some("https://data-api.binance.vision".into())
        } else {
            None
        };
        Self {
            http: reqwest::Client::builder()
                // 全量历史分页单页可达 ~170KB，受限网络下传输缓慢，超时需留足余量
                .timeout(Duration::from_secs(30))
                .connect_timeout(Duration::from_secs(10))
                .build()
                .expect("构建 HTTP 客户端"),
            http_bulk: reqwest::Client::builder()
                // 全市场 24h 行情响应体可达 ~1MB，受限网络实测传输需 1 分钟以上，给足余量，超时设 180s
                .timeout(Duration::from_secs(180))
                .connect_timeout(Duration::from_secs(10))
                .build()
                .expect("构建 HTTP 客户端"),
            base_url,
            api_key,
            api_secret,
            recv_window: DEFAULT_RECV_WINDOW,
            // 公共接口权重预算充裕：最小间隔 100ms（≤10 req/s）足够且保守
            rate: RateLimiter::new(Duration::from_millis(100)),
            max_retries: 3,
            kline_fallback_url,
            fallback_active: std::sync::atomic::AtomicBool::new(false),
        }
    }

    fn credentials(&self) -> Result<(&str, &str), BinanceError> {
        match (&self.api_key, &self.api_secret) {
            (Some(k), Some(s)) => Ok((k.as_str(), s.as_str())),
            _ => Err(BinanceError::Api {
                code: -1,
                msg: "缺少 API 密钥（请设置 BINANCE_API_KEY / BINANCE_API_SECRET 环境变量）".into(),
            }),
        }
    }

    /// GET 公共接口，返回解析后的 JSON。网络错误/5xx 重试，4xx 不重试。
    async fn get_json(&self, path: &str, query: &str) -> Result<Value, BinanceError> {
        self.get_json_on(&self.base_url, path, query).await
    }

    async fn get_json_on(&self, base: &str, path: &str, query: &str) -> Result<Value, BinanceError> {
        self.get_json_with(&self.http, base, path, query).await
    }

    /// 大响应专用：走长超时客户端（全市场 ticker）
    async fn get_json_bulk_on(&self, base: &str, path: &str, query: &str) -> Result<Value, BinanceError> {
        self.get_json_with(&self.http_bulk, base, path, query).await
    }

    async fn get_json_with(&self, http: &reqwest::Client, base: &str, path: &str, query: &str) -> Result<Value, BinanceError> {
        let url = format!("{base}{path}?{query}");
        let mut last_err = BinanceError::Network("未发起请求".into());
        for attempt in 0..=self.max_retries {
            self.rate.acquire().await;
            match http.get(&url).send().await {
                Ok(resp) => {
                    let status = resp.status();
                    let body = resp.text().await.unwrap_or_default();
                    if status.is_success() {
                        // 镜像偶发 HTTP 200 返回空 body：视为瞬时网络故障进入重试
                        if !body.trim().is_empty() {
                            return serde_json::from_str(&body)
                                .map_err(|e| BinanceError::Parse(format!("{e}: {body}")));
                        }
                        last_err = BinanceError::Network("HTTP 200 但响应体为空".into());
                    } else if status.is_server_error() {
                        last_err = BinanceError::Network(format!("HTTP {status}: {body}"));
                    } else {
                        // 4xx：业务错误（限频/参数非法等），重试无意义
                        let code = serde_json::from_str::<Value>(&body)
                            .ok()
                            .and_then(|v| v.get("code").and_then(|c| c.as_i64()))
                            .unwrap_or(status.as_u16() as i64);
                        let msg = serde_json::from_str::<Value>(&body)
                            .ok()
                            .and_then(|v| v.get("msg").and_then(|m| m.as_str().map(String::from)))
                            .unwrap_or(body.clone());
                        return Err(BinanceError::Api { code, msg });
                    }
                }
                Err(e) => last_err = BinanceError::Network(e.to_string()),
            }
            if attempt < self.max_retries {
                // 指数退避：200ms, 400ms, 800ms
                tokio::time::sleep(Duration::from_millis(200 << attempt)).await;
            }
        }
        Err(last_err)
    }

    /// POST 签名接口（下单等）
    async fn post_signed(&self, path: &str, params: &mut BTreeMap<String, String>) -> Result<Value, BinanceError> {
        let (key, secret) = self.credentials()?;
        params.insert("timestamp".into(), now_ms().to_string());
        params.insert("recvWindow".into(), self.recv_window.to_string());
        let query = build_query(params);
        let signature = sign(secret, &query);
        let url = format!("{}{}", self.base_url, path);
        let mut last_err = BinanceError::Network("未发起请求".into());
        for attempt in 0..=self.max_retries {
            self.rate.acquire().await;
            let body = format!("{query}&signature={signature}");
            match self
                .http
                .post(&url)
                .header("X-MBX-APIKEY", key)
                .header("Content-Type", "application/x-www-form-urlencoded")
                .body(body.clone())
                .send()
                .await
            {
                Ok(resp) => {
                    let status = resp.status();
                    let text = resp.text().await.unwrap_or_default();
                    if status.is_success() {
                        // 镜像偶发 HTTP 200 返回空 body：视为瞬时网络故障进入重试
                        if !text.trim().is_empty() {
                            return serde_json::from_str(&text)
                                .map_err(|e| BinanceError::Parse(format!("{e}: {text}")));
                        }
                        last_err = BinanceError::Network("HTTP 200 但响应体为空".into());
                    } else if status.is_server_error() {
                        last_err = BinanceError::Network(format!("HTTP {status}: {text}"));
                    } else {
                        let code = serde_json::from_str::<Value>(&text)
                            .ok()
                            .and_then(|v| v.get("code").and_then(|c| c.as_i64()))
                            .unwrap_or(status.as_u16() as i64);
                        let msg = serde_json::from_str::<Value>(&text)
                            .ok()
                            .and_then(|v| v.get("msg").and_then(|m| m.as_str().map(String::from)))
                            .unwrap_or(text.clone());
                        return Err(BinanceError::Api { code, msg });
                    }
                }
                Err(e) => last_err = BinanceError::Network(e.to_string()),
            }
            if attempt < self.max_retries {
                tokio::time::sleep(Duration::from_millis(200 << attempt)).await;
            }
        }
        Err(last_err)
    }

    /// 交易所服务器时间（毫秒），用于启动时时钟偏移自检
    pub async fn fetch_server_time(&self) -> Result<u64, BinanceError> {
        // 公共镜像对未知参数报 -1101：查询串除业务参数外不得携带任何参数（含占位）
        let v = self.get_json("/api/v3/time", "").await?;
        v.get("serverTime")
            .and_then(|t| t.as_u64())
            .ok_or_else(|| BinanceError::Parse("serverTime 解析失败".into()))
    }

    /// 查询品种精度（stepSize / minQty），用于下单前数量取整
    pub async fn fetch_step_size(&self, symbol: &str) -> Result<(f64, f64), BinanceError> {
        let v = self
            .get_json("/api/v3/exchangeInfo", &format!("symbol={symbol}"))
            .await?;
        let filter = v
            .pointer("/symbols/0/filters")
            .and_then(|f| f.as_array())
            .and_then(|fs| {
                fs.iter().find(|f| f.get("filterType").and_then(|t| t.as_str()) == Some("LOT_SIZE"))
            })
            .ok_or_else(|| BinanceError::Parse(format!("未找到 {symbol} 的 LOT_SIZE 过滤器")))?;
        let step: f64 = filter["stepSize"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| BinanceError::Parse("stepSize 解析失败".into()))?;
        let min_qty: f64 = filter["minQty"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| BinanceError::Parse("minQty 解析失败".into()))?;
        Ok((step, min_qty))
    }

    /// 查询 USDT 可用余额（启动自检用）
    pub async fn fetch_usdt_balance(&self) -> Result<f64, BinanceError> {
        let v = self.get_json_signed("/api/v3/account", &BTreeMap::new()).await?;
        let free = v
            .get("balances")
            .and_then(|b| b.as_array())
            .and_then(|arr| arr.iter().find(|b| b.get("asset").and_then(|a| a.as_str()) == Some("USDT")))
            .and_then(|b| b.get("free"))
            .and_then(|f| f.as_str())
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| BinanceError::Parse("USDT 余额解析失败".into()))?;
        Ok(free)
    }

    /// 查询全部非零可用余额（资产 -> free）。实盘对账用：
    /// 目标币本位资产（如 BTC）与 USDT 的可用数量都由此一次拉取。
    pub async fn fetch_balances(&self) -> Result<BTreeMap<String, f64>, BinanceError> {
        let v = self.get_json_signed("/api/v3/account", &BTreeMap::new()).await?;
        let arr = v
            .get("balances")
            .and_then(|b| b.as_array())
            .ok_or_else(|| BinanceError::Parse("balances 解析失败".into()))?;
        let mut out = BTreeMap::new();
        for b in arr {
            let asset = b.get("asset").and_then(|a| a.as_str()).unwrap_or("");
            let free: f64 = b
                .get("free")
                .and_then(|f| f.as_str())
                .and_then(|s| s.parse().ok())
                .unwrap_or(0.0);
            if !asset.is_empty() && free > 0.0 {
                out.insert(asset.to_string(), free);
            }
        }
        Ok(out)
    }

    /// 账户实际 taker 费率（/api/v3/account，单位为 bps，自动含 BNB 抵扣折扣）。
    /// 与"配置一个虚构费率"相对：实盘以此为准。
    pub async fn fetch_taker_fee_rate(&self) -> Result<f64, BinanceError> {
        let v = self.get_json_signed("/api/v3/account", &BTreeMap::new()).await?;
        let bps = v
            .get("takerCommission")
            .and_then(|t| t.as_i64())
            .ok_or_else(|| BinanceError::Parse("takerCommission 解析失败".into()))?;
        Ok(bps as f64 / 10_000.0)
    }

    /// 按 clientOrderId 查询订单：网络超时后的幂等恢复（已提交则取回真实成交）。
    /// 订单不存在（-2013）返回 Ok(None)。
    async fn fetch_order_by_client_id(
        &self,
        symbol: &str,
        client_order_id: &str,
    ) -> Result<Option<Value>, BinanceError> {
        let mut params = BTreeMap::new();
        params.insert("symbol".into(), symbol.to_string());
        params.insert("origClientOrderId".into(), client_order_id.to_string());
        match self.get_json_signed("/api/v3/order", &params).await {
            Ok(v) => {
                // 市价单提交即成交；NEW 视为未有效提交
                let status = v.get("status").and_then(|s| s.as_str()).unwrap_or("");
                Ok((status == "FILLED" || status == "PARTIALLY_FILLED").then_some(v))
            }
            Err(BinanceError::Api { code: -2013, .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// K线查询与解析（主域失败时自动切公共镜像并粘性保持；fetch_klines 与全量历史分页共用）
    async fn klines_query(&self, query: &str) -> Result<Vec<Kline>, ExecError> {
        use std::sync::atomic::Ordering;
        let v = if self.fallback_active.load(Ordering::Relaxed) {
            let fb = self.kline_fallback_url.as_deref().unwrap_or(&self.base_url);
            self.get_json_on(fb, "/api/v3/klines", query).await?
        } else {
            match self.get_json("/api/v3/klines", query).await {
                Ok(v) => v,
                Err(e) => match &self.kline_fallback_url {
                    Some(fb) => {
                        eprintln!("[binance] 主域行情不可达，切换公共镜像: {fb}");
                        self.fallback_active.store(true, Ordering::Relaxed);
                        self.get_json_on(fb, "/api/v3/klines", query).await?
                    }
                    None => return Err(e.into()),
                },
            }
        };
        let arr = v
            .as_array()
            .ok_or_else(|| BinanceError::Parse("klines 响应不是数组".into()))?;
        let mut out = Vec::with_capacity(arr.len());
        for row in arr {
            let row = row
                .as_array()
                .ok_or_else(|| BinanceError::Parse("kline 行不是数组".into()))?;
            let num = |i: usize| -> Result<f64, BinanceError> {
                row.get(i)
                    .and_then(|x| x.as_str())
                    .and_then(|s| s.parse().ok())
                    .ok_or_else(|| BinanceError::Parse(format!("kline 第 {i} 列解析失败")))
            };
            let int = |i: usize| -> Result<u64, BinanceError> {
                row.get(i)
                    .and_then(|x| x.as_u64())
                    .ok_or_else(|| BinanceError::Parse(format!("kline 第 {i} 列解析失败")))
            };
            out.push(Kline {
                open_time: int(0)?,
                open: num(1)?,
                high: num(2)?,
                low: num(3)?,
                close: num(4)?,
                volume: num(5)?,
                close_time: int(6)?,
            });
        }
        Ok(out)
    }

    /// 全市场 24h 行情（公共接口，type=MINI 权重 40）。一次返回全部现货对，
    /// 供 WebUI 市场总览；主域不可达时与K线共用镜像兜底。
    /// 注意：该接口对未知参数报 -1104（不能复用 time 的占位参数）。
    pub async fn fetch_ticker_24h(&self) -> Result<Vec<TickerQuote>, ExecError> {
        // type=MINI：响应体减半（无涨跌幅字段，解析器由开盘价折算）；
        // 响应体仍可达 ~1MB，走长超时客户端；查询串除 type 外不得带其他参数（-1104）
        const Q: &str = "type=MINI";
        let v = if self.fallback_active.load(Ordering::Relaxed) {
            let fb = self.kline_fallback_url.as_deref().unwrap_or(&self.base_url);
            self.get_json_bulk_on(fb, "/api/v3/ticker/24hr", Q).await?
        } else {
            match self.get_json_bulk_on(&self.base_url, "/api/v3/ticker/24hr", Q).await {
                Ok(v) => v,
                Err(e) => match &self.kline_fallback_url {
                    Some(fb) => {
                        eprintln!("[binance] 主域行情不可达，切换公共镜像: {fb}");
                        self.fallback_active.store(true, Ordering::Relaxed);
                        self.get_json_bulk_on(fb, "/api/v3/ticker/24hr", Q).await?
                    }
                    None => return Err(e.into()),
                },
            }
        };
        Ok(parse_ticker_24h(&v)?)
    }

    /// 指定窗口的K线（翻页用）：返回 open_time <= `end_time` 的最后 `limit` 根。
    pub async fn fetch_klines_window(
        &self,
        symbol: &str,
        interval: &str,
        limit: u32,
        end_time: u64,
    ) -> Result<Vec<Kline>, ExecError> {
        let query = format!("symbol={symbol}&interval={interval}&limit={limit}&endTime={end_time}");
        self.klines_query(&query).await
    }

    /// 全量历史K线：按 endTime 向前分页，最多取 max_bars 根（或取尽）。
    /// 供 dry-run/live 全量重放：评估相位锚定历史起点，等价于一直运行的进程，
    /// 避免窗口起点依赖造成的调仓信号滞后。
    /// `end_time` 为 None 时从最新bar开始。
    pub async fn fetch_klines_history(
        &self,
        symbol: &str,
        interval: &str,
        max_bars: u32,
        end_time: Option<u64>,
    ) -> Result<Vec<Kline>, ExecError> {
        let mut out: Vec<Kline> = Vec::new();
        let mut end_time: Option<u64> = end_time;
        loop {
            let mut query = format!("symbol={symbol}&interval={interval}&limit=1000");
            if let Some(et) = end_time {
                query.push_str(&format!("&endTime={et}"));
            }
            let page = self.klines_query(&query).await?;
            let n = page.len();
            if n == 0 {
                break;
            }
            let oldest = page[0].open_time;
            out.splice(0..0, page); // 旧页插到前面（时间升序）
            if n < 1000 || out.len() as u32 >= max_bars {
                break;
            }
            end_time = Some(oldest.saturating_sub(1));
        }
        if out.len() > max_bars as usize {
            out.drain(0..out.len() - max_bars as usize);
        }
        Ok(out)
    }

    async fn get_json_signed(
        &self,
        path: &str,
        extra: &BTreeMap<String, String>,
    ) -> Result<Value, BinanceError> {
        let (key, secret) = self.credentials()?;
        let mut params = extra.clone();
        params.insert("timestamp".into(), now_ms().to_string());
        params.insert("recvWindow".into(), self.recv_window.to_string());
        let query = build_query(&params);
        let signature = sign(secret, &query);
        let url = format!("{}{}?{}&signature={}", self.base_url, path, query, signature);
        let mut last_err = BinanceError::Network("未发起请求".into());
        for attempt in 0..=self.max_retries {
            self.rate.acquire().await;
            match self.http.get(&url).header("X-MBX-APIKEY", key).send().await {
                Ok(resp) => {
                    let status = resp.status();
                    let text = resp.text().await.unwrap_or_default();
                    if status.is_success() {
                        // 镜像偶发 HTTP 200 返回空 body：视为瞬时网络故障进入重试
                        if !text.trim().is_empty() {
                            return serde_json::from_str(&text)
                                .map_err(|e| BinanceError::Parse(format!("{e}: {text}")));
                        }
                        last_err = BinanceError::Network("HTTP 200 但响应体为空".into());
                    } else if status.is_server_error() {
                        last_err = BinanceError::Network(format!("HTTP {status}: {text}"));
                    } else {
                        let code = serde_json::from_str::<Value>(&text)
                            .ok()
                            .and_then(|v| v.get("code").and_then(|c| c.as_i64()))
                            .unwrap_or(status.as_u16() as i64);
                        let msg = serde_json::from_str::<Value>(&text)
                            .ok()
                            .and_then(|v| v.get("msg").and_then(|m| m.as_str().map(String::from)))
                            .unwrap_or(text.clone());
                        return Err(BinanceError::Api { code, msg });
                    }
                }
                Err(e) => last_err = BinanceError::Network(e.to_string()),
            }
            if attempt < self.max_retries {
                tokio::time::sleep(Duration::from_millis(200 << attempt)).await;
            }
        }
        Err(last_err)
    }
}

#[async_trait]
impl MarketData for BinanceClient {
    async fn fetch_klines(
        &self,
        symbol: &str,
        interval: &str,
        limit: u32,
    ) -> Result<Vec<Kline>, ExecError> {
        let query = format!("symbol={symbol}&interval={interval}&limit={limit}");
        self.klines_query(&query).await
    }

    async fn fetch_last_price(&self, symbol: &str) -> Result<f64, ExecError> {
        use std::sync::atomic::Ordering;
        let query = format!("symbol={symbol}");
        // 主域 -> 镜像（与K线同兜底路径）；镜像粘性保持时直接走镜像，避免先超时一次
        let v = if self.fallback_active.load(Ordering::Relaxed) {
            let fb = self.kline_fallback_url.as_deref().unwrap_or(&self.base_url);
            self.get_json_on(fb, "/api/v3/ticker/price", &query).await
        } else {
            match self.get_json("/api/v3/ticker/price", &query).await {
                Ok(v) => Ok(v),
                Err(e) => match &self.kline_fallback_url {
                    Some(fb) => self.get_json_on(fb, "/api/v3/ticker/price", &query).await,
                    None => Err(e),
                },
            }
        };
        if let Ok(v) = v {
            if let Some(p) = v.get("price").and_then(|p| p.as_str()).and_then(|s| s.parse().ok()) {
                return Ok(p);
            }
        }
        // 末端兜底：以最近一根 1m K线收盘价为最新价（最多滞后 1 分钟，监控盯市够用；
        // klines_query 自带镜像兜底，受限网络下不至于拿不到价）
        let ks = self
            .klines_query(&format!("symbol={symbol}&interval=1m&limit=1"))
            .await?;
        ks.last()
            .map(|k| k.close)
            .ok_or_else(|| BinanceError::Parse("K线为空，无法推导最新价".into()).into())
    }
}

/// RESULT 下单响应 / 查单响应的 fills 数组（可能多笔分撮合）聚合为单笔成交
fn aggregate_fills(v: &Value, order: &Order) -> Result<Fill, BinanceError> {
    let fills = v
        .get("fills")
        .and_then(|f| f.as_array())
        .ok_or_else(|| BinanceError::Parse("order 响应缺少 fills".into()))?;
    let mut qty = 0.0;
    let mut cost = 0.0;
    let mut fee = 0.0;
    for f in fills {
        let q: f64 = f["qty"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0);
        let p: f64 = f["price"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0);
        let c: f64 = f["commission"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0);
        qty += q;
        cost += q * p;
        fee += c;
    }
    if qty <= 0.0 {
        return Err(BinanceError::Parse("成交数量为 0".into()));
    }
    Ok(Fill {
        symbol: order.symbol.clone(),
        side: order.side,
        quantity: qty,
        price: cost / qty,
        fee,
        timestamp: v
            .get("transactTime")
            .or_else(|| v.get("time"))
            .and_then(|t| t.as_u64())
            .unwrap_or_else(now_ms),
    })
}

#[async_trait]
impl Broker for BinanceClient {
    /// 市价下单（live 通道使用）。数量须调用方按 stepSize 取整。
    ///
    /// `client_order_id` 为幂等 ID：网络超时后可能"请求已送达成交但响应丢失"，
    /// 此时凭该 ID 查询恢复真实成交，绝不盲目重发；上层重试须复用同一 ID。
    async fn place_order(
        &mut self,
        order: &Order,
        client_order_id: Option<&str>,
    ) -> Result<Fill, ExecError> {
        let side = match order.side {
            Side::Buy => "BUY",
            Side::Sell => "SELL",
        };
        let mut params = BTreeMap::new();
        params.insert("symbol".into(), order.symbol.clone());
        params.insert("side".into(), side.into());
        params.insert("type".into(), "MARKET".into());
        params.insert("quantity".into(), format!("{}", order.quantity));
        // 使用 FULL 类型以获取 fills 数组（RESULT 不包含fills，会导致解析失败）
        params.insert("newOrderRespType".into(), "FULL".into());
        if let Some(id) = client_order_id {
            params.insert("newClientOrderId".into(), id.to_string());
        }
        match self.post_signed("/api/v3/order", &mut params).await {
            Ok(v) => Ok(aggregate_fills(&v, order)?),
            Err(BinanceError::Network(e)) => {
                if let Some(id) = client_order_id {
                    match self.fetch_order_by_client_id(&order.symbol, id).await {
                        Ok(Some(v)) => return Ok(aggregate_fills(&v, order)?),
                        Ok(None) => {} // 订单未提交：报原错误，上层可用同一 ID 安全重试
                        Err(qe) => {
                            return Err(ExecError::Exchange(format!(
                                "下单超时且幂等查询失败: {e}; 查询错误: {qe}。请人工核对交易所订单记录！"
                            )));
                        }
                    }
                }
                Err(BinanceError::Network(e).into())
            }
            Err(e) => Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sign_deterministic() {
        // 固定输入固定输出（签名确定性 + BTreeMap 排序前提）
        let sig = sign("secret", "symbol=BTCUSDT&side=BUY&timestamp=1");
        assert_eq!(sig.len(), 64);
        assert_eq!(sig, sign("secret", "symbol=BTCUSDT&side=BUY&timestamp=1"));
        assert_ne!(sig, sign("other", "symbol=BTCUSDT&side=BUY&timestamp=1"));
        // 与已知向量对照（python hmac 预计算）
        let known = sign("NhqPtmdSJYdKjVHjA7PZj4Mge3R5YNiP1e3UZjInClVN65XAbvqqM6A7H5fATj0j",
            "symbol=LTCBTC&side=BUY&type=LIMIT&timeInForce=GTC&quantity=1&price=0.1&recvWindow=5000&timestamp=1499827319559");
        assert_eq!(known, "c8db56825ae71d6d79447849e617115f4a920fa2acdcab2b053c4b2838bd6b71");
    }

    #[test]
    fn test_build_query_sorted() {
        let mut p = BTreeMap::new();
        p.insert("b".into(), "2".into());
        p.insert("a".into(), "1".into());
        assert_eq!(build_query(&p), "a=1&b=2");
    }

    #[test]
    fn test_parse_ticker_24h() {
        let v: Value = serde_json::from_str(
            r#"[{"symbol":"BTCUSDT","lastPrice":"60000.50","priceChangePercent":"1.250","quoteVolume":"123456789.0","highPrice":"61000.0","lowPrice":"59000.0"},
                {"symbol":"ETHBTC","lastPrice":"0.05","priceChangePercent":"-0.5","quoteVolume":"99.1"},
                {"symbol":"SOLUSDT","lastPrice":"110.0","openPrice":"100.0","quoteVolume":"1.0"},
                {"lastPrice":"1.0"}]"#,
        )
        .unwrap();
        let qs = parse_ticker_24h(&v).unwrap();
        // 缺 symbol 的行被跳过
        assert_eq!(qs.len(), 3);
        assert_eq!(qs[0].symbol, "BTCUSDT");
        assert!((qs[0].last_price - 60_000.5).abs() < 1e-9);
        assert!((qs[0].price_change_pct - 1.25).abs() < 1e-9);
        // 24h 高低点（FULL/MINI 均提供）
        assert!((qs[0].high_price - 61_000.0).abs() < 1e-9);
        assert!((qs[0].low_price - 59_000.0).abs() < 1e-9);
        assert!((qs[1].quote_volume - 99.1).abs() < 1e-9);
        // MINI 无涨跌幅字段：由 (最新价-开盘价)/开盘价 折算，110 对 100 = +10%
        assert_eq!(qs[2].symbol, "SOLUSDT");
        assert!((qs[2].price_change_pct - 10.0).abs() < 1e-9);
        // 非数组响应应报错
        assert!(parse_ticker_24h(&Value::Null).is_err());
    }

    #[test]
    fn test_round_step() {
        assert_eq!(round_step(0.123456, 0.001), 0.123);
        assert_eq!(round_step(1.9999, 1.0), 1.0);
        assert_eq!(round_step(5.0, 0.01), 5.0);
        // 浮点误差不应少一步长：0.29/0.01 的二进制表示 < 29，直接除法会得 0.28
        assert_eq!(round_step(0.29, 0.01), 0.29);
        assert_eq!(round_step(0.30000000001, 0.1), 0.3);
        assert_eq!(round_step(0.0000123, 0.000001), 0.000012);
        assert_eq!(round_step(3.0, 0.0), 3.0);
    }

    #[tokio::test]
    async fn test_rate_limiter_spacing() {
        let rl = RateLimiter::new(Duration::from_millis(30));
        let t0 = now_ms();
        rl.acquire().await;
        rl.acquire().await;
        rl.acquire().await;
        // 3 次请求至少间隔 2 * 30ms（留 10ms 容差防时钟抖动）
        assert!(now_ms() - t0 >= 50, "elapsed={}", now_ms() - t0);
    }

    #[test]
    fn test_now_ms_returns_positive() {
        // now_ms 应该返回正数（Unix 时间戳毫秒）
        let ts = now_ms();
        assert!(ts > 1_000_000_000_000); // 大于 2001-09-09
        // u64 总是有限的，无需 is_finite 检查
    }

    #[test]
    fn test_round_step_edge_cases() {
        // 零步长：原样返回
        assert_eq!(round_step(1.5, 0.0), 1.5);
        
        // NaN/Inf：原样返回
        assert!(round_step(f64::NAN, 0.01).is_nan());
        assert_eq!(round_step(f64::INFINITY, 0.01), f64::INFINITY);
        
        // 负步长：原样返回
        assert_eq!(round_step(1.5, -0.01), 1.5);
        
        // 极小步长
        assert_eq!(round_step(0.00000001, 0.00000001), 0.00000001);
        
        // 数量为零
        assert_eq!(round_step(0.0, 0.01), 0.0);
        
        // 步长大于数量
        assert_eq!(round_step(0.005, 0.01), 0.0);
    }

    #[test]
    fn test_round_step_precision_various_decimals() {
        // 不同小数位数的步长
        assert_eq!(round_step(1.23456789, 0.1), 1.2);      // 1 位小数
        assert_eq!(round_step(1.23456789, 0.01), 1.23);    // 2 位小数
        assert_eq!(round_step(1.23456789, 0.001), 1.234);  // 3 位小数
        assert_eq!(round_step(1.23456789, 0.0001), 1.2345);// 4 位小数
        
        // 整数步长
        assert_eq!(round_step(123.456, 1.0), 123.0);
        assert_eq!(round_step(123.456, 10.0), 120.0);
        assert_eq!(round_step(123.456, 100.0), 100.0);
    }

    #[test]
    fn test_parse_ticker_empty_array() {
        // 空数组应返回空结果
        let v: Value = serde_json::from_str("[]").unwrap();
        let qs = parse_ticker_24h(&v).unwrap();
        assert!(qs.is_empty());
    }

    #[test]
    fn test_parse_ticker_missing_fields() {
        // 缺少部分字段的行应使用默认值 0.0
        let v: Value = serde_json::from_str(
            r#"[{"symbol":"TESTUSDT","lastPrice":"100.0"}]"#,
        )
        .unwrap();
        let qs = parse_ticker_24h(&v).unwrap();
        assert_eq!(qs.len(), 1);
        assert_eq!(qs[0].symbol, "TESTUSDT");
        assert!((qs[0].last_price - 100.0).abs() < 1e-9);
        assert!((qs[0].quote_volume - 0.0).abs() < 1e-9);
        assert!((qs[0].high_price - 0.0).abs() < 1e-9);
        assert!((qs[0].low_price - 0.0).abs() < 1e-9);
        // 无 priceChangePercent 且无 openPrice，涨跌幅应为 0
        assert!((qs[0].price_change_pct - 0.0).abs() < 1e-9);
    }

    #[test]
    fn test_parse_ticker_invalid_numbers() {
        // 无效数字应解析为 0.0
        let v: Value = serde_json::from_str(
            r#"[{"symbol":"TESTUSDT","lastPrice":"invalid","priceChangePercent":"abc","quoteVolume":"xyz"}]"#,
        )
        .unwrap();
        let qs = parse_ticker_24h(&v).unwrap();
        assert_eq!(qs.len(), 1);
        assert!((qs[0].last_price - 0.0).abs() < 1e-9);
        assert!((qs[0].price_change_pct - 0.0).abs() < 1e-9);
    }

    #[test]
    fn test_parse_ticker_zero_open_price() {
        // MINI 模式下开盘价为 0 时，涨跌幅应为 0（避免除零）
        let v: Value = serde_json::from_str(
            r#"[{"symbol":"TESTUSDT","lastPrice":"100.0","openPrice":"0.0"}]"#,
        )
        .unwrap();
        let qs = parse_ticker_24h(&v).unwrap();
        assert_eq!(qs.len(), 1);
        assert!((qs[0].price_change_pct - 0.0).abs() < 1e-9);
    }

    #[test]
    fn test_parse_ticker_negative_change() {
        // 测试下跌场景
        let v: Value = serde_json::from_str(
            r#"[{"symbol":"TESTUSDT","lastPrice":"90.0","openPrice":"100.0"}]"#,
        )
        .unwrap();
        let qs = parse_ticker_24h(&v).unwrap();
        assert_eq!(qs.len(), 1);
        assert!((qs[0].price_change_pct - (-10.0)).abs() < 1e-9);
    }

    #[test]
    fn test_build_query_empty() {
        // 空参数应返回空字符串
        let p = BTreeMap::new();
        assert_eq!(build_query(&p), "");
    }

    #[test]
    fn test_build_query_special_chars() {
        // 特殊字符应保持原样（URL 编码由 HTTP 客户端处理）
        let mut p = BTreeMap::new();
        p.insert("key".into(), "value with spaces".into());
        p.insert("symbol".into(), "BTC/USDT".into());
        let q = build_query(&p);
        assert!(q.contains("key=value with spaces"));
        assert!(q.contains("symbol=BTC/USDT"));
    }

    #[test]
    fn test_sign_empty_payload() {
        // 空 payload 也应能生成有效签名
        let sig = sign("secret", "");
        assert_eq!(sig.len(), 64);
        assert_ne!(sig, sign("other", ""));
    }

    #[test]
    fn test_sign_unicode() {
        // Unicode 内容签名
        let sig = sign("secret", "symbol=测试USDT&quantity=1.5");
        assert_eq!(sig.len(), 64);
        // 确定性
        assert_eq!(sig, sign("secret", "symbol=测试USDT&quantity=1.5"));
    }

    #[test]
    fn test_binance_error_to_exec_error() {
        // Network 错误转换
        let net_err = BinanceError::Network("connection timeout".into());
        let exec: ExecError = net_err.into();
        match exec {
            ExecError::Exchange(msg) => assert!(msg.contains("connection timeout")),
            _ => panic!("Expected Exchange variant"),
        }

        // Api 错误转换
        let api_err = BinanceError::Api { code: -1001, msg: "Unknown order".into() };
        let exec: ExecError = api_err.into();
        match exec {
            ExecError::Exchange(msg) => assert!(msg.contains("-1001") && msg.contains("Unknown order")),
            _ => panic!("Expected Exchange variant"),
        }

        // Parse 错误转换
        let parse_err = BinanceError::Parse("invalid JSON".into());
        let exec: ExecError = parse_err.into();
        match exec {
            ExecError::Exchange(msg) => assert!(msg.contains("invalid JSON")),
            _ => panic!("Expected Exchange variant"),
        }
    }

    #[test]
    fn test_ticker_quote_serialization() {
        // 测试 TickerQuote 可序列化（注意：TickerQuote 只实现了 Serialize，未实现 Deserialize）
        let ticker = TickerQuote {
            symbol: "BTCUSDT".to_string(),
            last_price: 60000.5,
            price_change_pct: 1.25,
            quote_volume: 123456789.0,
            high_price: 61000.0,
            low_price: 59000.0,
        };
        
        let json = serde_json::to_string(&ticker).unwrap();
        assert!(json.contains("BTCUSDT"));
        assert!(json.contains("60000.5"));
        assert!(json.contains("price_change_pct"));
        assert!(json.contains("quote_volume"));
    }

    #[tokio::test]
    async fn test_rate_limiter_concurrent() {
        use std::sync::Arc;
        
        let rl = Arc::new(RateLimiter::new(Duration::from_millis(50)));
        let mut handles = vec![];
        
        // 并发发起 5 次 acquire
        for i in 0..5 {
            let rl_clone = rl.clone();
            let handle = tokio::spawn(async move {
                let ts = rl_clone.acquire().await;
                (i, ts)
            });
            handles.push(handle);
        }
        
        // 等待所有任务完成
        let mut results = Vec::new();
        for handle in handles {
            if let Ok(result) = handle.await {
                results.push(result);
            }
        }
        
        // 所有时间戳应该不同（CAS 保证串行化）
        let mut timestamps: Vec<u64> = results.iter().map(|(_, ts)| *ts).collect();
        timestamps.sort();
        timestamps.dedup();
        assert_eq!(timestamps.len(), 5, "所有 acquire 应获得不同时间槽");
        
        // 相邻时间戳间隔至少 50ms
        for i in 1..timestamps.len() {
            assert!(timestamps[i] - timestamps[i-1] >= 45, "间隔过小: {}", timestamps[i] - timestamps[i-1]);
        }
    }

    #[test]
    fn test_rate_limiter_zero_interval() {
        // 零间隔限频器应允许立即连续调用
        let rt = tokio::runtime::Runtime::new().unwrap();
        let rl = RateLimiter::new(Duration::from_millis(0));
        
        rt.block_on(async {
            let t0 = now_ms();
            rl.acquire().await;
            rl.acquire().await;
            rl.acquire().await;
            // 3 次调用应在 10ms 内完成
            assert!(now_ms() - t0 < 10);
        });
    }
}
