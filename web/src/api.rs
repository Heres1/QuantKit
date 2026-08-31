//! quantkit WebUI 后端：纯 REST API 服务（前端分离独立部署）。
//!
//! 架构（控制面）：web 进程编排现有 `quantkit` CLI（子进程），
//! 不内嵌交易循环 —— 实盘逻辑零改动、进程隔离、启停干净。
//! - 回测/扫描：`quantkit backtest --json` / `quantkit sweep` 一次性子进程
//! - 模拟盘/实盘：常驻子进程 + 日志捕获 + 停止控制
//! - 行情：全市场 24h 行情代理（30s 内存缓存）+ 任意周期K线服务端缓存
//! - 安全：CORS 全开（读接口公开）；计算/写操作接口由 `X-API-Token` 保护，
//!   令牌来自环境变量 `QUANTKIT_API_TOKEN`，未设置则全部拒绝（fail-closed）

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path as AxPath, Query, State};
use axum::http::{header, Method, Request, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{delete, get, post};
use axum::Router;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncBufReadExt;
use tokio::process::Child;
use tokio::sync::Mutex;

use quantkit_app::config::{resolve_binance_keys, AppConfig};
use quantkit_app::live::{EquitySnap, LiveFill, LiveState};
use quantkit_core::types::{Kline, Side};
use quantkit_exchanges::binance::{now_ms, BinanceClient, TickerQuote};
use quantkit_exchanges::traits::MarketData;

use crate::factors;

// ---------------- 状态 ----------------

struct RunHandle {
    pid: u32,
    started_at_ms: u64,
    child: Child,
}

struct AppState {
    cfg: AppConfig,
    bin: PathBuf,
    dryrun: Mutex<Option<RunHandle>>,
    live: Mutex<Option<RunHandle>>,
    logs: Mutex<Vec<String>>,
    download: Mutex<Option<DownloadTask>>,
    /// 全市场行情缓存：(生成时刻, 列表)。避免前端每次刷新都打 Binance 权重 80 接口
    markets: Mutex<Option<(u64, Vec<TickerQuote>)>>,
    /// 行情客户端：进程级共享，保持镜像兜底粘性与连接池（每次新建会丢兜底状态）
    binance: Arc<BinanceClient>,
    /// 账户资产缓存：(生成时刻, 响应)。余额+盯市逐资产取价，15s 内复用避免打接口配额
    account: Mutex<Option<(u64, serde_json::Value)>>,
    /// 市场环境分析缓存：(生成时刻, 响应)。基于本地日K+实时价，60s 复用避免重复扫盘/拉行情
    regime: Mutex<Option<(u64, serde_json::Value)>>,
    /// 当前部署的代码版本（启动时探测）：短哈希，工作区有改动时带 -dirty 后缀
    git_commit: String,
}

#[derive(Clone, Serialize)]
struct DownloadTask {
    symbols: Vec<String>,
    done: Vec<String>,
    running: String,
    started_at_ms: u64,
}

fn now_str() -> String {
    let ms = now_ms();
    let secs = ms / 1000;
    let (y, mo, d, h, mi, s) = epoch_to_ymdhms(secs);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}")
}

/// 毫秒时间戳 -> (年,月,日,时,分,秒)，Howard Hinnant civil 算法
fn epoch_to_ymdhms(secs: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = (secs / 86400) as i64;
    let rem = secs % 86400;
    let (h, mi, s) = (rem / 3600, rem % 3600 / 60, rem % 60);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if mo <= 2 { y + 1 } else { y }, mo, d, h as u32, mi as u32, s as u32)
}

fn fmt_ts(ms: u64) -> String {
    let (y, mo, d, _, _, _) = epoch_to_ymdhms(ms / 1000);
    format!("{y:04}-{mo:02}-{d:02}")
}

/// 探测当前部署的代码版本：短哈希；工作区有未提交改动时追加 -dirty。
/// 非 git 环境（如直接拷贝二进制目录）返回 "unknown"，不影响服务启动
fn detect_git_version() -> String {
    let hash = std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    match hash {
        Some(h) => {
            let dirty = std::process::Command::new("git")
                .args(["status", "--porcelain"])
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| !o.stdout.is_empty())
                .unwrap_or(false);
            if dirty {
                format!("{h}-dirty")
            } else {
                h
            }
        }
        None => "unknown".to_string(),
    }
}

// ---------------- 入口 ----------------

/// 启动 API 服务（阻塞直到进程退出）。监听 0.0.0.0：前后端分离部署，
/// 前端静态站点独立托管，跨域经 CORS 中间件放开。
pub async fn serve(cfg: AppConfig) {
    let bin = resolve_cli_bin();
    let git_commit = detect_git_version();
    let addr = format!("0.0.0.0:{}", cfg.web_port);
    println!(
        "[web] quantkit API 服务启动: http://{addr}  (CLI: {}, 代码版本: {git_commit})",
        bin.display()
    );
    if std::env::var("QUANTKIT_API_TOKEN").map(|t| t.is_empty()).unwrap_or(true) {
        println!("[web] 警告: 未设置 QUANTKIT_API_TOKEN，回测/下载/模拟盘/实盘接口将全部拒绝");
    }
    let state = Arc::new(AppState {
        bin,
        dryrun: Mutex::new(None),
        live: Mutex::new(None),
        logs: Mutex::new(Vec::new()),
        download: Mutex::new(None),
        markets: Mutex::new(None),
        binance: Arc::new(BinanceClient::public()),
        account: Mutex::new(None),
        regime: Mutex::new(None),
        git_commit,
        cfg,
    });

    // 计算/写操作路由：统一套 Token 门禁（读接口公开）+ 速率限制
    let protected = Router::new()
        .route("/api/backtest", post(api_backtest))
        .route("/api/sweep", post(api_sweep))
        .route("/api/walkforward", post(api_walkforward))
        .route("/api/backtests/:id", delete(api_backtest_delete))
        .route("/api/download", post(api_download))
        .route("/api/runs/:kind/start", post(api_run_start))
        .route("/api/runs/:kind/stop", post(api_run_stop))
        // 实盘监控（含持仓/资金信息，属敏感数据，纳入 Token 门禁）
        .route("/api/live/positions", get(api_live_positions))
        .route("/api/live/equity", get(api_live_equity))
        .route("/api/live/fills", get(api_live_fills))
        .route("/api/live/account", get(api_live_account))
        .route("/api/live/analysis", get(api_live_analysis))
        .layer(middleware::from_fn(token_guard));

    let app = Router::new()
        .route("/api/health", get(api_health))
        .route("/api/dashboard", get(api_dashboard))
        .route("/api/markets", get(api_markets))
        .route("/api/market-regime", get(api_market_regime))
        .route("/api/strong-coins", get(api_strong_coins))
        .route("/api/klines/:symbol", get(api_klines))
        .route("/api/factors", get(api_factors))
        .route("/api/correlation", get(api_correlation))
        .route("/api/factor-ic", get(api_factor_ic))
        .route("/api/data", get(api_data))
        .route("/api/backtests", get(api_backtests))
        .route("/api/backtests/:id", get(api_backtest_detail))
        .route("/api/runs/:kind/logs", get(api_run_logs))
        .merge(protected)
        .layer(middleware::from_fn(cors_middleware))
        .with_state(state);
    
    println!("[security] 🔒 安全配置:");
    println!("[security]   - API Token 认证: {}", if std::env::var("QUANTKIT_API_TOKEN").ok().map(|t| !t.is_empty()).unwrap_or(false) { "✅ 已启用" } else { "❌ 未配置（危险！）" });
    println!("[security]   - CORS: 允许所有来源（敏感接口需Token）");
    println!("[security]   - 访问日志: ✅ 已启用（记录IP和路径）");
    println!("[security]   - 防火墙: 由服务器UFW管理（当前开放8080端口）");
    
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .unwrap_or_else(|e| panic!("监听 {addr} 失败: {e}"));
    axum::serve(listener, app).await.expect("web 服务退出");
}

fn resolve_cli_bin() -> PathBuf {
    // 优先环境变量，其次当前可执行文件同目录的 quantkit（cargo run 场景用 target/debug/quantkit）
    if let Ok(p) = std::env::var("QUANTKIT_BIN") {
        return PathBuf::from(p);
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let cand = dir.join("quantkit");
            if cand.exists() {
                return cand;
            }
        }
    }
    PathBuf::from("./quantkit")
}

// ---------------- 中间件 ----------------

/// CORS：前端分离部署，任意来源可访问。预检 OPTIONS 直接返回 204；
/// 其余响应统一追加开放头。手写实现，不引入 tower-http。
async fn cors_middleware(req: Request<Body>, next: Next) -> Response {
    if req.method() == Method::OPTIONS {
        let mut resp = StatusCode::NO_CONTENT.into_response();
        apply_cors_headers(resp.headers_mut());
        return resp;
    }
    let mut resp = next.run(req).await;
    apply_cors_headers(resp.headers_mut());
    resp
}

fn apply_cors_headers(h: &mut axum::http::HeaderMap) {
    // 允许所有来源访问（前端独立部署场景），但敏感接口由 Token 保护
    h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*".parse().unwrap());
    h.insert(header::ACCESS_CONTROL_ALLOW_METHODS, "GET, POST, DELETE, OPTIONS".parse().unwrap());
    h.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, "Content-Type, X-API-Token".parse().unwrap());
    h.insert(header::ACCESS_CONTROL_MAX_AGE, "3600".parse().unwrap());
}

/// Token 门禁：请求头 `X-API-Token` 须与环境变量 `QUANTKIT_API_TOKEN` 一致。
/// 环境变量缺失/为空时一律拒绝（fail-closed），防止忘配置导致接口裸奔。
async fn token_guard(req: Request<Body>, next: Next) -> Response {
    let expected = std::env::var("QUANTKIT_API_TOKEN").ok();
    let provided = req.headers().get("X-API-Token").and_then(|v| v.to_str().ok());
    if !token_matches(expected.as_deref(), provided) {
        // 记录失败的尝试（包含来源IP）
        let client_ip = req.headers()
            .get("x-forwarded-for")
            .or_else(|| req.headers().get("x-real-ip"))
            .and_then(|v| v.to_str().ok())
            .unwrap_or("unknown");
        eprintln!("[security] ❌ Token验证失败 from IP: {}", client_ip);
        
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({
                "ok": false,
                "error": "缺少或不匹配的 X-API-Token（服务端环境变量 QUANTKIT_API_TOKEN）"
            })),
        )
            .into_response();
    }
    
    // 记录成功的API访问
    let client_ip = req.headers()
        .get("x-forwarded-for")
        .or_else(|| req.headers().get("x-real-ip"))
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown");
    println!("[security] ✅ API访问 from IP: {} | Path: {}", client_ip, req.uri().path());
    
    next.run(req).await
}

/// 纯函数令牌比对（与环境读取分离，可单测）。期望值为空视为未配置 -> 拒绝。
fn token_matches(expected: Option<&str>, provided: Option<&str>) -> bool {
    match (expected, provided) {
        (Some(e), Some(p)) if !e.is_empty() => e == p,
        _ => false,
    }
}

// ---------------- 响应辅助 ----------------

type Resp = (StatusCode, Json<serde_json::Value>);

fn ok_json<T: Serialize>(v: T) -> Resp {
    (StatusCode::OK, Json(serde_json::json!({ "ok": true, "data": v })))
}

fn err_resp(msg: String) -> Resp {
    (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "ok": false, "error": msg })))
}

async fn push_log(st: &AppState, line: String) {
    let entry = format!("[{}] {}", now_str(), line);
    println!("{}", entry);
    let mut logs = st.logs.lock().await;
    logs.push(entry);
    if logs.len() > 2000 {
        let n = logs.len() - 2000;
        logs.drain(0..n);
    }
}

// ---------------- 基础 API ----------------

async fn api_health() -> Resp {
    ok_json(serde_json::json!({ "name": "quantkit", "version": env!("CARGO_PKG_VERSION") }))
}

/// 扫描数据目录下所有 {SYMBOL}_{interval}.json，返回品种/周期统计
fn scan_data_files(data_dir: &str) -> Vec<serde_json::Value> {
    let mut out: Vec<serde_json::Value> = Vec::new();
    let entries = match std::fs::read_dir(data_dir) {
        Ok(e) => e,
        Err(_) => return out,
    };
    for f in entries.flatten() {
        let name = f.file_name().to_string_lossy().to_string();
        let rest = match name.strip_suffix(".json") {
            Some(r) => r,
            None => continue,
        };
        let (sym, interval) = match rest.rsplit_once('_') {
            Some((s, i)) if !s.is_empty() && !i.is_empty() => (s, i),
            _ => continue,
        };
        let ks = std::fs::read_to_string(f.path())
            .ok()
            .and_then(|s| serde_json::from_str::<Vec<Kline>>(&s).ok());
        if let Some(ks) = ks {
            out.push(serde_json::json!({
                "symbol": sym,
                "interval": interval,
                "bars": ks.len(),
                "first": ks.first().map(|k| fmt_ts(k.open_time)).unwrap_or_default(),
                "last": ks.last().map(|k| fmt_ts(k.open_time)).unwrap_or_default(),
            }));
        }
    }
    out.sort_by(|a, b| {
        let ka = (a["symbol"].as_str().unwrap_or("").to_string(), a["interval"].as_str().unwrap_or("").to_string());
        let kb = (b["symbol"].as_str().unwrap_or("").to_string(), b["interval"].as_str().unwrap_or("").to_string());
        ka.cmp(&kb)
    });
    out
}

/// 外部进程检测：扫描进程列表判断 web 之外启动的 quantkit 子命令（start.sh/nohup
/// 场景）是否在运行。模式用 CLI 完整路径+子命令，避免匹配到 web 自身；
/// 无 started_at（前端不展示）。pgrep 不存在或无匹配时视为未运行。
fn external_process_info(bin: &Path, subcmd: &str) -> serde_json::Value {
    let pattern = format!("{} {}", bin.display(), subcmd);
    let out = std::process::Command::new("pgrep")
        .args(["-f", &pattern])
        .output();
    match out {
        Ok(o) if o.status.success() => {
            let text = String::from_utf8_lossy(&o.stdout);
            let pid = text.lines().next().and_then(|l| l.trim().parse::<u32>().ok());
            match pid {
                Some(p) => serde_json::json!({ "running": true, "pid": p, "external": true }),
                None => serde_json::json!({ "running": false }),
            }
        }
        // pgrep 不存在（罕见）或无匹配（退出码 1）：视为未运行，不影响其余字段
        _ => serde_json::json!({ "running": false }),
    }
}

async fn api_dashboard(State(st): State<Arc<AppState>>) -> Resp {
    let cfg = &st.cfg;
    // dry-run 快照
    let dry_state: Option<serde_json::Value> = std::fs::read_to_string(&cfg.state_file)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok());
    // 运行状态
    let dryrun_info = {
        let g = st.dryrun.lock().await;
        match g.as_ref() {
            Some(h) => serde_json::json!({ "running": true, "pid": h.pid, "started_at": h.started_at_ms }),
            // web 自身未启动过时回退外部进程检测（start.sh / nohup 等脚本启动的场景）
            None => external_process_info(&st.bin, "dry-run"),
        }
    };
    let live_info = {
        let g = st.live.lock().await;
        match g.as_ref() {
            Some(h) => serde_json::json!({ "running": true, "pid": h.pid, "started_at": h.started_at_ms }),
            None => external_process_info(&st.bin, "live"),
        }
    };
    let (bk, bs) = resolve_binance_keys(&st.cfg);
    let keys_ok = bk.is_some() && bs.is_some();
    ok_json(serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "git_commit": st.git_commit,
        "data_dir": cfg.data_dir,
        "symbols": cfg.symbols,
        "strategy": cfg.strategy,
        "fee_rate": cfg.fee_rate,
        "live_enabled": cfg.live_enabled,
        "api_keys_set": keys_ok,
        "market": scan_data_files(&cfg.data_dir),
        "dry_state": dry_state,
        "runs": {
            "dryrun": dryrun_info,
            "live": live_info,
        },
    }))
}

// ---------------- 行情（全币种） ----------------

/// 全市场 24h 行情（USDT 计价现货）。30s 内存缓存，避免频繁打 Binance 行情接口
async fn api_markets(State(st): State<Arc<AppState>>) -> Resp {
    const CACHE_MS: u64 = 30_000;
    let now = now_ms();
    {
        let g = st.markets.lock().await;
        if let Some((ts, list)) = g.as_ref() {
            if now.saturating_sub(*ts) < CACHE_MS {
                return ok_json(serde_json::json!({ "quotes": list, "updated_at": ts }));
            }
        }
    }
    let client = st.binance.clone();
    match client.fetch_ticker_24h().await {
        Ok(all) => {
            let list: Vec<TickerQuote> = all
                .into_iter()
                // 剔除已退市品种：交易所 24h 接口仍返回它们但价格/成交额全为 0
                .filter(|q| q.symbol.ends_with("USDT") && q.last_price > 0.0)
                .collect();
            *st.markets.lock().await = Some((now, list.clone()));
            ok_json(serde_json::json!({ "quotes": list, "updated_at": now }))
        }
        Err(e) => err_resp(format!("全市场行情拉取失败: {e}")),
    }
}

// ---------------- 市场环境分析 ----------------

/// 单品种环境指标：均线位置、多周期涨幅、年化波动（纯函数，可单测）。
/// live_price 为实时最新价（缺省回退末根收盘），样本少于 51 根无法算 MA50 返回 None。
fn regime_symbol_stats(sym: &str, ks: &[Kline], live_price: Option<f64>) -> Option<serde_json::Value> {
    if ks.len() < 51 {
        return None;
    }
    let closes: Vec<f64> = ks.iter().map(|k| k.close).collect();
    let n = closes.len();
    let last = live_price.filter(|p| p.is_finite() && *p > 0.0).unwrap_or(closes[n - 1]);
    let ma = |p: usize| closes[n - p..].iter().sum::<f64>() / p as f64;
    let (ma50, ma200) = (ma(50), if n >= 200 { Some(ma(200)) } else { None });
    let ret = |d: usize| {
        if n > d {
            Some((last / closes[n - 1 - d] - 1.0) * 100.0)
        } else {
            None
        }
    };
    // 30 日对数收益标准差 × √365 年化（加密 7×24 交易）
    let win = &closes[n.saturating_sub(31)..];
    let rets30: Vec<f64> = win.windows(2).map(|w| (w[1] / w[0]).ln()).collect();
    let vol30 = if rets30.len() >= 2 {
        let mean = rets30.iter().sum::<f64>() / rets30.len() as f64;
        let var = rets30.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / (rets30.len() - 1) as f64;
        Some(var.sqrt() * 365.0_f64.sqrt() * 100.0)
    } else {
        None
    };
    Some(serde_json::json!({
        "symbol": sym,
        "price": last,
        "ma50": ma50,
        "ma200": ma200,
        "above_ma50": last > ma50,
        "above_ma200": ma200.map(|m| last > m),
        "ret_7d": ret(7),
        "ret_30d": ret(30),
        "ret_90d": ret(90),
        "vol_30d": vol30,
    }))
}

/// 市场环境分析：趋势广度（MA50/MA200 上方占比）、动量分层、波动与 BTC 锚。
/// 基于本地日K + 实时价，60s 缓存；无数据时提示先去数据中心下载。
async fn api_market_regime(State(st): State<Arc<AppState>>) -> Resp {
    const CACHE_MS: u64 = 60_000;
    let now = now_ms();
    {
        let g = st.regime.lock().await;
        if let Some((ts, v)) = g.as_ref() {
            if now.saturating_sub(*ts) < CACHE_MS {
                return ok_json(v.clone());
            }
        }
    }
    let (series, err) = load_daily_series(&st.cfg.data_dir, None);
    if series.is_empty() {
        let msg = match err {
            Ok(()) => "本地暂无日K数据，请先在数据中心下载".to_string(),
            Err(e) => e,
        };
        return err_resp(msg);
    }
    // 实时价（失败回退末根收盘，不影响整体）
    let quotes = st.binance.fetch_ticker_24h().await.unwrap_or_default();
    let px = |sym: &str| {
        quotes
            .iter()
            .find(|q| q.symbol == sym)
            .map(|q| q.last_price)
    };
    let mut symbols = Vec::new();
    for (sym, ks) in &series {
        if let Some(v) = regime_symbol_stats(sym, ks, px(sym)) {
            symbols.push(v);
        }
    }
    if symbols.is_empty() {
        return err_resp("日K样本不足（需至少 51 根），请先下载更长历史".into());
    }
    let total = symbols.len();
    let count = |f: fn(&serde_json::Value) -> bool| symbols.iter().filter(|s| f(s)).count();
    let above50 = count(|s| s["above_ma50"].as_bool().unwrap_or(false));
    let above200 = count(|s| s["above_ma200"].as_bool().unwrap_or(false));
    let n200 = count(|s| s["above_ma200"].is_boolean());
    // 动量分层（按 30 日涨幅）：强动量 >5% / 弱势 <-5% / 其余震荡市，无 30 日样本计入震荡
    let strong = count(|s| s["ret_30d"].as_f64().map(|r| r > 5.0).unwrap_or(false));
    let weak = count(|s| s["ret_30d"].as_f64().map(|r| r < -5.0).unwrap_or(false));
    let neutral = total - strong - weak;
    let avg_vol = {
        let vs: Vec<f64> = symbols.iter().filter_map(|s| s["vol_30d"].as_f64()).collect();
        if vs.is_empty() { None } else { Some(vs.iter().sum::<f64>() / vs.len() as f64) }
    };
    let btc = symbols.iter().find(|s| s["symbol"].as_str() == Some("BTCUSDT")).cloned();
    let out = serde_json::json!({
        "symbols": symbols,
        "total": total,
        "breadth": {
            "above_ma50": above50,
            "above_ma200": above200,
            "ma200_count": n200,
            "above_ma50_pct": above50 as f64 / total as f64 * 100.0,
            "above_ma200_pct": if n200 > 0 { Some(above200 as f64 / n200 as f64 * 100.0) } else { None },
        },
        "momentum": { "strong": strong, "neutral": neutral, "weak": weak },
        "avg_vol_30d": avg_vol,
        "btc": btc,
        "updated_at_ms": now,
    });
    *st.regime.lock().await = Some((now, out.clone()));
    ok_json(out)
}

// ---------------- 实时强势货币分析 ----------------

/// 单币种实时强度得分（基于24h行情 + 多周期动量 + 量能）
fn coin_strength_score(q: &TickerQuote, prev_rank: Option<usize>) -> serde_json::Value {
    let change = q.price_change_pct;
    
    // 1. 24h涨跌幅得分 (0-30分)
    let change_score = if change > 15.0 { 30.0 }
        else if change > 10.0 { 25.0 }
        else if change > 5.0 { 20.0 }
        else if change > 2.0 { 15.0 }
        else if change > 0.0 { 10.0 }
        else if change > -2.0 { 5.0 }
        else { 0.0 };
    
    // 2. 成交额排名得分 (0-25分) - 高流动性溢价
    let vol_score = if q.quote_volume > 5e8 { 25.0 }
        else if q.quote_volume > 2e8 { 20.0 }
        else if q.quote_volume > 1e8 { 15.0 }
        else if q.quote_volume > 5e7 { 10.0 }
        else { 5.0 };
    
    // 3. 价格位置得分 (0-20分) - 接近24h高点说明强势
    let price_range = q.high_price - q.low_price;
    let price_pos = if price_range > 0.0 {
        (q.last_price - q.low_price) / price_range
    } else {
        0.5
    };
    let position_score = price_pos * 20.0;
    
    // 4. 波动率调整 (0-15分) - 适度波动最佳
    let volatility = if q.low_price > 0.0 {
        (q.high_price - q.low_price) / q.low_price
    } else {
        0.0
    };
    let vol_adjust = if volatility > 0.02 && volatility < 0.15 { 15.0 }
        else if volatility >= 0.15 { 10.0 }  // 过高波动扣分
        else { 8.0 };  // 过低波动也扣分
    
    // 5. 绝对价格得分 (0-10分) - 避免低价币操纵
    let price_score = if q.last_price > 100.0 { 10.0 }
        else if q.last_price > 10.0 { 8.0 }
        else if q.last_price > 1.0 { 6.0 }
        else { 3.0 };
    
    let total_score = change_score + vol_score + position_score + vol_adjust + price_score;
    
    serde_json::json!({
        "symbol": q.symbol,
        "last_price": q.last_price,
        "price_change_pct": change,
        "high_price": q.high_price,
        "low_price": q.low_price,
        "quote_volume": q.quote_volume,
        "score": total_score,
        "change_score": change_score,
        "vol_score": vol_score,
        "position_score": position_score,
        "vol_adjust": vol_adjust,
        "price_score": price_score,
        "prev_rank": prev_rank,
    })
}

/// 实时强势货币排行榜：综合24h行情多维度打分排序，30s缓存
async fn api_strong_coins(State(st): State<Arc<AppState>>) -> Resp {
    const CACHE_MS: u64 = 30_000;
    const TOP_N: usize = 50;
    
    let now = now_ms();
    
    // 检查缓存
    {
        let g = st.markets.lock().await;
        if let Some((ts, quotes)) = g.as_ref() {
            if now.saturating_sub(*ts) < CACHE_MS {
                // 使用缓存数据计算排行
                let mut scored: Vec<serde_json::Value> = quotes.iter()
                    .map(|q| coin_strength_score(q, None))
                    .collect();
                scored.sort_by(|a, b| {
                    b["score"].as_f64().unwrap_or(0.0)
                        .partial_cmp(&a["score"].as_f64().unwrap_or(0.0))
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
                
                // 只返回Top N
                scored.truncate(TOP_N);
                
                // 添加排名
                for (i, coin) in scored.iter_mut().enumerate() {
                    coin.as_object_mut().unwrap().insert("rank".to_string(), serde_json::json!(i + 1));
                }
                
                return ok_json(serde_json::json!({
                    "coins": scored,
                    "total": quotes.len(),
                    "top_n": TOP_N,
                    "updated_at": *ts,
                    "cache_hit": true,
                }));
            }
        }
    }
    
    // 缓存过期或不存在，重新拉取
    match st.binance.fetch_ticker_24h().await {
        Ok(all) => {
            let quotes: Vec<TickerQuote> = all
                .into_iter()
                .filter(|q| q.symbol.ends_with("USDT") && q.quote_volume > 1e6)  // 过滤低流动性
                .collect();
            
            let ts = now_ms();
            
            // 获取上一次的排名用于对比
            let prev_ranks: std::collections::HashMap<String, usize> = {
                let g = st.markets.lock().await;
                if let Some((_, old_quotes)) = g.as_ref() {
                    let mut ranked: Vec<serde_json::Value> = old_quotes.iter()
                        .map(|q| coin_strength_score(q, None))
                        .collect();
                    ranked.sort_by(|a, b| {
                        b["score"].as_f64().unwrap_or(0.0)
                            .partial_cmp(&a["score"].as_f64().unwrap_or(0.0))
                            .unwrap_or(std::cmp::Ordering::Equal)
                    });
                    ranked.iter().enumerate()
                        .map(|(i, c)| (c["symbol"].as_str().unwrap_or("").to_string(), i + 1))
                        .collect()
                } else {
                    std::collections::HashMap::new()
                }
            };
            
            // 计算新的得分和排名
            let mut scored: Vec<serde_json::Value> = quotes.iter()
                .map(|q| {
                    let prev = prev_ranks.get(&q.symbol).copied();
                    coin_strength_score(q, prev)
                })
                .collect();
            
            scored.sort_by(|a, b| {
                b["score"].as_f64().unwrap_or(0.0)
                    .partial_cmp(&a["score"].as_f64().unwrap_or(0.0))
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            
            scored.truncate(TOP_N);
            
            // 添加排名和排名变化
            for (i, coin) in scored.iter_mut().enumerate() {
                let rank = i + 1;
                let prev = coin["prev_rank"].as_u64().map(|v| v as usize);
                let rank_change = prev.map(|p| p as i32 - rank as i32);
                
                let obj = coin.as_object_mut().unwrap();
                obj.insert("rank".to_string(), serde_json::json!(rank));
                obj.insert("rank_change".to_string(), serde_json::json!(rank_change));
            }
            
            // 更新缓存
            *st.markets.lock().await = Some((ts, quotes.clone()));
            
            ok_json(serde_json::json!({
                "coins": scored,
                "total": quotes.len(),
                "top_n": TOP_N,
                "updated_at": ts,
                "cache_hit": false,
            }))
        }
        Err(e) => err_resp(format!("行情拉取失败: {e}")),
    }
}

// ---------------- K线（任意周期 + 服务端缓存） ----------------

#[derive(Deserialize)]
struct KlineReq {
    #[serde(default = "default_interval")]
    interval: String,
    #[serde(default = "default_kline_limit")]
    limit: usize,
    /// 指定时返回 open_time <= end_time 的K线（前端向前翻页，不读写缓存）
    end_time: Option<u64>,
}

fn default_interval() -> String {
    "1d".into()
}
fn default_kline_limit() -> usize {
    500
}

async fn api_klines(
    State(st): State<Arc<AppState>>,
    AxPath(symbol): AxPath<String>,
    Query(q): Query<KlineReq>,
) -> Resp {
    let symbol = symbol.to_uppercase();
    // 品种/周期只允许字母数字（防路径注入与非法参数）
    if symbol.is_empty() || !symbol.chars().all(|c| c.is_ascii_alphanumeric()) {
        return err_resp("非法品种名".into());
    }
    if q.interval.is_empty() || !q.interval.chars().all(|c| c.is_ascii_alphanumeric()) {
        return err_resp("非法周期".into());
    }
    let limit = q.limit.clamp(1, 1000);
    // 向前翻页：直透 Binance，不动缓存
    if let Some(et) = q.end_time {
        let client = st.binance.clone();
        return match client
            .fetch_klines_window(&symbol, &q.interval, limit as u32, et)
            .await
        {
            Ok(ks) => ok_json(serde_json::json!({
                "symbol": symbol, "interval": q.interval, "total": 0, "klines": ks
            })),
            Err(e) => err_resp(format!("K线拉取失败: {e}")),
        };
    }
    serve_klines_cached(&st, &symbol, &q.interval, limit).await
}

/// 先命中本地缓存再增量补齐尾部（单次补齐上限 1000 根，控制首次延迟）；
/// 缓存文件 {SYMBOL}_{interval}.json 与回测数据目录兼容（{SYMBOL}_1d.json）。
async fn serve_klines_cached(st: &AppState, symbol: &str, interval: &str, limit: usize) -> Resp {
    let path = PathBuf::from(&st.cfg.data_dir).join(format!("{symbol}_{interval}.json"));
    let mut ks: Vec<Kline> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    // 缓存新鲜度：末根收盘时间落后约 2 个周期即补齐（留一周期的时钟/延迟容差）
    let now = now_ms();
    let stale = ks.is_empty()
        || now > ks.last().map(|k| k.close_time).unwrap_or(0) + 2 * interval_ms(interval);
    if stale {
        let client = st.binance.clone();
        match client.fetch_klines_history(symbol, interval, 1000, None).await {
            Ok(fetched) => {
                ks = merge_klines(ks, fetched);
                // 原子写回（临时文件 + rename）
                if let Ok(json) = serde_json::to_string(&ks) {
                    let tmp = path.with_extension("json.tmp");
                    if std::fs::write(&tmp, &json).is_ok() {
                        let _ = std::fs::rename(&tmp, &path);
                    }
                }
            }
            Err(e) => {
                if ks.is_empty() {
                    return err_resp(format!("K线拉取失败（无缓存）: {e}"));
                }
                // 已有缓存：网络失败时降级返回旧数据
            }
        }
    }
    let tail: Vec<&Kline> = ks.iter().rev().take(limit).collect::<Vec<_>>().into_iter().rev().collect();
    ok_json(serde_json::json!({
        "symbol": symbol, "interval": interval, "total": ks.len(), "klines": tail
    }))
}

/// 缓存与新拉K线合并：按 open_time 升序去重（同时间戳后者覆盖前者）
fn merge_klines(cached: Vec<Kline>, fetched: Vec<Kline>) -> Vec<Kline> {
    let mut map: BTreeMap<u64, Kline> = BTreeMap::new();
    for k in cached.into_iter().chain(fetched) {
        map.insert(k.open_time, k);
    }
    map.into_values().collect()
}

/// 周期对应的近似毫秒长度（缓存新鲜度判断用）
fn interval_ms(interval: &str) -> u64 {
    if interval.is_empty() {
        return 86_400_000;
    }
    let (num, unit) = interval.split_at(interval.len() - 1);
    let n: u64 = num.parse().unwrap_or(1).max(1);
    let base = match unit {
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        "w" => 7 * 86_400_000,
        _ => 86_400_000,
    };
    n * base
}

// ---------------- 因子选股（多因子截面快照） ----------------

#[derive(Deserialize)]
struct FactorsReq {
    /// 品种过滤（逗号分隔，空 = 全部本地日K品种）
    symbols: Option<String>,
}

/// 多因子截面：基于本地日K缓存计算（不请求交易所），样本不足 210 根的品种剔除。
/// 读接口，无需令牌。
async fn api_factors(State(st): State<Arc<AppState>>, Query(q): Query<FactorsReq>) -> Resp {
    let (data, load_err) = load_daily_series(&st.cfg.data_dir, q.symbols.as_deref());
    if let Err(e) = load_err {
        return err_resp(e);
    }
    let mut rows: Vec<factors::FactorRow> = data
        .iter()
        .filter_map(|(sym, ks)| factors::compute_raw(sym, ks))
        .collect();
    factors::finalize_scores(&mut rows);
    ok_json(serde_json::json!({
        "factors": rows,
        "count": rows.len(),
        "updated_at": now_ms(),
        "weights": { "动量": 0.30, "趋势": 0.20, "资金流": 0.15, "低波动": 0.15, "量能": 0.10, "低回撤": 0.10 },
    }))
}

/// 本地日K数据集：(品种名, 日K序列) 列表
type DailySeries = Vec<(String, Vec<Kline>)>;

/// 从本地数据目录加载全部品种日K（按品种名排序）；`filter` 为逗号分隔白名单。
/// 损坏/无法解析的文件静默跳过，不阻断全池。
fn load_daily_series(
    data_dir: &str,
    filter: Option<&str>,
) -> (DailySeries, Result<(), String>) {
    let whitelist: Vec<String> = filter
        .unwrap_or("")
        .split(',')
        .map(|s| s.trim().to_uppercase())
        .filter(|s| !s.is_empty())
        .collect();
    let dir = PathBuf::from(data_dir);
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) => return (Vec::new(), Err(format!("数据目录不可读: {e}"))),
    };
    let mut out: DailySeries = Vec::new();
    for f in entries.flatten() {
        let name = f.file_name().to_string_lossy().to_string();
        let Some(sym) = name.strip_suffix("_1d.json") else { continue };
        if sym.is_empty() || (!whitelist.is_empty() && !whitelist.contains(&sym.to_string())) {
            continue;
        }
        let Some(ks) = std::fs::read_to_string(f.path())
            .ok()
            .and_then(|s| serde_json::from_str::<Vec<Kline>>(&s).ok())
        else {
            continue;
        };
        out.push((sym.to_string(), ks));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    (out, Ok(()))
}

#[derive(Deserialize)]
struct CorrReq {
    /// 回看交易日数（默认 60，10..=365）
    days: Option<usize>,
    symbols: Option<String>,
}

/// 品种两两日收益率相关矩阵（近 N 个交易日，按日期对齐）。读接口，无需令牌。
async fn api_correlation(State(st): State<Arc<AppState>>, Query(q): Query<CorrReq>) -> Resp {
    let days = q.days.unwrap_or(60).clamp(10, 365);
    let (data, load_err) = load_daily_series(&st.cfg.data_dir, q.symbols.as_deref());
    if let Err(e) = load_err {
        return err_resp(e);
    }
    let (symbols, matrix, used) = factors::correlation_matrix(&data, days);
    ok_json(serde_json::json!({
        "symbols": symbols,
        "matrix": matrix,
        "days": used.min(days),
        "updated_at": now_ms(),
    }))
}

#[derive(Deserialize)]
struct IcReq {
    factor: String,
    /// 前瞻交易日数（默认 20，5..=120）
    horizon: Option<usize>,
    /// 评估日间隔（默认 5，1..=30）
    step: Option<usize>,
    symbols: Option<String>,
}

/// 因子 IC 回测验证：因子截面值与未来收益的滚动 Pearson 相关。读接口，无需令牌。
async fn api_factor_ic(State(st): State<Arc<AppState>>, Query(q): Query<IcReq>) -> Resp {
    if !factors::FACTORS.contains(&q.factor.as_str()) {
        return err_resp(format!(
            "未知因子: {}，可选: {}",
            q.factor,
            factors::FACTORS.join(", ")
        ));
    }
    let horizon = q.horizon.unwrap_or(20).clamp(5, 120);
    let step = q.step.unwrap_or(5).clamp(1, 30);
    let (data, load_err) = load_daily_series(&st.cfg.data_dir, q.symbols.as_deref());
    if let Err(e) = load_err {
        return err_resp(e);
    }
    match factors::factor_ic(&data, &q.factor, horizon, step) {
        Some(r) => ok_json(serde_json::json!({
            "factor": r.factor,
            "horizon": r.horizon,
            "step": r.step,
            "n": r.n,
            "ic_mean": r.ic_mean,
            "ic_std": r.ic_std,
            "icir": r.icir,
            "hit_rate": r.hit_rate,
            "series": r.series.iter().map(|(ms, ic)| serde_json::json!({ "ms": ms, "ic": ic })).collect::<Vec<_>>(),
        })),
        None => err_resp("样本不足，无法计算 IC（至少需 4 个品种且历史足够长）".into()),
    }
}

// ---------------- 数据中心 ----------------

async fn api_data(State(st): State<Arc<AppState>>) -> Resp {
    let files = scan_data_files(&st.cfg.data_dir);
    let dl = st.download.lock().await;
    ok_json(serde_json::json!({ "market": files, "download": *dl }))
}

#[derive(Deserialize)]
struct DlReq {
    symbols: String,
    #[serde(default = "default_max_bars")]
    max_bars: u32,
    /// K线周期，缺省日线（决定缓存文件名 {SYMBOL}_{interval}.json）
    #[serde(default = "default_interval")]
    interval: String,
}

fn default_max_bars() -> u32 {
    2000
}

async fn api_download(State(st): State<Arc<AppState>>, Json(req): Json<DlReq>) -> Resp {
    let symbols: Vec<String> = req
        .symbols
        .split(',')
        .map(|s| s.trim().to_uppercase())
        .filter(|s| !s.is_empty())
        .collect();
    if symbols.is_empty() {
        return err_resp("品种列表为空".into());
    }
    // 周期只允许字母数字（防路径注入，与 K线接口一致）
    if req.interval.is_empty() || !req.interval.chars().all(|c| c.is_ascii_alphanumeric()) {
        return err_resp("非法周期".into());
    }
    {
        let g = st.download.lock().await;
        if g.is_some() {
            return err_resp("已有下载任务进行中".into());
        }
    }
    let dir = PathBuf::from(&st.cfg.data_dir);
    std::fs::create_dir_all(&dir).ok();
    *st.download.lock().await = Some(DownloadTask {
        symbols: symbols.clone(),
        done: Vec::new(),
        running: symbols[0].clone(),
        started_at_ms: now_ms(),
    });
    let st2 = st.clone();
    let max_bars = req.max_bars;
    let interval = req.interval.clone();
    let symbols_for_task = symbols.clone();
    tokio::spawn(async move {
        let client = st2.binance.clone();
        for sym in symbols_for_task.iter() {
            {
                let mut g = st2.download.lock().await;
                if let Some(t) = g.as_mut() {
                    t.running = sym.clone();
                }
            }
            push_log(&st2, format!("[下载] {sym} 拉取 {interval} 历史K线（上限 {max_bars} 根）...")).await;
            match client.fetch_klines_history(sym, &interval, max_bars, None).await {
                Ok(ks) => {
                    let path = dir.join(format!("{sym}_{interval}.json"));
                    match serde_json::to_string(&ks) {
                        Ok(json) => {
                            let tmp = path.with_extension("json.tmp");
                            if std::fs::write(&tmp, &json).is_ok() && std::fs::rename(&tmp, &path).is_ok() {
                                push_log(&st2, format!("[下载] {sym} 完成: {} 根", ks.len())).await;
                            } else {
                                push_log(&st2, format!("[下载] {sym} 写文件失败")).await;
                            }
                        }
                        Err(e) => push_log(&st2, format!("[下载] {sym} 序列化失败: {e}")).await,
                    }
                    let mut g = st2.download.lock().await;
                    if let Some(t) = g.as_mut() {
                        t.done.push(sym.clone());
                    }
                }
                Err(e) => push_log(&st2, format!("[下载] {sym} 失败: {e}")).await,
            }
        }
        push_log(&st2, "[下载] 全部任务结束".to_string()).await;
        *st2.download.lock().await = None;
    });
    ok_json(serde_json::json!({ "accepted": symbols, "max_bars": req.max_bars }))
}

// ---------------- 回测 ----------------

#[derive(Deserialize, Serialize, Default)]
struct BtReq {
    strategy: Option<String>,
    symbols: Option<String>,
    /// K线周期（5m/15m/30m/1h/4h/12h/1d/1w），缺省为日线
    interval: Option<String>,
    cash: Option<f64>,
    momentum_days: Option<usize>,
    ma_days: Option<usize>,
    rebalance_days: Option<u64>,
    trailing_stop: Option<f64>,
    /// 持仓品种数（1=集中轮动；>1=动量前N等额分散）
    top_n: Option<usize>,
    /// 市场状态过滤均线天数（0=禁用）
    regime_ma: Option<usize>,
    /// 熊市广度阈值（0-1）
    regime_breadth: Option<f64>,
    /// 组合回撤熔断阈值（<=0 禁用）
    circuit_breaker: Option<f64>,
    /// 熔断后冷却天数
    circuit_cooldown: Option<u64>,
    /// ma_cross 快线周期（根）
    ma_fast: Option<usize>,
    /// ma_cross 慢线周期（根，须大于快线）
    ma_slow: Option<usize>,
    /// grid 网格格数（区间等分数）
    grid_levels: Option<usize>,
    /// grid 区间回看天数（用最近这段时间的最高/最低价定上下界）
    grid_lookback_days: Option<usize>,
    /// grid 止损：跌破区间下界该比例即清仓（0=关闭）
    grid_stop_loss: Option<f64>,
    /// grid 每品种网格预算（0=首次布网时按品种数均分现金）
    grid_budget: Option<f64>,
    /// dca 每期每品种买入金额
    dca_amount: Option<f64>,
    /// dca 定投间隔天数（7=周投）
    dca_interval_days: Option<u64>,
    /// dca 智能加码趋势均线天数（0=关闭加码）
    dca_ma_days: Option<usize>,
    /// dca 收盘价低于趋势均线时的加码倍数
    dca_dip_multiplier: Option<f64>,
    fee: Option<f64>,
    fill: Option<String>,
    /// 回测窗口起/止（YYYY-MM-DD）
    start: Option<String>,
    end: Option<String>,
    /// sweep 网格声明，如 "momentum_days=30,60;trailing_stop=0.08,0.12"
    grid: Option<String>,
    /// sweep 训练/测试切分比例（0-1）
    split: Option<f64>,
    /// 基准品种（买入持有对比，如 BTCUSDT）
    benchmark: Option<String>,
    /// walkforward 折数（>=1）
    windows: Option<usize>,
    /// walkforward 每折训练窗占总跨度比例（0-1）
    train_ratio: Option<f64>,
    /// walkforward 窗口模式：true=扩张窗（训练起点固定），缺省/false=滚动窗
    anchored: Option<bool>,
    /// walkforward 选参指标（annualized/sharpe/calmar/sortino），CLI 侧校验非法值
    rank: Option<String>,
}

fn bt_args(req: &BtReq) -> Vec<String> {
    let mut a: Vec<String> = Vec::new();
    if let Some(v) = &req.strategy {
        a.extend(["--strategy".into(), v.clone()]);
    }
    if let Some(v) = &req.symbols {
        if !v.trim().is_empty() {
            a.extend(["--symbols".into(), v.trim().to_string()]);
        }
    }
    if let Some(v) = &req.interval {
        if !v.trim().is_empty() {
            a.extend(["--interval".into(), v.trim().to_string()]);
        }
    }
    if let Some(v) = req.cash {
        a.extend(["--cash".into(), v.to_string()]);
    }
    if let Some(v) = req.momentum_days {
        a.extend(["--momentum-days".into(), v.to_string()]);
    }
    if let Some(v) = req.ma_days {
        a.extend(["--ma-days".into(), v.to_string()]);
    }
    if let Some(v) = req.rebalance_days {
        a.extend(["--rebalance-days".into(), v.to_string()]);
    }
    if let Some(v) = req.trailing_stop {
        a.extend(["--trailing-stop".into(), v.to_string()]);
    }
    if let Some(v) = req.top_n {
        a.extend(["--top-n".into(), v.to_string()]);
    }
    if let Some(v) = req.regime_ma {
        a.extend(["--regime-ma".into(), v.to_string()]);
    }
    if let Some(v) = req.regime_breadth {
        a.extend(["--regime-breadth".into(), v.to_string()]);
    }
    if let Some(v) = req.circuit_breaker {
        a.extend(["--circuit-breaker".into(), v.to_string()]);
    }
    if let Some(v) = req.circuit_cooldown {
        a.extend(["--circuit-cooldown".into(), v.to_string()]);
    }
    if let Some(v) = req.ma_fast {
        a.extend(["--ma-fast".into(), v.to_string()]);
    }
    if let Some(v) = req.ma_slow {
        a.extend(["--ma-slow".into(), v.to_string()]);
    }
    if let Some(v) = req.grid_levels {
        a.extend(["--grid-levels".into(), v.to_string()]);
    }
    if let Some(v) = req.grid_lookback_days {
        a.extend(["--grid-lookback-days".into(), v.to_string()]);
    }
    if let Some(v) = req.grid_stop_loss {
        a.extend(["--grid-stop-loss".into(), v.to_string()]);
    }
    if let Some(v) = req.grid_budget {
        a.extend(["--grid-budget".into(), v.to_string()]);
    }
    if let Some(v) = req.dca_amount {
        a.extend(["--dca-amount".into(), v.to_string()]);
    }
    if let Some(v) = req.dca_interval_days {
        a.extend(["--dca-interval-days".into(), v.to_string()]);
    }
    if let Some(v) = req.dca_ma_days {
        a.extend(["--dca-ma-days".into(), v.to_string()]);
    }
    if let Some(v) = req.dca_dip_multiplier {
        a.extend(["--dca-dip-multiplier".into(), v.to_string()]);
    }
    if let Some(v) = req.fee {
        a.extend(["--fee".into(), v.to_string()]);
    }
    if let Some(v) = &req.fill {
        a.extend(["--fill".into(), v.clone()]);
    }
    if let Some(v) = &req.start {
        a.extend(["--start".into(), v.clone()]);
    }
    if let Some(v) = &req.end {
        a.extend(["--end".into(), v.clone()]);
    }
    if let Some(v) = &req.grid {
        if !v.trim().is_empty() {
            a.extend(["--grid".into(), v.trim().to_string()]);
        }
    }
    if let Some(v) = req.split {
        a.extend(["--split".into(), v.to_string()]);
    }
    if let Some(v) = &req.benchmark {
        if !v.trim().is_empty() {
            a.extend(["--benchmark".into(), v.trim().to_string()]);
        }
    }
    if let Some(v) = req.windows {
        a.extend(["--windows".into(), v.to_string()]);
    }
    if let Some(v) = req.train_ratio {
        a.extend(["--train-ratio".into(), v.to_string()]);
    }
    // --anchored 是布尔开关，CLI 只检查该 flag 是否存在：false 必须整个不传，
    // 更不能在其后跟值（会被当成下一个位置参数）
    if req.anchored == Some(true) {
        a.push("--anchored".into());
    }
    if let Some(v) = &req.rank {
        if !v.trim().is_empty() {
            a.extend(["--rank".into(), v.trim().to_string()]);
        }
    }
    a
}

async fn run_cli(st: &AppState, args: &[String]) -> Result<String, String> {
    let out = tokio::process::Command::new(&st.bin)
        .args(args)
        .output()
        .await
        .map_err(|e| format!("无法执行 CLI({}): {e}（请先 cargo build）", st.bin.display()))?;
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    if !out.status.success() {
        return Err(format!("命令失败: {}{}", stdout, stderr));
    }
    Ok(stdout)
}

async fn api_backtest(State(st): State<Arc<AppState>>, Json(req): Json<BtReq>) -> Resp {
    let mut args = vec!["backtest".to_string(), "--json".to_string()];
    args.extend(bt_args(&req));
    match run_cli(&st, &args).await {
        Ok(out) => {
            // stdout 最后一行是 JSON
            let line = out.lines().rev().find(|l| l.starts_with('{')).unwrap_or("");
            match serde_json::from_str::<serde_json::Value>(line) {
                Ok(v) => {
                    // 成功的回测一律落盘存档（失败静默，不影响返回）
                    if let Some(id) = save_backtest_record(&st, &req, &v) {
                        push_log(&st, format!("[回测] 已存档记录 {id}")).await;
                    }
                    ok_json(v)
                }
                Err(e) => err_resp(format!("解析回测输出失败: {e}\n{out}")),
            }
        }
        Err(e) => err_resp(e),
    }
}

// ---------------- 回测记录（存档/列表/详情/删除） ----------------

fn backtests_dir(st: &AppState) -> PathBuf {
    PathBuf::from(&st.cfg.data_dir).join("backtests")
}

/// 记录 ID 只允许字母数字与 -_（防路径注入）
fn valid_record_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 40
        && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// 存档一条回测记录（原子写）；ID 用毫秒时间戳，冲突时追加后缀（概率极低）
fn save_backtest_record(st: &AppState, req: &BtReq, result: &serde_json::Value) -> Option<String> {
    let dir = backtests_dir(st);
    std::fs::create_dir_all(&dir).ok()?;
    let mut id = now_ms().to_string();
    while dir.join(format!("{id}.json")).exists() {
        id.push('x');
    }
    let record = serde_json::json!({
        "id": id,
        "created_at_ms": now_ms(),
        "request": serde_json::to_value(req).ok()?,
        "result": result,
    });
    let path = dir.join(format!("{id}.json"));
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string(&record).ok()?).ok()?;
    std::fs::rename(&tmp, &path).ok()?;
    Some(id)
}

/// 记录列表（只读公开）：按时间倒序，最多 100 条，只返摘要指标不返曲线/回合
async fn api_backtests(State(st): State<Arc<AppState>>) -> Resp {
    let dir = backtests_dir(&st);
    let mut items: Vec<serde_json::Value> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for f in rd.flatten() {
            let name = f.file_name().to_string_lossy().to_string();
            let Some(id) = name.strip_suffix(".json") else { continue };
            if !valid_record_id(id) {
                continue;
            }
            let rec: serde_json::Value = match std::fs::read_to_string(f.path())
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
            {
                Some(v) => v,
                None => continue, // 损坏文件跳过，不阻断列表
            };
            let result = &rec["result"];
            items.push(serde_json::json!({
                "id": id,
                "created_at_ms": rec.get("created_at_ms"),
                "strategy": result.get("strategy"),
                "symbols": result.get("symbols"),
                "benchmark": result.get("benchmark").map(|b| b["symbol"].clone()),
                "metrics": result.get("metrics"),
            }));
        }
    }
    // ID 为毫秒时间戳，字典序即时间序（倒序）
    items.sort_by(|a, b| {
        b["id"].as_str().unwrap_or("").cmp(a["id"].as_str().unwrap_or(""))
    });
    items.truncate(100);
    ok_json(serde_json::json!(items))
}

async fn api_backtest_detail(State(st): State<Arc<AppState>>, AxPath(id): AxPath<String>) -> Resp {
    if !valid_record_id(&id) {
        return err_resp("非法记录 ID".into());
    }
    let path = backtests_dir(&st).join(format!("{id}.json"));
    match std::fs::read_to_string(&path) {
        Ok(s) => match serde_json::from_str::<serde_json::Value>(&s) {
            Ok(v) => ok_json(v),
            Err(e) => err_resp(format!("记录解析失败: {e}")),
        },
        Err(_) => err_resp(format!("回测记录 {id} 不存在")),
    }
}

async fn api_backtest_delete(State(st): State<Arc<AppState>>, AxPath(id): AxPath<String>) -> Resp {
    if !valid_record_id(&id) {
        return err_resp("非法记录 ID".into());
    }
    let path = backtests_dir(&st).join(format!("{id}.json"));
    if !path.exists() {
        return err_resp(format!("回测记录 {id} 不存在"));
    }
    match std::fs::remove_file(&path) {
        Ok(_) => ok_json(serde_json::json!({ "deleted": id })),
        Err(e) => err_resp(format!("删除失败: {e}")),
    }
}

async fn api_sweep(State(st): State<Arc<AppState>>, Json(req): Json<BtReq>) -> Resp {
    // CLI 以 JSONL 输出（每行一个参数组合），直接收集为数组，无需文本解析

    let mut args = vec!["sweep".to_string(), "--json".to_string()];
    args.extend(bt_args(&req));
    match run_cli(&st, &args).await {
        Ok(out) => {
            let rows: Vec<serde_json::Value> = out
                .lines()
                .filter(|l| l.trim_start().starts_with('{'))
                .filter_map(|l| serde_json::from_str(l).ok())
                .collect();
            if rows.is_empty() {
                return err_resp(format!("解析 sweep 输出失败:\n{out}"));
            }
            ok_json(serde_json::json!(rows))
        }
        Err(e) => err_resp(e),
    }
}

/// 为 dry-run / live 构造 CLI 参数：只转发「策略与执行」相关项。
///
/// 刻意不复用 [`bt_args`]：回测专用项（start/end/benchmark/grid/split/windows/
/// train_ratio/anchored/rank）对常驻运行没有意义，透传过去只会让人误以为生效。
///
/// 这个函数存在的原因是一个信任缺陷：此前启动接口只传 `--data`，
/// 于是模拟盘/实盘永远跑 `quantkit.toml` 里的策略——用户在界面上验证完网格策略
/// 再点「启动模拟盘」，实际跑起来的却是动量轮动，而且没有任何提示。
fn run_args(req: &BtReq) -> Vec<String> {
    let mut a: Vec<String> = Vec::new();
    if let Some(v) = &req.strategy {
        a.extend(["--strategy".into(), v.clone()]);
    }
    if let Some(v) = &req.interval {
        if !v.trim().is_empty() {
            a.extend(["--interval".into(), v.trim().to_string()]);
        }
    }
    if let Some(v) = &req.symbols {
        if !v.trim().is_empty() {
            a.extend(["--symbols".into(), v.trim().to_string()]);
        }
    }
    if let Some(v) = req.cash {
        a.extend(["--cash".into(), v.to_string()]);
    }
    if let Some(v) = req.fee {
        a.extend(["--fee".into(), v.to_string()]);
    }
    if let Some(v) = &req.fill {
        a.extend(["--fill".into(), v.clone()]);
    }
    // 动量轮动
    if let Some(v) = req.momentum_days {
        a.extend(["--momentum-days".into(), v.to_string()]);
    }
    if let Some(v) = req.ma_days {
        a.extend(["--ma-days".into(), v.to_string()]);
    }
    if let Some(v) = req.rebalance_days {
        a.extend(["--rebalance-days".into(), v.to_string()]);
    }
    if let Some(v) = req.trailing_stop {
        a.extend(["--trailing-stop".into(), v.to_string()]);
    }
    // 风控与分散
    if let Some(v) = req.top_n {
        a.extend(["--top-n".into(), v.to_string()]);
    }
    if let Some(v) = req.regime_ma {
        a.extend(["--regime-ma".into(), v.to_string()]);
    }
    if let Some(v) = req.regime_breadth {
        a.extend(["--regime-breadth".into(), v.to_string()]);
    }
    if let Some(v) = req.circuit_breaker {
        a.extend(["--circuit-breaker".into(), v.to_string()]);
    }
    if let Some(v) = req.circuit_cooldown {
        a.extend(["--circuit-cooldown".into(), v.to_string()]);
    }
    // 均线交叉
    if let Some(v) = req.ma_fast {
        a.extend(["--ma-fast".into(), v.to_string()]);
    }
    if let Some(v) = req.ma_slow {
        a.extend(["--ma-slow".into(), v.to_string()]);
    }
    // 智能网格
    if let Some(v) = req.grid_levels {
        a.extend(["--grid-levels".into(), v.to_string()]);
    }
    if let Some(v) = req.grid_lookback_days {
        a.extend(["--grid-lookback-days".into(), v.to_string()]);
    }
    if let Some(v) = req.grid_stop_loss {
        a.extend(["--grid-stop-loss".into(), v.to_string()]);
    }
    if let Some(v) = req.grid_budget {
        a.extend(["--grid-budget".into(), v.to_string()]);
    }
    // 定投
    if let Some(v) = req.dca_amount {
        a.extend(["--dca-amount".into(), v.to_string()]);
    }
    if let Some(v) = req.dca_interval_days {
        a.extend(["--dca-interval-days".into(), v.to_string()]);
    }
    if let Some(v) = req.dca_ma_days {
        a.extend(["--dca-ma-days".into(), v.to_string()]);
    }
    if let Some(v) = req.dca_dip_multiplier {
        a.extend(["--dca-dip-multiplier".into(), v.to_string()]);
    }
    a
}

async fn api_walkforward(State(st): State<Arc<AppState>>, Json(req): Json<BtReq>) -> Resp {
    let mut args = vec!["walkforward".to_string(), "--json".to_string()];
    args.extend(bt_args(&req));
    match run_cli(&st, &args).await {
        Ok(out) => {
            // CLI 以 JSONL 输出：前 N 行为逐折结果，末行带 summary=true 为汇总。
            // 这里拆成两段返回，前端无需按标记字段自行分流
            let mut folds: Vec<serde_json::Value> = Vec::new();
            let mut summary: Option<serde_json::Value> = None;
            for line in out.lines().filter(|l| l.trim_start().starts_with('{')) {
                let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                    continue;
                };
                if v["summary"].as_bool() == Some(true) {
                    summary = Some(v);
                } else {
                    folds.push(v);
                }
            }
            match summary {
                Some(s) if !folds.is_empty() => {
                    ok_json(serde_json::json!({ "folds": folds, "summary": s }))
                }
                _ => err_resp(format!("解析 walkforward 输出失败:\n{out}")),
            }
        }
        Err(e) => err_resp(e),
    }
}

// ---------------- 模拟盘 / 实盘 ----------------

async fn api_run_start(
    State(st): State<Arc<AppState>>,
    AxPath(kind): AxPath<String>,
    body: Option<Json<BtReq>>,
) -> Resp {
    let cmd = match kind.as_str() {
        "dryrun" => "dry-run",
        "live" => "live",
        _ => return err_resp(format!("未知运行模式: {kind}")),
    };
    let slot = match kind.as_str() {
        "dryrun" => &st.dryrun,
        _ => &st.live,
    };
    let mut guard = slot.lock().await;
    if guard.is_some() {
        return err_resp(format!("{kind} 已在运行"));
    }
    // live 前置检查：配置开关 + 密钥（CLI 内部还有四道门禁，这里是快速失败）
    if kind == "live" {
        if !st.cfg.live_enabled {
            return err_resp("live 未开启：需在 quantkit.toml 设 live_enabled = true".into());
        }
        if std::env::var("BINANCE_API_KEY").is_err() || std::env::var("BINANCE_API_SECRET").is_err() {
            // 环境变量缺失时回退配置文件 [binance] 段（与 binance-rust 连接方式一致）
            let (bk, bs) = resolve_binance_keys(&st.cfg);
            if bk.is_none() || bs.is_none() {
                return err_resp("缺少 Binance 密钥：设环境变量 BINANCE_API_KEY / BINANCE_API_SECRET，或在 quantkit.toml 的 [binance] 段配置".into());
            }
        }
    }
    // 构造子进程：live 需要管道化 stdin（用于代答二次确认），其余继承默认
    let extra = body.map(|Json(req)| run_args(&req)).unwrap_or_default();
    let mut cmd_builder = tokio::process::Command::new(&st.bin);
    cmd_builder
        .arg(cmd)
        .arg("--data")
        .arg(&st.cfg.data_dir)
        .args(&extra)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    if kind == "live" {
        cmd_builder.stdin(std::process::Stdio::piped());
    }
    let mut child = match cmd_builder.spawn() {
        Ok(c) => c,
        Err(e) => return err_resp(format!("启动失败: {e}")),
    };
    // live 的 CLI 二次确认（输入 YES）由 WebUI 的显式"启动"点击代行：
    // 该接口本身受 Token 保护，且上方已完成配置开关 + 密钥前置检查，
    // 不代答则子进程因无交互输入直接退出（历史缺陷）。
    if kind == "live" {
        if let Some(mut stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            if let Err(e) = stdin.write_all(b"YES\n").await {
                push_log(&st, format!("[live] 二次确认写入失败: {e}")).await;
            }
            drop(stdin); // 关闭输入，子进程继续执行
        }
    }
    let pid = child.id().unwrap_or(0);
    push_log(&st, format!("[{kind}] 已启动 (pid={pid})")).await;
    // 生效参数入日志：此前无法追溯「究竟跑的是什么」，正是策略静默走偏没被发现的原因
    if extra.is_empty() {
        push_log(
            &st,
            format!("[{kind}] 未指定策略参数，沿用 quantkit.toml：strategy={} interval={}", st.cfg.strategy, st.cfg.interval),
        )
        .await;
    } else {
        push_log(&st, format!("[{kind}] 生效参数: {}", extra.join(" "))).await;
    }
    // 日志泵：stdout + stderr 汇入统一日志
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let st2 = st.clone();
    let tag = kind.clone();
    tokio::spawn(async move {
        // select! 并发消费两个流：串行读会在一个流静默时阻塞另一个，导致日志丢失/延迟
        let mut stdout = stdout.map(|o| tokio::io::BufReader::new(o).lines());
        let mut stderr = stderr.map(|e| tokio::io::BufReader::new(e).lines());
        loop {
            tokio::select! {
                r = async { stdout.as_mut().unwrap().next_line().await }, if stdout.is_some() => match r {
                    Ok(Some(line)) => push_log(&st2, format!("[{tag}] {line}")).await,
                    _ => stdout = None,
                },
                r = async { stderr.as_mut().unwrap().next_line().await }, if stderr.is_some() => match r {
                    Ok(Some(line)) => push_log(&st2, format!("[{tag}/err] {line}")).await,
                    _ => stderr = None,
                },
                else => break,
            }
        }
    });
    *guard = Some(RunHandle { pid, started_at_ms: now_ms(), child });
    ok_json(serde_json::json!({ "started": kind, "pid": pid }))
}

async fn api_run_stop(State(st): State<Arc<AppState>>, AxPath(kind): AxPath<String>) -> Resp {
    let slot = match kind.as_str() {
        "dryrun" => &st.dryrun,
        _ => &st.live,
    };
    let mut guard = slot.lock().await;
    match guard.as_mut() {
        None => err_resp(format!("{kind} 未在运行")),
        Some(h) => {
            let pid = h.pid;
            let _ = h.child.kill().await;
            let _ = h.child.wait().await;
            *guard = None;
            push_log(&st, format!("[{kind}] 已停止 (pid={pid})")).await;
            ok_json(serde_json::json!({ "stopped": kind, "pid": pid }))
        }
    }
}

async fn api_run_logs(State(st): State<Arc<AppState>>, AxPath(kind): AxPath<String>) -> Resp {
    if !matches!(kind.as_str(), "dryrun" | "live" | "all") {
        return err_resp(format!("未知运行模式: {kind}"));
    }
    let logs = st.logs.lock().await;
    let tail: Vec<&String> = logs.iter().rev().take(300).collect();
    let mut out: Vec<String> = tail.into_iter().rev().cloned().collect();
    if kind != "all" {
        let prefix = format!("[{kind}");
        out.retain(|l| l.contains(&prefix));
    }
    ok_json(serde_json::json!({ "lines": out }))
}

// ---------------- 实盘监控（读 live 状态文件，不依赖实盘进程在线） ----------------

/// live 状态文件路径（与 live 通道同规则：`live_{state_file}`）
fn load_live_state(cfg: &AppConfig) -> Result<LiveState, String> {
    let path = PathBuf::from(&cfg.state_file).with_file_name(format!("live_{}", cfg.state_file));
    let s = std::fs::read_to_string(&path)
        .map_err(|_| format!("实盘状态文件不存在（尚未运行过实盘）: {}", path.display()))?;
    serde_json::from_str(&s).map_err(|e| format!("实盘状态文件解析失败: {e}"))
}

/// 实盘持仓盯市：最新价/市值/浮动盈亏（公开行情接口取价，无需密钥；
/// USDT 与总资产来自实盘进程每轮写入的权益快照）
async fn api_live_positions(State(st): State<Arc<AppState>>) -> Resp {
    let state = match load_live_state(&st.cfg) {
        Ok(s) => s,
        Err(e) => return err_resp(e),
    };
    let mut positions = Vec::new();
    for p in &state.positions {
        let last_price = st
            .binance
            .fetch_last_price(&p.symbol)
            .await
            .unwrap_or(f64::NAN);
        let value = p.quantity * last_price;
        let pnl = (last_price - p.avg_entry_price) * p.quantity;
        let pnl_pct = if p.avg_entry_price > 0.0 {
            (last_price - p.avg_entry_price) / p.avg_entry_price
        } else {
            f64::NAN
        };
        positions.push(serde_json::json!({
            "symbol": p.symbol,
            "quantity": p.quantity,
            "avg_entry_price": p.avg_entry_price,
            "last_price": last_price,
            "value": value,
            "pnl": pnl,
            "pnl_pct": pnl_pct,
        }));
    }
    let last_snap = state.equity_history.last();
    ok_json(serde_json::json!({
        "positions": positions,
        "usdt": last_snap.map(|s| s.usdt),
        "total": last_snap.map(|s| s.total),
        "positions_value": last_snap.map(|s| s.positions_value),
        "last_bar_ts": state.last_bar_ts,
        "updated_at_ms": state.updated_at_ms,
    }))
}

/// 当日盈亏：最新点相对当日（UTC）首点的变化；当日只有一点时返回 None。
/// 纯函数可单测。
fn day_change(series: &[EquitySnap]) -> Option<(f64, f64)> {
    if series.len() < 2 {
        return None;
    }
    let last = series.last()?;
    let (y, mo, d, _, _, _) = epoch_to_ymdhms(last.ts / 1000);
    let day_start = series.iter().find(|s| {
        let (y2, mo2, d2, _, _, _) = epoch_to_ymdhms(s.ts / 1000);
        (y2, mo2, d2) == (y, mo, d)
    })?;
    if day_start.ts == last.ts {
        return None;
    }
    let chg = last.total - day_start.total;
    let pct = if day_start.total != 0.0 { chg / day_start.total } else { f64::NAN };
    Some((chg, pct))
}

/// 实盘权益曲线：实盘进程每轮盯市写入的快照序列 + 当日盈亏
async fn api_live_equity(State(st): State<Arc<AppState>>) -> Resp {
    let state = match load_live_state(&st.cfg) {
        Ok(s) => s,
        Err(e) => return err_resp(e),
    };
    let (day_pnl, day_pnl_pct) = match day_change(&state.equity_history) {
        Some((c, p)) => (Some(c), Some(p)),
        None => (None, None),
    };
    ok_json(serde_json::json!({
        "series": state.equity_history,
        "day_pnl": day_pnl,
        "day_pnl_pct": day_pnl_pct,
    }))
}

/// 实盘成交流水（倒序，最新在前），含成交原因；`limit` 默认 100、上限 1000
async fn api_live_fills(
    State(st): State<Arc<AppState>>,
    Query(q): Query<BTreeMap<String, String>>,
) -> Resp {
    let state = match load_live_state(&st.cfg) {
        Ok(s) => s,
        Err(e) => return err_resp(e),
    };
    let limit: usize = q
        .get("limit")
        .and_then(|s| s.parse().ok())
        .unwrap_or(100)
        .min(1000);
    let fills: Vec<serde_json::Value> = state
        .fills
        .iter()
        .rev()
        .take(limit)
        .map(|f| {
            serde_json::json!({
                "ts": f.ts,
                "symbol": f.symbol,
                "side": match f.side { Side::Buy => "买入", Side::Sell => "卖出" },
                "quantity": f.quantity,
                "price": f.price,
                "fee": f.fee,
                "reason": f.reason,
            })
        })
        .collect();
    ok_json(serde_json::json!({ "fills": fills, "total": state.fills.len() }))
}

/// 真实账户资产：Binance 全量非零余额 + 逐资产盯市估值（15s 缓存）。
/// 不依赖实盘进程/状态文件，用配置密钥直查交易所；估值口径：
/// USDT 按 1，其余按 {asset}USDT 最新价；无 USDT 交易对的资产只给数量不算估值。
async fn api_live_account(State(st): State<Arc<AppState>>) -> Resp {
    const CACHE_MS: u64 = 15_000;
    let now = now_ms();
    {
        let g = st.account.lock().await;
        if let Some((ts, v)) = g.as_ref() {
            if now.saturating_sub(*ts) < CACHE_MS {
                return ok_json(v.clone());
            }
        }
    }
    // 签名客户端：密钥取环境变量或配置文件 [binance] 段
    let (key, secret) = resolve_binance_keys(&st.cfg);
    let (Some(key), Some(secret)) = (key, secret) else {
        return err_resp("未配置 Binance 密钥（环境变量或 quantkit.toml [binance] 段）".into());
    };
    let client = BinanceClient::with_credentials(Some(key), Some(secret));
    let balances = match client.fetch_balances().await {
        Ok(b) => b,
        Err(e) => return err_resp(format!("余额查询失败: {e}")),
    };
    let mut assets: Vec<serde_json::Value> = Vec::new();
    let mut total = 0.0;
    for (asset, free) in &balances {
        // USDT 无需取价；其余用 {asset}USDT 最新价，失败则估值为 null（不计入总资产）
        let price = if asset == "USDT" {
            Some(1.0)
        } else {
            st.binance
                .fetch_last_price(&format!("{asset}USDT"))
                .await
                .ok()
                .filter(|p| p.is_finite())
        };
        let value = price.map(|p| free * p);
        if let Some(v) = value {
            total += v;
        }
        assets.push(serde_json::json!({
            "asset": asset,
            "free": free,
            "price": price,
            "value": value,
        }));
    }
    // 市值降序；无法估值的排最后（按数量降序）
    assets.sort_by(|a, b| {
        let va = a["value"].as_f64();
        let vb = b["value"].as_f64();
        match (va, vb) {
            (Some(x), Some(y)) => y.partial_cmp(&x).unwrap_or(std::cmp::Ordering::Equal),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => b["free"]
                .as_f64()
                .unwrap_or(0.0)
                .partial_cmp(&a["free"].as_f64().unwrap_or(0.0))
                .unwrap_or(std::cmp::Ordering::Equal),
        }
    });
    // 灰尘资产：市值 <$10（低于常见 minNotional，策略无法处理，提示人工关注）
    let dust: Vec<String> = assets
        .iter()
        .filter(|a| a["value"].as_f64().map(|v| v < 10.0).unwrap_or(false))
        .map(|a| a["asset"].as_str().unwrap_or("").to_string())
        .collect();
    let out = serde_json::json!({
        "assets": assets,
        "total": total,
        "dust": dust,
        "updated_at_ms": now,
    });
    *st.account.lock().await = Some((now, out.clone()));
    ok_json(out)
}

// ---------------- 实盘绩效分析 ----------------

/// 已平仓交易轮次：一次完整“买入→卖出”为一轮，净利润 = 毛盈亏 − 两腿手续费（净利润口径）
#[derive(Debug, Clone, Serialize)]
struct TradeRound {
    symbol: String,
    buy_ts: u64,
    sell_ts: u64,
    quantity: f64,
    buy_price: f64,
    sell_price: f64,
    fee: f64,
    net_profit: f64,
    /// 净利润 / 买入成本（小数，0.01 = 1%）
    net_pct: f64,
    hold_days: f64,
}

/// 未平仓轮次（配对后仍有剩余买入量）
#[derive(Debug, Clone, Serialize)]
struct OpenRound {
    symbol: String,
    quantity: f64,
    avg_cost: f64,
    open_ts: u64,
}

/// 买卖配对（按品种、加权平均成本）：部分卖出按持仓比例分摊买入手续费；
/// 卖出手续费按匹配量占卖出量比例分摊。成交需按时间升序传入。
fn pair_rounds(fills: &[LiveFill]) -> (Vec<TradeRound>, Vec<OpenRound>) {
    #[derive(Default)]
    struct Pos {
        qty: f64,
        cost: f64,
        fee: f64,
        open_ts: u64,
    }
    let mut pos: std::collections::BTreeMap<String, Pos> = std::collections::BTreeMap::new();
    let mut rounds = Vec::new();
    for f in fills {
        let empty = pos.get(&f.symbol).map(|p| p.qty <= 1e-12).unwrap_or(true);
        let p = pos.entry(f.symbol.clone()).or_default();
        match f.side {
            Side::Buy => {
                if empty {
                    p.open_ts = f.ts;
                }
                p.qty += f.quantity;
                p.cost += f.quantity * f.price;
                p.fee += f.fee;
            }
            Side::Sell => {
                if p.qty <= 1e-12 {
                    continue; // 无持仓可卖（理论上不会发生，防御性跳过）
                }
                let matched = f.quantity.min(p.qty);
                let avg = p.cost / p.qty;
                let gross = (f.price - avg) * matched;
                let buy_fee = p.fee * matched / p.qty;
                let sell_fee = if f.quantity > 0.0 { f.fee * matched / f.quantity } else { f.fee };
                let fee = buy_fee + sell_fee;
                let net = gross - fee;
                let cost_matched = avg * matched;
                rounds.push(TradeRound {
                    symbol: f.symbol.clone(),
                    buy_ts: p.open_ts,
                    sell_ts: f.ts,
                    quantity: matched,
                    buy_price: avg,
                    sell_price: f.price,
                    fee,
                    net_profit: net,
                    net_pct: if cost_matched > 0.0 { net / cost_matched } else { 0.0 },
                    hold_days: f.ts.saturating_sub(p.open_ts) as f64 / 86_400_000.0,
                });
                p.qty -= matched;
                p.cost -= cost_matched;
                p.fee -= buy_fee;
            }
        }
    }
    let opens = pos
        .iter()
        .filter(|(_, p)| p.qty > 1e-12)
        .map(|(sym, p)| OpenRound {
            symbol: sym.clone(),
            quantity: p.qty,
            avg_cost: p.cost / p.qty,
            open_ts: p.open_ts,
        })
        .collect();
    (rounds, opens)
}

/// 权益曲线回撤序列（%，≤0）与最大回撤；峰值取历史最高总资产。
fn drawdown_series(snaps: &[EquitySnap]) -> (Vec<(u64, f64)>, f64) {
    let mut peak = 0.0_f64;
    let mut min_dd = 0.0_f64;
    let out = snaps
        .iter()
        .map(|s| {
            peak = peak.max(s.total);
            let dd = if peak > 0.0 { (s.total / peak - 1.0) * 100.0 } else { 0.0 };
            min_dd = min_dd.min(dd);
            (s.ts, dd)
        })
        .collect();
    (out, min_dd)
}

/// 实盘绩效：轮次配对（净利润口径）、胜率、总手续费、回撤序列与最大回撤。
/// 基于状态文件的成交/权益快照，不请求交易所，无缓存（文件读取代价低）。
async fn api_live_analysis(State(st): State<Arc<AppState>>) -> Resp {
    let state = match load_live_state(&st.cfg) {
        Ok(s) => s,
        Err(e) => return err_resp(e),
    };
    let (rounds, opens) = pair_rounds(&state.fills);
    // 归一化负零（无平仓/无手续费时求和可能得 -0.0，序列化会带负号）
    let closed_profit: f64 = {
        let s: f64 = rounds.iter().map(|r| r.net_profit).sum();
        if s == 0.0 { 0.0 } else { s }
    };
    let total_fee: f64 = {
        let s: f64 = state.fills.iter().map(|f| f.fee).sum();
        if s == 0.0 { 0.0 } else { s }
    };
    let wins = rounds.iter().filter(|r| r.net_profit > 0.0).count();
    let win_rate = if rounds.is_empty() {
        None
    } else {
        Some(wins as f64 / rounds.len() as f64)
    };
    let (dd, max_dd_pct) = drawdown_series(&state.equity_history);
    let equity_drawdown: Vec<serde_json::Value> = dd
        .into_iter()
        .map(|(ts, v)| serde_json::json!({ "ts": ts, "dd_pct": v }))
        .collect();
    let mut rounds_json: Vec<serde_json::Value> = rounds
        .iter()
        .rev()
        .map(|r| {
            serde_json::json!({
                "symbol": r.symbol,
                "buy_ts": r.buy_ts,
                "sell_ts": r.sell_ts,
                "quantity": r.quantity,
                "buy_price": r.buy_price,
                "sell_price": r.sell_price,
                "fee": r.fee,
                "net_profit": r.net_profit,
                "net_pct": r.net_pct,
                "hold_days": r.hold_days,
            })
        })
        .collect();
    rounds_json.truncate(100);
    ok_json(serde_json::json!({
        "rounds": rounds_json,
        "open_positions": opens,
        "round_count": rounds.len(),
        "closed_profit": closed_profit,
        "total_fee": total_fee,
        "win_rate": win_rate,
        "equity_drawdown": equity_drawdown,
        "max_dd_pct": max_dd_pct,
        "updated_at_ms": now_ms(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kline(ts: u64, close: f64) -> Kline {
        Kline {
            open_time: ts,
            open: close,
            high: close,
            low: close,
            close,
            volume: 1.0,
            close_time: ts + 1000,
        }
    }

    #[test]
    fn test_merge_klines_dedup_and_order() {
        let cached = vec![kline(1, 10.0), kline(2, 11.0)];
        // 重叠的 ts=2 用新值（11.5），并追加 ts=3
        let fetched = vec![kline(2, 11.5), kline(3, 12.0)];
        let merged = merge_klines(cached, fetched);
        assert_eq!(merged.len(), 3);
        assert_eq!(merged[0].open_time, 1);
        assert!((merged[1].close - 11.5).abs() < 1e-12, "新数据应覆盖旧缓存");
        assert_eq!(merged[2].open_time, 3);
    }

    #[test]
    fn test_interval_ms() {
        assert_eq!(interval_ms("1d"), 86_400_000);
        assert_eq!(interval_ms("4h"), 4 * 3_600_000);
        assert_eq!(interval_ms("15m"), 15 * 60_000);
        assert_eq!(interval_ms("1w"), 7 * 86_400_000);
        assert_eq!(interval_ms(""), 86_400_000);
    }

    #[test]
    fn test_token_matches() {
        assert!(token_matches(Some("secret"), Some("secret")));
        assert!(!token_matches(Some("secret"), Some("wrong")));
        assert!(!token_matches(Some("secret"), None));
        // 未配置（None 或空）一律拒绝：fail-closed
        assert!(!token_matches(None, Some("secret")));
        assert!(!token_matches(Some(""), Some("")));
    }

    fn snap(ts: u64, total: f64) -> EquitySnap {
        EquitySnap { ts, total, usdt: total, positions_value: 0.0 }
    }

    #[test]
    fn test_day_change() {
        // 空/单点：无法计算当日盈亏
        assert!(day_change(&[]).is_none());
        assert!(day_change(&[snap(1_700_000_000_000, 100.0)]).is_none());
        // 跨日：昨日 100 -> 今日 105，当日盈亏取当日首点起算（+5%）
        // 1_700_000_000s = 2023-11-14 22:13 UTC；1_700_006_400s = 11-15 00:00 UTC
        let series = vec![
            snap(1_700_000_000_000, 100.0), // 2023-11-14
            snap(1_700_006_400_000, 100.0), // 2023-11-15 首点（00:00）
            snap(1_700_050_000_000, 105.0), // 2023-11-15 末点（12:13，同日）
        ];
        let (chg, pct) = day_change(&series).unwrap();
        assert!((chg - 5.0).abs() < 1e-9);
        assert!((pct - 0.05).abs() < 1e-9);
    }

    fn fill(ts: u64, sym: &str, side: Side, qty: f64, price: f64, fee: f64) -> LiveFill {
        LiveFill {
            ts,
            symbol: sym.to_string(),
            side,
            quantity: qty,
            price,
            fee,
            reason: String::new(),
        }
    }

    #[test]
    fn test_pair_rounds_net_profit() {
        // 买 1@100（费 0.075）卖 1@110（费 0.0825）：毛赚 10，净利润 = 10 − 两腿手续费
        let fills = vec![
            fill(1_000, "SOLUSDT", Side::Buy, 1.0, 100.0, 0.075),
            fill(2_000, "SOLUSDT", Side::Sell, 1.0, 110.0, 0.0825),
        ];
        let (rounds, opens) = pair_rounds(&fills);
        assert_eq!(rounds.len(), 1);
        assert!(opens.is_empty());
        let r = &rounds[0];
        assert!((r.net_profit - (10.0 - 0.075 - 0.0825)).abs() < 1e-9);
        assert!((r.net_pct - (10.0 - 0.1575) / 100.0).abs() < 1e-9);
    }

    #[test]
    fn test_pair_rounds_partial_and_open() {
        // 买 2@100 卖 1@90（部分平仓），剩余 1 枚未平仓；买入手续费按匹配量比例分摊
        let fills = vec![
            fill(1_000, "BTCUSDT", Side::Buy, 2.0, 100.0, 0.15),
            fill(2_000, "BTCUSDT", Side::Sell, 1.0, 90.0, 0.0675),
        ];
        let (rounds, opens) = pair_rounds(&fills);
        assert_eq!(rounds.len(), 1);
        assert!((rounds[0].net_profit - (-10.0 - 0.075 - 0.0675)).abs() < 1e-9);
        assert_eq!(opens.len(), 1);
        assert!((opens[0].quantity - 1.0).abs() < 1e-9);
        assert!((opens[0].avg_cost - 100.0).abs() < 1e-9);
    }

    #[test]
    fn test_drawdown_series() {
        let snaps = vec![snap(1, 100.0), snap(2, 120.0), snap(3, 90.0), snap(4, 110.0)];
        let (dd, max_dd) = drawdown_series(&snaps);
        assert_eq!(dd.len(), 4);
        assert!((dd[2].1 - (-25.0)).abs() < 1e-9); // 90 / 峰值 120 − 1 = −25%
        assert!((max_dd - (-25.0)).abs() < 1e-9);
    }

    #[test]
    fn test_regime_symbol_stats() {
        // 60 根递增收盘 100..159：价在均线上方；样本不足 200 无 MA200
        let ks: Vec<Kline> = (0..60)
            .map(|i| kline(i as u64 * 86_400_000, 100.0 + i as f64))
            .collect();
        let v = regime_symbol_stats("TESTUSDT", &ks, None).unwrap();
        assert_eq!(v["above_ma50"].as_bool(), Some(true));
        assert!(v["ma200"].is_null());
        assert!((v["ret_7d"].as_f64().unwrap() - (159.0 / 152.0 - 1.0) * 100.0).abs() < 1e-9);
        // 实时价优先于末根收盘；样本不足 51 根返回 None
        let v2 = regime_symbol_stats("TESTUSDT", &ks, Some(90.0)).unwrap();
        assert_eq!(v2["above_ma50"].as_bool(), Some(false));
        assert!(regime_symbol_stats("T", &ks[..50], None).is_none());
    }
}
