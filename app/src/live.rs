//! live 实盘通道。
//!
//! 安全设计：
//! - 总开关 `live_enabled` 默认 false，必须在配置文件中显式开启
//! - 启动自检：时钟偏移、USDT 余额、各品种下单精度（stepSize）
//! - 启动对账：本地状态持仓与交易所实际可用余额比对，漂移告警；
//!   `live_auto_heal = true` 时以交易所真实数量重写本地状态（自愈），
//!   并接管未记录的池内资产为持仓（市值 <$10 的灰尘资产跳过）
//! - 二次确认：打印风险提示后必须手动输入 YES 才进入主循环
//! - 密钥：环境变量优先，回退配置文件 `[binance]` 段（BINANCE_API_KEY / BINANCE_API_SECRET）
//!
//! 决策方式与 dry-run 相同（收盘确认后确定性重放得出目标持仓），
//! 区别在执行：目标持仓与当前持仓做差量，经 [`Broker`] 真实下单，
//! 数量按 stepSize 取整并按交易所真实可用余额钳制；成交与状态原子持久化，
//! 重启不重复下单。

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use quantkit_core::engine::{run_backtest, BacktestConfig, FillPrice};
use quantkit_core::executor::FillModel;
use quantkit_core::interval::Interval;
use quantkit_core::types::{Order, Position, Side};
use quantkit_exchanges::binance::{now_ms, round_step, BinanceClient, MyTrade, OpenOrder};
use quantkit_exchanges::traits::MarketData;

use crate::config::AppConfig;
use crate::events::{DomainEvent, QuantKitEventBus};
use crate::notify::{fill_message, Notifier};

// 日志宏导入
use crate::{action, error, info, warn};

/// 单笔真实成交记录（审计用）；`reason` 记录信号来源，供监控页与通知展示。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiveFill {
    pub ts: u64,
    pub symbol: String,
    pub side: Side,
    pub quantity: f64,
    pub price: f64,
    pub fee: f64,
    /// 成交原因（旧状态文件无此字段，反序列化默认空）
    #[serde(default)]
    pub reason: String,
}

/// 权益快照（每轮盯市记录一点，构成实盘权益曲线）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EquitySnap {
    pub ts: u64,
    /// 总资产 = USDT 可用 + 持仓市值（最新价）
    pub total: f64,
    pub usdt: f64,
    pub positions_value: f64,
}

/// live 状态（原子持久化）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LiveState {
    pub last_bar_ts: u64,
    pub positions: Vec<Position>,
    pub fills: Vec<LiveFill>,
    /// 权益曲线（旧状态文件无此字段，反序列化默认空）
    #[serde(default)]
    pub equity_history: Vec<EquitySnap>,
    pub updated_at_ms: u64,
    /// 场外订单同步：每品种已同步到的最大 myTrades tradeId（增量查询水位）
    #[serde(default)]
    pub trade_watermarks: BTreeMap<String, u64>,
    /// 程序自下订单登记：品种 -> [(orderId, 该订单响应的最大 tradeId)]。
    /// myTrades 增量行按 orderId 命中即"自己的单"（已入账，跳过），否则为场外成交；
    /// 水印推过 max_trade_id 后该条目可安全裁剪。
    #[serde(default)]
    pub own_orders: BTreeMap<String, Vec<(u64, u64)>>,
    /// 已通知过的在途挂单 orderId（重启不重复通知）
    #[serde(default)]
    pub known_open_order_ids: Vec<u64>,
}

/// 对账报告：漂移描述 + 自愈后的持仓（以交易所真实可用数量为准）
#[derive(Debug, Clone, Default)]
pub struct ReconReport {
    pub drifts: Vec<String>,
    pub synced: Vec<Position>,
    /// 交易所持有而状态未记录的池内资产（品种，可用数量）：
    /// 默认只告警；开启自愈时以最新价接管为持仓（见 [`run`]）
    pub unrecorded: Vec<(String, f64)>,
}

/// 从交易对提取本位资产（BTCUSDT -> BTC；非 USDT 对原样返回）
pub fn base_of(sym: &str) -> &str {
    sym.strip_suffix("USDT").filter(|s| !s.is_empty()).unwrap_or(sym)
}

/// 账户对账（纯函数，可单测）：比对本地状态持仓与交易所余额（可用+冻结总额）。
///
/// 用总额而非仅可用：手动限价挂单会锁定余额（如全额挂卖时 free=0），
/// 只看 free 会把"持仓在、挂单中"误判为"已在场外卖出"。
///
/// 漂移来源通常是：手续费以本币抵扣、人工在交易所买卖、历史漏记成交。
/// `synced` 为以真实数量重写后的持仓（自愈用）；交易所存在而状态未记录的
/// 池内资产告警并收入 `unrecorded`，是否接管由调用方根据自愈开关决定。
pub fn reconcile(
    symbols: &[String],
    local: &[Position],
    balances: &BTreeMap<String, (f64, f64)>,
) -> ReconReport {
    let mut report = ReconReport::default();
    // 数量相对偏差容忍度：0.1%（正常成交与状态应严格一致，偏差即漂移）
    const TOL: f64 = 0.001;
    let mut tracked_bases: Vec<String> = Vec::new();
    for p in local {
        let base = base_of(&p.symbol).to_string();
        tracked_bases.push(base.clone());
        match balances.get(&base) {
            None => {
                report.drifts.push(format!(
                    "状态记录持仓 {} 量{:.8}，但交易所无余额（可用+冻结均为 0，可能已在场外卖出）",
                    p.symbol, p.quantity
                ));
            }
            Some(&(free, locked)) => {
                let actual = free + locked;
                if p.quantity > 0.0 && (actual - p.quantity).abs() / p.quantity > TOL {
                    report.drifts.push(format!(
                        "{} 数量偏差：状态 {:.8} / 交易所总额 {:.8}（可用 {:.8} + 冻结 {:.8}）",
                        p.symbol, p.quantity, actual, free, locked
                    ));
                }
                // 自愈：以交易所真实总额为准（入场价保留原记录）
                report.synced.push(Position {
                    symbol: p.symbol.clone(),
                    quantity: actual,
                    avg_entry_price: p.avg_entry_price,
                });
            }
        }
    }
    // 交易所持有而状态未记录的本位资产（限策略池内品种，避免无关币种刷屏）
    for sym in symbols {
        let base = base_of(sym).to_string();
        if tracked_bases.contains(&base) {
            continue;
        }
        if let Some(&(free, locked)) = balances.get(&base) {
            let amt = free + locked;
            if amt <= 0.0 {
                continue;
            }
            report.drifts.push(format!(
                "交易所有余额 {base} {amt:.8}（可用 {free:.8} + 冻结 {locked:.8}），但状态未记录持仓（可能人工买入或历史漏记）"
            ));
            report.unrecorded.push((sym.clone(), amt));
        }
    }
    report
}

// ==================== 场外订单同步（手动单检测与入账） ====================

/// 按时间序把场外成交逐笔应用到持仓（纯函数，可单测）。
/// 买入：加权均价加仓/建仓；卖出：减仓/清仓。
/// 超卖截断为 0 并产出告警（流水仍记真实全量，审计完整）；
/// 无持仓记录的卖出不产生负仓位，仅告警。
pub fn apply_external_trades(
    positions: &mut Vec<Position>,
    trades: &[(String, MyTrade)],
) -> Vec<String> {
    let mut warnings = Vec::new();
    let mut sorted: Vec<&(String, MyTrade)> = trades.iter().collect();
    sorted.sort_by_key(|(_, t)| t.time);
    for (sym, t) in sorted {
        if t.is_buyer {
            match positions.iter_mut().find(|p| &p.symbol == sym) {
                Some(p) => {
                    let new_qty = p.quantity + t.qty;
                    if new_qty > 0.0 {
                        p.avg_entry_price =
                            (p.quantity * p.avg_entry_price + t.qty * t.price) / new_qty;
                    }
                    p.quantity = new_qty;
                }
                None => positions.push(Position {
                    symbol: sym.clone(),
                    quantity: t.qty,
                    avg_entry_price: t.price,
                }),
            }
        } else {
            match positions.iter_mut().find(|p| &p.symbol == sym) {
                Some(p) => {
                    if t.qty >= p.quantity * 0.999 {
                        if t.qty > p.quantity * 1.001 {
                            warnings.push(format!(
                                "场外卖出 {} 数量 {:.8} 超过记录持仓 {:.8}，仓位截断为 0",
                                sym, t.qty, p.quantity
                            ));
                        }
                        p.quantity = 0.0;
                    } else {
                        p.quantity -= t.qty;
                    }
                }
                None => warnings.push(format!(
                    "场外卖出 {} 数量 {:.8}，但本地无持仓记录（可能历史买入未被跟踪）",
                    sym, t.qty
                )),
            }
        }
    }
    positions.retain(|p| p.quantity > 0.0);
    warnings
}

/// 场外成交按（品种, 方向）聚合成流水条目（纯函数，可单测）。
/// 手续费按数值累加；若含非 USDT 抵扣资产（如 BNB），在 reason 中说明。
pub fn aggregate_external_fills(trades: &[(String, MyTrade)]) -> Vec<LiveFill> {
    // 按（品种, 方向）聚合的累加器
    #[derive(Default)]
    struct Acc {
        qty: f64,
        cost: f64,
        fee: f64,
        ts: u64,
        fee_mixed: bool,
    }
    let mut groups: BTreeMap<(String, bool), Acc> = BTreeMap::new();
    for (sym, t) in trades {
        let e = groups.entry((sym.clone(), t.is_buyer)).or_default();
        e.qty += t.qty;
        e.cost += t.qty * t.price;
        e.fee += t.commission;
        e.ts = e.ts.max(t.time);
        if t.commission > 0.0 && t.commission_asset != "USDT" && !t.commission_asset.is_empty() {
            e.fee_mixed = true;
        }
    }
    groups
        .into_iter()
        .map(|((sym, is_buyer), a)| {
            let mut reason = "场外手动单".to_string();
            if a.fee_mixed {
                reason.push_str("（含非USDT手续费资产）");
            }
            LiveFill {
                ts: a.ts,
                symbol: sym,
                side: if is_buyer { Side::Buy } else { Side::Sell },
                quantity: a.qty,
                price: if a.qty > 0.0 { a.cost / a.qty } else { 0.0 },
                fee: a.fee,
                reason,
            }
        })
        .collect()
}

/// 首次水印初始化：从最旧向新翻页，取最大 tradeId 作为水印。
/// 不回补历史成交（历史漏记由启动对账接管）。无成交返回 0。
/// 翻页超上限（10 万笔）报错，由调用方下轮重试——绝不把水印停在历史中间。
async fn bootstrap_watermark(client: &BinanceClient, symbol: &str) -> Result<u64, String> {
    let mut from: Option<u64> = None;
    let mut max_id = 0u64;
    for _ in 0..100 {
        let page = client
            .fetch_my_trades(symbol, from, 1000)
            .await
            .map_err(|e| e.to_string())?;
        let n = page.len();
        if let Some(m) = page.iter().map(|t| t.id).max() {
            max_id = max_id.max(m);
            from = Some(m + 1);
        }
        if n < 1000 {
            return Ok(max_id);
        }
    }
    Err(format!("{symbol} 成交历史超过 10 万笔，水印初始化放弃（请人工处理）"))
}

/// 场外成交同步（每轮）：对池内品种增量拉取 myTrades，按 orderId 分类
/// （命中程序自下订单登记的跳过），手动成交纳入持仓与流水并告警。
/// 补偿链路：单品种失败只告警、水印不动、下轮重试，绝不阻断调仓主路。
async fn sync_external_trades(
    cfg: &AppConfig,
    client: &BinanceClient,
    st: &mut LiveState,
    state_path: &Path,
    notify: &Notifier,
) {
    let mut external: Vec<(String, MyTrade)> = Vec::new();
    let mut changed = false;

    for sym in &cfg.symbols {
        // 惰性初始化水印：首次见到该品种时翻全历史定位水位（不回补历史）
        if !st.trade_watermarks.contains_key(sym) {
            match bootstrap_watermark(client, sym).await {
                Ok(wm) => {
                    info!("🧭 {sym} 成交水印初始化: {wm}（此前历史不回填，漏记由对账接管）");
                    st.trade_watermarks.insert(sym.clone(), wm);
                    changed = true;
                }
                Err(e) => {
                    warn!("⏭️ {sym} 水印初始化失败（下轮重试）: {e}");
                    continue;
                }
            }
        }
        let wm = match st.trade_watermarks.get(sym) {
            Some(&w) => w,
            None => continue,
        };

        // 增量拉取（fromId 语义含边界值，后面按 id > wm 去重；整页续翻，每轮最多 3 页）
        let mut rows: Vec<MyTrade> = Vec::new();
        let mut from = wm + 1;
        let mut failed = false;
        for _ in 0..3 {
            match client.fetch_my_trades(sym, Some(from), 1000).await {
                Ok(page) => {
                    let n = page.len();
                    if let Some(m) = page.iter().map(|t| t.id).max() {
                        from = m + 1;
                    }
                    rows.extend(page);
                    if n < 1000 {
                        break;
                    }
                }
                Err(e) => {
                    warn!("⏭️ {sym} myTrades 拉取失败（水印不动，下轮重试）: {e}");
                    failed = true;
                    break;
                }
            }
        }
        if failed {
            continue;
        }

        let own_ids: Vec<u64> = st
            .own_orders
            .get(sym)
            .map(|v| v.iter().map(|(o, _)| *o).collect())
            .unwrap_or_default();
        let mut max_id = wm;
        for t in rows {
            if t.id <= wm {
                continue;
            }
            max_id = max_id.max(t.id);
            if own_ids.contains(&t.order_id) {
                continue; // 程序自下单：下单时已入账
            }
            external.push((sym.clone(), t));
        }
        if max_id > wm {
            st.trade_watermarks.insert(sym.clone(), max_id);
            changed = true;
        }
    }

    // 裁剪自有订单登记：max_trade_id 已被水印覆盖的条目不会再被查询命中。
    // （0 = 幂等恢复路径无法得知，保留以保证分类正确）
    for (sym, orders) in st.own_orders.iter_mut() {
        let wm = st.trade_watermarks.get(sym).copied().unwrap_or(0);
        let before = orders.len();
        orders.retain(|&(_, max_tid)| max_tid == 0 || max_tid > wm);
        if orders.len() != before {
            changed = true;
        }
    }

    if !external.is_empty() {
        let fills = aggregate_external_fills(&external);
        let warnings = apply_external_trades(&mut st.positions, &external);
        for f in &fills {
            let side = match f.side {
                Side::Buy => "买入",
                Side::Sell => "卖出",
            };
            action!(
                "🖐️",
                "检测到场外成交: {} {} | 量 {:.8} | 价 {:.6} | 费 {:.6}",
                f.symbol, side, f.quantity, f.price, f.fee
            );
            notify
                .send(&fill_message(&f.symbol, side, f.quantity, f.price, f.fee, &f.reason))
                .await;
        }
        for w in &warnings {
            warn!("🖐️ {w}");
            notify.send(&format!("[quantkit 实盘] {w}")).await;
        }
        st.fills.extend(fills);
        changed = true;
    }

    if changed {
        st.updated_at_ms = now_ms();
        if let Err(e) = save_state_atomic(state_path, st) {
            error!("场外同步后状态保存失败: {e}");
        }
    }
}

/// 在途挂单监控（每轮）：全账户拉取一次（含 OCO 两腿），过滤池内品种，
/// 新出现的挂单日志 + Telegram 通知并持久化；消失不通知
/// （成交由 myTrades 链路通知，撤单不影响状态）。
async fn sync_open_orders(
    cfg: &AppConfig,
    client: &BinanceClient,
    st: &mut LiveState,
    state_path: &Path,
    notify: &Notifier,
) {
    let all = match client.fetch_open_orders(None).await {
        Ok(v) => v,
        Err(e) => {
            warn!("⏭️ openOrders 查询失败（本轮跳过）: {e}");
            return;
        }
    };
    let pool: Vec<&OpenOrder> = all.iter().filter(|o| cfg.symbols.contains(&o.symbol)).collect();
    let fresh: Vec<&OpenOrder> = pool
        .iter()
        .filter(|o| !st.known_open_order_ids.contains(&o.order_id))
        .copied()
        .collect();
    let cur_ids: Vec<u64> = pool.iter().map(|o| o.order_id).collect();

    for o in &fresh {
        let side = if o.side == "BUY" { "买入" } else { "卖出" };
        action!(
            "📋",
            "检测到在途挂单（场外下单）: {} {} | 委托价 {:.6} | 量 {:.8} | 已成交 {:.8}",
            o.symbol, side, o.price, o.orig_qty, o.executed_qty
        );
    }
    if !fresh.is_empty() {
        let summary: Vec<String> = fresh
            .iter()
            .map(|o| format!("{} {}", o.symbol, if o.side == "BUY" { "买入" } else { "卖出" }))
            .collect();
        notify
            .send(&format!(
                "[quantkit 实盘] 检测到 {} 笔在途挂单（场外下单）: {}",
                fresh.len(),
                summary.join("、")
            ))
            .await;
    }

    if cur_ids != st.known_open_order_ids {
        st.known_open_order_ids = cur_ids;
        st.updated_at_ms = now_ms();
        if let Err(e) = save_state_atomic(state_path, st) {
            error!("挂单同步后状态保存失败: {e}");
        }
    }
}

pub async fn run(cfg: &AppConfig, interval: Interval) {
    // --- 门禁：显式开关 ---
    if !cfg.live_enabled {
        error!("实盘未启用：请在配置文件中显式设置 live_enabled = true");
        return;
    }
    // 密钥：环境变量优先，回退配置文件 [binance] 段（与 binance-rust 连接方式一致）
    let (key, secret) = match crate::config::resolve_binance_keys(cfg) {
        (Some(k), Some(s)) => (k, s),
        _ => {
            error!("缺少密钥：请设置 BINANCE_API_KEY / BINANCE_API_SECRET 环境变量，或在 quantkit.toml 的 [binance] 段配置 api_key / secret_key");
            return;
        }
    };
    let mut client = BinanceClient::with_credentials(Some(key), Some(secret));
    // 通知器：配置了 Telegram 才启用，未配置为 no-op（不影响交易）
    let notify = Notifier::new(&cfg.telegram_bot_token, &cfg.telegram_chat_id);
    if notify.enabled() {
        info!("📱 Telegram 通知已启用");
    }

    // --- 启动自检 ---
    let fee_rate = match preflight(&mut client, cfg).await {
        Ok(f) => f,
        Err(_) => return,
    };

    // --- 二次确认 ---
    println!();
    action!("⚠️", "警告：即将以真实资金在 Binance 现货运行策略");
    info!("品种 {:?} | 周期 {} | 初始资金口径 {:.2} | 策略由收盘信号驱动",
        cfg.symbols, interval.as_str(), cfg.initial_cash);
    print!("确认启动请输入 YES（其他任何输入退出）: ");
    std::io::stdout().flush().ok();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() || line.trim() != "YES" {
        info!("已取消");
        return;
    }
    notify
        .send(&format!(
            "[quantkit 实盘] 已启动：品种 {:?} | 初始资金口径 {:.2} | 策略 {}",
            cfg.symbols, cfg.initial_cash, cfg.strategy
        ))
        .await;

    // --- 事件总线初始化（可选） ---
    let event_bus = if cfg.ws_enabled {
        println!("[live] 📡 事件总线已启用 (ws_enabled={})", cfg.ws_enabled);
        info!("📡 事件总线已启用");
        
        let bus = Arc::new(QuantKitEventBus::new(1024));
        
        // 初始化 K线缓存：从 REST 拉取一次历史数据
        match bus.init_kline_cache_from_rest(&client, &cfg.symbols, interval.as_str(), 2000).await {
            Ok(_) => println!("[live] ✅ K线缓存初始化成功"),
            Err(e) => {
                error!("[live] ❌ K线缓存初始化失败: {}，将使用空缓存", e);
            }
        }
        
        // 启动后台分发器
        tokio::spawn(bus.clone().run_dispatcher());
        
        // 启动 WebSocket 后台任务：接收实时数据并发布到事件总线
        let symbols_clone = cfg.symbols.clone();
        let interval_clone = interval.as_str().to_string();
        let bus_clone = bus.clone();
        tokio::spawn(async move {
            use quantkit_exchanges::binance_ws;
            
            println!("[live] [ws] 正在启动 WebSocket 后台任务...");
            
            // 创建 WebSocket 客户端
            let config = binance_ws::WsConfig::default();
            let ws_client = binance_ws::BinanceWsClient::new(config, 1024);
            
            // 获取订阅者
            let mut receiver = ws_client.subscribe();
            
            // 在后台运行 WebSocket
            let symbols_vec = symbols_clone.clone();
            let interval_str = interval_clone.clone();
            tokio::spawn(async move {
                if let Err(e) = ws_client.run(&symbols_vec, &interval_str).await {
                    eprintln!("[live] [ws] WebSocket 运行错误: {}", e);
                }
            });
            
            // 监听消息并发布到事件总线
            println!("[live] [ws] WebSocket 已启动，开始监听消息...");
            loop {
                match receiver.recv().await {
                    Ok((symbol, kline)) => {
                        let event = DomainEvent::KlineUpdate {
                            symbol: symbol.clone(),
                            kline: kline.clone(),
                        };
                        
                        if let Err(e) = bus_clone.publish(event).await {
                            eprintln!("[live] [ws] 发布事件失败: {}", e);
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        eprintln!("[live] [ws] WebSocket 通道已关闭");
                        break;
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        eprintln!("[live] [ws] 消息滞后，丢弃 {} 条旧消息", n);
                        continue;
                    }
                }
            }
        });
        
        Some(bus)
    } else {
        println!("[live] 📡 事件总线未启用 (ws_enabled={})", cfg.ws_enabled);
        None
    };

    // --- 启动对账：本地状态与交易所真实持仓比对（失败只告警不阻断） ---
    let state_path = PathBuf::from(&cfg.state_file)
        .with_file_name(format!("live_{}", cfg.state_file));
    let mut state: Option<LiveState> = load_state(&state_path);
    if let Some(s) = &state {
        info!("📂 恢复状态: 持仓 {} 个 | 历史成交 {} 笔",
            s.positions.len(), s.fills.len());
    }
    match client.fetch_balances_full().await {
        Ok(bal) => {
            let local = state.as_ref().map(|s| s.positions.as_slice()).unwrap_or(&[]);
            let report = reconcile(&cfg.symbols, local, &bal);
            if report.drifts.is_empty() {
                action!("✅", "对账通过：状态与交易所持仓一致");
            } else {
                for d in &report.drifts {
                    warn!("对账漂移: {d}");
                }
                // 漂移聚合为一条消息推送，避免刷屏；未配置通知时 no-op
                notify
                    .send(&format!("[quantkit 实盘] 启动对账发现漂移：\n{}", report.drifts.join("\n")))
                    .await;
                if cfg.live_auto_heal {
                    // 接管未记录的池内资产：以最新价盯市，市值 <$10 的灰尘跳过（低于
                    // minQty 卖不掉，接管会导致卖出反复失败）；取价失败只告警不接管
                    let mut positions = report.synced;
                    for (sym, qty) in &report.unrecorded {
                        match client.fetch_last_price(sym).await {
                            Ok(px) if px > 0.0 => {
                                if qty * px < 10.0 {
                                    warn!("{sym} {qty:.8} 市值约 {:.2} USDT（<$10 灰尘），跳过接管，请人工处理",
                                        qty * px
                                    );
                                } else {
                                    action!("🔄", "接管未记录持仓: {sym} {qty:.8}（盯市价 {px:.6}）");
                                    positions.push(Position {
                                        symbol: sym.clone(),
                                        quantity: *qty,
                                        avg_entry_price: px,
                                    });
                                }
                            }
                            _ => error!("{sym} 取价失败，跳过接管，请人工处理"),
                        }
                    }
                    let st = state.get_or_insert_with(|| LiveState {
                        last_bar_ts: 0,
                        positions: Vec::new(),
                        fills: Vec::new(),
                        equity_history: Vec::new(),
                        updated_at_ms: now_ms(),
                        ..Default::default()
                    });
                    st.positions = positions;
                    st.updated_at_ms = now_ms();
                    if let Err(e) = save_state_atomic(&state_path, st) {
                        error!("自愈后状态保存失败: {e}");
                    } else {
                        action!("💊", "已自愈：持仓数量以交易所真实余额（可用+冻结）为准");
                    }
                } else {
                    warn!("存在漂移但未开启自愈（live_auto_heal = false）：继续按本地状态运行，请人工核对");
                }
            }
        }
        Err(e) => error!("对账跳过（余额查询失败）: {e}"),
    }

    // --- 主循环 ---
    // 状态文件与 dry-run 隔离，避免两种模式互相覆盖
    loop {
        if let Err(e) = run_cycle(
            cfg,
            interval,
            &mut client,
            &state_path,
            &mut state,
            fee_rate,
            &notify,
            event_bus.as_ref(), // 传入事件总线
        ).await {
            error!("本轮失败（下轮重试）: {e}");
            // 整轮失败也推送（连续失败时可结合日志排查）
            notify.send(&format!("[quantkit 实盘] 本轮执行失败（下轮重试）: {e}")).await;
        }
        tokio::time::sleep(Duration::from_secs(cfg.poll_secs)).await;
    }
}

/// 启动自检：时钟偏移 / 余额 / 下单精度 / 真实费率。返回实际使用的费率。
async fn preflight(client: &mut BinanceClient, cfg: &AppConfig) -> Result<f64, ()> {
    // 时钟：本地与服务器偏移超过 2s 将导致签名被拒（recvWindow）
    match client.fetch_server_time().await {
        Ok(server) => {
            let drift = now_ms() as i64 - server as i64;
            info!("🕐 自检-时钟偏移: {drift} ms");
            if drift.unsigned_abs() > 2_000 {
                error!("时钟偏移过大（>2s），请先校时再启动");
                return Err(());
            }
        }
        Err(e) => {
            error!("自检-无法连接交易所: {e}");
            return Err(());
        }
    }
    match client.fetch_usdt_balance().await {
        Ok(bal) => info!("💰 自检-USDT 可用余额: {bal:.2}"),
        Err(e) => {
            error!("自检-余额查询失败（密钥权限不足？）: {e}");
            return Err(());
        }
    }
    for sym in &cfg.symbols {
        match client.fetch_step_size(sym).await {
            Ok((step, min_qty)) => info!("⚙️  自检-{sym} 精度: stepSize={step} minQty={min_qty}"),
            Err(e) => {
                error!("自检-{sym} 精度查询失败: {e}");
                return Err(());
            }
        }
    }
    // 费率：拉取账户真实 taker 费率（自动含 BNB 抵扣折扣）；失败退回配置值
    let fee_rate = match client.fetch_taker_fee_rate().await {
        Ok(f) => {
            info!("💸 自检-账户实际 taker 费率: {f:.4}");
            f
        }
        Err(e) => {
            warn!("自检-费率查询失败({e})，退回配置值 {:.4}", cfg.fee_rate);
            cfg.fee_rate
        }
    };
    Ok(fee_rate)
}

/// 单轮：权益快照（盯市）-> 收盘数据重放 -> 目标持仓 -> 差量真实下单 -> 原子保存
async fn run_cycle(
    cfg: &AppConfig,
    interval: Interval,
    client: &mut BinanceClient,
    state_path: &Path,
    state: &mut Option<LiveState>,
    fee_rate: f64,
    notify: &Notifier,
    event_bus: Option<&Arc<QuantKitEventBus>>, // 改为事件总线
) -> Result<(), String> {
    let now = now_ms();

    // 1) 确保状态存在（前置：即使本轮 K线 失败，场外同步与快照仍能执行）
    if state.is_none() {
        *state = Some(LiveState {
            last_bar_ts: 0,
            positions: Vec::new(),
            fills: Vec::new(),
            equity_history: Vec::new(),
            updated_at_ms: now,
            ..Default::default()
        });
    }
    let st = state.as_mut().expect("状态已初始化");

    // 2) 场外订单同步：手动成交/在途挂单检测（补偿链路，失败只告警不阻断调仓）
    if cfg.live_sync_external {
        sync_external_trades(cfg, client, st, state_path, notify).await;
        sync_open_orders(cfg, client, st, state_path, notify).await;
    }

    // 3) 获取 K线数据：优先使用事件总线缓存，回退到 REST
    let mut data: BTreeMap<String, Vec<quantkit_core::types::Kline>> = BTreeMap::new();
    
    if let Some(bus) = event_bus {
        // 从事件总线缓存读取
        println!("[live] 📡 从事件总线缓存读取 K线数据...");
        let cache = bus.kline_cache();
        let cache_guard = cache.lock();
        
        for sym in &cfg.symbols {
            if let Some(klines) = cache_guard.get(sym) {
                // 过滤出已收盘的 K线
                let mut ks: Vec<_> = klines.iter()
                    .filter(|k| k.close_time <= now)
                    .cloned()
                    .collect();
                ks.sort_by_key(|k| k.open_time);
                println!("[live] 📡 {} 从缓存获取 {} 根K线", sym, ks.len());
                data.insert(sym.clone(), ks);
            } else {
                // 缓存中没有，回退到 REST
                println!("[live] ⚠️  {} 缓存为空，回退到 REST", sym);
                let mut ks = client.fetch_klines_history(sym, interval.as_str(), 2000, None).await
                    .map_err(|e| format!("拉取 {sym} K线失败: {e}"))?;
                ks.retain(|k| k.close_time <= now);
                data.insert(sym.clone(), ks);
            }
        }
    } else {
        // 未启用事件总线，使用原有 REST 逻辑
        for sym in &cfg.symbols {
            let mut ks = client.fetch_klines_history(sym, interval.as_str(), 2000, None).await
                .map_err(|e| format!("拉取 {sym} K线失败: {e}"))?;
            ks.retain(|k| k.close_time <= now);
            data.insert(sym.clone(), ks);
        }
    }
    
    let last_bar_ts = data
        .values()
        .flat_map(|v| v.last().map(|k| k.open_time))
        .max()
        .unwrap_or(0);

    // 4) 权益快照：每轮盯市一点（与是否同bar决策无关），构成实盘权益曲线；
    // 有新快照即落盘，监控页随时可读到最新总资产
    if snapshot_equity(client, st).await {
        st.updated_at_ms = now_ms();
        save_state_atomic(state_path, st).map_err(|e| format!("状态保存失败: {e}"))?;
    }

    // 同bar不重复决策（重启安全：已处理过的bar不再下单）
    if st.last_bar_ts == last_bar_ts {
        return Ok(());
    }

    // 5) 重放得出目标持仓（只要品种，不要模拟数量——数量用真实余额定；
    // Top-N 分散时目标可为多品种）
    let mut strategy = crate::build_strategy(cfg, interval);
    let bt = BacktestConfig {
        initial_cash: cfg.initial_cash,
        model: FillModel::new(cfg.slippage_pct, fee_rate),
        max_history: crate::history_window(cfg, interval),
        fill_price: FillPrice::SameBarClose,
        circuit_breaker_pct: cfg.circuit_breaker_pct,
        circuit_breaker_cooldown_ms: cfg.circuit_breaker_cooldown_days * 86_400_000,
        // 回测/模拟/实盘均从第一根K线就开始交易，无热身段
        signal_start_ts: 0,
    };
    let r = run_backtest(strategy.as_mut(), &data, &bt).map_err(|e| format!("重放失败: {e}"))?;
    let mut targets: Vec<String> = r.final_positions.iter().map(|p| p.symbol.clone()).collect();
    targets.sort();

    // 4) 差量执行（多持仓：集合变化才调仓，留存品种不动，与回测口径一致）
    let mut held_syms: Vec<String> = st.positions.iter().map(|p| p.symbol.clone()).collect();
    held_syms.sort();

    if targets != held_syms {
        // 拉取真实可用余额：卖出数量据此钳制（防状态漂移导致超卖）；
        // 查询失败退回状态数量（与原行为一致），不因对账故障阻断调仓
        let actual = match client.fetch_balances().await {
            Ok(b) => Some(b),
            Err(e) => {
                warn!("余额查询失败，卖出按本地状态数量执行: {e}");
                None
            }
        };

        // 3a) 先卖：退出目标集合的持仓全部卖出（回笼资金才能买入）
        let to_sell: Vec<Position> = st
            .positions
            .iter()
            .filter(|p| !targets.contains(&p.symbol))
            .cloned()
            .collect();
        for h in &to_sell {
            let (step, _) = client.fetch_step_size(&h.symbol).await
                .map_err(|e| format!("精度查询失败: {e}"))?;
            // 钳制：不超过交易所真实可用余额（交易所无该资产则跳过并告警）
            let qty = match actual.as_ref().and_then(|m| m.get(base_of(&h.symbol))) {
                Some(&avail) => {
                    if avail < h.quantity * 0.999 {
                        warn!("{} 可用余额 {:.8} 低于状态 {:.8}，按真实余额卖出",
                            h.symbol, avail, h.quantity);
                    }
                    round_step(h.quantity.min(avail), step)
                }
                None => {
                    error!("交易所无 {} 可用余额，跳过卖出（状态漂移，请人工核对）", h.symbol);
                    0.0
                }
            };
            if qty > 0.0 {
                // 确定性幂等 ID：同 bar+品种+方向重试时 ID 相同，交易所据此防重复下单
                let id = format!("qk_{}_{}_S", last_bar_ts, h.symbol);
                let reason = "信号调仓：卖出非目标品种";
                let exec = match client.place_order_with_meta(&Order::market_sell(&h.symbol, qty), Some(&id)).await {
                    Ok(x) => x,
                    Err(e) => {
                        let msg = format!("卖出 {} 失败: {e}", h.symbol);
                        notify.send(&format!("[quantkit 实盘] {msg}")).await;
                        return Err(msg);
                    }
                };
                // 登记自有订单：场外同步按 orderId 识别，避免把程序单误判为手动单
                st.own_orders.entry(h.symbol.clone()).or_default().push((exec.order_id, exec.max_trade_id));
                let fill = exec.fill;
                action!("🔴", "卖出 {} | 量 {:.8} | 价 {:.6} | 费 {:.6}",
                    fill.symbol, fill.quantity, fill.price, fill.fee);
                notify.send(&fill_message(&fill.symbol, "卖出", fill.quantity, fill.price, fill.fee, reason)).await;
                st.fills.push(LiveFill {
                    ts: fill.timestamp, symbol: fill.symbol.clone(), side: Side::Sell,
                    quantity: fill.quantity, price: fill.price, fee: fill.fee,
                    reason: reason.to_string(),
                });
            }
        }
        st.positions.retain(|p| targets.contains(&p.symbol));

        // 3b) 再买：新进入品种以真实可用余额等额分配（留存品种不调仓）
        let new_targets: Vec<String> = targets
            .iter()
            .filter(|t| !held_syms.contains(t))
            .cloned()
            .collect();
        if !new_targets.is_empty() {
            let bal = client.fetch_usdt_balance().await
                .map_err(|e| format!("余额查询失败: {e}"))?;
            // 预留约 0.2% 余量（单边手续费 + 市价成交价可能高于最新价），再按品种数均分
            let per = bal * 0.998 / new_targets.len() as f64;
            for sym in &new_targets {
                let price = client.fetch_last_price(sym).await
                    .map_err(|e| format!("价格查询失败: {e}"))?;
                let (step, min_qty) = client.fetch_step_size(sym).await
                    .map_err(|e| format!("精度查询失败: {e}"))?;
                let qty = round_step(per / price, step);
                if qty >= min_qty && price > 0.0 {
                    let buy_id = format!("qk_{}_{}_B", last_bar_ts, sym);
                    let reason = "信号调仓：买入新目标品种";
                    let exec = match client.place_order_with_meta(&Order::market_buy(sym, qty), Some(&buy_id)).await {
                        Ok(x) => x,
                        Err(e) => {
                            let msg = format!("买入 {sym} 失败: {e}");
                            notify.send(&format!("[quantkit 实盘] {msg}")).await;
                            return Err(msg);
                        }
                    };
                    st.own_orders.entry(sym.clone()).or_default().push((exec.order_id, exec.max_trade_id));
                    let fill = exec.fill;
                    action!("🟢", "买入 {} | 量 {:.8} | 价 {:.6} | 费 {:.6}",
                        fill.symbol, fill.quantity, fill.price, fill.fee);
                    notify.send(&fill_message(&fill.symbol, "买入", fill.quantity, fill.price, fill.fee, reason)).await;
                    st.fills.push(LiveFill {
                        ts: fill.timestamp, symbol: fill.symbol.clone(), side: Side::Buy,
                        quantity: fill.quantity, price: fill.price, fee: fill.fee,
                        reason: reason.to_string(),
                    });
                    st.positions.push(Position {
                        symbol: fill.symbol.clone(),
                        quantity: fill.quantity,
                        avg_entry_price: fill.price,
                    });
                } else {
                    warn!("{sym} 目标买入量 {qty} 低于 minQty {min_qty}，放弃");
                }
            }
        }
    }

    // 5) 原子持久化
    st.last_bar_ts = last_bar_ts;
    st.updated_at_ms = now_ms();
    save_state_atomic(state_path, st).map_err(|e| format!("状态保存失败: {e}"))?;
    
    // 只在有持仓变化或成交时才输出详细日志
    if targets != held_syms || !st.positions.is_empty() {
        info!("📊 Bar {} | 持仓: {} | 累计成交 {} 笔",
            last_bar_ts,
            if st.positions.is_empty() {
                "空仓".to_string()
            } else {
                st.positions.iter()
                    .map(|p| format!("{} {:.6}", p.symbol, p.quantity))
                    .collect::<Vec<_>>()
                    .join(", ")
            },
            st.fills.len()
        );
    }
    Ok(())
}

/// 权益快照：以最新价盯市，记录总资产/USDT/持仓市值。返回是否新增了点。
/// 失败只告警不阻断（快照是监控旁路，不是交易链路）。
async fn snapshot_equity(client: &mut BinanceClient, st: &mut LiveState) -> bool {
    let balances = match client.fetch_balances_full().await {
        Ok(b) => b,
        Err(e) => {
            warn!("权益快照跳过（余额查询失败）: {e}");
            return false;
        }
    };
    // USDT 计总额：挂单锁定的部分仍是自己的资产，不计入会让曲线在挂单时假跳水
    let usdt = balances.get("USDT").map(|&(f, l)| f + l).unwrap_or(0.0);
    let mut positions_value = 0.0;
    for p in &st.positions {
        match client.fetch_last_price(&p.symbol).await {
            Ok(px) => positions_value += p.quantity * px,
            Err(e) => warn!("{} 最新价查询失败，快照不含其市值: {e}", p.symbol),
        }
    }
    st.equity_history.push(EquitySnap {
        ts: now_ms(),
        total: usdt + positions_value,
        usdt,
        positions_value,
    });
    // 上限保护：每天 ~1440 点（每分钟轮询），10 万点约两个多月；超出后滚动丢弃最早的 1000 点
    if st.equity_history.len() > 100_000 {
        st.equity_history.drain(0..1000);
    }
    true
}

/// 读取状态文件；不存在返回 None，损坏告警并返回 None（web 急停同样复用此语义）
pub fn load_state(path: &Path) -> Option<LiveState> {
    let s = std::fs::read_to_string(path).ok()?;
    match serde_json::from_str(&s) {
        Ok(st) => Some(st),
        Err(e) => {
            error!("状态文件损坏({e})，将从空仓重新跟踪（请人工核对交易所持仓！）");
            None
        }
    }
}

/// 原子写状态文件（先写 .tmp 再 rename，避免半截文件）
pub fn save_state_atomic(path: &Path, st: &LiveState) -> Result<(), std::io::Error> {
    let json = serde_json::to_string_pretty(st).expect("状态序列化");
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pos(sym: &str, qty: f64) -> Position {
        Position {
            symbol: sym.into(),
            quantity: qty,
            avg_entry_price: 100.0,
        }
    }

    #[test]
    fn test_base_of() {
        assert_eq!(base_of("BTCUSDT"), "BTC");
        assert_eq!(base_of("USDT"), "USDT"); // 空串回退原样，避免误判
    }

    #[test]
    fn test_reconcile_consistent() {
        let symbols = vec!["BTCUSDT".to_string(), "ETHUSDT".to_string()];
        let local = vec![pos("BTCUSDT", 1.0)];
        let mut bal = BTreeMap::new();
        bal.insert("BTC".into(), (1.0, 0.0));
        bal.insert("USDT".into(), (5000.0, 0.0));
        let r = reconcile(&symbols, &local, &bal);
        assert!(r.drifts.is_empty(), "{:?}", r.drifts);
        assert_eq!(r.synced.len(), 1);
        assert!((r.synced[0].quantity - 1.0).abs() < 1e-12);
    }

    #[test]
    fn test_reconcile_quantity_drift_and_heal() {
        // 手续费本币抵扣导致实际少于状态：告警且自愈用真实数量，入场价保留
        let symbols = vec!["BTCUSDT".to_string()];
        let local = vec![pos("BTCUSDT", 1.0)];
        let mut bal = BTreeMap::new();
        bal.insert("BTC".into(), (0.998, 0.0));
        let r = reconcile(&symbols, &local, &bal);
        assert_eq!(r.drifts.len(), 1);
        assert!(r.drifts[0].contains("数量偏差"));
        assert!((r.synced[0].quantity - 0.998).abs() < 1e-12);
        assert!((r.synced[0].avg_entry_price - 100.0).abs() < 1e-12);
    }

    #[test]
    fn test_reconcile_missing_balance() {
        // 状态有持仓但交易所无该资产：告警，不进自愈列表（无法以真实数量重建）
        let symbols = vec!["BTCUSDT".to_string()];
        let local = vec![pos("BTCUSDT", 1.0)];
        let bal = BTreeMap::new();
        let r = reconcile(&symbols, &local, &bal);
        assert_eq!(r.drifts.len(), 1);
        assert!(r.drifts[0].contains("无余额"));
        assert!(r.synced.is_empty());
    }

    fn trade(id: u64, is_buyer: bool, qty: f64, price: f64, time: u64) -> MyTrade {
        MyTrade {
            id,
            order_id: id * 10,
            price,
            qty,
            commission: 0.0,
            commission_asset: "USDT".into(),
            time,
            is_buyer,
        }
    }

    #[test]
    fn test_reconcile_locked_balance_no_false_drift() {
        // 限价挂单锁定余额（可用=0、全额冻结）：总额口径不误判为"已在场外卖出"
        let symbols = vec!["LINKUSDT".to_string()];
        let local = vec![pos("LINKUSDT", 17.36)];
        let mut bal = BTreeMap::new();
        bal.insert("LINK".into(), (0.0, 17.36));
        let r = reconcile(&symbols, &local, &bal);
        assert!(r.drifts.is_empty(), "{:?}", r.drifts);
        assert!((r.synced[0].quantity - 17.36).abs() < 1e-12);

        // 部分可用 + 部分冻结：总额一致同样不告警
        bal.insert("LINK".into(), (7.36, 10.0));
        let r = reconcile(&symbols, &local, &bal);
        assert!(r.drifts.is_empty(), "{:?}", r.drifts);
    }

    #[test]
    fn test_reconcile_unrecorded_locked_only() {
        // 未记录资产全部冻结（如手动买入后挂出）：同样按总额告警并收入接管列表
        let symbols = vec!["BTCUSDT".to_string()];
        let local: Vec<Position> = vec![];
        let mut bal = BTreeMap::new();
        bal.insert("BTC".into(), (0.0, 0.5));
        let r = reconcile(&symbols, &local, &bal);
        assert_eq!(r.drifts.len(), 1);
        assert_eq!(r.unrecorded.len(), 1);
        assert_eq!(r.unrecorded[0], ("BTCUSDT".to_string(), 0.5));
    }

    #[test]
    fn test_apply_external_trades_open_and_add_weighted() {
        // 买入建仓；再加仓时入场价按加权均价更新
        let mut positions: Vec<Position> = vec![];
        let trades = vec![
            ("BTCUSDT".to_string(), trade(1, true, 1.0, 100.0, 1000)),
            ("BTCUSDT".to_string(), trade(2, true, 1.0, 200.0, 2000)),
        ];
        let w = apply_external_trades(&mut positions, &trades);
        assert!(w.is_empty(), "{:?}", w);
        assert_eq!(positions.len(), 1);
        assert!((positions[0].quantity - 2.0).abs() < 1e-12);
        assert!((positions[0].avg_entry_price - 150.0).abs() < 1e-12);
    }

    #[test]
    fn test_apply_external_trades_reduce_and_close() {
        // 卖出部分减仓（入场价不变）；卖光清仓并从列表移除
        let mut positions = vec![pos("BTCUSDT", 2.0)];
        let trades = vec![("BTCUSDT".to_string(), trade(1, false, 0.5, 120.0, 1000))];
        let w = apply_external_trades(&mut positions, &trades);
        assert!(w.is_empty(), "{:?}", w);
        assert!((positions[0].quantity - 1.5).abs() < 1e-12);
        assert!((positions[0].avg_entry_price - 100.0).abs() < 1e-12);

        let trades = vec![("BTCUSDT".to_string(), trade(2, false, 1.5, 130.0, 2000))];
        let w = apply_external_trades(&mut positions, &trades);
        assert!(w.is_empty(), "{:?}", w);
        assert!(positions.is_empty(), "清仓应移除持仓");
    }

    #[test]
    fn test_apply_external_trades_oversell_truncation() {
        // 超卖（>0.1%）：告警且仓位截断为 0
        let mut positions = vec![pos("BTCUSDT", 1.0)];
        let trades = vec![("BTCUSDT".to_string(), trade(1, false, 1.2, 120.0, 1000))];
        let w = apply_external_trades(&mut positions, &trades);
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("超过记录持仓"));
        assert!(positions.is_empty());
    }

    #[test]
    fn test_apply_external_trades_close_within_dust_tolerance() {
        // 卖出量略超持仓（≤0.1%，灰尘级别）：正常清仓不告警
        let mut positions = vec![pos("BTCUSDT", 1.0)];
        let trades = vec![("BTCUSDT".to_string(), trade(1, false, 1.0005, 120.0, 1000))];
        let w = apply_external_trades(&mut positions, &trades);
        assert!(w.is_empty(), "{:?}", w);
        assert!(positions.is_empty());
    }

    #[test]
    fn test_apply_external_trades_sell_without_position() {
        // 无持仓记录的卖出：仅告警，不产生负仓位
        let mut positions: Vec<Position> = vec![];
        let trades = vec![("ETHUSDT".to_string(), trade(1, false, 5.0, 3000.0, 1000))];
        let w = apply_external_trades(&mut positions, &trades);
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("无持仓记录"));
        assert!(positions.is_empty());
    }

    #[test]
    fn test_apply_external_trades_time_ordering() {
        // 同批乱序传入：按 time 排序应用（先卖后买），先卖因无持仓告警
        let mut positions: Vec<Position> = vec![];
        let trades = vec![
            ("BTCUSDT".to_string(), trade(2, true, 2.0, 110.0, 2000)),
            ("BTCUSDT".to_string(), trade(1, false, 1.0, 105.0, 1000)),
        ];
        let w = apply_external_trades(&mut positions, &trades);
        assert_eq!(w.len(), 1, "{:?}", w);
        assert_eq!(positions.len(), 1);
        assert!((positions[0].quantity - 2.0).abs() < 1e-12);
        assert!((positions[0].avg_entry_price - 110.0).abs() < 1e-12);
    }

    #[test]
    fn test_aggregate_external_fills_grouping() {
        // 按（品种, 方向）聚合：加权均价、手续费累加、ts 取最大；非 USDT 手续费标注
        let trades = vec![
            ("BTCUSDT".to_string(), MyTrade {
                id: 1, order_id: 10, price: 100.0, qty: 1.0,
                commission: 0.5, commission_asset: "USDT".into(), time: 1000, is_buyer: true,
            }),
            ("BTCUSDT".to_string(), MyTrade {
                id: 2, order_id: 11, price: 200.0, qty: 3.0,
                commission: 1.5, commission_asset: "USDT".into(), time: 2000, is_buyer: true,
            }),
            ("ETHUSDT".to_string(), MyTrade {
                id: 3, order_id: 12, price: 3000.0, qty: 2.0,
                commission: 0.01, commission_asset: "BNB".into(), time: 1500, is_buyer: false,
            }),
        ];
        let fills = aggregate_external_fills(&trades);
        assert_eq!(fills.len(), 2);
        // BTreeMap 按键有序：BTCUSDT 买入在前，ETHUSDT 卖出在后
        let buy = &fills[0];
        assert_eq!(buy.symbol, "BTCUSDT");
        assert_eq!(buy.side, Side::Buy);
        assert!((buy.quantity - 4.0).abs() < 1e-12);
        assert!((buy.price - 175.0).abs() < 1e-12); // (1*100 + 3*200) / 4
        assert!((buy.fee - 2.0).abs() < 1e-12);
        assert_eq!(buy.ts, 2000);
        assert_eq!(buy.reason, "场外手动单");
        let sell = &fills[1];
        assert_eq!(sell.symbol, "ETHUSDT");
        assert_eq!(sell.side, Side::Sell);
        assert!((sell.quantity - 2.0).abs() < 1e-12);
        assert_eq!(sell.reason, "场外手动单（含非USDT手续费资产）");
    }

    #[test]
    fn test_reconcile_stray_base_asset() {
        // 交易所有策略池内本位资产但状态未记录：告警；池外资产忽略；
        // USDT 本身不算漂移；容忍度内的微小偏差不告警（0.1%）
        let symbols = vec!["BTCUSDT".to_string(), "SOLUSDT".to_string()];
        let local = vec![pos("BTCUSDT", 1.0)];
        let mut bal = BTreeMap::new();
        bal.insert("BTC".into(), (1.0005, 0.0)); // 0.05% < 0.1%：不告警
        bal.insert("SOL".into(), (10.0, 0.0)); // 未记录持仓：告警
        bal.insert("DOGE".into(), (999.0, 0.0)); // 池外：忽略
        bal.insert("USDT".into(), (100.0, 0.0)); // 计价币：忽略
        let r = reconcile(&symbols, &local, &bal);
        assert_eq!(r.drifts.len(), 1);
        assert!(r.drifts[0].contains("SOL"));
        // 未记录池内资产收入 unrecorded（接管与否由调用方决定）；池外/计价币不进
        assert_eq!(r.unrecorded.len(), 1);
        assert_eq!(r.unrecorded[0], ("SOLUSDT".to_string(), 10.0));
        assert!(r.synced.len() == 1, "已记录持仓照常进 synced");
    }

    #[test]
    fn test_base_of_edge_cases() {
        // 测试各种边界情况
        assert_eq!(base_of("ETHUSDT"), "ETH");
        assert_eq!(base_of("SOLUSDT"), "SOL");
        assert_eq!(base_of("BNBUSDT"), "BNB");
        
        // 不含 USDT 后缀的情况
        assert_eq!(base_of("BTC"), "BTC");
        assert_eq!(base_of(""), "");
        
        // 特殊品种
        assert_eq!(base_of("1000SHIBUSDT"), "1000SHIB");
    }

    #[test]
    fn test_reconcile_empty_state() {
        // 本地状态为空，交易所有一些余额
        let symbols = vec!["BTCUSDT".to_string()];
        let local: Vec<Position> = vec![];
        let mut bal = BTreeMap::new();
        bal.insert("BTC".into(), (1.0, 0.0));
        
        let r = reconcile(&symbols, &local, &bal);
        
        // 应该产生未记录资产的告警
        assert_eq!(r.drifts.len(), 1);
        assert!(r.drifts[0].contains("未记录持仓"));
        assert_eq!(r.unrecorded.len(), 1);
        assert_eq!(r.unrecorded[0], ("BTCUSDT".to_string(), 1.0));
        assert!(r.synced.is_empty());
    }

    #[test]
    fn test_reconcile_multiple_positions() {
        // 多个持仓的对账
        let symbols = vec![
            "BTCUSDT".to_string(),
            "ETHUSDT".to_string(),
            "SOLUSDT".to_string(),
        ];
        let local = vec![
            pos("BTCUSDT", 1.0),
            pos("ETHUSDT", 10.0),
            pos("SOLUSDT", 100.0),
        ];
        let mut bal = BTreeMap::new();
        bal.insert("BTC".into(), (1.0, 0.0));
        bal.insert("ETH".into(), (10.0, 0.0));
        bal.insert("SOL".into(), (100.0, 0.0));
        bal.insert("USDT".into(), (5000.0, 0.0));
        
        let r = reconcile(&symbols, &local, &bal);
        
        // 所有持仓都一致，无漂移
        assert!(r.drifts.is_empty());
        assert_eq!(r.synced.len(), 3);
    }

    #[test]
    fn test_reconcile_tolerance_boundary() {
        // 测试容忍度边界（0.1%）
        let symbols = vec!["BTCUSDT".to_string()];
        
        // 刚好在容忍度内（0.1%）
        let local = vec![pos("BTCUSDT", 1.0)];
        let mut bal = BTreeMap::new();
        bal.insert("BTC".into(), (1.001, 0.0)); // 0.1% 偏差
        
        let r = reconcile(&symbols, &local, &bal);
        // 0.1% 刚好等于 TOL，不应该告警（> 才告警）
        assert!(r.drifts.is_empty());
        
        // 稍微超过容忍度
        bal.insert("BTC".into(), (1.0011, 0.0)); // 0.11% 偏差
        let r = reconcile(&symbols, &local, &bal);
        assert_eq!(r.drifts.len(), 1);
        assert!(r.drifts[0].contains("数量偏差"));
    }

    #[test]
    fn test_save_and_load_state() {
        use std::io::Write;
        
        let temp_dir = std::env::temp_dir();
        let test_file = temp_dir.join(format!("test_state_{}.json", std::process::id()));
        
        // 创建测试状态
        let state = LiveState {
            last_bar_ts: 1234567890000,
            positions: vec![
                Position {
                    symbol: "BTCUSDT".to_string(),
                    quantity: 1.5,
                    avg_entry_price: 50000.0,
                },
            ],
            fills: vec![LiveFill {
                ts: 1234567890000,
                symbol: "BTCUSDT".to_string(),
                side: Side::Buy,
                quantity: 1.5,
                price: 50000.0,
                fee: 75.0,
                reason: "signal".to_string(),
            }],
            equity_history: vec![EquitySnap {
                ts: 1234567890000,
                total: 100000.0,
                usdt: 50000.0,
                positions_value: 50000.0,
            }],
            updated_at_ms: 1234567890000,
            ..Default::default()
        };
        
        // 保存状态
        save_state_atomic(&test_file, &state).unwrap();
        
        // 验证文件存在
        assert!(test_file.exists());
        
        // 加载状态
        let loaded = load_state(&test_file);
        assert!(loaded.is_some());
        
        let loaded = loaded.unwrap();
        assert_eq!(loaded.last_bar_ts, state.last_bar_ts);
        assert_eq!(loaded.positions.len(), 1);
        assert_eq!(loaded.positions[0].symbol, "BTCUSDT");
        assert!((loaded.positions[0].quantity - 1.5).abs() < 1e-10);
        assert_eq!(loaded.fills.len(), 1);
        assert_eq!(loaded.equity_history.len(), 1);
        
        // 清理测试文件
        std::fs::remove_file(&test_file).ok();
    }

    #[test]
    fn test_load_corrupted_state() {
        let temp_dir = std::env::temp_dir();
        let test_file = temp_dir.join(format!("test_corrupt_{}.json", std::process::id()));
        
        // 写入无效的 JSON
        std::fs::write(&test_file, "this is not valid json").unwrap();
        
        // 加载应该返回 None
        let loaded = load_state(&test_file);
        assert!(loaded.is_none());
        
        // 清理测试文件
        std::fs::remove_file(&test_file).ok();
    }

    #[test]
    fn test_live_fill_serialization() {
        let fill = LiveFill {
            ts: 1234567890000,
            symbol: "BTCUSDT".to_string(),
            side: Side::Buy,
            quantity: 1.0,
            price: 50000.0,
            fee: 50.0,
            reason: "test_signal".to_string(),
        };
        
        // 序列化为 JSON
        let json = serde_json::to_string(&fill).unwrap();
        assert!(json.contains("BTCUSDT"));
        assert!(json.contains("test_signal"));
        
        // 反序列化
        let deserialized: LiveFill = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.symbol, fill.symbol);
        assert_eq!(deserialized.quantity, fill.quantity);
        assert_eq!(deserialized.reason, fill.reason);
    }

    #[test]
    fn test_equity_snap_structure() {
        let snap = EquitySnap {
            ts: 1234567890000,
            total: 100000.0,
            usdt: 50000.0,
            positions_value: 50000.0,
        };
        
        assert_eq!(snap.ts, 1234567890000);
        assert!((snap.total - 100000.0).abs() < 1e-10);
        assert!((snap.usdt - 50000.0).abs() < 1e-10);
        assert!((snap.positions_value - 50000.0).abs() < 1e-10);
    }

    #[test]
    fn test_reconcile_zero_quantity_position() {
        // 零数量持仓应该被忽略
        let symbols = vec!["BTCUSDT".to_string()];
        let local = vec![pos("BTCUSDT", 0.0)];
        let mut bal = BTreeMap::new();
        bal.insert("BTC".into(), (1.0, 0.0));
        
        let r = reconcile(&symbols, &local, &bal);
        
        // 零数量不应产生漂移告警
        assert!(r.drifts.is_empty());
    }

    #[test]
    fn test_equity_snap_serialization() {
        // 测试权益快照的序列化和反序列化
        let snap = EquitySnap {
            ts: 1234567890000,
            total: 123456.789,
            usdt: 60000.0,
            positions_value: 63456.789,
        };
        
        let json = serde_json::to_string(&snap).unwrap();
        let deserialized: EquitySnap = serde_json::from_str(&json).unwrap();
        
        assert_eq!(deserialized.ts, snap.ts);
        assert!((deserialized.total - snap.total).abs() < 1e-6);
        assert!((deserialized.usdt - snap.usdt).abs() < 1e-6);
        assert!((deserialized.positions_value - snap.positions_value).abs() < 1e-6);
    }

    #[test]
    fn test_live_state_with_empty_history() {
        // 测试没有历史成交和权益历史的空状态
        let state = LiveState {
            last_bar_ts: 0,
            positions: Vec::new(),
            fills: Vec::new(),
            equity_history: Vec::new(),
            updated_at_ms: 1234567890000,
            ..Default::default()
        };
        
        let json = serde_json::to_string(&state).unwrap();
        let loaded: LiveState = serde_json::from_str(&json).unwrap();
        
        assert_eq!(loaded.positions.len(), 0);
        assert_eq!(loaded.fills.len(), 0);
        assert_eq!(loaded.equity_history.len(), 0);
    }

    #[test]
    fn test_reconcile_large_drift() {
        // 测试大幅数量偏差（>10%）
        let symbols = vec!["ETHUSDT".to_string()];
        let local = vec![pos("ETHUSDT", 100.0)];
        let mut bal = BTreeMap::new();
        bal.insert("ETH".into(), (50.0, 0.0)); // 50% 偏差
        
        let r = reconcile(&symbols, &local, &bal);
        
        assert_eq!(r.drifts.len(), 1);
        assert!(r.drifts[0].contains("数量偏差"));
        assert!((r.synced[0].quantity - 50.0).abs() < 1e-10);
    }

    #[test]
    fn test_reconcile_multiple_unrecorded_assets() {
        // 多个未记录资产的场景
        let symbols = vec![
            "BTCUSDT".to_string(),
            "ETHUSDT".to_string(),
            "SOLUSDT".to_string(),
        ];
        let local: Vec<Position> = vec![]; // 本地无持仓
        let mut bal = BTreeMap::new();
        bal.insert("BTC".into(), (0.5, 0.0));
        bal.insert("ETH".into(), (5.0, 0.0));
        bal.insert("SOL".into(), (50.0, 0.0));
        
        let r = reconcile(&symbols, &local, &bal);
        
        // 所有池内资产都应告警
        assert_eq!(r.drifts.len(), 3);
        assert_eq!(r.unrecorded.len(), 3);
    }

    #[test]
    fn test_save_and_load_large_state() {
        // 测试包含大量历史成交的状态保存
        use std::io::Write;
        
        let temp_dir = std::env::temp_dir();
        let test_file = temp_dir.join(format!("test_large_{}.json", std::process::id()));
        
        // 创建包含 1000 笔成交的状态
        let mut fills = Vec::new();
        for i in 0..1000 {
            fills.push(LiveFill {
                ts: 1234567890000 + i * 60000,
                symbol: if i % 2 == 0 { "BTCUSDT".to_string() } else { "ETHUSDT".to_string() },
                side: if i % 2 == 0 { Side::Buy } else { Side::Sell },
                quantity: 0.01 * (i as f64 + 1.0),
                price: 50000.0 + (i as f64) * 100.0,
                fee: 10.0 + (i as f64) * 0.5,
                reason: format!("signal_{}", i),
            });
        }
        
        let state = LiveState {
            last_bar_ts: 1234567890000,
            positions: vec![],
            fills,
            equity_history: Vec::new(),
            updated_at_ms: 1234567890000,
            ..Default::default()
        };
        
        save_state_atomic(&test_file, &state).unwrap();
        let loaded = load_state(&test_file).unwrap();
        
        assert_eq!(loaded.fills.len(), 1000);
        assert_eq!(loaded.fills[0].reason, "signal_0");
        assert_eq!(loaded.fills[999].reason, "signal_999");
        
        std::fs::remove_file(&test_file).ok();
    }

    #[test]
    fn test_reconcile_mixed_scenario() {
        // 混合场景：部分一致、部分漂移、部分未记录
        let symbols = vec![
            "BTCUSDT".to_string(),
            "ETHUSDT".to_string(),
            "SOLUSDT".to_string(),
        ];
        let local = vec![
            pos("BTCUSDT", 1.0),      // 一致
            pos("ETHUSDT", 10.0),     // 漂移
        ];
        let mut bal = BTreeMap::new();
        bal.insert("BTC".into(), (1.0, 0.0));       // 一致
        bal.insert("ETH".into(), (9.5, 0.0));       // 5% 偏差
        bal.insert("SOL".into(), (100.0, 0.0));     // 未记录
        
        let r = reconcile(&symbols, &local, &bal);
        
        // 应该有 2 条告警：ETH 漂移 + SOL 未记录
        assert_eq!(r.drifts.len(), 2);
        assert!(r.drifts.iter().any(|d| d.contains("ETH") && d.contains("数量偏差")));
        assert!(r.drifts.iter().any(|d| d.contains("SOL") && d.contains("未记录")));
        
        // synced 只包含已记录的 ETH（以真实数量为准）
        assert_eq!(r.synced.len(), 2);
        assert!((r.synced[1].quantity - 9.5).abs() < 1e-10);
        
        // unrecorded 包含 SOL
        assert_eq!(r.unrecorded.len(), 1);
        assert_eq!(r.unrecorded[0], ("SOLUSDT".to_string(), 100.0));
    }

    #[test]
    fn test_equity_history_rollover() {
        // 测试权益历史超过 100,000 点时的滚动删除
        let mut history = Vec::new();
        for i in 0..100_500 {
            history.push(EquitySnap {
                ts: 1234567890000 + i as u64 * 60000,
                total: 100000.0 + (i as f64) * 10.0,
                usdt: 50000.0,
                positions_value: 50000.0 + (i as f64) * 10.0,
            });
        }
        
        let state = LiveState {
            last_bar_ts: 0,
            positions: Vec::new(),
            fills: Vec::new(),
            equity_history: history,
            updated_at_ms: 1234567890000,
            ..Default::default()
        };
        
        // 模拟 snapshot_equity 中的滚动逻辑
        let mut st = state;
        if st.equity_history.len() > 100_000 {
            st.equity_history.drain(0..1000);
        }
        
        assert_eq!(st.equity_history.len(), 99_500);
        // 最早的 1000 条已被删除
        assert_eq!(st.equity_history[0].ts, 1234567890000 + 1000 * 60000);
    }

    #[test]
    fn test_live_fill_empty_reason_backward_compat() {
        // 测试旧状态文件没有 reason 字段时的向后兼容性
        let json_without_reason = r#"{
            "ts": 1234567890000,
            "symbol": "BTCUSDT",
            "side": "Buy",
            "quantity": 1.0,
            "price": 50000.0,
            "fee": 50.0
        }"#;
        
        let fill: LiveFill = serde_json::from_str(json_without_reason).unwrap();
        assert_eq!(fill.reason, ""); // 默认值
    }

    #[test]
    fn test_reconcile_all_symbols_consistent() {
        // 所有品种都一致的完整场景
        let symbols = vec![
            "BTCUSDT".to_string(),
            "ETHUSDT".to_string(),
            "BNBUSDT".to_string(),
            "SOLUSDT".to_string(),
        ];
        let local = vec![
            pos("BTCUSDT", 1.0),
            pos("ETHUSDT", 10.0),
            pos("BNBUSDT", 5.0),
            pos("SOLUSDT", 100.0),
        ];
        let mut bal = BTreeMap::new();
        bal.insert("BTC".into(), (1.0, 0.0));
        bal.insert("ETH".into(), (10.0, 0.0));
        bal.insert("BNB".into(), (5.0, 0.0));
        bal.insert("SOL".into(), (100.0, 0.0));
        bal.insert("USDT".into(), (10000.0, 0.0));
        
        let r = reconcile(&symbols, &local, &bal);
        
        assert!(r.drifts.is_empty());
        assert_eq!(r.synced.len(), 4);
        assert!(r.unrecorded.is_empty());
    }

    #[test]
    fn test_base_of_non_usdt_pairs() {
        // 非 USDT 交易对的处理（虽然实际很少见）
        assert_eq!(base_of("BTCETH"), "BTCETH"); // 不含 USDT 后缀
        assert_eq!(base_of("ETHBTC"), "ETHBTC");
    }

    #[test]
    fn test_load_nonexistent_state() {
        // 加载不存在的文件应该返回 None
        let path = PathBuf::from("/tmp/nonexistent_state_12345.json");
        let loaded = load_state(&path);
        assert!(loaded.is_none());
    }

    #[test]
    fn test_save_and_load_empty_positions() {
        // 测试空持仓列表的保存和加载
        use std::io::Write;
        
        let temp_dir = std::env::temp_dir();
        let test_file = temp_dir.join(format!("test_empty_pos_{}.json", std::process::id()));
        
        let state = LiveState {
            last_bar_ts: 0,
            positions: Vec::new(),
            fills: Vec::new(),
            equity_history: Vec::new(),
            updated_at_ms: 1234567890000,
            ..Default::default()
        };
        
        save_state_atomic(&test_file, &state).unwrap();
        let loaded = load_state(&test_file).unwrap();
        
        assert_eq!(loaded.positions.len(), 0);
        assert_eq!(loaded.fills.len(), 0);
        
        std::fs::remove_file(&test_file).ok();
    }

    #[test]
    fn test_reconcile_with_negative_drift() {
        // 测试交易所余额少于状态的情况（负偏差）
        let symbols = vec!["BTCUSDT".to_string()];
        let local = vec![pos("BTCUSDT", 1.0)];
        let mut bal = BTreeMap::new();
        bal.insert("BTC".into(), (0.5, 0.0)); // 只有状态的一半
        
        let r = reconcile(&symbols, &local, &bal);
        
        assert_eq!(r.drifts.len(), 1);
        assert!(r.drifts[0].contains("数量偏差"));
        assert!((r.synced[0].quantity - 0.5).abs() < 1e-10);
    }

    #[test]
    fn test_equity_snap_zero_values() {
        // 测试权益快照全为零的情况
        let snap = EquitySnap {
            ts: 0,
            total: 0.0,
            usdt: 0.0,
            positions_value: 0.0,
        };
        
        let json = serde_json::to_string(&snap).unwrap();
        let deserialized: EquitySnap = serde_json::from_str(&json).unwrap();
        
        assert_eq!(deserialized.total, 0.0);
        assert_eq!(deserialized.usdt, 0.0);
        assert_eq!(deserialized.positions_value, 0.0);
    }

    #[test]
    fn test_live_fill_extreme_values() {
        // 测试极端数值的成交记录
        let fill = LiveFill {
            ts: u64::MAX,
            symbol: "BTCUSDT".to_string(),
            side: Side::Buy,
            quantity: f64::MAX,
            price: f64::MAX,
            fee: 0.0,
            reason: String::new(),
        };
        
        let json = serde_json::to_string(&fill).unwrap();
        let deserialized: LiveFill = serde_json::from_str(&json).unwrap();
        
        assert_eq!(deserialized.ts, u64::MAX);
        assert_eq!(deserialized.quantity, f64::MAX);
        assert_eq!(deserialized.price, f64::MAX);
    }

    #[test]
    fn test_reconcile_partial_overlap() {
        // 部分品种有持仓，部分没有
        let symbols = vec![
            "BTCUSDT".to_string(),
            "ETHUSDT".to_string(),
            "SOLUSDT".to_string(),
        ];
        let local = vec![pos("BTCUSDT", 1.0)]; // 只有 BTC
        let mut bal = BTreeMap::new();
        bal.insert("BTC".into(), (1.0, 0.0));       // 一致
        bal.insert("ETH".into(), (5.0, 0.0));       // 未记录
        // SOL 无余额
        
        let r = reconcile(&symbols, &local, &bal);
        
        // ETH 未记录告警
        assert_eq!(r.drifts.len(), 1);
        assert!(r.drifts[0].contains("ETH"));
        assert_eq!(r.unrecorded.len(), 1);
        assert_eq!(r.unrecorded[0], ("ETHUSDT".to_string(), 5.0));
        
        // synced 包含 BTC
        assert_eq!(r.synced.len(), 1);
    }

    #[test]
    fn test_save_state_atomic_overwrite() {
        // 测试原子覆盖已存在的文件
        use std::io::Write;
        
        let temp_dir = std::env::temp_dir();
        let test_file = temp_dir.join(format!("test_overwrite_{}.json", std::process::id()));
        
        // 第一次保存
        let state1 = LiveState {
            last_bar_ts: 1000,
            positions: vec![],
            fills: vec![],
            equity_history: vec![],
            updated_at_ms: 1000,
            ..Default::default()
        };
        save_state_atomic(&test_file, &state1).unwrap();
        
        // 第二次保存（覆盖）
        let state2 = LiveState {
            last_bar_ts: 2000,
            positions: vec![],
            fills: vec![],
            equity_history: vec![],
            updated_at_ms: 2000,
            ..Default::default()
        };
        save_state_atomic(&test_file, &state2).unwrap();
        
        // 验证是最新的状态
        let loaded = load_state(&test_file).unwrap();
        assert_eq!(loaded.last_bar_ts, 2000);
        
        std::fs::remove_file(&test_file).ok();
    }

    #[test]
    fn test_reconcile_very_small_quantities() {
        // 测试极小数值的对账（如 SHIB 等小额币种）
        let symbols = vec!["SHIBUSDT".to_string()];
        let local = vec![pos("SHIBUSDT", 1000000.0)];
        let mut bal = BTreeMap::new();
        bal.insert("SHIB".into(), (1000000.5, 0.0)); // 微小偏差
        
        let r = reconcile(&symbols, &local, &bal);
        
        // 0.5/1000000 = 0.00005% < 0.1%，应该在容忍度内
        assert!(r.drifts.is_empty());
    }

    #[test]
    fn test_equity_history_boundary() {
        // 测试权益历史刚好达到上限时的行为
        let mut history = Vec::new();
        for i in 0..100_000 {
            history.push(EquitySnap {
                ts: i as u64 * 60000,
                total: 100000.0,
                usdt: 50000.0,
                positions_value: 50000.0,
            });
        }
        
        let state = LiveState {
            last_bar_ts: 0,
            positions: Vec::new(),
            fills: Vec::new(),
            equity_history: history,
            updated_at_ms: 0,
            ..Default::default()
        };
        
        // 刚好 100,000 点，不应该触发滚动删除
        let mut st = state;
        if st.equity_history.len() > 100_000 {
            st.equity_history.drain(0..1000);
        }
        
        assert_eq!(st.equity_history.len(), 100_000);
    }
}
