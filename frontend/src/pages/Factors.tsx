// 因子选股：多因子截面快照（后端本地日K计算）——排序/搜索/Top-N 圈选/一键发送回测，
// 附因子 IC 验证与品种相关性矩阵。

import { useCallback, useEffect, useMemo, useState } from "react";
import { useNavigate } from "react-router-dom";
import { api } from "../api";
import { EquityChart } from "../charts";
import { fmtDateTime, fmtNum, fmtVol } from "../fmt";
import { setBtSymbols } from "../store";
import type { CorrSnapshot, FactorRow, FactorsSnapshot, IcReport } from "../types";

type SortKey = keyof Pick<
  FactorRow,
  | "rank"
  | "score"
  | "price"
  | "mom20"
  | "mom60"
  | "mom120"
  | "vol_ann"
  | "rsi14"
  | "ma50_dev"
  | "ma200_dev"
  | "vol_ratio"
  | "dd_from_high"
  | "avg_quote_volume"
  | "cmf20"
  | "flow20"
>;

// 百分比展示（小数 -> %）
const pct = (v: number) => `${fmtNum(v * 100, 1)}%`;

// IC 验证可选因子（与后端 FACTORS 一致）
const FACTOR_LABELS: [string, string][] = [
  ["mom20", "动量20"],
  ["mom60", "动量60"],
  ["mom120", "动量120"],
  ["vol_ann", "30日波动"],
  ["rsi14", "RSI14"],
  ["ma50_dev", "MA50偏离"],
  ["ma200_dev", "MA200偏离"],
  ["vol_ratio", "量能比"],
  ["dd_from_high", "120日回撤"],
  ["avg_quote_volume", "日均成交额"],
  ["cmf20", "CMF资金流"],
  ["flow20", "净资金流"],
];

// 相关矩阵单元格配色：绿=正相关、红=负相关，深浅表示强度
const cellStyle = (v: number | null) => {
  if (v == null || Number.isNaN(v)) return { textAlign: "center" as const };
  const a = Math.min(Math.abs(v), 1) * 0.55;
  return {
    background: v >= 0 ? `rgba(38,166,154,${a})` : `rgba(239,83,80,${a})`,
    textAlign: "center" as const,
  };
};

export default function Factors() {
  const [snap, setSnap] = useState<FactorsSnapshot | null>(null);
  const [err, setErr] = useState("");
  const [busy, setBusy] = useState(false);
  const [q, setQ] = useState("");
  const [sortKey, setSortKey] = useState<SortKey>("rank");
  const [asc, setAsc] = useState(true);
  const [topN, setTopN] = useState(3);
  const [selected, setSelected] = useState<Set<string>>(new Set());
  // 因子 IC 验证
  const [icFactor, setIcFactor] = useState("mom60");
  const [icHorizon, setIcHorizon] = useState(20);
  const [ic, setIc] = useState<IcReport | null>(null);
  const [icBusy, setIcBusy] = useState(false);
  const [icErr, setIcErr] = useState("");
  // 相关性矩阵
  const [corrDays, setCorrDays] = useState(60);
  const [corr, setCorr] = useState<CorrSnapshot | null>(null);
  const [corrErr, setCorrErr] = useState("");
  const nav = useNavigate();

  const load = useCallback(async () => {
    setBusy(true);
    setErr("");
    try {
      setSnap(await api.factors());
    } catch (e) {
      setErr((e as Error).message);
    } finally {
      setBusy(false);
    }
  }, []);

  useEffect(() => {
    load();
  }, [load]);

  // IC 验证：因子/前瞻天数变化时重算（后端本地计算，秒级返回）
  useEffect(() => {
    let alive = true;
    setIcBusy(true);
    setIcErr("");
    api
      .factorIc(icFactor, icHorizon)
      .then((r) => alive && setIc(r))
      .catch((e) => {
        if (alive) {
          setIc(null);
          setIcErr((e as Error).message);
        }
      })
      .finally(() => alive && setIcBusy(false));
    return () => {
      alive = false;
    };
  }, [icFactor, icHorizon]);

  // 相关性矩阵：回看窗口变化时重算
  useEffect(() => {
    let alive = true;
    setCorrErr("");
    api
      .correlation(corrDays)
      .then((r) => alive && setCorr(r))
      .catch((e) => {
        if (alive) {
          setCorr(null);
          setCorrErr((e as Error).message);
        }
      });
    return () => {
      alive = false;
    };
  }, [corrDays]);

  const rows = useMemo(() => {
    let list = snap?.factors ?? [];
    const kw = q.trim().toUpperCase();
    if (kw) list = list.filter((r) => r.symbol.includes(kw));
    return [...list].sort((a, b) => {
      const d = (a[sortKey] as number) - (b[sortKey] as number);
      return asc ? d : -d;
    });
  }, [snap, q, sortKey, asc]);

  const sortBy = (k: SortKey) => {
    if (k === sortKey) {
      setAsc(!asc);
    } else {
      setSortKey(k);
      // 名次/回撤/波动默认升序有意义，其余因子默认降序（越大越好）
      setAsc(k === "rank" || k === "dd_from_high" || k === "vol_ann");
    }
  };

  const th = (label: string, key: SortKey, title?: string) => (
    <th className="sortable" title={title ?? "点击排序"} onClick={() => sortBy(key)}>
      {label}
      {sortKey === key ? (asc ? " ▴" : " ▾") : ""}
    </th>
  );

  const toggle = (sym: string) =>
    setSelected((s) => {
      const n = new Set(s);
      if (n.has(sym)) n.delete(sym);
      else n.add(sym);
      return n;
    });

  const pickTopN = () => {
    const top = [...(snap?.factors ?? [])]
      .sort((a, b) => a.rank - b.rank)
      .slice(0, Math.max(1, topN))
      .map((r) => r.symbol);
    setSelected(new Set(top));
  };

  // 一键发送：写入回测品种列表（覆盖式），跳转回测中心
  const sendToBacktest = () => {
    const syms = [...selected];
    if (syms.length === 0) return;
    setBtSymbols(syms);
    nav("/backtest");
  };

  const weights = snap?.weights ?? {};
  const weightTxt = Object.entries(weights)
    .map(([k, v]) => `${k} ${Math.round(v * 100)}%`)
    .join(" + ");

  return (
    <div>
      <h2>
        因子选股
        <span className="page-sub">
          多因子截面打分 · 本地日K计算（无网络依赖）· 样本 ≥ 210 根
        </span>
      </h2>

      <div className="card">
        <h3>打分方法（截面 z 标准化后加权）</h3>
        <div className="muted">
          综合得分 = {weightTxt || "…"}；动量取 20/60/120 日均值，趋势取 MA50/MA200 偏离均值，
          资金流取近 20 日符号化成交额；低波动与低回撤为负向因子（越小越好）。
          RSI 与 CMF 仅供参考，不参与打分。
          {snap && (snap.skipped ?? 0) > 0 && ` 另有 ${snap.skipped} 个品种因样本不足被剔除。`}
        </div>
      </div>

      <div className="toolbar">
        <input
          placeholder="搜索品种，如 BTC"
          value={q}
          onChange={(e) => setQ(e.target.value)}
          style={{ width: 200 }}
        />
        <button onClick={load} disabled={busy}>
          {busy ? "计算中..." : "重新计算"}
        </button>
        <span style={{ flex: 1 }} />
        <label className="field">
          Top N
          <input
            type="number"
            min="1"
            value={topN}
            onChange={(e) => setTopN(Math.max(1, Number(e.target.value)))}
            style={{ width: 70 }}
          />
        </label>
        <button onClick={pickTopN} disabled={!snap || snap.factors.length === 0}>
          圈选前 {Math.max(1, topN)} 名
        </button>
        <button className="primary" disabled={selected.size === 0} onClick={sendToBacktest}>
          发送回测（{selected.size}）
        </button>
      </div>

      {err && <div className="error">{err}</div>}
      {snap && (
        <div className="toolbar">
          <span className="muted">
            共 {snap.count} 个品种参与打分 · 因子截面时间 {fmtDateTime(snap.updated_at)} ·
            勾选后「发送回测」将覆盖回测中心的品种列表
          </span>
        </div>
      )}

      <div className="card table-wrap">
        <table>
          <thead>
            <tr>
              <th></th>
              {th("名次", "rank")}
              <th>品种</th>
              {th("综合得分", "score")}
              {th("现价", "price")}
              {th("动量20", "mom20")}
              {th("动量60", "mom60")}
              {th("动量120", "mom120")}
              {th("年化波动", "vol_ann", "近 30 日日收益标准差年化")}
              {th("RSI14", "rsi14", "仅供参考，不参与打分")}
              {th("MA50偏离", "ma50_dev")}
              {th("MA200偏离", "ma200_dev")}
              {th("量能比", "vol_ratio", "5日均量 / 30日均量")}
              {th("距120日高点", "dd_from_high", "自近 120 日最高价的回撤")}
              {th("CMF20", "cmf20", "蔡金资金流 [-1,1]，仅供参考")}
              {th("净资金流20", "flow20", "近 20 日符号化成交额（涨日计正、跌日计负）")}
              {th("日均成交额", "avg_quote_volume", "近 30 日估算")}
            </tr>
          </thead>
          <tbody>
            {rows.map((r) => (
              <tr key={r.symbol} className="clickable" onClick={() => toggle(r.symbol)}>
                <td>
                  <input
                    type="checkbox"
                    checked={selected.has(r.symbol)}
                    onChange={() => toggle(r.symbol)}
                    onClick={(e) => e.stopPropagation()}
                  />
                </td>
                <td>{r.rank}</td>
                <td>{r.symbol}</td>
                <td className={r.score >= 0 ? "heat-pos" : "heat-neg"}>{fmtNum(r.score, 2)}</td>
                <td className={r.mom20 >= 0 ? "pos" : "neg"}>{pct(r.mom20)}</td>
                <td className={r.mom60 >= 0 ? "pos" : "neg"}>{pct(r.mom60)}</td>
                <td className={r.mom120 >= 0 ? "pos" : "neg"}>{pct(r.mom120)}</td>
                <td>{pct(r.vol_ann)}</td>
                <td>{fmtNum(r.rsi14, 1)}</td>
                <td className={r.ma50_dev >= 0 ? "pos" : "neg"}>{pct(r.ma50_dev)}</td>
                <td className={r.ma200_dev >= 0 ? "pos" : "neg"}>{pct(r.ma200_dev)}</td>
                <td>{fmtNum(r.vol_ratio, 2)}</td>
                <td className={r.dd_from_high > 0.1 ? "neg" : ""}>{pct(r.dd_from_high)}</td>
                <td className={r.cmf20 >= 0 ? "pos" : "neg"}>{fmtNum(r.cmf20, 2)}</td>
                <td className={r.flow20 >= 0 ? "pos" : "neg"}>{fmtVol(r.flow20)}</td>
                <td>{fmtVol(r.avg_quote_volume)}</td>
              </tr>
            ))}
          </tbody>
        </table>
        {rows.length === 0 && !err && (
          <div className="muted">
            {busy ? "计算中..." : "无数据：请先在「数据中心」下载日线（每品种 ≥ 210 根）"}
          </div>
        )}
      </div>

      {selected.size > 0 && (
        <div className="toolbar">
          <span className="muted">已选：{[...selected].sort().join(", ")}</span>
          <button onClick={() => setSelected(new Set())}>清空</button>
        </div>
      )}

      <div className="card">
        <h3>因子 IC 验证（因子对未来收益的预测力）</h3>
        <div className="toolbar">
          <label className="field">
            因子
            <select value={icFactor} onChange={(e) => setIcFactor(e.target.value)}>
              {FACTOR_LABELS.map(([k, label]) => (
                <option key={k} value={k}>
                  {label}
                </option>
              ))}
            </select>
          </label>
          <label className="field">
            前瞻天数
            <select
              value={icHorizon}
              onChange={(e) => setIcHorizon(Number(e.target.value))}
            >
              {[10, 20, 40].map((h) => (
                <option key={h} value={h}>
                  {h} 日
                </option>
              ))}
            </select>
          </label>
          {icBusy && <span className="muted">计算中...</span>}
        </div>
        {icErr && <div className="error">{icErr}</div>}
        {ic && (
          <>
            <div className="toolbar">
              <span>
                IC均值 <b className={ic.ic_mean >= 0 ? "pos" : "neg"}>{fmtNum(ic.ic_mean, 4)}</b>
              </span>
              <span>
                ICIR <b className={ic.icir >= 0 ? "pos" : "neg"}>{fmtNum(ic.icir, 2)}</b>
              </span>
              <span>
                IC&gt;0 占比 <b>{pct(ic.hit_rate)}</b>
              </span>
              <span className="muted">
                样本 {ic.n} 个评估日 · 每隔 {ic.step} 个交易日 · 前瞻 {ic.horizon} 日
              </span>
            </div>
            <EquityChart
              curve={ic.series.map((p) => ({ timestamp: p.ms, equity: p.ic }))}
              height={220}
            />
          </>
        )}
        <div className="muted">
          经验参考：|IC均值| &gt; 0.03 因子有一定预测力，ICIR &gt; 0.5 较稳定；
          评估日取全品种公共交易日，因子截面值与未来 N 日收益做 Pearson 相关。
        </div>
      </div>

      <div className="card">
        <h3>相关性矩阵（日收益率 · 近 {corr?.days ?? corrDays} 个交易日）</h3>
        <div className="toolbar">
          <label className="field">
            回看天数
            <select value={corrDays} onChange={(e) => setCorrDays(Number(e.target.value))}>
              {[30, 60, 120, 250].map((d) => (
                <option key={d} value={d}>
                  {d} 日
                </option>
              ))}
            </select>
          </label>
        </div>
        {corrErr && <div className="error">{corrErr}</div>}
        {corr && (
          <div className="table-wrap">
            <table>
              <thead>
                <tr>
                  <th></th>
                  {corr.symbols.map((s) => (
                    <th key={s} style={{ textAlign: "center" }}>
                      {s.replace("USDT", "")}
                    </th>
                  ))}
                </tr>
              </thead>
              <tbody>
                {corr.symbols.map((si, i) => (
                  <tr key={si}>
                    <td>{si.replace("USDT", "")}</td>
                    {corr.matrix[i].map((v, j) => (
                      <td
                        key={j}
                        style={cellStyle(v)}
                        title={`${si} × ${corr.symbols[j]}`}
                      >
                        {v == null || Number.isNaN(v) ? "–" : v.toFixed(2)}
                      </td>
                    ))}
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
        <div className="muted">
          绿 = 正相关、红 = 负相关，颜色深浅表示强度；分散配置时优先挑选低相关品种。
        </div>
      </div>

      <div className="muted" style={{ marginTop: 8 }}>
        提示：现价为日线最新收盘价（非实时）；发送回测后可在回测中心配合
        Top-N 分散与市场状态过滤使用。
      </div>
    </div>
  );
}
