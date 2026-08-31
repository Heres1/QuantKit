// 市场总览：市场环境分析（广度/动量/波动）+ Binance 全部 USDT 现货对，搜索 + 排序 + 30s 自动刷新

import { useCallback, useEffect, useMemo, useState } from "react";
import { useNavigate } from "react-router-dom";
import { api } from "../api";
import { fmtDateTime, fmtNum, fmtPct, fmtPrice, fmtVol } from "../fmt";
import { addBtSymbol } from "../store";
import type { MarketRegime, Quote } from "../types";

type SortKey = "symbol" | "last_price" | "price_change_pct" | "high_price" | "low_price" | "quote_volume";

export default function Market() {
  const [quotes, setQuotes] = useState<Quote[]>([]);
  const [updatedAt, setUpdatedAt] = useState(0);
  const [q, setQ] = useState("");
  const [sortKey, setSortKey] = useState<SortKey>("quote_volume");
  const [err, setErr] = useState("");
  const [added, setAdded] = useState<Set<string>>(new Set());
  const nav = useNavigate();

  const load = useCallback(async () => {
    try {
      const d = await api.markets();
      setQuotes(d.quotes);
      setUpdatedAt(d.updated_at);
      setErr("");
    } catch (e) {
      setErr((e as Error).message);
    }
  }, []);

  useEffect(() => {
    load();
    const t = setInterval(load, 30_000);
    return () => clearInterval(t);
  }, [load]);

  const rows = useMemo(() => {
    let list = quotes;
    const kw = q.trim().toUpperCase();
    if (kw) list = list.filter((x) => x.symbol.includes(kw));
    return [...list].sort((a, b) =>
      sortKey === "symbol"
        ? a.symbol.localeCompare(b.symbol)
        : (b[sortKey] as number) - (a[sortKey] as number)
    );
  }, [quotes, q, sortKey]);

  const th = (label: string, key: SortKey) => (
    <th
      className="sortable"
      onClick={() => setSortKey(key)}
      title="点击排序"
    >
      {label}
      {sortKey === key ? " ▾" : ""}
    </th>
  );

  const addToBt = (sym: string) => {
    addBtSymbol(sym);
    setAdded((s) => new Set(s).add(sym));
  };

  // 概览统计（交易所/量化平台市场页惯例）
  const up = quotes.filter((x) => x.price_change_pct >= 0).length;
  const down = quotes.length - up;
  const totalVol = quotes.reduce((s, x) => s + x.quote_volume, 0);

  return (
    <div>
      <h2>
        市场总览
        <span className="page-sub">Binance USDT 现货 · {rows.length} 个交易对</span>
      </h2>
      <MarketRegimeCard />
      <div className="grid" style={{ marginBottom: 16 }}>
        <div className="stat">
          <div className="k">交易对总数</div>
          <div className="v">{quotes.length}</div>
        </div>
        <div className="stat">
          <div className="k">24h 上涨</div>
          <div className="v pos">{up}</div>
        </div>
        <div className="stat">
          <div className="k">24h 下跌</div>
          <div className="v neg">{down}</div>
        </div>
        <div className="stat">
          <div className="k">24h 总成交额</div>
          <div className="v">{fmtVol(totalVol)}</div>
        </div>
      </div>
      <div className="toolbar">
        <input
          placeholder="搜索品种，如 BTC"
          value={q}
          onChange={(e) => setQ(e.target.value)}
          style={{ width: 240 }}
        />
        <button onClick={load}>刷新</button>
        {updatedAt > 0 && (
          <span className="muted">行情更新：{fmtDateTime(updatedAt)}（服务端 30s 缓存）</span>
        )}
      </div>
      {err && <div className="error">{err}</div>}
      <div className="card table-wrap">
        <table>
          <thead>
            <tr>
              {th("品种", "symbol")}
              {th("最新价", "last_price")}
              {th("24h 涨跌", "price_change_pct")}
              {th("24h 最高", "high_price")}
              {th("24h 最低", "low_price")}
              {th("24h 成交额", "quote_volume")}
              <th></th>
            </tr>
          </thead>
          <tbody>
            {rows.map((r) => (
              <tr
                key={r.symbol}
                className="clickable"
                onClick={() => nav(`/symbol/${r.symbol}`)}
              >
                <td>{r.symbol}</td>
                <td>{fmtPrice(r.last_price)}</td>
                <td className={r.price_change_pct >= 0 ? "pos" : "neg"}>
                  {fmtPct(r.price_change_pct)}
                </td>
                <td>{r.high_price > 0 ? fmtPrice(r.high_price) : "-"}</td>
                <td>{r.low_price > 0 ? fmtPrice(r.low_price) : "-"}</td>
                <td>{fmtVol(r.quote_volume)}</td>
                <td>
                  <button
                    className="small"
                    disabled={added.has(r.symbol)}
                    onClick={(e) => {
                      e.stopPropagation();
                      addToBt(r.symbol);
                    }}
                  >
                    {added.has(r.symbol) ? "已加入回测" : "+ 回测"}
                  </button>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
        {rows.length === 0 && !err && <div className="muted">加载中...</div>}
      </div>
    </div>
  );
}

/** 环境结论：由趋势广度 + BTC 锚推导，辅助判断“现在该不该重仓参与” */
function regimeConclusion(r: MarketRegime): { text: string; cls: "pos" | "neg" | "muted" } {
  const p = r.breadth.above_ma50_pct;
  const btcBelow = r.btc != null && !r.btc.above_ma50;
  if (p >= 70 && !btcBelow) {
    return { text: `强趋势市：${p.toFixed(0)}% 品种站上 50 日均线且 BTC 领涨，趋势类策略可适当参与`, cls: "pos" };
  }
  if (p >= 40) {
    return { text: `分化市：${p.toFixed(0)}% 品种站上 50 日均线，动量分化，建议精选强势品种、控制仓位`, cls: "muted" };
  }
  return { text: `弱势市：仅 ${p.toFixed(0)}% 品种站上 50 日均线，建议轻仓或空仓观望，等待广度回暖`, cls: "neg" };
}

/** 市场环境分析：趋势广度（MA50/MA200 上方占比）、动量分层、波动与 BTC 锚。
 * 回答投资前置问题：“现在的市场环境适合什么程度的参与”。 */
function MarketRegimeCard() {
  const [r, setR] = useState<MarketRegime | null>(null);
  const [err, setErr] = useState("");
  const nav = useNavigate();

  const load = useCallback(async () => {
    try {
      setR(await api.marketRegime());
      setErr("");
    } catch (e) {
      setErr((e as Error).message);
    }
  }, []);

  useEffect(() => {
    load();
    const t = setInterval(load, 60_000);
    return () => clearInterval(t);
  }, [load]);

  if (err) {
    return (
      <div className="card" style={{ marginBottom: 16 }}>
        <h2 style={{ margin: 0 }}>市场环境分析</h2>
        <div className="muted">{err}</div>
      </div>
    );
  }
  if (!r) return null;

  const sorted = [...r.symbols].sort((a, b) => (b.ret_30d ?? -1e9) - (a.ret_30d ?? -1e9));
  const verdict = regimeConclusion(r);
  const pctCell = (v: number | null) =>
    v == null ? "-" : <span className={v >= 0 ? "pos" : "neg"}>{fmtPct(v)}</span>;

  return (
    <div className="card table-wrap" style={{ marginBottom: 16 }}>
      <div className="toolbar">
        <h2 style={{ margin: 0 }}>市场环境分析</h2>
        <span className="muted">池内 {r.total} 品种 · 本地日K + 实时价 · 60s 缓存</span>
      </div>
      <div className={verdict.cls} style={{ marginBottom: 12, fontWeight: 600 }}>
        {verdict.text}
      </div>
      <div className="grid">
        <div className="stat">
          <div className="k">MA50 广度（趋势多头）</div>
          <div className="v">
            {r.breadth.above_ma50}/{r.total}（{r.breadth.above_ma50_pct.toFixed(0)}%）
          </div>
        </div>
        <div className="stat">
          <div className="k">MA200 广度（长期趋势）</div>
          <div className="v">
            {r.breadth.ma200_count > 0
              ? `${r.breadth.above_ma200}/${r.breadth.ma200_count}（${r.breadth.above_ma200_pct?.toFixed(0) ?? "-"}%）`
              : "样本不足"}
          </div>
        </div>
        <div className="stat">
          <div className="k">30 日动量分层</div>
          <div className="v">
            <span className="pos">{r.momentum.strong} 强</span>{" / "}
            {r.momentum.neutral} 震荡{" / "}
            <span className="neg">{r.momentum.weak} 弱</span>
          </div>
        </div>
        <div className="stat">
          <div className="k">平均 30 日年化波动</div>
          <div className="v">{r.avg_vol_30d != null ? `${fmtNum(r.avg_vol_30d, 1)}%` : "-"}</div>
        </div>
        <div className="stat">
          <div className="k">BTC（市场锚）</div>
          <div className="v" style={{ fontSize: 15 }}>
            {r.btc == null ? "-" : (
              <>
                <span className={r.btc.above_ma50 ? "pos" : "neg"}>
                  {r.btc.above_ma50 ? "MA50 上方" : "MA50 下方"}
                </span>
                {r.btc.ret_30d != null && <> · 30日 {fmtPct(r.btc.ret_30d)}</>}
              </>
            )}
          </div>
        </div>
      </div>
      <table>
        <thead>
          <tr>
            <th>品种</th>
            <th>最新价</th>
            <th>7 日</th>
            <th>30 日</th>
            <th>90 日</th>
            <th>MA50</th>
            <th>MA200</th>
            <th>30 日波动(年化)</th>
          </tr>
        </thead>
        <tbody>
          {sorted.map((s) => (
            <tr key={s.symbol} className="clickable" onClick={() => nav(`/symbol/${s.symbol}`)}>
              <td>{s.symbol}</td>
              <td>{fmtPrice(s.price)}</td>
              <td>{pctCell(s.ret_7d)}</td>
              <td>{pctCell(s.ret_30d)}</td>
              <td>{pctCell(s.ret_90d)}</td>
              <td className={s.above_ma50 ? "pos" : "neg"}>{s.above_ma50 ? "上方" : "下方"}</td>
              <td className={s.above_ma200 == null ? "" : s.above_ma200 ? "pos" : "neg"}>
                {s.above_ma200 == null ? "-" : s.above_ma200 ? "上方" : "下方"}
              </td>
              <td>{s.vol_30d != null ? `${fmtNum(s.vol_30d, 1)}%` : "-"}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}
