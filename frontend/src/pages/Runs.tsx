// 模拟盘/实盘监控：运行配置（策略/周期/品种/策略专属参数）+ 实盘盯市
// （持仓盈亏/权益曲线/成交流水/今日盈亏）+ 真实账户资产（交易所直查）
// + 状态卡片 + 启停（需 Token）+ 实时日志（3s 轮询）

import { useCallback, useEffect, useState } from "react";
import { api, getToken, type RunParams } from "../api";
import { DrawdownChart, EquityChart } from "../charts";
import { fmtDateTime, fmtNum, fmtPrice } from "../fmt";
import { getLastBtParams } from "../store";
import type {
  Dashboard,
  LiveAccount,
  LiveAnalysis,
  LiveEquity,
  LiveFillRow,
  LiveOpenOrderRow,
  LiveOverview,
  PanicResult,
  RunInfo,
} from "../types";

type Kind = "dryrun" | "live";

/** 盈亏着色（与图表配色一致） */
function pnlColor(v: number | null | undefined): string | undefined {
  if (v == null || !isFinite(v)) return undefined;
  return v >= 0 ? "#26a69a" : "#ef5350";
}

/** 当日盈亏（按浏览器本地日界）：后端 day_pnl 以 UTC 日界计算，
 * 对东八区用户会跨日错位，这里用权益序列在本地时区重新计算 */
function localDayChange(series: { ts: number; total: number }[]): { chg: number; pct: number } | null {
  if (series.length < 2) return null;
  const dayKey = (ts: number) => {
    const d = new Date(ts);
    return `${d.getFullYear()}-${d.getMonth()}-${d.getDate()}`;
  };
  const last = series[series.length - 1];
  const key = dayKey(last.ts);
  const start = series.find((s) => dayKey(s.ts) === key);
  if (!start || start.ts === last.ts) return null;
  const chg = last.total - start.total;
  const pct = start.total !== 0 ? chg / start.total : 0;
  return { chg, pct };
}

/** 策略中文名：与回测中心下拉选项逐字一致，避免同一策略两页两个叫法 */
const STRATEGY_LABELS: Record<string, string> = {
  momentum: "动量轮动",
  trend: "趋势+追踪止损",
  ma_cross: "均线交叉",
  grid: "智能网格",
  dca: "定投",
};

/** 运行配置表单：品种以逗号分隔字符串保存（与后端 --symbols 同形） */
interface RunForm {
  strategy: string;
  interval: string;
  symbols: string;
  cash: number;
  fee: number;
  fill: string;
  topN: number;
  regimeMa: number;
  regimeBreadth: number;
  circuitBreaker: number;
  circuitCooldown: number;
  momentumDays: number;
  maDays: number;
  rebalanceDays: number;
  trailingStop: number;
  maFast: number;
  maSlow: number;
  gridLevels: number;
  gridLookbackDays: number;
  gridStopLoss: number;
  gridBudget: number;
  dcaAmount: number;
  dcaIntervalDays: number;
  dcaMaDays: number;
  dcaDipMultiplier: number;
}

// 默认值与回测中心保持一致，便于两页对照
const DEFAULT_FORM: RunForm = {
  strategy: "momentum",
  interval: "1d",
  symbols: "",
  cash: 10000,
  fee: 0.00075,
  fill: "next_open",
  topN: 1,
  regimeMa: 0,
  regimeBreadth: 0.5,
  circuitBreaker: 0,
  circuitCooldown: 30,
  momentumDays: 90,
  maDays: 50,
  rebalanceDays: 30,
  trailingStop: 0.12,
  maFast: 10,
  maSlow: 30,
  gridLevels: 10,
  gridLookbackDays: 30,
  gridStopLoss: 0.15,
  gridBudget: 0,
  dcaAmount: 100,
  dcaIntervalDays: 7,
  dcaMaDays: 200,
  dcaDipMultiplier: 2,
};

/** 策略专属参数只在选中该策略时提交，其余交由 CLI 各自默认值（与回测中心同一口径） */
function formToParams(f: RunForm): RunParams {
  const symbols = f.symbols.trim();
  return {
    strategy: f.strategy,
    interval: f.interval,
    cash: f.cash,
    fee: f.fee,
    fill: f.fill,
    top_n: f.topN,
    regime_ma: f.regimeMa,
    regime_breadth: f.regimeBreadth,
    circuit_breaker: f.circuitBreaker,
    circuit_cooldown: f.circuitCooldown,
    ...(symbols ? { symbols } : {}), // 留空 = 沿用服务端配置的品种
    ...(f.strategy === "momentum" || f.strategy === "trend"
      ? {
          momentum_days: f.momentumDays,
          ma_days: f.maDays,
          rebalance_days: f.rebalanceDays,
          trailing_stop: f.trailingStop,
        }
      : {}),
    ...(f.strategy === "ma_cross" ? { ma_fast: f.maFast, ma_slow: f.maSlow } : {}),
    ...(f.strategy === "grid"
      ? {
          grid_levels: f.gridLevels,
          grid_lookback_days: f.gridLookbackDays,
          grid_stop_loss: f.gridStopLoss,
          grid_budget: f.gridBudget,
        }
      : {}),
    ...(f.strategy === "dca"
      ? {
          dca_amount: f.dcaAmount,
          dca_interval_days: f.dcaIntervalDays,
          dca_ma_days: f.dcaMaDays,
          dca_dip_multiplier: f.dcaDipMultiplier,
        }
      : {}),
  };
}

/** 把持久化的回测参数回填成表单：缺失字段保留当前默认 */
function paramsToForm(p: RunParams, base: RunForm): RunForm {
  const n = (v: number | undefined, d: number) => (v === undefined ? d : v);
  return {
    ...base,
    strategy: p.strategy ?? base.strategy,
    interval: p.interval ?? base.interval,
    symbols: p.symbols ?? base.symbols,
    cash: n(p.cash, base.cash),
    fee: n(p.fee, base.fee),
    fill: p.fill ?? base.fill,
    topN: n(p.top_n, base.topN),
    regimeMa: n(p.regime_ma, base.regimeMa),
    regimeBreadth: n(p.regime_breadth, base.regimeBreadth),
    circuitBreaker: n(p.circuit_breaker, base.circuitBreaker),
    circuitCooldown: n(p.circuit_cooldown, base.circuitCooldown),
    momentumDays: n(p.momentum_days, base.momentumDays),
    maDays: n(p.ma_days, base.maDays),
    rebalanceDays: n(p.rebalance_days, base.rebalanceDays),
    trailingStop: n(p.trailing_stop, base.trailingStop),
    maFast: n(p.ma_fast, base.maFast),
    maSlow: n(p.ma_slow, base.maSlow),
    gridLevels: n(p.grid_levels, base.gridLevels),
    gridLookbackDays: n(p.grid_lookback_days, base.gridLookbackDays),
    gridStopLoss: n(p.grid_stop_loss, base.gridStopLoss),
    gridBudget: n(p.grid_budget, base.gridBudget),
    dcaAmount: n(p.dca_amount, base.dcaAmount),
    dcaIntervalDays: n(p.dca_interval_days, base.dcaIntervalDays),
    dcaMaDays: n(p.dca_ma_days, base.dcaMaDays),
    dcaDipMultiplier: n(p.dca_dip_multiplier, base.dcaDipMultiplier),
  };
}

/** 运行配置：模拟盘/实盘共用一份参数，启动时随请求体下发给 CLI。
 * 存在的意义是让「即将跑什么」在点启动前就可见——此前界面完全不提，
 * 用户在回测中心验证过网格、启动后实际跑的却是配置文件里的动量轮动。 */
function RunConfig({
  form,
  setForm,
}: {
  form: RunForm;
  setForm: (f: RunForm) => void;
}) {
  const [hint, setHint] = useState("");
  const set = <K extends keyof RunForm>(k: K, v: RunForm[K]) => setForm({ ...form, [k]: v });

  const applyBacktestParams = () => {
    const p = getLastBtParams();
    if (!p) {
      setHint("本地没有回测参数记录，请先在「回测中心」运行一次回测");
      return;
    }
    setForm(paramsToForm(p, DEFAULT_FORM));
    setHint(`已载入回测中心参数：${STRATEGY_LABELS[p.strategy ?? ""] ?? p.strategy ?? "-"} · ${p.interval ?? "1d"}`);
  };

  return (
    <div className="card">
      <div className="toolbar">
        <h2 style={{ margin: 0 }}>运行配置</h2>
        <span className="muted">模拟盘与实盘共用；启动时随请求下发，留空项沿用服务端 quantkit.toml</span>
        <span style={{ flex: 1 }} />
        <button onClick={applyBacktestParams}>沿用回测中心参数</button>
      </div>
      {hint && <div className="muted">{hint}</div>}
      <div className="form-grid">
        <label className="field">
          策略
          <select value={form.strategy} onChange={(e) => set("strategy", e.target.value)}>
            <option value="momentum">动量轮动</option>
            <option value="trend">趋势+追踪止损</option>
            <option value="ma_cross">均线交叉</option>
            <option value="grid">智能网格</option>
            <option value="dca">定投</option>
          </select>
        </label>
        <label className="field">
          K线周期
          <select value={form.interval} onChange={(e) => set("interval", e.target.value)}>
            <option value="5m">5m</option>
            <option value="15m">15m</option>
            <option value="30m">30m</option>
            <option value="1h">1h</option>
            <option value="4h">4h</option>
            <option value="12h">12h</option>
            <option value="1d">1d（日线）</option>
            <option value="1w">1w（周线）</option>
          </select>
        </label>
        <label className="field span2">
          品种（逗号分隔，空=沿用服务端配置）
          <input
            placeholder="BTCUSDT,ETHUSDT"
            value={form.symbols}
            onChange={(e) => set("symbols", e.target.value.toUpperCase())}
          />
        </label>
        <label className="field">
          初始资金
          <input type="number" value={form.cash} onChange={(e) => set("cash", Number(e.target.value))} />
        </label>
        <label className="field">
          单边费率
          <input
            type="number"
            step="0.00005"
            value={form.fee}
            onChange={(e) => set("fee", Number(e.target.value))}
          />
        </label>
        <label className="field span2">
          撮合口径
          <select value={form.fill} onChange={(e) => set("fill", e.target.value)}>
            <option value="next_open">next_open（无未来函数）</option>
            <option value="same_close">same_close</option>
          </select>
        </label>
      </div>

      {(form.strategy === "momentum" || form.strategy === "trend") && (
        <>
          <h3>动量参数</h3>
          <div className="form-grid">
            <label className="field">
              动量天数
              <input
                type="number"
                value={form.momentumDays}
                onChange={(e) => set("momentumDays", Number(e.target.value))}
              />
            </label>
            <label className="field">
              均线天数
              <input
                type="number"
                value={form.maDays}
                onChange={(e) => set("maDays", Number(e.target.value))}
              />
            </label>
            <label className="field">
              调仓间隔(天)
              <input
                type="number"
                value={form.rebalanceDays}
                onChange={(e) => set("rebalanceDays", Number(e.target.value))}
              />
            </label>
            <label className="field">
              追踪止损(0=关)
              <input
                type="number"
                step="0.01"
                value={form.trailingStop}
                onChange={(e) => set("trailingStop", Number(e.target.value))}
              />
            </label>
          </div>
          <div className="muted">
            动量天数：比较各品种近多少天涨幅来排名，越短越灵敏也越容易被噪声骗；
            均线天数：收盘价站上该均线才允许买入；
            调仓间隔：每隔多少天重排一次，太短会被手续费吃掉收益；
            追踪止损：从持仓最高点回落该比例即卖出
          </div>
        </>
      )}

      {form.strategy === "ma_cross" && (
        <>
          <h3>均线交叉参数</h3>
          <div className="form-grid">
            <label className="field">
              快线周期(根)
              <input
                type="number"
                value={form.maFast}
                onChange={(e) => set("maFast", Number(e.target.value))}
              />
            </label>
            <label className="field">
              慢线周期(根)
              <input
                type="number"
                value={form.maSlow}
                onChange={(e) => set("maSlow", Number(e.target.value))}
              />
            </label>
          </div>
          <div className="muted">
            快线上穿慢线买入、下穿卖出。周期单位是「根」，按上方所选K线周期计
            （4h 周期下快线 10 = 10 根 4 小时线）；两者差距越大信号越少越稳，
            慢线须大于快线
          </div>
        </>
      )}

      {form.strategy === "grid" && (
        <>
          <h3>网格参数</h3>
          <div className="form-grid">
            <label className="field">
              网格格数
              <input
                type="number"
                min="2"
                value={form.gridLevels}
                onChange={(e) => set("gridLevels", Math.max(2, Number(e.target.value)))}
              />
            </label>
            <label className="field">
              区间回看(天)
              <input
                type="number"
                value={form.gridLookbackDays}
                onChange={(e) => set("gridLookbackDays", Number(e.target.value))}
              />
            </label>
            <label className="field">
              网格止损(0=关)
              <input
                type="number"
                step="0.01"
                value={form.gridStopLoss}
                onChange={(e) => set("gridStopLoss", Number(e.target.value))}
              />
            </label>
            <label className="field">
              每品种预算(0=均分)
              <input
                type="number"
                value={form.gridBudget}
                onChange={(e) => set("gridBudget", Number(e.target.value))}
              />
            </label>
          </div>
          <div className="muted">
            网格格数：把价格区间等分成多少档，档位越密单笔越小、交易越频繁；
            区间回看：用最近多少天的最高/最低价确定网格上下界；
            网格止损：跌破区间下界该比例即清仓，防单边下跌一路补仓被套死（网格唯一的致命风险，不建议设 0）；
            每品种预算：每个品种分配多少计价资金布网，0 = 首次布网时按品种数均分现金
          </div>
        </>
      )}

      {form.strategy === "dca" && (
        <>
          <h3>定投参数</h3>
          <div className="form-grid">
            <label className="field">
              每期金额
              <input
                type="number"
                value={form.dcaAmount}
                onChange={(e) => set("dcaAmount", Number(e.target.value))}
              />
            </label>
            <label className="field">
              定投间隔(天)
              <input
                type="number"
                min="1"
                value={form.dcaIntervalDays}
                onChange={(e) => set("dcaIntervalDays", Math.max(1, Number(e.target.value)))}
              />
            </label>
            <label className="field">
              趋势均线(0=关加码)
              <input
                type="number"
                value={form.dcaMaDays}
                onChange={(e) => set("dcaMaDays", Number(e.target.value))}
              />
            </label>
            <label className="field">
              低位加码倍数
              <input
                type="number"
                step="0.5"
                value={form.dcaDipMultiplier}
                onChange={(e) => set("dcaDipMultiplier", Number(e.target.value))}
              />
            </label>
          </div>
          <div className="muted">
            每期金额：每个品种每次买入的计价资金；
            定投间隔：每隔多少天买一次（7 = 周投，30 ≈ 月投）；
            趋势均线：价格低于该均线视为便宜，触发加码，0 = 关闭、恒定投固定金额；
            低位加码倍数：便宜时按几倍金额买入（越大越激进，需备足现金）
          </div>
        </>
      )}
      <h3>风控与分散</h3>
      <div className="form-grid">
        <label className="field">
          持仓品种数(1=集中)
          <input
            type="number"
            min="1"
            value={form.topN}
            onChange={(e) => set("topN", Math.max(1, Number(e.target.value)))}
          />
        </label>
        <label className="field">
          市场状态均线(0=关)
          <input
            type="number"
            value={form.regimeMa}
            onChange={(e) => set("regimeMa", Number(e.target.value))}
          />
        </label>
        <label className="field">
          熊市广度阈值(0-1)
          <input
            type="number"
            step="0.05"
            value={form.regimeBreadth}
            onChange={(e) => set("regimeBreadth", Number(e.target.value))}
          />
        </label>
        <label className="field">
          熔断回撤(0=关)
          <input
            type="number"
            step="0.01"
            value={form.circuitBreaker}
            onChange={(e) => set("circuitBreaker", Number(e.target.value))}
          />
        </label>
        <label className="field span2">
          熔断冷却(天)
          <input
            type="number"
            value={form.circuitCooldown}
            onChange={(e) => set("circuitCooldown", Number(e.target.value))}
          />
        </label>
      </div>
      <div className="muted">熔断：组合权益自峰值回撤超阈值即全清仓，冷却期内禁止买入</div>
      <div className="muted">
        所选周期的数据需服务端已下载（数据文件 {"{品种}"}_{form.interval}.json），否则进程会因数据加载失败退出；
        「天」类参数会按周期自动换算成对应K线根数
      </div>
    </div>
  );
}

function RunCard({
  kind,
  info,
  liveEnabled,
  apiKeysSet,
  logs,
  form,
  onStart,
  onStop,
  busy,
}: {
  kind: Kind;
  info: RunInfo | undefined;
  liveEnabled: boolean;
  apiKeysSet: boolean;
  logs: string[];
  form: RunForm;
  onStart: () => void;
  onStop: () => void;
  busy: boolean;
}) {
  const running = info?.running ?? false;
  const isLive = kind === "live";
  const blocked = isLive && (!liveEnabled || !apiKeysSet);
  const summary = `即将运行：${STRATEGY_LABELS[form.strategy] ?? form.strategy} · ${form.interval} · ${
    form.symbols.trim() || "服务端配置品种"
  }`;

  return (
    <div className="card">
      <div className="toolbar">
        <h2 style={{ margin: 0 }}>{isLive ? "实盘" : "模拟盘"}</h2>
        <span className={`badge ${running ? "on" : "off"}`}>
          {running ? "运行中" : "已停止"}
        </span>
        {running && info?.pid !== undefined && (
          <span className="muted">PID {info.pid}</span>
        )}
        {running && info?.started_at !== undefined && (
          <span className="muted">启动于 {fmtDateTime(info.started_at)}</span>
        )}
        <span style={{ flex: 1 }} />
        {!running ? (
          <button
            className="primary"
            disabled={busy || blocked || !getToken()}
            onClick={onStart}
          >
            {busy ? "执行中..." : "启动"}
          </button>
        ) : (
          <button
            className="danger"
            disabled={busy || !getToken()}
            onClick={onStop}
          >
            {busy ? "执行中..." : "停止"}
          </button>
        )}
      </div>
      {/* 运行中时表单已不代表进程实际参数，只在启动前展示，避免误读 */}
      {!running &&
        (isLive ? (
          <div className="warn">{summary}（真金白银，启动前请核对上方运行配置）</div>
        ) : (
          <div className="muted">{summary}</div>
        ))}
      {isLive && blocked && (
        <div className="warn">
          实盘门禁未通过：
          {!liveEnabled && " 配置文件未设 live_enabled = true；"}
          {!apiKeysSet && " 未配置 Binance API 密钥；"}
          服务端配置完成前无法启动。
        </div>
      )}
      {!getToken() && (
        <div className="warn">启停接口受保护，请先在「设置」页填写 X-API-Token</div>
      )}
      <div className="logs">
        {logs.length > 0 ? logs.join("\n") : "暂无日志"}
      </div>
    </div>
  );
}

export default function Runs() {
  const [dash, setDash] = useState<Dashboard | null>(null);
  const [logs, setLogs] = useState<Record<Kind, string[]>>({ dryrun: [], live: [] });
  const [form, setForm] = useState<RunForm>(DEFAULT_FORM);
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState("");

  const load = useCallback(async () => {
    try {
      const d = await api.dashboard();
      setDash(d);
      setErr("");
    } catch (e) {
      setErr((e as Error).message);
    }
  }, []);

  // 状态 5s 轮询 + 日志 3s 轮询
  useEffect(() => {
    load();
    const t1 = setInterval(load, 5000);
    const t2 = setInterval(async () => {
      for (const kind of ["dryrun", "live"] as Kind[]) {
        try {
          const d = await api.runLogs(kind);
          setLogs((prev) => ({ ...prev, [kind]: d.lines.slice(-200) }));
        } catch {
          // 日志缺失（尚未运行过）不视为错误
        }
      }
    }, 3000);
    return () => {
      clearInterval(t1);
      clearInterval(t2);
    };
  }, [load]);

  const act = (kind: Kind, start: boolean) => async () => {
    setBusy(true);
    setErr("");
    try {
      if (start) await api.runStart(kind, formToParams(form));
      else await api.runStop(kind);
      await load();
    } catch (e) {
      setErr((e as Error).message);
    } finally {
      setBusy(false);
    }
  };

  return (
    <div>
      <h2>模拟盘 · 实盘</h2>
      {err && <div className="error">{err}</div>}
      <AccountAssets />
      <LiveMonitor />
      <LivePerformance />
      <RunConfig form={form} setForm={setForm} />
      <div className="row">
        <RunCard
          kind="dryrun"
          info={dash?.runs.dryrun}
          liveEnabled
          apiKeysSet
          logs={logs.dryrun}
          form={form}
          onStart={act("dryrun", true)}
          onStop={act("dryrun", false)}
          busy={busy}
        />
        <RunCard
          kind="live"
          info={dash?.runs.live}
          liveEnabled={dash?.live_enabled ?? false}
          apiKeysSet={dash?.api_keys_set ?? false}
          logs={logs.live}
          form={form}
          onStart={act("live", true)}
          onStop={act("live", false)}
          busy={busy}
        />
      </div>
    </div>
  );
}

/** 真实账户资产：直查 Binance 余额+盯市（后端 15s 缓存），不依赖实盘进程/状态文件。
 * 连服务器后端时看的是真实账户；本地后端未配密钥时显示友好错误。 */
function AccountAssets() {
  const [acct, setAcct] = useState<LiveAccount | null>(null);
  const [err, setErr] = useState("");

  const load = useCallback(async () => {
    try {
      setAcct(await api.liveAccount());
      setErr("");
    } catch (e) {
      setErr((e as Error).message);
    }
  }, []);

  useEffect(() => {
    load();
    const t = setInterval(load, 15_000);
    return () => clearInterval(t);
  }, [load]);

  if (err) {
    return (
      <div className="card">
        <h2 style={{ margin: 0 }}>账户资产（Binance 实时）</h2>
        <div className="muted">
          {err}
          {err.includes("Token") && "（受保护接口：请先在「设置」页填写）"}
        </div>
      </div>
    );
  }
  if (!acct) return null;

  // 最大单项集中度：风险分散视角（单资产占比越高风险越集中）
  const topShare = acct.assets.reduce<number | null>((best, a) => {
    if (a.value == null || acct.total <= 0) return best;
    const s = (a.value / acct.total) * 100;
    return best == null || s > best ? s : best;
  }, null);

  return (
    <div className="card table-wrap">
      <div className="toolbar">
        <h2 style={{ margin: 0 }}>账户资产（Binance 实时）</h2>
        <span className="muted">直查交易所余额 + 盯市估值（15s 缓存）</span>
      </div>
      <div className="grid">
        <div className="stat">
          <div className="k">账户总资产（可估值）</div>
          <div className="v">{fmtNum(acct.total)}</div>
        </div>
        <div className="stat">
          <div className="k">资产项</div>
          <div className="v">{acct.assets.length} 种</div>
        </div>
        <div className="stat">
          <div className="k">最大单项集中度</div>
          <div className="v">{topShare != null ? `${topShare.toFixed(1)}%` : "-"}</div>
        </div>
        <div className="stat">
          <div className="k">灰尘资产（&lt;$10）</div>
          <div className="v">{acct.dust.length > 0 ? acct.dust.join(" / ") : "无"}</div>
        </div>
      </div>
      <table>
        <thead>
          <tr>
            <th>资产</th>
            <th>可用数量</th>
            <th>最新价（USDT）</th>
            <th>市值（USDT）</th>
            <th style={{ minWidth: 140 }}>占比</th>
          </tr>
        </thead>
        <tbody>
          {acct.assets.map((a) => (
            <tr key={a.asset}>
              <td>{a.asset}</td>
              <td>{fmtNum(a.free, 8)}</td>
              <td>{a.price != null ? (a.asset === "USDT" ? "1.0000" : fmtPrice(a.price)) : "-"}</td>
              <td>{a.value != null ? fmtNum(a.value) : "-"}</td>
              <td>
                {a.value != null && acct.total > 0 ? (
                  <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
                    <div style={{ flex: 1, height: 6, background: "#1c2230", borderRadius: 3, overflow: "hidden" }}>
                      <div
                        style={{
                          width: `${Math.min(100, (a.value / acct.total) * 100)}%`,
                          height: "100%",
                          background: "#5b8def",
                        }}
                      />
                    </div>
                    <span className="muted" style={{ minWidth: 44, textAlign: "right" }}>
                      {((a.value / acct.total) * 100).toFixed(1)}%
                    </span>
                  </div>
                ) : (
                  "-"
                )}
              </td>
            </tr>
          ))}
        </tbody>
      </table>
      {acct.dust.length > 0 && (
        <div className="muted">
          灰尘资产市值低于常见最小下单额（minNotional），策略无法处理，如需清理请人工在交易所操作。
        </div>
      )}
    </div>
  );
}

/** 一键急停确认对话框：说明影响 → 勾选确认 → 执行 → 展示结果摘要 */
function PanicDialog({ onClose, onDone }: { onClose: () => void; onDone: () => void }) {
  const [ack, setAck] = useState(false);
  const [busy, setBusy] = useState(false);
  const [result, setResult] = useState<PanicResult | null>(null);
  const [err, setErr] = useState("");

  const run = async () => {
    setBusy(true);
    setErr("");
    try {
      setResult(await api.livePanic());
      onDone();
    } catch (e) {
      setErr((e as Error).message);
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="modal-overlay" onClick={busy ? undefined : onClose}>
      <div className="modal-panel" onClick={(e) => e.stopPropagation()}>
        <h2 style={{ marginTop: 0, color: "#ef5350" }}>🛑 一键急停</h2>
        {!result ? (
          <>
            <p>
              执行后将 <b>立即</b>：
            </p>
            <ol style={{ lineHeight: 1.9, paddingLeft: 22 }}>
              <li>停止实盘策略进程（不再产生新下单）</li>
              <li>撤销池内品种全部在途挂单（释放冻结资金）</li>
              <li>市价卖出池内全部持仓，回笼为 USDT</li>
            </ol>
            <p className="muted">
              市价成交按实时价格、产生 taker 手续费；操作不可撤销。之后若想继续交易，需手动重新启动实盘。
            </p>
            {err && <p style={{ color: "#ef5350" }}>{err}</p>}
            <label style={{ display: "flex", gap: 8, alignItems: "center", margin: "14px 0" }}>
              <input type="checkbox" checked={ack} onChange={(e) => setAck(e.target.checked)} />
              我已知晓上述影响，确认执行
            </label>
            <div style={{ display: "flex", gap: 10, justifyContent: "flex-end" }}>
              <button onClick={onClose} disabled={busy}>
                取消
              </button>
              <button className="danger" onClick={run} disabled={!ack || busy}>
                {busy ? "执行中..." : "确认急停"}
              </button>
            </div>
          </>
        ) : (
          <>
            <p>
              已执行：进程{result.stopped_process ? `已停止 (pid=${result.stopped_process})` : "未在运行"}
              ，撤单 {result.cancelled_orders.length} 笔，市价卖出 {result.sold.length} 个品种
              {result.failed.length > 0 ? `，${result.failed.length} 个失败` : ""}。
            </p>
            {result.sold.length > 0 && (
              <table>
                <thead>
                  <tr>
                    <th>品种</th>
                    <th>卖出数量</th>
                    <th>成交价</th>
                    <th>手续费</th>
                  </tr>
                </thead>
                <tbody>
                  {result.sold.map((s) => (
                    <tr key={s.symbol}>
                      <td>{s.symbol}</td>
                      <td>{s.quantity}</td>
                      <td>{fmtPrice(s.price)}</td>
                      <td>{fmtNum(s.fee)}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            )}
            {result.failed.length > 0 && (
              <div style={{ color: "#ef5350", marginTop: 10 }}>
                {result.failed.map((f) => (
                  <div key={f.symbol}>
                    {f.symbol} 卖出失败：{f.error}
                  </div>
                ))}
              </div>
            )}
            {result.notes.length > 0 && (
              <div className="muted" style={{ marginTop: 10 }}>
                {result.notes.map((n, i) => (
                  <div key={i}>{n}</div>
                ))}
              </div>
            )}
            {result.positions_left > 0 && (
              <p style={{ color: "#ef5350" }}>
                仍有 {result.positions_left} 条持仓记录未清（卖出失败或交易所仍有余额），请人工核对！
              </p>
            )}
            <div style={{ display: "flex", justifyContent: "flex-end", marginTop: 14 }}>
              <button className="primary" onClick={onClose}>
                关闭
              </button>
            </div>
          </>
        )}
      </div>
    </div>
  );
}

/** 实盘监控面板：读后端三接口（状态文件 + 盯市），10s 轮询。
 * 实盘进程不在线也能读（数据在状态文件）；未运行过实盘时只显示提示。 */
function LiveMonitor() {
  const [ov, setOv] = useState<LiveOverview | null>(null);
  const [eq, setEq] = useState<LiveEquity | null>(null);
  const [fills, setFills] = useState<LiveFillRow[]>([]);
  const [fillTotal, setFillTotal] = useState(0);
  const [orders, setOrders] = useState<LiveOpenOrderRow[]>([]);
  const [err, setErr] = useState("");
  const [panicOpen, setPanicOpen] = useState(false);

  const load = useCallback(async () => {
    try {
      const [p, e, f, oo] = await Promise.all([
        api.livePositions(),
        api.liveEquity(),
        api.liveFills(50),
        // 挂单接口失败不影响监控主面板（如后端未升级时）
        api.liveOpenOrders().catch(() => null),
      ]);
      setOv(p);
      setEq(e);
      setFills(f.fills);
      setFillTotal(f.total);
      setOrders(oo?.orders ?? []);
      setErr("");
    } catch (e) {
      setErr((e as Error).message);
    }
  }, []);

  useEffect(() => {
    load();
    const t = setInterval(load, 10_000);
    return () => clearInterval(t);
  }, [load]);

  if (err || !ov) {
    return (
      <div className="card">
        <div className="toolbar">
          <h2 style={{ margin: 0 }}>实盘监控</h2>
        </div>
        <div className="muted">
          {err ?? "加载中..."}
          {err.includes("Token") && "（受保护接口：请先在「设置」页填写）"}
        </div>
      </div>
    );
  }

  const unrealized = ov.positions.reduce((s, p) => s + (p.pnl ?? 0), 0);
  // 图表组件内部已做 ms→s 换算，这里保持毫秒原值（此前多除一次 1000，
  // 导致全部点落到 1970 年、横轴日期错误）
  const curve = (eq?.series ?? []).map((s) => ({
    timestamp: s.ts,
    equity: s.total,
  }));
  const dayPnl = localDayChange(eq?.series ?? []);

  return (
    <>
      <div className="card">
        <div className="toolbar">
          <h2 style={{ margin: 0 }}>实盘监控</h2>
          <span className="muted">数据更新于 {fmtDateTime(ov.updated_at_ms)}（10s 自动刷新）</span>
          <button
            className="danger small"
            style={{ marginLeft: "auto" }}
            onClick={() => setPanicOpen(true)}
            title="停止策略进程 + 撤销挂单 + 市价清仓全部持仓"
          >
            🛑 一键急停
          </button>
        </div>
        {panicOpen && <PanicDialog onClose={() => setPanicOpen(false)} onDone={load} />}
        <div className="grid">
          <div className="stat">
            <div className="k">总资产</div>
            <div className="v">{ov.total != null ? fmtNum(ov.total) : "-"}</div>
          </div>
          <div className="stat">
            <div className="k">今日盈亏</div>
            <div className="v" style={{ color: pnlColor(dayPnl?.chg) }}>
              {dayPnl
                ? `${dayPnl.chg >= 0 ? "+" : ""}${fmtNum(dayPnl.chg)}（${(dayPnl.pct * 100).toFixed(2)}%）`
                : "-"}
            </div>
          </div>
          <div className="stat">
            <div className="k">USDT 可用</div>
            <div className="v">{ov.usdt != null ? fmtNum(ov.usdt) : "-"}</div>
          </div>
          <div className="stat">
            <div className="k">持仓市值</div>
            <div className="v">{ov.positions_value != null ? fmtNum(ov.positions_value) : "-"}</div>
          </div>
          <div className="stat">
            <div className="k">浮动盈亏</div>
            <div className="v" style={{ color: pnlColor(unrealized) }}>
              {unrealized >= 0 ? "+" : ""}
              {fmtNum(unrealized)}
            </div>
          </div>
          <div className="stat">
            <div className="k">累计成交</div>
            <div className="v">{fillTotal} 笔</div>
          </div>
        </div>
      </div>

      {curve.length >= 2 && (
        <div className="card">
          <h3>实盘权益曲线（总资产盯市）</h3>
          <EquityChart curve={curve} height={260} />
        </div>
      )}

      <div className="card table-wrap">
        <h3>持仓（最新价盯市）</h3>
        {ov.positions.length === 0 ? (
          <div className="muted">当前空仓</div>
        ) : (
          <table>
            <thead>
              <tr>
                <th>品种</th>
                <th>数量</th>
                <th>入场均价</th>
                <th>最新价</th>
                <th>市值</th>
                <th>浮动盈亏</th>
                <th>盈亏幅度</th>
              </tr>
            </thead>
            <tbody>
              {ov.positions.map((p) => (
                <tr key={p.symbol}>
                  <td>{p.symbol}</td>
                  <td>{fmtNum(p.quantity, 6)}</td>
                  <td>{fmtPrice(p.avg_entry_price)}</td>
                  <td>{p.last_price != null && isFinite(p.last_price) ? fmtPrice(p.last_price) : "-"}</td>
                  <td>{p.value != null && isFinite(p.value) ? fmtNum(p.value) : "-"}</td>
                  <td style={{ color: pnlColor(p.pnl) }}>
                    {p.pnl != null && isFinite(p.pnl) ? `${p.pnl >= 0 ? "+" : ""}${fmtNum(p.pnl)}` : "-"}
                  </td>
                  <td style={{ color: pnlColor(p.pnl_pct) }}>
                    {p.pnl_pct != null && isFinite(p.pnl_pct)
                      ? `${p.pnl_pct >= 0 ? "+" : ""}${(p.pnl_pct * 100).toFixed(2)}%`
                      : "-"}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </div>

      {orders.length > 0 && (
        <div className="card table-wrap">
          <h3>在途挂单（{orders.length} 笔，锁定资金不计入可用余额）</h3>
          <table>
            <thead>
              <tr>
                <th>品种</th>
                <th>方向</th>
                <th>委托价</th>
                <th>数量</th>
                <th>已成交</th>
                <th>状态</th>
              </tr>
            </thead>
            <tbody>
              {orders.map((o, i) => (
                <tr key={`${o.symbol}-${i}`}>
                  <td>{o.symbol}</td>
                  <td style={{ color: o.side === "BUY" ? "#26a69a" : "#ef5350" }}>
                    {o.side === "BUY" ? "买入" : "卖出"}
                  </td>
                  <td>{fmtPrice(o.price)}</td>
                  <td>{fmtNum(o.orig_qty, 6)}</td>
                  <td>{fmtNum(o.executed_qty, 6)}</td>
                  <td className="muted">{o.status}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}

      <div className="card table-wrap">
        <h3>成交流水（最新 50 笔，共 {fillTotal} 笔）</h3>
        {fills.length === 0 ? (
          <div className="muted">尚无真实成交</div>
        ) : (
          <table>
            <thead>
              <tr>
                <th>时间</th>
                <th>方向</th>
                <th>品种</th>
                <th>数量</th>
                <th>价格</th>
                <th>手续费</th>
                <th>原因</th>
              </tr>
            </thead>
            <tbody>
              {fills.map((f, i) => (
                <tr key={`${f.ts}-${f.symbol}-${i}`}>
                  <td>{fmtDateTime(f.ts)}</td>
                  <td style={{ color: f.side === "买入" ? "#26a69a" : "#ef5350" }}>{f.side}</td>
                  <td>{f.symbol}</td>
                  <td>{fmtNum(f.quantity, 6)}</td>
                  <td>{fmtPrice(f.price)}</td>
                  <td>{fmtNum(f.fee, 6)}</td>
                  <td className="muted">{f.reason || "-"}</td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </div>
    </>
  );
}

/** 实盘绩效：买卖配对轮次的净利润（已扣两腿手续费）/胜率/回撤，
 * 回答“实盘到底赚了多少”；基于状态文件成交记录，15s 刷新。 */
function LivePerformance() {
  const [an, setAn] = useState<LiveAnalysis | null>(null);
  const [err, setErr] = useState("");

  const load = useCallback(async () => {
    try {
      setAn(await api.liveAnalysis());
      setErr("");
    } catch (e) {
      setErr((e as Error).message);
    }
  }, []);

  useEffect(() => {
    load();
    const t = setInterval(load, 15_000);
    return () => clearInterval(t);
  }, [load]);

  if (err) {
    return (
      <div className="card">
        <h2 style={{ margin: 0 }}>实盘绩效分析</h2>
        <div className="muted">
          {err}
          {err.includes("Token") && "（受保护接口：请先在「设置」页填写）"}
        </div>
      </div>
    );
  }
  if (!an) return null;

  return (
    <>
      <div className="card">
        <div className="toolbar">
          <h2 style={{ margin: 0 }}>实盘绩效分析</h2>
          <span className="muted">轮次配对 · 净利润口径（已扣手续费）· 15s 刷新</span>
        </div>
        <div className="grid">
          <div className="stat">
            <div className="k">累计已平仓净利润</div>
            <div className="v" style={{ color: pnlColor(an.closed_profit) }}>
              {an.closed_profit > 0 ? "+" : ""}
              {fmtNum(an.closed_profit)}
            </div>
          </div>
          <div className="stat">
            <div className="k">累计手续费</div>
            <div className="v">{fmtNum(an.total_fee, 4)}</div>
          </div>
          <div className="stat">
            <div className="k">已完成轮次</div>
            <div className="v">{an.round_count} 轮</div>
          </div>
          <div className="stat">
            <div className="k">胜率</div>
            <div className="v">
              {an.win_rate != null ? `${(an.win_rate * 100).toFixed(1)}%` : "-"}
            </div>
          </div>
          <div className="stat">
            <div className="k">最大回撤</div>
            <div className="v neg">
              {an.equity_drawdown.length > 0 ? `${an.max_dd_pct.toFixed(2)}%` : "-"}
            </div>
          </div>
        </div>
      </div>

      {an.equity_drawdown.length >= 2 && (
        <div className="card">
          <h3>权益回撤曲线（相对历史峰值）</h3>
          <DrawdownChart points={an.equity_drawdown} height={200} />
        </div>
      )}

      <div className="card table-wrap">
        <h3>已完成轮次（最近 {an.rounds.length} 轮，共 {an.round_count} 轮）</h3>
        {an.rounds.length === 0 ? (
          <div className="muted">暂无已完成轮次（持仓平仓后计为一轮）</div>
        ) : (
          <table>
            <thead>
              <tr>
                <th>平仓时间</th>
                <th>品种</th>
                <th>数量</th>
                <th>买入均价</th>
                <th>卖出价</th>
                <th>手续费</th>
                <th>净利润</th>
                <th>收益率</th>
                <th>持仓天数</th>
              </tr>
            </thead>
            <tbody>
              {an.rounds.map((r, i) => (
                <tr key={`${r.sell_ts}-${r.symbol}-${i}`}>
                  <td>{fmtDateTime(r.sell_ts)}</td>
                  <td>{r.symbol}</td>
                  <td>{fmtNum(r.quantity, 6)}</td>
                  <td>{fmtPrice(r.buy_price)}</td>
                  <td>{fmtPrice(r.sell_price)}</td>
                  <td>{fmtNum(r.fee, 6)}</td>
                  <td style={{ color: pnlColor(r.net_profit) }}>
                    {r.net_profit >= 0 ? "+" : ""}
                    {fmtNum(r.net_profit)}
                  </td>
                  <td style={{ color: pnlColor(r.net_pct) }}>
                    {r.net_pct >= 0 ? "+" : ""}
                    {(r.net_pct * 100).toFixed(2)}%
                  </td>
                  <td>{r.hold_days.toFixed(1)} 天</td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
        {an.open_positions.length > 0 && (
          <div className="muted">
            未平仓：
            {an.open_positions
              .map(
                (o) =>
                  `${o.symbol} ${fmtNum(o.quantity, 6)} @ 均价 ${fmtPrice(o.avg_cost)}（${fmtDateTime(o.open_ts)} 建仓）`,
              )
              .join("；")}
          </div>
        )}
      </div>
    </>
  );
}
