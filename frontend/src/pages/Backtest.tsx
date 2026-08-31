// 回测中心：全参数表单 + 权益曲线 + 逐笔回合 + 参数扫描（高亮最优）+ 滚动前进验证

import { useEffect, useState } from "react";
import { api, getToken, type BacktestParams, type RunParams } from "../api";
import { EquityChart, EquityChartMulti } from "../charts";
import { fmtDate, fmtDateTime, fmtNum, fmtPrice } from "../fmt";
import { getBtSymbols, setBtSymbols, setLastBtParams } from "../store";
import type {
  BacktestMetrics,
  BacktestRecord,
  BacktestRecordSummary,
  BacktestResult,
  EquityPoint,
  SweepRow,
  Trade,
  WalkforwardResult,
} from "../types";

// 对比图配色（按选择顺序循环）
const PALETTE = ["#5b8def", "#f5a623", "#26a69a", "#ef5350", "#ab47bc", "#8d6e63"];

// 月度收益：按本地月份分组权益曲线（与界面其他日期的本地时区口径一致），
// 月收益 = 月末/上月末 - 1（首月以曲线首点为基准）
function monthlyReturns(curve: EquityPoint[]): { year: number; month: number; ret: number }[] {
  const months: { year: number; month: number; last: number }[] = [];
  for (const p of curve) {
    const d = new Date(p.timestamp);
    const y = d.getFullYear();
    const mo = d.getMonth();
    const cur = months[months.length - 1];
    if (!cur || cur.year !== y || cur.month !== mo) {
      months.push({ year: y, month: mo, last: p.equity });
    } else {
      cur.last = p.equity;
    }
  }
  if (months.length === 0) return [];
  return months.map((x, i) => {
    const prev = i === 0 ? curve[0].equity : months[i - 1].last;
    return { year: x.year, month: x.month, ret: prev > 0 ? (x.last / prev - 1) * 100 : 0 };
  });
}

// 品种盈亏归因：按品种汇总已平仓回合（净盈亏已扣手续费）
function symbolAttribution(trades: Trade[]) {
  const map = new Map<string, { n: number; wins: number; pnl: number; days: number }>();
  for (const t of trades) {
    const e = map.get(t.symbol) ?? { n: 0, wins: 0, pnl: 0, days: 0 };
    e.n += 1;
    if (t.pnl > 0) e.wins += 1;
    e.pnl += t.pnl;
    e.days += (t.exit_time - t.entry_time) / 86_400_000;
    map.set(t.symbol, e);
  }
  return [...map.entries()]
    .map(([symbol, e]) => ({
      symbol,
      n: e.n,
      winRate: (e.wins / e.n) * 100,
      pnl: e.pnl,
      avgDays: e.days / e.n,
    }))
    .sort((a, b) => b.pnl - a.pnl);
}

// 按选参指标取对应指标值（字段映射与后端 RankMetric 一致）；
// Calmar/Sortino 无定义时后端不序列化，返回 undefined 交由 opt() 显示占位
function rankValue(m: BacktestMetrics, rank: string): number | undefined {
  switch (rank) {
    case "sharpe":
      return m.sharpe_ratio;
    case "calmar":
      return m.calmar_ratio;
    case "sortino":
      return m.sortino_ratio;
    default:
      return m.annualized_return_pct;
  }
}

export default function Backtest() {
  const [symbols, setSymbols] = useState<string[]>(getBtSymbols());
  const [symInput, setSymInput] = useState("");
  const [strategy, setStrategy] = useState("momentum");
  const [interval, setInterval_] = useState("1d");
  const [cash, setCash] = useState(10000);
  const [momentumDays, setMomentumDays] = useState(90);
  const [maDays, setMaDays] = useState(50);
  const [rebalanceDays, setRebalanceDays] = useState(30);
  const [trailingStop, setTrailingStop] = useState(0.12);
  const [maFast, setMaFast] = useState(10);
  const [maSlow, setMaSlow] = useState(30);
  const [gridLevels, setGridLevels] = useState(10);
  const [gridLookbackDays, setGridLookbackDays] = useState(30);
  const [gridStopLoss, setGridStopLoss] = useState(0.15);
  const [gridBudget, setGridBudget] = useState(0);
  const [dcaAmount, setDcaAmount] = useState(100);
  const [dcaIntervalDays, setDcaIntervalDays] = useState(7);
  const [dcaMaDays, setDcaMaDays] = useState(200);
  const [dcaDipMultiplier, setDcaDipMultiplier] = useState(2);
  const [topN, setTopN] = useState(1);
  const [regimeMa, setRegimeMa] = useState(0);
  const [regimeBreadth, setRegimeBreadth] = useState(0.5);
  const [circuitBreaker, setCircuitBreaker] = useState(0);
  const [circuitCooldown, setCircuitCooldown] = useState(30);
  const [fee, setFee] = useState(0.00075);
  const [fill, setFill] = useState("next_open");
  const [startDate, setStartDate] = useState("");
  const [endDate, setEndDate] = useState("");
  const [gridSpec, setGridSpec] = useState("");
  const [splitRatio, setSplitRatio] = useState("");
  const [benchmark, setBenchmark] = useState("BTCUSDT");
  const [records, setRecords] = useState<BacktestRecordSummary[]>([]);
  const [selectedIds, setSelectedIds] = useState<string[]>([]);
  const [compare, setCompare] = useState<BacktestRecord[]>([]);
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState("");
  const [result, setResult] = useState<BacktestResult | null>(null);
  const [sweep, setSweep] = useState<SweepRow[]>([]);
  const [wfWindows, setWfWindows] = useState(5);
  const [wfTrainRatio, setWfTrainRatio] = useState(0.7);
  const [wfAnchored, setWfAnchored] = useState(false);
  const [wfRank, setWfRank] = useState("annualized");
  const [wf, setWf] = useState<WalkforwardResult | null>(null);

  // 策略专属参数只在选中该策略时提交：既避免存档里混入无关参数，也让 CLI 走各自默认值
  const params = (): BacktestParams => ({
    strategy,
    symbols: symbols.join(","),
    interval,
    cash,
    top_n: topN,
    regime_ma: regimeMa,
    regime_breadth: regimeBreadth,
    circuit_breaker: circuitBreaker,
    circuit_cooldown: circuitCooldown,
    fee,
    fill,
    start: startDate || undefined,
    end: endDate || undefined,
    benchmark: benchmark.trim() || undefined,
    ...(strategy === "momentum" || strategy === "trend"
      ? {
          momentum_days: momentumDays,
          ma_days: maDays,
          rebalance_days: rebalanceDays,
          trailing_stop: trailingStop,
        }
      : {}),
    ...(strategy === "ma_cross" ? { ma_fast: maFast, ma_slow: maSlow } : {}),
    ...(strategy === "grid"
      ? {
          grid_levels: gridLevels,
          grid_lookback_days: gridLookbackDays,
          grid_stop_loss: gridStopLoss,
          grid_budget: gridBudget,
        }
      : {}),
    ...(strategy === "dca"
      ? {
          dca_amount: dcaAmount,
          dca_interval_days: dcaIntervalDays,
          dca_ma_days: dcaMaDays,
          dca_dip_multiplier: dcaDipMultiplier,
        }
      : {}),
  });


  // 运行接口只认策略与执行参数，剥掉回测专属字段后持久化，供模拟盘/实盘页沿用
  const runParams = (): RunParams => {
    const { start: _s, end: _e, benchmark: _b, ...rest } = params();
    return rest;
  };

  const run = async (fn: () => Promise<void>) => {
    setBusy(true);
    setErr("");
    try {
      await fn();
    } catch (e) {
      setErr((e as Error).message);
    } finally {
      setBusy(false);
    }
  };

  const loadRecords = async () => {
    try {
      setRecords(await api.backtests());
    } catch {
      // 后端未启动或存档目录不存在时静默（列表置空）
    }
  };

  useEffect(() => {
    loadRecords();
  }, []);

  const runBacktest = () =>
    run(async () => {
      setSweep([]);
      setWf(null);
      setResult(await api.backtest(params()));
      setLastBtParams(runParams()); // 已验证过的参数，供模拟盘/实盘页沿用
      loadRecords(); // 后端已自动存档，刷新记录列表
    });

  const toggleSelect = (id: string) =>
    setSelectedIds((s) => (s.includes(id) ? s.filter((x) => x !== id) : [...s, id]));

  const loadRecord = (id: string) =>
    run(async () => {
      const rec = await api.backtestRecord(id);
      setSweep([]);
      setWf(null);
      setCompare([]);
      setResult(rec.result);
    });

  const deleteRecord = (id: string) =>
    run(async () => {
      await api.deleteBacktest(id);
      setSelectedIds((s) => s.filter((x) => x !== id));
      await loadRecords();
    });

  const runCompare = () =>
    run(async () => {
      const recs = await Promise.all(selectedIds.map((id) => api.backtestRecord(id)));
      setResult(null);
      setSweep([]);
      setWf(null);
      setCompare(recs);
    });

  const runSweep = () =>
    run(async () => {
      setResult(null);
      setWf(null);
      setSweep(
        await api.sweep({
          ...params(),
          grid: gridSpec.trim() || undefined,
          split: splitRatio ? Number(splitRatio) : undefined,
        }),
      );
    });

  const runWalkforward = () =>
    run(async () => {
      setResult(null);
      setSweep([]);
      setWf(
        await api.walkforward({
          ...params(),
          grid: gridSpec.trim() || undefined,
          windows: wfWindows,
          train_ratio: wfTrainRatio,
          anchored: wfAnchored,
          rank: wfRank,
        }),
      );
    });

  const addSymbol = () => {
    const s = symInput.trim().toUpperCase();
    if (s && !symbols.includes(s)) {
      const next = [...symbols, s];
      setSymbols(next);
      setBtSymbols(next);
    }
    setSymInput("");
  };

  const delSymbol = (s: string) => {
    const next = symbols.filter((x) => x !== s);
    setSymbols(next);
    setBtSymbols(next);
  };

  // 扫描最优：切分模式按训练集年化、否则按全期年化（后端已按此排序，高亮首行有效组合）
  const hasSplit = sweep.length > 0 && sweep[0].train !== undefined;
  let bestIdx = -1;
  let bestAnn = -Infinity;
  sweep.forEach((r, i) => {
    const v = (hasSplit ? r.train?.annualized_return_pct : r.metrics?.annualized_return_pct) ?? -Infinity;
    if (v > bestAnn) {
      bestAnn = v;
      bestIdx = i;
    }
  });
  const paramCols = sweep.length > 0 ? Object.keys(sweep[0].params) : [];
  const opt = (v?: number) => (v === undefined ? "-" : fmtNum(v, 2));
  const cls = (v?: number) => (v === undefined ? "" : v >= 0 ? "pos" : "neg");

  const m = result?.metrics;
  const monthly = result ? monthlyReturns(result.equity_curve) : [];
  const monthlyYears = [...new Set(monthly.map((x) => x.year))];
  const attribution = result ? symbolAttribution(result.trades) : [];
  const compareSeries = compare.map((r, i) => ({
    name: `${r.result.strategy} · ${(r.result.symbols ?? []).length}品种 #${r.id.slice(-6)}`,
    color: PALETTE[i % PALETTE.length],
    points: r.result.equity_curve,
  }));

  return (
    <div>
      <h2>
        回测中心
        <span className="page-sub">左侧参数面板 · 右侧结果区 · 样本外验收 · 基准对比</span>
      </h2>
      <div className="bt-layout">
        <aside className="bt-side">
          {!getToken() && (
            <div className="warn">未配置管理令牌：回测接口受保护，请先在「设置」页填写 X-API-Token</div>
          )}
          <div className="card">
            <h3>回测品种（需服务端已缓存 {interval} 数据）</h3>
            <div className="toolbar">
              <input
                placeholder="输入品种，如 BTCUSDT"
                value={symInput}
                onChange={(e) => setSymInput(e.target.value)}
                onKeyDown={(e) => e.key === "Enter" && addSymbol()}
              />
              <button onClick={addSymbol}>添加</button>
            </div>
            <div className="chips">
              {symbols.map((s) => (
                <span className="chip" key={s}>
                  {s}
                  <span className="x" onClick={() => delSymbol(s)}>
                    ×
                  </span>
                </span>
              ))}
              {symbols.length === 0 && <span className="muted">未选择品种</span>}
            </div>
          </div>

          <div className="card">
            <h3>策略与执行</h3>
            <div className="form-grid">
              <label className="field">
                策略
                <select value={strategy} onChange={(e) => setStrategy(e.target.value)}>
                  <option value="momentum">动量轮动</option>
                  <option value="trend">趋势+追踪止损</option>
                  <option value="ma_cross">均线交叉</option>
                  <option value="grid">智能网格</option>
                  <option value="dca">定投</option>
                </select>
              </label>
              <label className="field">
                K线周期
                <select value={interval} onChange={(e) => setInterval_(e.target.value)}>
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
              <label className="field">
                初始资金
                <input
                  type="number"
                  value={cash}
                  onChange={(e) => setCash(Number(e.target.value))}
                />
              </label>
              <label className="field">
                单边费率
                <input
                  type="number"
                  step="0.00005"
                  value={fee}
                  onChange={(e) => setFee(Number(e.target.value))}
                />
              </label>
              <label className="field span2">
                撮合口径
                <select value={fill} onChange={(e) => setFill(e.target.value)}>
                  <option value="next_open">next_open（无未来函数）</option>
                  <option value="same_close">same_close</option>
                </select>
              </label>
            </div>
            <div className="muted">
              所选周期的数据需已下载（数据文件 {"{品种}"}_{interval}.json），否则回测会报数据加载失败；
              「天」类参数会按周期自动换算成对应K线根数
            </div>
          </div>

          {(strategy === "momentum" || strategy === "trend") && (
            <div className="card">
              <h3>动量参数</h3>
              <div className="form-grid">
                <label className="field">
                  动量天数
                  <input
                    type="number"
                    value={momentumDays}
                    onChange={(e) => setMomentumDays(Number(e.target.value))}
                  />
                </label>
                <label className="field">
                  均线天数
                  <input
                    type="number"
                    value={maDays}
                    onChange={(e) => setMaDays(Number(e.target.value))}
                  />
                </label>
                <label className="field">
                  调仓间隔(天)
                  <input
                    type="number"
                    value={rebalanceDays}
                    onChange={(e) => setRebalanceDays(Number(e.target.value))}
                  />
                </label>
                <label className="field">
                  追踪止损(0=关)
                  <input
                    type="number"
                    step="0.01"
                    value={trailingStop}
                    onChange={(e) => setTrailingStop(Number(e.target.value))}
                  />
                </label>
              </div>
              <div className="muted">
                动量天数：比较各品种近多少天涨幅来排名，越短越灵敏也越容易被噪声骗；
                均线天数：收盘价站上该均线才允许买入；
                调仓间隔：每隔多少天重排一次，太短会被手续费吃掉收益；
                追踪止损：从持仓最高点回落该比例即卖出
              </div>
            </div>
          )}

          {strategy === "ma_cross" && (
            <div className="card">
              <h3>均线交叉参数</h3>
              <div className="form-grid">
                <label className="field">
                  快线周期(根)
                  <input
                    type="number"
                    value={maFast}
                    onChange={(e) => setMaFast(Number(e.target.value))}
                  />
                </label>
                <label className="field">
                  慢线周期(根)
                  <input
                    type="number"
                    value={maSlow}
                    onChange={(e) => setMaSlow(Number(e.target.value))}
                  />
                </label>
              </div>
              <div className="muted">
                快线上穿慢线买入、下穿卖出。周期单位是「根」，按上方所选K线周期计
                （4h 周期下快线 10 = 10 根 4 小时线）；两者差距越大信号越少越稳，
                慢线须大于快线
              </div>
            </div>
          )}

          {strategy === "grid" && (
            <div className="card">
              <h3>网格参数</h3>
              <div className="form-grid">
                <label className="field">
                  网格格数
                  <input
                    type="number"
                    min="2"
                    value={gridLevels}
                    onChange={(e) => setGridLevels(Math.max(2, Number(e.target.value)))}
                  />
                </label>
                <label className="field">
                  区间回看(天)
                  <input
                    type="number"
                    value={gridLookbackDays}
                    onChange={(e) => setGridLookbackDays(Number(e.target.value))}
                  />
                </label>
                <label className="field">
                  网格止损(0=关)
                  <input
                    type="number"
                    step="0.01"
                    value={gridStopLoss}
                    onChange={(e) => setGridStopLoss(Number(e.target.value))}
                  />
                </label>
                <label className="field">
                  每品种预算(0=均分)
                  <input
                    type="number"
                    value={gridBudget}
                    onChange={(e) => setGridBudget(Number(e.target.value))}
                  />
                </label>
              </div>
              <div className="muted">
                网格格数：把价格区间等分成多少档，档位越密单笔越小、交易越频繁；
                区间回看：用最近多少天的最高/最低价确定网格上下界；
                网格止损：跌破区间下界该比例即清仓，防单边下跌一路补仓被套死（网格唯一的致命风险，不建议设 0）；
                每品种预算：每个品种分配多少计价资金布网，0 = 首次布网时按品种数均分现金
              </div>
            </div>
          )}

          {strategy === "dca" && (
            <div className="card">
              <h3>定投参数</h3>
              <div className="form-grid">
                <label className="field">
                  每期金额
                  <input
                    type="number"
                    value={dcaAmount}
                    onChange={(e) => setDcaAmount(Number(e.target.value))}
                  />
                </label>
                <label className="field">
                  定投间隔(天)
                  <input
                    type="number"
                    min="1"
                    value={dcaIntervalDays}
                    onChange={(e) => setDcaIntervalDays(Math.max(1, Number(e.target.value)))}
                  />
                </label>
                <label className="field">
                  趋势均线(0=关加码)
                  <input
                    type="number"
                    value={dcaMaDays}
                    onChange={(e) => setDcaMaDays(Number(e.target.value))}
                  />
                </label>
                <label className="field">
                  低位加码倍数
                  <input
                    type="number"
                    step="0.5"
                    value={dcaDipMultiplier}
                    onChange={(e) => setDcaDipMultiplier(Number(e.target.value))}
                  />
                </label>
              </div>
              <div className="muted">
                每期金额：每个品种每次买入的计价资金；
                定投间隔：每隔多少天买一次（7 = 周投，30 ≈ 月投）；
                趋势均线：价格低于该均线视为便宜，触发加码，0 = 关闭、恒定投固定金额；
                低位加码倍数：便宜时按几倍金额买入（越大越激进，需备足现金）
              </div>
            </div>
          )}

          <div className="card">
            <h3>风控与分散</h3>
            <div className="form-grid">
              <label className="field">
                持仓品种数(1=集中)
                <input
                  type="number"
                  min="1"
                  value={topN}
                  onChange={(e) => setTopN(Math.max(1, Number(e.target.value)))}
                />
              </label>
              <label className="field">
                市场状态均线(0=关)
                <input
                  type="number"
                  value={regimeMa}
                  onChange={(e) => setRegimeMa(Number(e.target.value))}
                />
              </label>
              <label className="field">
                熊市广度阈值(0-1)
                <input
                  type="number"
                  step="0.05"
                  value={regimeBreadth}
                  onChange={(e) => setRegimeBreadth(Number(e.target.value))}
                />
              </label>
              <label className="field">
                熔断回撤(0=关)
                <input
                  type="number"
                  step="0.01"
                  value={circuitBreaker}
                  onChange={(e) => setCircuitBreaker(Number(e.target.value))}
                />
              </label>
              <label className="field span2">
                熔断冷却(天)
                <input
                  type="number"
                  value={circuitCooldown}
                  onChange={(e) => setCircuitCooldown(Number(e.target.value))}
                />
              </label>
            </div>
            <div className="muted">熔断：组合权益自峰值回撤超阈值即全清仓，冷却期内禁止买入</div>
          </div>

          <div className="card">
            <h3>回测窗口与基准</h3>
            <div className="form-grid">
              <label className="field">
                起始日期
                <input type="date" value={startDate} onChange={(e) => setStartDate(e.target.value)} />
              </label>
              <label className="field">
                结束日期
                <input type="date" value={endDate} onChange={(e) => setEndDate(e.target.value)} />
              </label>
              <label className="field span2">
                基准品种（空=不对比）
                <input
                  placeholder="BTCUSDT"
                  value={benchmark}
                  onChange={(e) => setBenchmark(e.target.value.toUpperCase())}
                />
              </label>
            </div>
          </div>

          <div className="card">
            <h3>参数扫描与样本外验收</h3>
            <div className="form-grid">
              <label className="field span2">
                扫描网格（空=默认）
                <input
                  placeholder="momentum_days=30,60;trailing_stop=0.08,0.12"
                  value={gridSpec}
                  onChange={(e) => setGridSpec(e.target.value)}
                />
              </label>
              <label className="field span2">
                训练/测试切分
                <input
                  type="number"
                  step="0.05"
                  placeholder="0.7（空=不切分）"
                  value={splitRatio}
                  onChange={(e) => setSplitRatio(e.target.value)}
                />
              </label>
            </div>
          </div>

          <div className="card">
            <h3>滚动前进验证</h3>
            <div className="form-grid">
              <label className="field">
                折数
                <input
                  type="number"
                  min="1"
                  value={wfWindows}
                  onChange={(e) => setWfWindows(Math.max(1, Number(e.target.value)))}
                />
              </label>
              <label className="field">
                训练占比
                <input
                  type="number"
                  step="0.05"
                  value={wfTrainRatio}
                  onChange={(e) => setWfTrainRatio(Number(e.target.value))}
                />
              </label>
              <label className="field">
                窗口模式
                <select value={wfAnchored ? "anchored" : "rolling"} onChange={(e) => setWfAnchored(e.target.value === "anchored")}>
                  <option value="rolling">滚动（只看最近历史）</option>
                  <option value="anchored">扩张（历史越用越多）</option>
                </select>
              </label>
              <label className="field">
                选参指标
                <select value={wfRank} onChange={(e) => setWfRank(e.target.value)}>
                  <option value="annualized">年化收益</option>
                  <option value="sharpe">夏普比率</option>
                  <option value="calmar">Calmar（收益/回撤）</option>
                  <option value="sortino">Sortino（只罚下跌）</option>
                </select>
              </label>
            </div>
            <div className="muted">
              把历史切成多段，每段先用前面一截「调参」，再用紧随其后、调参时完全没看过的一截「考试」，
              反复多次——这比一次性切分更接近真实的定期调参过程。复用上方的扫描网格。
              折数：切成几段考试，越多越可信、也越慢；
              训练占比：每段里拿多少比例的数据用来调参（0.7 = 七成调参、三成考试）；
              窗口模式：滚动 = 只用最近一段历史调参（适应行情变化），扩张 = 从最早到当下全用上（样本更多但更迟钝）；
              选参指标：按哪个标准从网格里挑「最优」参数，只想赚得多选年化，想赚得稳选夏普或 Calmar
            </div>
          </div>

          <div className="bt-actions">
            <button className="primary" disabled={busy || symbols.length === 0} onClick={runBacktest}>
              {busy ? "运行中..." : "运行回测"}
            </button>
            <button disabled={busy || symbols.length === 0} onClick={runSweep}>
              参数扫描
            </button>
            <button disabled={busy || symbols.length === 0} onClick={runWalkforward}>
              滚动前进验证
            </button>
            <div className="muted">所有收益指标均为净利润口径（已扣往返手续费）</div>
          </div>
        </aside>

        <div className="bt-main">
      {err && <div className="error">{err}</div>}

      {result && m && (
        <>
          <div className="card">
            <div className="grid">
              <div className="stat">
                <div className="k">总收益率（净）</div>
                <div className={`v ${m.total_return_pct >= 0 ? "pos" : "neg"}`}>
                  {fmtNum(m.total_return_pct, 1)}%
                </div>
              </div>
              <div className="stat">
                <div className="k">年化收益率</div>
                <div className="v">{fmtNum(m.annualized_return_pct, 1)}%</div>
              </div>
              <div className="stat">
                <div className="k">最大回撤</div>
                <div className="v neg">{fmtNum(m.max_drawdown_pct, 1)}%</div>
              </div>
              <div className="stat">
                <div className="k">夏普比率</div>
                <div className="v">{fmtNum(m.sharpe_ratio)}</div>
              </div>
              <div className="stat">
                <div className="k">Sortino 比率</div>
                <div className="v">{opt(m.sortino_ratio)}</div>
              </div>
              <div className="stat">
                <div className="k">年化波动率</div>
                <div className="v">{fmtNum(m.annualized_volatility_pct, 1)}%</div>
              </div>
              <div className="stat">
                <div className="k">最长回撤期</div>
                <div className="v">{fmtNum(m.max_drawdown_duration_days, 0)} 天</div>
              </div>
              <div className="stat">
                <div className="k">Profit Factor</div>
                <div className="v">{opt(m.profit_factor)}</div>
              </div>
              <div className="stat">
                <div className="k">盈亏比</div>
                <div className="v">{opt(m.payoff_ratio)}</div>
              </div>
              <div className="stat">
                <div className="k">Calmar</div>
                <div className="v">{opt(m.calmar_ratio)}</div>
              </div>
              <div className="stat">
                <div className="k">暴露率</div>
                <div className="v">{m.exposure_pct === undefined ? "-" : fmtNum(m.exposure_pct, 1) + "%"}</div>
              </div>
              <div className="stat">
                <div className="k">交易回合 / 胜率</div>
                <div className="v">
                  {m.num_round_trips} / {fmtNum(m.win_rate_pct, 1)}%
                </div>
              </div>
              <div className="stat">
                <div className="k">累计手续费</div>
                <div className="v">{fmtNum(m.total_fees)}</div>
              </div>
              <div className="stat">
                <div className="k">期末现金</div>
                <div className="v">{fmtNum(result.final_cash)}</div>
              </div>
              <div className="stat">
                <div className="k">期末持仓</div>
                <div className="v">
                  {result.final_positions.length === 0
                    ? "空仓"
                    : result.final_positions.map((p) => p.symbol).join(", ")}
                </div>
              </div>
            </div>
          </div>

          {result.benchmark && (
            <div className="card">
              <h3>基准对比（{result.benchmark.symbol} 买入持有，同初始资金）</h3>
              <div className="grid">
                <div className="stat">
                  <div className="k">基准总收益</div>
                  <div className={`v ${result.benchmark.metrics.total_return_pct >= 0 ? "pos" : "neg"}`}>
                    {fmtNum(result.benchmark.metrics.total_return_pct, 1)}%
                  </div>
                </div>
                <div className="stat">
                  <div className="k">基准年化</div>
                  <div className="v">{fmtNum(result.benchmark.metrics.annualized_return_pct, 1)}%</div>
                </div>
                <div className="stat">
                  <div className="k">基准最大回撤</div>
                  <div className="v neg">{fmtNum(result.benchmark.metrics.max_drawdown_pct, 1)}%</div>
                </div>
                <div className="stat">
                  <div className="k">年化超额（α）</div>
                  <div className={`v ${result.benchmark.excess_annualized_pct >= 0 ? "pos" : "neg"}`}>
                    {fmtNum(result.benchmark.excess_annualized_pct, 1)}%
                  </div>
                </div>
                <div className="stat">
                  <div className="k">β</div>
                  <div className="v">{opt(result.benchmark.beta)}</div>
                </div>
                <div className="stat">
                  <div className="k">相关系数</div>
                  <div className="v">{opt(result.benchmark.correlation)}</div>
                </div>
                <div className="stat">
                  <div className="k">信息比率</div>
                  <div className="v">{opt(result.benchmark.information_ratio)}</div>
                </div>
              </div>
            </div>
          )}

          <div className="card">
            <h2>
              权益曲线
              {result.benchmark ? `（蓝=策略，橙虚线=基准 ${result.benchmark.symbol}）` : ""}
            </h2>
            <EquityChart curve={result.equity_curve} benchmark={result.benchmark?.curve} />
          </div>

          <div className="card">
            <h3>深度分析</h3>
            <h3>月度收益（%）</h3>
            <div className="table-wrap">
              <table>
                <thead>
                  <tr>
                    <th>年份</th>
                    {Array.from({ length: 12 }, (_, i) => (
                      <th key={i}>{i + 1}月</th>
                    ))}
                  </tr>
                </thead>
                <tbody>
                  {monthlyYears.map((y) => (
                    <tr key={y}>
                      <td>{y}</td>
                      {Array.from({ length: 12 }, (_, i) => {
                        const cell = monthly.find((x) => x.year === y && x.month === i);
                        return (
                          <td key={i} className={cell ? (cell.ret >= 0 ? "heat-pos" : "heat-neg") : ""}>
                            {cell ? fmtNum(cell.ret, 1) : "-"}
                          </td>
                        );
                      })}
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
            <h3>品种盈亏归因（净盈亏已扣手续费）</h3>
            <div className="table-wrap">
              <table>
                <thead>
                  <tr>
                    <th>品种</th>
                    <th>回合</th>
                    <th>胜率%</th>
                    <th>净盈亏</th>
                    <th>平均持仓天数</th>
                  </tr>
                </thead>
                <tbody>
                  {attribution.map((a) => (
                    <tr key={a.symbol}>
                      <td>{a.symbol}</td>
                      <td>{a.n}</td>
                      <td>{fmtNum(a.winRate, 1)}</td>
                      <td className={a.pnl >= 0 ? "pos" : "neg"}>{fmtNum(a.pnl)}</td>
                      <td>{fmtNum(a.avgDays, 1)}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
              {attribution.length === 0 && <div className="muted">无已平仓回合</div>}
            </div>
          </div>
          <div className="card">
            <h3>逐笔回合（净盈亏已扣手续费）</h3>
            <div className="table-wrap">
              <table>
                <thead>
                  <tr>
                    <th>#</th>
                    <th>品种</th>
                    <th>入场日期</th>
                    <th>出场日期</th>
                    <th>入场价</th>
                    <th>出场价</th>
                    <th>数量</th>
                    <th>净盈亏</th>
                  </tr>
                </thead>
                <tbody>
                  {result.trades.map((t, i) => (
                    <tr key={i}>
                      <td>{i + 1}</td>
                      <td>{t.symbol}</td>
                      <td>{fmtDate(t.entry_time)}</td>
                      <td>{fmtDate(t.exit_time)}</td>
                      <td>{fmtPrice(t.entry_price)}</td>
                      <td>{fmtPrice(t.exit_price)}</td>
                      <td>{fmtNum(t.quantity, 4)}</td>
                      <td className={t.pnl >= 0 ? "pos" : "neg"}>{fmtNum(t.pnl)}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
              {result.trades.length === 0 && <div className="muted">无已平仓回合</div>}
            </div>
          </div>
        </>
      )}

      {sweep.length > 0 && (
        <div className="card">
          <h3>
            参数扫描
            {hasSplit ? "（训练集选参 → 测试集验收，按训练年化排序）" : "（全期回测，按年化收益排序）"}
            ，高亮最优组合
          </h3>
          <div className="table-wrap">
            <table>
              <thead>
                <tr>
                  {paramCols.map((c) => (
                    <th key={c}>{c}</th>
                  ))}
                  {hasSplit ? (
                    <>
                      <th>训练年化%</th>
                      <th>训练回合</th>
                      <th>测试年化%</th>
                      <th>测试回撤%</th>
                      <th>测试夏普</th>
                      <th>测试PF</th>
                      <th>测试回合</th>
                    </>
                  ) : (
                    <>
                      <th>总收益%</th>
                      <th>年化%</th>
                      <th>最大回撤%</th>
                      <th>夏普</th>
                      <th>Sortino</th>
                      <th>年化波动%</th>
                      <th>PF</th>
                      <th>Calmar</th>
                      <th>暴露率%</th>
                      <th>胜率%</th>
                    </>
                  )}
                </tr>
              </thead>
              <tbody>
                {sweep.map((r, i) => (
                  <tr key={i} className={i === bestIdx ? "best" : ""}>
                    {paramCols.map((c) => (
                      <td key={c}>{r.params[c]}</td>
                    ))}
                    {hasSplit ? (
                      <>
                        <td>{opt(r.train?.annualized_return_pct)}</td>
                        <td>{r.train?.num_round_trips ?? "-"}</td>
                        <td className={cls(r.test?.annualized_return_pct)}>{opt(r.test?.annualized_return_pct)}</td>
                        <td>{opt(r.test?.max_drawdown_pct)}</td>
                        <td>{opt(r.test?.sharpe_ratio)}</td>
                        <td>{opt(r.test?.profit_factor)}</td>
                        <td>{r.test?.num_round_trips ?? "-"}</td>
                      </>
                    ) : (
                      <>
                        <td>{opt(r.metrics?.total_return_pct)}</td>
                        <td className={cls(r.metrics?.annualized_return_pct)}>{opt(r.metrics?.annualized_return_pct)}</td>
                        <td>{opt(r.metrics?.max_drawdown_pct)}</td>
                        <td>{opt(r.metrics?.sharpe_ratio)}</td>
                        <td>{opt(r.metrics?.sortino_ratio)}</td>
                        <td>{opt(r.metrics?.annualized_volatility_pct)}</td>
                        <td>{opt(r.metrics?.profit_factor)}</td>
                        <td>{opt(r.metrics?.calmar_ratio)}</td>
                        <td>{r.metrics?.exposure_pct === undefined ? "-" : fmtNum(r.metrics.exposure_pct, 1)}</td>
                        <td>{opt(r.metrics?.win_rate_pct)}</td>
                      </>
                    )}
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        </div>
      )}

      {wf && (
        <div className="card">
          <h3>
            滚动前进验证（{wf.summary.folds} 折 · {wfAnchored ? "扩张窗" : "滚动窗"} · 按
            {wf.summary.rank_metric}选参）
          </h3>
          <div className="table-wrap">
            <table>
              <thead>
                <tr>
                  <th>折号</th>
                  <th>训练区间（用来调参）</th>
                  <th>测试区间（样本外考试）</th>
                  <th>选中参数</th>
                  <th>训练{wf.summary.rank_metric}</th>
                  <th>测试{wf.summary.rank_metric}</th>
                  <th>测试收益%</th>
                </tr>
              </thead>
              <tbody>
                {wf.folds.map((f, i) => (
                  <tr key={i}>
                    <td>{i + 1}</td>
                    <td>
                      {f.train_start} ~ {f.train_end}
                    </td>
                    <td>
                      {f.test_start} ~ {f.test_end}
                    </td>
                    <td>
                      {Object.entries(f.params)
                        .map(([k, v]) => `${k}=${v}`)
                        .join(", ")}
                    </td>
                    <td>{opt(rankValue(f.train, wfRank))}</td>
                    <td className={cls(rankValue(f.test, wfRank))}>{opt(rankValue(f.test, wfRank))}</td>
                    <td className={cls(f.test.total_return_pct)}>{fmtNum(f.test.total_return_pct, 1)}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
          <h3>样本外汇总（逐折复利，净利润口径）</h3>
          <div className="grid">
            <div className="stat">
              <div className="k">样本外总收益</div>
              <div className={`v ${cls(wf.summary.oos_total_return_pct)}`}>
                {fmtNum(wf.summary.oos_total_return_pct, 1)}%
              </div>
            </div>
            <div className="stat">
              <div className="k">样本外年化</div>
              <div className={`v ${cls(wf.summary.oos_annualized_pct)}`}>
                {fmtNum(wf.summary.oos_annualized_pct, 1)}%
              </div>
            </div>
            <div className="stat">
              <div className="k">测试窗胜率</div>
              <div className="v">
                {wf.summary.positive_folds}/{wf.summary.folds} 折为正
              </div>
            </div>
            <div className="stat">
              <div className="k">过拟合差距</div>
              <div className={`v ${wf.summary.overfit_gap > 0 ? "neg" : "pos"}`}>
                {fmtNum(wf.summary.overfit_gap, 2)}
              </div>
            </div>
          </div>
          <div className="muted">
            样本外总收益/年化：假设你真按这套流程每到窗口边界就重新调参、资金一路滚下去，
            在「调参时没见过的数据」上实际能拿到的收益（覆盖 {fmtNum(wf.summary.oos_days, 0)} 天）
            —— 这是本页唯一值得当真的收益数字，回测页那个漂亮的总收益里含着选参的便宜。
            测试窗胜率：几段考试里有几段是赚钱的，比例太低说明策略只在个别行情里能用。
            过拟合差距 = 训练{wf.summary.rank_metric}均值 {fmtNum(wf.summary.train_mean, 2)} −
            测试均值 {fmtNum(wf.summary.test_mean, 2)}：调参时看到的成绩比真实考试成绩高出多少。
            差距为正是正常的（挑参数必然占训练数据的便宜），但差距越大，越说明这套参数只是把历史行情
            背了下来、并没有学到真规律，拿去实盘很可能亏钱。若差距大到训练亮眼而测试为负，
            请果断放弃这组参数，改用更少的网格维度、更宽的取值间隔或更长的调仓间隔重来。
          </div>
        </div>
      )}

      <div className="card">
        <h3>回测记录（服务端自动存档）</h3>
        <div className="toolbar">
          <button disabled={busy || selectedIds.length < 2} onClick={runCompare}>
            对比所选（{selectedIds.length}）
          </button>
          <button disabled={busy} onClick={loadRecords}>
            刷新
          </button>
          {compare.length > 0 && <button onClick={() => setCompare([])}>关闭对比</button>}
          <span className="muted">删除需管理令牌；勾选 ≥2 条可叠加对比权益曲线</span>
        </div>
        <div className="table-wrap">
          <table>
            <thead>
              <tr>
                <th></th>
                <th>时间</th>
                <th>策略</th>
                <th>品种</th>
                <th>基准</th>
                <th>总收益%</th>
                <th>年化%</th>
                <th>回撤%</th>
                <th>夏普</th>
                <th>操作</th>
              </tr>
            </thead>
            <tbody>
              {records.map((r) => (
                <tr key={r.id}>
                  <td>
                    <input
                      type="checkbox"
                      checked={selectedIds.includes(r.id)}
                      onChange={() => toggleSelect(r.id)}
                    />
                  </td>
                  <td>{r.created_at_ms ? fmtDateTime(r.created_at_ms) : "-"}</td>
                  <td>{r.strategy ?? "-"}</td>
                  <td>{(r.symbols ?? []).join(", ")}</td>
                  <td>{r.benchmark ?? "-"}</td>
                  <td className={cls(r.metrics?.total_return_pct)}>{opt(r.metrics?.total_return_pct)}</td>
                  <td className={cls(r.metrics?.annualized_return_pct)}>{opt(r.metrics?.annualized_return_pct)}</td>
                  <td>{opt(r.metrics?.max_drawdown_pct)}</td>
                  <td>{opt(r.metrics?.sharpe_ratio)}</td>
                  <td>
                    <button disabled={busy} onClick={() => loadRecord(r.id)}>
                      查看
                    </button>{" "}
                    <button disabled={busy} onClick={() => deleteRecord(r.id)}>
                      删除
                    </button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
          {records.length === 0 && <div className="muted">暂无存档（运行回测后自动保存）</div>}
        </div>
      </div>

      {compare.length >= 2 && (
        <div className="card">
          <h3>记录对比（权益曲线归一化至 100）</h3>
          <EquityChartMulti series={compareSeries} />
          <div className="table-wrap">
            <table>
              <thead>
                <tr>
                  <th>记录</th>
                  <th>策略</th>
                  <th>品种</th>
                  <th>总收益%</th>
                  <th>年化%</th>
                  <th>最大回撤%</th>
                  <th>夏普</th>
                  <th>Sortino</th>
                  <th>最长回撤期(天)</th>
                  <th>PF</th>
                  <th>Calmar</th>
                  <th>回合/胜率%</th>
                </tr>
              </thead>
              <tbody>
                {compare.map((r, i) => (
                  <tr key={r.id}>
                    <td>
                      <span style={{ color: PALETTE[i % PALETTE.length] }}>●</span> #{r.id.slice(-6)}
                    </td>
                    <td>{r.result.strategy}</td>
                    <td>{r.result.symbols.join(", ")}</td>
                    <td className={cls(r.result.metrics.total_return_pct)}>
                      {fmtNum(r.result.metrics.total_return_pct, 1)}
                    </td>
                    <td className={cls(r.result.metrics.annualized_return_pct)}>
                      {fmtNum(r.result.metrics.annualized_return_pct, 1)}
                    </td>
                    <td>{fmtNum(r.result.metrics.max_drawdown_pct, 1)}</td>
                    <td>{fmtNum(r.result.metrics.sharpe_ratio)}</td>
                    <td>{opt(r.result.metrics.sortino_ratio)}</td>
                    <td>{opt(r.result.metrics.max_drawdown_duration_days)}</td>
                    <td>{opt(r.result.metrics.profit_factor)}</td>
                    <td>{opt(r.result.metrics.calmar_ratio)}</td>
                    <td>
                      {r.result.metrics.num_round_trips} / {fmtNum(r.result.metrics.win_rate_pct, 1)}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        </div>
      )}
        </div>
      </div>
    </div>
  );
}
