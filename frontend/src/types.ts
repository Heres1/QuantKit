// quantkit 后端数据类型（与 Rust 端 serde 序列化字段对齐，snake_case）

export interface Quote {
  symbol: string;
  last_price: number;
  price_change_pct: number;
  quote_volume: number;
  /** 24h 最高/最低价 */
  high_price: number;
  low_price: number;
}

/** 强势币种排行榜（综合评分） */
export interface StrongCoin {
  rank: number;
  rank_change: number | null;
  symbol: string;
  score: number;
  change_score: number;
  vol_score: number;
  position_score: number;
  vol_adjust: number;
  price_score: number;
  last_price: number;
  price_change_pct: number;
  high_price: number;
  low_price: number;
  quote_volume: number;
  prev_rank?: number;
}

export interface StrongCoinsResponse {
  coins: StrongCoin[];
  total: number;
  top_n: number;
  updated_at: number;
  cache_hit: boolean;
}

export interface Kline {
  open_time: number;
  open: number;
  high: number;
  low: number;
  close: number;
  volume: number;
  close_time: number;
}

export interface DataFile {
  symbol: string;
  interval: string;
  bars: number;
  first: string;
  last: string;
}

export interface Trade {
  symbol: string;
  entry_price: number;
  exit_price: number;
  quantity: number;
  pnl: number;
  entry_time: number;
  exit_time: number;
}

export interface BacktestMetrics {
  total_return_pct: number;
  annualized_return_pct: number;
  max_drawdown_pct: number;
  sharpe_ratio: number;
  /** 年化波动率（%） */
  annualized_volatility_pct: number;
  /** 最长回撤期（自峰值到收复峰值的天数） */
  max_drawdown_duration_days: number;
  win_rate_pct: number;
  num_round_trips: number;
  total_fees: number;
  /** Option 字段：无定义时后端不序列化（缺失即 -） */
  profit_factor?: number;
  payoff_ratio?: number;
  calmar_ratio?: number;
  exposure_pct?: number;
  /** 无下行波动时后端不序列化 */
  sortino_ratio?: number;
}

export interface EquityPoint {
  timestamp: number;
  equity: number;
}

export interface FinalPosition {
  symbol: string;
  quantity: number;
  avg_entry_price: number;
}

// 基准对比报告：基准买入持有指标 + 策略相对基准统计量（后端 CLI --benchmark 产出）
export interface BenchmarkReport {
  symbol: string;
  metrics: BacktestMetrics;
  /** 年化超额收益（%）= 策略年化 - 基准年化 */
  excess_annualized_pct: number;
  beta?: number;
  correlation?: number;
  information_ratio?: number;
  /** 与策略权益曲线时间对齐后的基准权益曲线（同初始资金） */
  curve: EquityPoint[];
}

export interface BacktestResult {
  strategy: string;
  symbols: string[];
  bars: number;
  metrics: BacktestMetrics;
  equity_curve: EquityPoint[];
  trades: Trade[];
  final_cash: number;
  final_positions: FinalPosition[];
  benchmark?: BenchmarkReport;
}

// sweep 单行：参数组合 + 指标（切分模式为 train/test 双段，否则 metrics 全期）
export interface SweepRow {
  params: Record<string, number>;
  metrics?: BacktestMetrics;
  train?: BacktestMetrics;
  test?: BacktestMetrics;
  error?: string;
}

// 滚动前进验证单折：训练窗选参 → 紧随其后的测试窗（样本外）验收
export interface WalkforwardFold {
  train_start: string;
  train_end: string;
  test_start: string;
  test_end: string;
  /** 该折训练窗选出的最优参数 */
  params: Record<string, number>;
  train: BacktestMetrics;
  test: BacktestMetrics;
}

// 各折样本外表现汇总（逐折复利口径）
export interface WalkforwardSummary {
  folds: number;
  /** 逐折测试窗收益复利后的总收益（%） */
  oos_total_return_pct: number;
  oos_annualized_pct: number;
  /** 各折测试窗合计跨度（天） */
  oos_days: number;
  /** 测试窗收益为正的折数 */
  positive_folds: number;
  /** 选参指标中文名（如"年化%"） */
  rank_metric: string;
  train_mean: number;
  test_mean: number;
  /** 过拟合差距 = 训练均值 − 测试均值，越大说明参数越可能只拟合了历史 */
  overfit_gap: number;
}

export interface WalkforwardResult {
  folds: WalkforwardFold[];
  summary: WalkforwardSummary;
}

export interface RunInfo {
  running: boolean;
  pid?: number;
  started_at?: number;
}

// 回测记录：服务端存档（{data_dir}/backtests/{id}.json）
export interface BacktestRecordSummary {
  id: string;
  created_at_ms?: number;
  strategy?: string;
  symbols?: string[];
  benchmark?: string;
  metrics?: BacktestMetrics;
}

export interface BacktestRecord {
  id: string;
  created_at_ms?: number;
  request: Record<string, unknown>;
  result: BacktestResult;
}

export interface Dashboard {
  version: string;
  data_dir: string;
  symbols: string[];
  strategy: string;
  fee_rate: number;
  live_enabled: boolean;
  api_keys_set: boolean;
  market: DataFile[];
  dry_state: {
    equity?: number;
    cash?: number;
    positions?: FinalPosition[];
    trades?: Trade[];
    last_bar_ts?: number;
    updated_at_ms?: number;
  } | null;
  runs: { dryrun: RunInfo; live: RunInfo };
}

export interface DownloadTask {
  symbols: string[];
  done: string[];
  running: string;
  started_at_ms: number;
}

// 多因子截面快照（后端基于本地日K计算，样本不足 210 根的品种已剔除）
export interface FactorRow {
  symbol: string;
  price: number;
  /** 20/60/120 日动量（小数） */
  mom20: number;
  mom60: number;
  mom120: number;
  /** 30 日年化波动率（小数） */
  vol_ann: number;
  /** RSI(14)，0-100 */
  rsi14: number;
  /** 收盘价相对 MA50 / MA200 偏离（小数） */
  ma50_dev: number;
  ma200_dev: number;
  /** 量能比：5日均量 / 30日均量 */
  vol_ratio: number;
  /** 相对近 120 日高点的回撤（正数小数） */
  dd_from_high: number;
  /** 近 30 日日均成交额估算 */
  avg_quote_volume: number;
  /** 蔡金资金流 [-1,1]，>0 净流入 */
  cmf20: number;
  /** 近 20 日符号化成交额（净资金流代理，USDT） */
  flow20: number;
  bars: number;
  /** 综合得分（截面 z 加权和） */
  score: number;
  /** 名次（1 = 最优） */
  rank: number;
}

export interface FactorsSnapshot {
  factors: FactorRow[];
  count: number;
  /** 样本不足被剔除的品种数（后端可能不返回） */
  skipped?: number;
  updated_at: number;
  weights: Record<string, number>;
}

// 相关性矩阵：品种两两日收益率 Pearson 相关（近 N 交易日）
export interface CorrSnapshot {
  symbols: string[];
  /** matrix[i][j] 为 symbols[i] 与 symbols[j] 的相关系数；样本不足时为 null */
  matrix: (number | null)[][];
  /** 实际使用的交易日数 */
  days: number;
  updated_at: number;
}

// 因子 IC 验证：因子截面值与未来收益的滚动相关
export interface IcPoint {
  ms: number;
  ic: number;
}

export interface IcReport {
  factor: string;
  horizon: number;
  step: number;
  /** 有效评估日数量 */
  n: number;
  ic_mean: number;
  ic_std: number;
  /** ICIR = IC均值 / IC标准差 */
  icir: number;
  /** IC > 0 的评估日占比（小数） */
  hit_rate: number;
  series: IcPoint[];
}

// 实盘监控（后端读 live 状态文件 + 公开行情盯市；进程不在线也可读）
export interface LivePosition {
  symbol: string;
  quantity: number;
  avg_entry_price: number;
  /** 最新价（取价失败为 null） */
  last_price: number | null;
  value: number | null;
  /** 浮动盈亏（最新价 - 入场价）* 数量 */
  pnl: number | null;
  pnl_pct: number | null;
}

export interface LiveOverview {
  positions: LivePosition[];
  /** 来自最近一次权益快照（实盘进程每轮写入） */
  usdt: number | null;
  total: number | null;
  positions_value: number | null;
  last_bar_ts: number;
  updated_at_ms: number;
}

export interface LiveEquitySnap {
  ts: number;
  total: number;
  usdt: number;
  positions_value: number;
}

export interface LiveEquity {
  series: LiveEquitySnap[];
  /** 当日（UTC）盈亏；当日不足两点为 null */
  day_pnl: number | null;
  day_pnl_pct: number | null;
}

/** 账户单项资产（交易所真实余额 + 盯市估值） */
export interface LiveAccountAsset {
  asset: string;
  free: number;
  /** 无 USDT 交易对或取价失败时为 null */
  price: number | null;
  value: number | null;
}

export interface LiveAccount {
  assets: LiveAccountAsset[];
  /** 可估值资产的合计（USDT 计价） */
  total: number;
  /** 市值 <$10 的灰尘资产清单 */
  dust: string[];
  updated_at_ms: number;
}

export interface LiveFillRow {
  ts: number;
  symbol: string;
  /** "买入" / "卖出" */
  side: string;
  quantity: number;
  price: number;
  fee: number;
  /** 信号来源（如"信号调仓：卖出非目标品种"） */
  reason: string;
}

// 市场环境分析（基于本地日K + 实时价，样本 <51 根的品种已剔除）
export interface RegimeSymbol {
  symbol: string;
  price: number;
  ma50: number;
  /** 样本不足 200 根时为 null */
  ma200: number | null;
  above_ma50: boolean;
  above_ma200: boolean | null;
  /** 7/30/90 日涨幅（%）；样本不足为 null */
  ret_7d: number | null;
  ret_30d: number | null;
  ret_90d: number | null;
  /** 30 日年化波动率（%） */
  vol_30d: number | null;
}

export interface MarketRegime {
  symbols: RegimeSymbol[];
  total: number;
  breadth: {
    above_ma50: number;
    above_ma200: number;
    ma200_count: number;
    above_ma50_pct: number;
    above_ma200_pct: number | null;
  };
  /** 30 日动量分层：强 >5% / 弱势 <-5% / 其余震荡 */
  momentum: { strong: number; neutral: number; weak: number };
  avg_vol_30d: number | null;
  btc: RegimeSymbol | null;
  updated_at_ms: number;
}

// 实盘绩效：买卖配对轮次（净利润口径，已扣两腿手续费）
export interface TradeRoundRow {
  symbol: string;
  buy_ts: number;
  sell_ts: number;
  quantity: number;
  buy_price: number;
  sell_price: number;
  fee: number;
  net_profit: number;
  /** 净利润 / 买入成本（小数） */
  net_pct: number;
  hold_days: number;
}

export interface LiveAnalysis {
  rounds: TradeRoundRow[];
  open_positions: { symbol: string; quantity: number; avg_cost: number; open_ts: number }[];
  round_count: number;
  /** 已平仓轮次净利润合计 */
  closed_profit: number;
  /** 全部成交手续费合计 */
  total_fee: number;
  /** 胜率（小数）；无平仓轮次为 null */
  win_rate: number | null;
  equity_drawdown: { ts: number; dd_pct: number }[];
  /** 最大回撤（%，≤0） */
  max_dd_pct: number;
  updated_at_ms: number;
}
