// 统一 API 客户端：响应约定 { ok: true, data } / { ok: false, error }，
// 与后端 api.rs 一致。受保护接口的令牌从 localStorage 读取，自动带 X-API-Token 头。

import type {
  BacktestRecord,
  BacktestRecordSummary,
  BacktestResult,
  CorrSnapshot,
  Dashboard,
  DataFile,
  DownloadTask,
  FactorsSnapshot,
  IcReport,
  Kline,
  LiveAccount,
  LiveAnalysis,
  LiveEquity,
  LiveFillRow,
  LiveOverview,
  MarketRegime,
  Quote,
  StrongCoinsResponse,
  SweepRow,
  WalkforwardResult,
} from "./types";

// 后端地址：运行时可切换。优先级：设置页保存的地址（localStorage）>
// 构建时 VITE_API_BASE > 默认；避免后端迁移后必须重新构建前端。
// 默认直连实盘服务器（公网 8080），无需 SSH 隧道；本地调试可在设置页切回"本地"。
const DEFAULT_API_BASE =
  (import.meta.env.VITE_API_BASE as string | undefined) ?? "http://43.154.120.27:8080";

const API_BASE_KEY = "quantkit_api_base";

export function getApiBase(): string {
  const v = localStorage.getItem(API_BASE_KEY);
  return v && v.trim() ? v.trim().replace(/\/+$/, "") : DEFAULT_API_BASE;
}

export function setApiBase(base: string) {
  const v = base.trim().replace(/\/+$/, "");
  if (v) localStorage.setItem(API_BASE_KEY, v);
  else localStorage.removeItem(API_BASE_KEY);
}

const TOKEN_KEY = "quantkit_token";

export function getToken(): string {
  return localStorage.getItem(TOKEN_KEY) ?? "";
}

export function setToken(t: string) {
  if (t) localStorage.setItem(TOKEN_KEY, t);
  else localStorage.removeItem(TOKEN_KEY);
}

export class ApiError extends Error {}

async function request<T>(path: string, init?: RequestInit): Promise<T> {
  const headers: Record<string, string> = {};
  if (init?.body) headers["Content-Type"] = "application/json";
  const token = getToken();
  if (token) headers["X-API-Token"] = token;
  let resp: Response;
  const base = getApiBase();
  try {
    resp = await fetch(`${base}${path}`, { ...init, headers });
  } catch (e) {
    throw new ApiError(`无法连接后端 ${base}：${(e as Error).message}`);
  }
  const text = await resp.text();
  let body: { ok?: boolean; data?: T; error?: string } | null = null;
  try {
    body = JSON.parse(text);
  } catch {
    // 非 JSON 响应
  }
  if (!body || body.ok !== true) {
    throw new ApiError(body?.error || `请求失败（HTTP ${resp.status}）`);
  }
  return body.data as T;
}

export interface BacktestParams {
  strategy?: string;
  symbols?: string;
  /** K线周期（5m/15m/30m/1h/4h/12h/1d/1w），缺省日线 */
  interval?: string;
  cash?: number;
  momentum_days?: number;
  ma_days?: number;
  rebalance_days?: number;
  trailing_stop?: number;
  /** 持仓品种数（1=集中轮动；>1=动量前N等额分散） */
  top_n?: number;
  /** 市场状态过滤均线天数（0=禁用） */
  regime_ma?: number;
  /** 熊市广度阈值（0-1） */
  regime_breadth?: number;
  /** 组合回撤熔断阈值（<=0 禁用） */
  circuit_breaker?: number;
  /** 熔断后冷却天数 */
  circuit_cooldown?: number;
  /** ma_cross 快线周期（根，按所选K线周期计） */
  ma_fast?: number;
  /** ma_cross 慢线周期（根，须大于快线） */
  ma_slow?: number;
  /** grid 网格格数（区间等分数） */
  grid_levels?: number;
  /** grid 区间回看天数（用最近这段时间的最高/最低价定上下界） */
  grid_lookback_days?: number;
  /** grid 止损：跌破区间下界该比例即清仓（0=关闭） */
  grid_stop_loss?: number;
  /** grid 每品种网格预算（0=按品种数均分现金） */
  grid_budget?: number;
  /** dca 每期每品种买入金额 */
  dca_amount?: number;
  /** dca 定投间隔天数（7=周投） */
  dca_interval_days?: number;
  /** dca 智能加码趋势均线天数（0=关闭加码） */
  dca_ma_days?: number;
  /** dca 跌破趋势均线时的加码倍数 */
  dca_dip_multiplier?: number;
  fee?: number;
  fill?: string;
  /** 回测窗口 YYYY-MM-DD */
  start?: string;
  end?: string;
  /** sweep 网格声明："momentum_days=30,60;trailing_stop=0.08,0.12" */
  grid?: string;
  /** sweep 训练/测试切分比例 */
  split?: number;
  /** 基准品种（买入持有对比，如 BTCUSDT；空 = 不对比） */
  benchmark?: string;
  /** walkforward 折数（默认 5） */
  windows?: number;
  /** walkforward 每折训练窗占比（默认 0.7） */
  train_ratio?: number;
  /** walkforward 窗口模式：true=扩张窗，false/缺省=滚动窗 */
  anchored?: boolean;
  /** walkforward 选参指标：annualized / sharpe / calmar / sortino */
  rank?: string;
}

/** 模拟盘/实盘启动接口接受的参数：回测专属字段（时间窗、扫描网格、基准、
 * 滚动验证）对常驻运行无意义，后端也会忽略，因此在类型层面就排除掉。 */
export type RunParams = Omit<
  BacktestParams,
  "start" | "end" | "grid" | "split" | "benchmark" | "windows" | "train_ratio" | "anchored" | "rank"
>;

export const api = {
  health: () => request<{ name: string; version: string }>("/api/health"),
  dashboard: () => request<Dashboard>("/api/dashboard"),
  markets: () => request<{ quotes: Quote[]; updated_at: number }>("/api/markets"),
  /** 市场环境分析（趋势广度/动量分层/波动，60s 缓存；基于本地日K） */
  marketRegime: () => request<MarketRegime>("/api/market-regime"),
  /** 实时强势货币排行榜（Top 50，30s 缓存） */
  strongCoins: () => request<StrongCoinsResponse>("/api/strong-coins"),
  klines: (symbol: string, interval: string, limit = 500, endTime?: number) => {
    let url = `/api/klines/${symbol}?interval=${interval}&limit=${limit}`;
    if (endTime !== undefined) url += `&end_time=${endTime}`;
    return request<{ symbol: string; interval: string; total: number; klines: Kline[] }>(url);
  },
  data: () => request<{ market: DataFile[]; download: DownloadTask | null }>("/api/data"),
  /** 多因子截面（本地日K计算；symbols 空 = 全部品种） */
  factors: (symbols?: string) =>
    request<FactorsSnapshot>(`/api/factors${symbols ? `?symbols=${encodeURIComponent(symbols)}` : ""}`),
  /** 品种两两日收益率相关矩阵（近 days 个交易日，默认 60） */
  correlation: (days = 60) => request<CorrSnapshot>(`/api/correlation?days=${days}`),
  /** 因子 IC 回测验证：因子截面值与未来 horizon 个交易日收益的滚动相关 */
  factorIc: (factor: string, horizon = 20) =>
    request<IcReport>(`/api/factor-ic?factor=${encodeURIComponent(factor)}&horizon=${horizon}`),
  download: (symbols: string, maxBars: number, interval = "1d") =>
    request<{ accepted: string[]; max_bars: number }>("/api/download", {
      method: "POST",
      body: JSON.stringify({ symbols, max_bars: maxBars, interval }),
    }),
  backtest: (p: BacktestParams) =>
    request<BacktestResult>("/api/backtest", { method: "POST", body: JSON.stringify(p) }),
  sweep: (p: BacktestParams) =>
    request<SweepRow[]>("/api/sweep", { method: "POST", body: JSON.stringify(p) }),
  /** 滚动前进验证：多折「训练窗选参 → 紧随其后的测试窗验收」（需 Token） */
  walkforward: (p: BacktestParams) =>
    request<WalkforwardResult>("/api/walkforward", { method: "POST", body: JSON.stringify(p) }),
  backtests: () => request<BacktestRecordSummary[]>("/api/backtests"),
  backtestRecord: (id: string) => request<BacktestRecord>(`/api/backtests/${id}`),
  deleteBacktest: (id: string) =>
    request<{ deleted: string }>(`/api/backtests/${id}`, { method: "DELETE" }),
  /** 启动模拟盘/实盘：p 省略时不带 body，后端回退 quantkit.toml 配置 */
  runStart: (kind: "dryrun" | "live", p?: RunParams) =>
    request<{ started: string; pid: number }>(`/api/runs/${kind}/start`, {
      method: "POST",
      ...(p ? { body: JSON.stringify(p) } : {}),
    }),
  runStop: (kind: "dryrun" | "live") =>
    request<{ stopped: string; pid: number }>(`/api/runs/${kind}/stop`, { method: "POST" }),
  runLogs: (kind: "dryrun" | "live" | "all") =>
    request<{ lines: string[] }>(`/api/runs/${kind}/logs`),
  /** 实盘持仓盯市（读状态文件 + 最新价；需 Token） */
  livePositions: () => request<LiveOverview>("/api/live/positions"),
  /** 实盘权益曲线 + 当日盈亏（需 Token） */
  liveEquity: () => request<LiveEquity>("/api/live/equity"),
  /** 实盘成交流水（倒序，含成交原因；需 Token） */
  liveFills: (limit = 100) =>
    request<{ fills: LiveFillRow[]; total: number }>(`/api/live/fills?limit=${limit}`),
  /** 真实账户资产（交易所余额+盯市，15s 缓存；需 Token） */
  liveAccount: () => request<LiveAccount>("/api/live/account"),
  /** 实盘绩效：轮次配对净利润/胜率/回撤（需 Token） */
  liveAnalysis: () => request<LiveAnalysis>("/api/live/analysis"),
};
