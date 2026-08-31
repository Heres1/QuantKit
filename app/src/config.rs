//! TOML 配置（所有字段带默认值，缺文件即用默认）

use quantkit_core::interval::Interval;
use serde::Deserialize;

/// 配置文件 `[binance]` 密钥段（与 binance-rust 同一写法：api_key / secret_key）
#[derive(Debug, Clone, Default, Deserialize)]
pub struct BinanceKeys {
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub secret_key: String,
}

fn default_data_dir() -> String {
    "../binance-rust/data/history".into()
}
fn default_initial_cash() -> f64 {
    10_000.0
}
fn default_fee_rate() -> f64 {
    0.00075
}
fn default_fill_mode() -> String {
    "next_open".into()
}
fn default_interval() -> String {
    "1d".into()
}
fn default_slippage_pct() -> f64 {
    0.0
}
fn default_symbols() -> Vec<String> {
    vec!["BTCUSDT".into(), "ETHUSDT".into(), "SOLUSDT".into()]
}
fn default_strategy() -> String {
    "momentum".into()
}
fn default_ma_cross_fast() -> usize {
    10
}
fn default_ma_cross_slow() -> usize {
    30
}
fn default_grid_levels() -> usize {
    10
}
fn default_grid_lookback_days() -> usize {
    30
}
fn default_grid_stop_loss_pct() -> f64 {
    0.15
}
fn default_grid_budget_per_symbol() -> f64 {
    0.0
}
fn default_dca_amount() -> f64 {
    100.0
}
fn default_dca_interval_days() -> u64 {
    7
}
fn default_dca_ma_days() -> usize {
    200
}
fn default_dca_dip_multiplier() -> f64 {
    2.0
}
fn default_momentum_days() -> usize {
    90
}
fn default_ma_days() -> usize {
    50
}
fn default_rebalance_days() -> u64 {
    30
}
fn default_trailing_stop_pct() -> f64 {
    0.12
}
fn default_trailing_stop_enabled() -> bool {
    true
}
fn default_cooldown_days() -> u64 {
    30
}
fn default_top_n() -> usize {
    1
}
fn default_regime_ma_days() -> usize {
    0
}
fn default_regime_min_breadth() -> f64 {
    0.5
}
fn default_circuit_breaker_pct() -> f64 {
    0.0
}
fn default_circuit_breaker_cooldown_days() -> u64 {
    30
}
fn default_state_file() -> String {
    "quantkit_state.json".into()
}
fn default_poll_secs() -> u64 {
    60
}
fn default_live_enabled() -> bool {
    false
}
fn default_live_auto_heal() -> bool {
    false
}
fn default_live_sync_external() -> bool {
    true
}
fn default_web_port() -> u16 {
    8080
}
fn default_telegram_token() -> String {
    String::new()
}
fn default_telegram_chat_id() -> String {
    String::new()
}
fn default_ws_enabled() -> bool {
    false
}
fn default_ws_only_closed_bars() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    #[serde(default = "default_data_dir")]
    pub data_dir: String,
    #[serde(default = "default_initial_cash")]
    pub initial_cash: f64,
    /// 单边手续费率：默认 0.00075 = Binance 现货 VIP0 taker 0.1% 启用 BNB 抵扣后的真实费率；
    /// 未开 BNB 抵扣时设为 0.001；live 启动时自动拉取账户实际费率覆盖此值
    #[serde(default = "default_fee_rate")]
    pub fee_rate: f64,
    #[serde(default = "default_slippage_pct")]
    pub slippage_pct: f64,
    /// next_open（默认，无未来函数）| same_close（对标老回测口径）
    #[serde(default = "default_fill_mode")]
    pub fill_mode: String,
    /// K线周期：5m | 15m | 30m | 1h | 4h | 12h | 1d（默认）| 1w。
    /// 同时决定数据文件名 `{SYMBOL}_{interval}.json` 与「天」参数换算成「根」的口径
    #[serde(default = "default_interval")]
    pub interval: String,
    #[serde(default = "default_symbols")]
    pub symbols: Vec<String>,
    /// momentum | trend | ma_cross | grid | dca
    #[serde(default = "default_strategy")]
    pub strategy: String,
    /// ma_cross 模板策略：快线周期
    #[serde(default = "default_ma_cross_fast")]
    pub ma_cross_fast: usize,
    /// ma_cross 模板策略：慢线周期（须 > 快线）
    #[serde(default = "default_ma_cross_slow")]
    pub ma_cross_slow: usize,
    /// grid 网格格数（区间等分数）
    #[serde(default = "default_grid_levels")]
    pub grid_levels: usize,
    /// grid 区间回看天数：用最近这段时间的最高/最低价推导网格上下界
    #[serde(default = "default_grid_lookback_days")]
    pub grid_lookback_days: usize,
    /// grid 止损：跌破区间下界该比例即清仓停止本轮网格（0 = 关闭）
    #[serde(default = "default_grid_stop_loss_pct")]
    pub grid_stop_loss_pct: f64,
    /// grid 每品种网格预算（计价资产）；0 = 首次布网时按品种数均分当时现金
    #[serde(default = "default_grid_budget_per_symbol")]
    pub grid_budget_per_symbol: f64,
    /// dca 每期每品种买入金额（计价资产）
    #[serde(default = "default_dca_amount")]
    pub dca_amount: f64,
    /// dca 定投间隔天数（7 = 周投）
    #[serde(default = "default_dca_interval_days")]
    pub dca_interval_days: u64,
    /// dca 智能加码趋势均线天数（0 = 关闭加码，恒定投固定金额）
    #[serde(default = "default_dca_ma_days")]
    pub dca_ma_days: usize,
    /// dca 收盘价低于趋势均线时的加码倍数（<=1 视为不加码）
    #[serde(default = "default_dca_dip_multiplier")]
    pub dca_dip_multiplier: f64,
    #[serde(default = "default_momentum_days")]
    pub momentum_days: usize,
    #[serde(default = "default_ma_days")]
    pub ma_days: usize,
    #[serde(default = "default_rebalance_days")]
    pub rebalance_days: u64,
    #[serde(default = "default_trailing_stop_enabled")]
    pub trailing_stop_enabled: bool,
    #[serde(default = "default_trailing_stop_pct")]
    pub trailing_stop_pct: f64,
    #[serde(default = "default_cooldown_days")]
    pub cooldown_days: u64,
    /// 持仓品种数：1 = 集中轮动；>1 = 动量前 N 等额分散
    #[serde(default = "default_top_n")]
    pub top_n: usize,
    /// 市场状态过滤均线天数（0 = 禁用）：池内站上该均线的品种占比过低判熊市清仓
    #[serde(default = "default_regime_ma_days")]
    pub regime_ma_days: usize,
    /// 熊市阈值：站上均线的品种占比低于此值判为熊市（0-1）
    #[serde(default = "default_regime_min_breadth")]
    pub regime_min_breadth: f64,
    /// 组合回撤熔断：权益自峰值回撤 >= 该值全清仓并冷却（0 = 禁用）
    #[serde(default = "default_circuit_breaker_pct")]
    pub circuit_breaker_pct: f64,
    /// 熔断后冷却天数：冷却期内禁止开仓
    #[serde(default = "default_circuit_breaker_cooldown_days")]
    pub circuit_breaker_cooldown_days: u64,
    /// dry-run 状态快照文件
    #[serde(default = "default_state_file")]
    pub state_file: String,
    /// dry-run 轮询间隔（秒）
    #[serde(default = "default_poll_secs")]
    pub poll_secs: u64,
    /// live 实盘总开关：默认关闭，必须在配置文件中显式设为 true 才允许启动
    #[serde(default = "default_live_enabled")]
    pub live_enabled: bool,
    /// live 对账自愈：启动对账发现漂移时，以交易所真实可用余额重写本地状态持仓。
    /// 默认 false：只告警不改状态，由人工核对后决定。
    #[serde(default = "default_live_auto_heal")]
    pub live_auto_heal: bool,
    /// live 场外订单同步：每轮增量查询 myTrades/openOrders，把手动成交纳入
    /// 状态与流水并告警。默认开启；异常时可一键关停而不回滚代码。
    #[serde(default = "default_live_sync_external")]
    pub live_sync_external: bool,
    /// WebUI 监听端口（quantkit serve / quantkit-web）
    #[serde(default = "default_web_port")]
    pub web_port: u16,
    /// Telegram Bot 令牌（实盘关键事件推送；空 = 不启用通知）
    #[serde(default = "default_telegram_token")]
    pub telegram_bot_token: String,
    /// Telegram 接收消息的 chat_id（与 telegram_bot_token 同时配置才启用）
    #[serde(default = "default_telegram_chat_id")]
    pub telegram_chat_id: String,
    /// Binance 密钥段（实盘）。优先级：环境变量 BINANCE_API_KEY / BINANCE_API_SECRET > 此段；
    /// quantkit.toml 已在 .gitignore 中，不会被误提交（与 binance-rust 的连接方式一致）
    #[serde(default)]
    pub binance: BinanceKeys,
    /// WebSocket 实时数据源开关：默认关闭，保持 REST 轮询模式
    #[serde(default = "default_ws_enabled")]
    pub ws_enabled: bool,
    /// WebSocket 只接收已闭合的 K线（避免未完结数据干扰决策）
    #[serde(default = "default_ws_only_closed_bars")]
    pub ws_only_closed_bars: bool,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            data_dir: default_data_dir(),
            initial_cash: default_initial_cash(),
            fee_rate: default_fee_rate(),
            slippage_pct: default_slippage_pct(),
            fill_mode: default_fill_mode(),
            interval: default_interval(),
            symbols: default_symbols(),
            strategy: default_strategy(),
            ma_cross_fast: default_ma_cross_fast(),
            ma_cross_slow: default_ma_cross_slow(),
            grid_levels: default_grid_levels(),
            grid_lookback_days: default_grid_lookback_days(),
            grid_stop_loss_pct: default_grid_stop_loss_pct(),
            grid_budget_per_symbol: default_grid_budget_per_symbol(),
            dca_amount: default_dca_amount(),
            dca_interval_days: default_dca_interval_days(),
            dca_ma_days: default_dca_ma_days(),
            dca_dip_multiplier: default_dca_dip_multiplier(),
            momentum_days: default_momentum_days(),
            ma_days: default_ma_days(),
            rebalance_days: default_rebalance_days(),
            trailing_stop_enabled: default_trailing_stop_enabled(),
            trailing_stop_pct: default_trailing_stop_pct(),
            cooldown_days: default_cooldown_days(),
            top_n: default_top_n(),
            regime_ma_days: default_regime_ma_days(),
            regime_min_breadth: default_regime_min_breadth(),
            circuit_breaker_pct: default_circuit_breaker_pct(),
            circuit_breaker_cooldown_days: default_circuit_breaker_cooldown_days(),
            state_file: default_state_file(),
            poll_secs: default_poll_secs(),
            live_enabled: default_live_enabled(),
            live_auto_heal: default_live_auto_heal(),
            live_sync_external: default_live_sync_external(),
            web_port: default_web_port(),
            telegram_bot_token: default_telegram_token(),
            telegram_chat_id: default_telegram_chat_id(),
            binance: BinanceKeys::default(),
            ws_enabled: default_ws_enabled(),
            ws_only_closed_bars: default_ws_only_closed_bars(),
        }
    }
}

impl AppConfig {
    /// 解析 K线周期；非法值返回错误而非静默退回日线——
    /// 拼错周期却按日线跑完，会得到一份看起来正常但完全错误的回测报告。
    pub fn interval(&self) -> Result<Interval, String> {
        Interval::parse(&self.interval)
    }

    /// CLI 入口用：周期非法时打印错误并退出
    pub fn interval_or_exit(&self) -> Interval {
        match self.interval() {
            Ok(iv) => iv,
            Err(e) => {
                eprintln!("[配置] {e}");
                std::process::exit(1);
            }
        }
    }
}

fn env_or_cfg(env: &str, cfg_val: &str) -> Option<String> {
    match std::env::var(env).ok().filter(|s| !s.is_empty()) {
        Some(v) => Some(v),
        None if !cfg_val.is_empty() => Some(cfg_val.to_string()),
        None => None,
    }
}

/// 解析 Binance 密钥：环境变量优先，回退配置文件 `[binance]` 段（binance-rust 方式）
pub fn resolve_binance_keys(cfg: &AppConfig) -> (Option<String>, Option<String>) {
    (
        env_or_cfg("BINANCE_API_KEY", &cfg.binance.api_key),
        env_or_cfg("BINANCE_API_SECRET", &cfg.binance.secret_key),
    )
}

/// 从 TOML 文件加载配置；文件不存在或解析失败时使用默认配置
pub fn load_config(path: Option<&str>) -> AppConfig {
    let path = path.unwrap_or("quantkit.toml");
    match std::fs::read_to_string(path) {
        Ok(s) => match toml::from_str(&s) {
            Ok(cfg) => cfg,
            Err(e) => {
                eprintln!("配置解析失败({}): {}，使用默认配置", path, e);
                AppConfig::default()
            }
        },
        Err(_) => AppConfig::default(),
    }
}
