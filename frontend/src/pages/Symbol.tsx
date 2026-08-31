// 币种详情：蜡烛图（任意周期）+ 向前翻页 + 加入回测

import { useCallback, useEffect, useState } from "react";
import { useNavigate, useParams } from "react-router-dom";
import { api } from "../api";
import { CandleChart } from "../charts";
import { fmtDate, fmtNum, fmtPct, fmtPrice, fmtVol } from "../fmt";
import { addBtSymbol, getBtSymbols } from "../store";
import type { Kline, Quote } from "../types";

const INTERVALS = ["1h", "4h", "1d", "1w"];

// 日线统计（详情页概览带）：与后端因子口径一致的客户端实现
type DailyStats = {
  high365: number;
  low365: number;
  ddFromHigh: number;
  volAnn: number;
  rsi14: number;
  ma50Dev: number;
  ma200Dev: number;
} | null;

function dailyStats(ks: Kline[]): DailyStats {
  if (ks.length < 210) return null;
  const n = ks.length;
  const win = ks.slice(-Math.min(365, n));
  const high365 = Math.max(...win.map((k) => k.high));
  const low365 = Math.min(...win.map((k) => k.low));
  const close = ks[n - 1].close;
  // 30 日日收益标准差年化（×√365）
  const rets: number[] = [];
  for (let i = n - 30; i < n; i++) rets.push(ks[i].close / ks[i - 1].close - 1);
  const mean = rets.reduce((a, b) => a + b, 0) / rets.length;
  const sd = Math.sqrt(rets.reduce((a, r) => a + (r - mean) ** 2, 0) / rets.length);
  // RSI(14)：近 14 根涨跌幅简单均值口径（与后端一致）
  let gain = 0;
  let loss = 0;
  for (let i = n - 14; i < n; i++) {
    const d = ks[i].close - ks[i - 1].close;
    if (d > 0) gain += d;
    else loss -= d;
  }
  const maDev = (days: number) => {
    const ma = ks.slice(-days).reduce((a, k) => a + k.close, 0) / days;
    return close / ma - 1;
  };
  return {
    high365,
    low365,
    ddFromHigh: high365 > 0 ? Math.max(0, (high365 - close) / high365) : 0,
    volAnn: sd * Math.sqrt(365),
    rsi14: gain + loss <= 1e-12 ? 50 : (100 * gain) / (gain + loss),
    ma50Dev: maDev(50),
    ma200Dev: maDev(200),
  };
}

export default function SymbolPage() {
  const { sym = "" } = useParams();
  const [tf, setTf] = useState("1d");
  const [klines, setKlines] = useState<Kline[]>([]);
  const [total, setTotal] = useState(0);
  const [err, setErr] = useState("");
  const [loading, setLoading] = useState(false);
  const [quote, setQuote] = useState<Quote | null>(null);
  const [stats, setStats] = useState<DailyStats>(null);
  const nav = useNavigate();
  const inBt = getBtSymbols().includes(sym);
  const [added, setAdded] = useState(inBt);

  // 从市场缓存取 24h 行情摘要
  useEffect(() => {
    api
      .markets()
      .then((d) => setQuote(d.quotes.find((x) => x.symbol === sym) ?? null))
      .catch(() => setQuote(null));
  }, [sym]);

  // 日线统计概览：独立拉取 1d 缓存（400 根足够 365 日窗口），不受当前图表周期影响
  useEffect(() => {
    let on = true;
    api
      .klines(sym, "1d", 400)
      .then((d) => on && setStats(dailyStats(d.klines)))
      .catch(() => on && setStats(null));
    return () => {
      on = false;
    };
  }, [sym]);

  const load = useCallback(
    async (endTime?: number, prepend = false) => {
      setLoading(true);
      setErr("");
      try {
        const d = await api.klines(sym, tf, 500, endTime);
        setTotal(d.total);
        setKlines((prev) =>
          prepend ? [...d.klines, ...prev] : d.klines
        );
      } catch (e) {
        setErr((e as Error).message);
      } finally {
        setLoading(false);
      }
    },
    [sym, tf]
  );

  // 切换品种/周期时重新加载
  useEffect(() => {
    setKlines([]);
    load();
  }, [load]);

  const loadEarlier = () => {
    if (klines.length === 0) return;
    load(klines[0].open_time - 1, true);
  };

  const last = klines[klines.length - 1];

  return (
    <div>
      <div className="toolbar">
        <h2 style={{ margin: 0 }}>{sym}</h2>
        {quote && (
          <>
            <span style={{ fontSize: 17, fontWeight: 600 }}>
              {fmtPrice(quote.last_price)}
            </span>
            <span className={quote.price_change_pct >= 0 ? "pos" : "neg"}>
              {fmtPct(quote.price_change_pct)}
            </span>
            <span className="muted">24h 成交额 {fmtVol(quote.quote_volume)}</span>
          </>
        )}
        <span style={{ flex: 1 }} />
        <button
          className="primary"
          disabled={added}
          onClick={() => {
            addBtSymbol(sym);
            setAdded(true);
          }}
        >
          {added ? "已在回测列表" : "+ 加入回测"}
        </button>
        <button onClick={() => nav("/backtest")}>去回测</button>
      </div>

      {stats && (
        <div className="grid" style={{ marginBottom: 16 }}>
          <div className="stat">
            <div className="k">365日最高</div>
            <div className="v">{fmtPrice(stats.high365)}</div>
          </div>
          <div className="stat">
            <div className="k">365日最低</div>
            <div className="v">{fmtPrice(stats.low365)}</div>
          </div>
          <div className="stat">
            <div className="k">距365日高点</div>
            <div className="v neg">{fmtPct(-stats.ddFromHigh * 100)}</div>
          </div>
          <div className="stat">
            <div className="k">30日年化波动</div>
            <div className="v">{fmtPct(stats.volAnn * 100)}</div>
          </div>
          <div className="stat">
            <div className="k">RSI(14)</div>
            <div className="v">{fmtNum(stats.rsi14, 1)}</div>
          </div>
          <div className="stat">
            <div className="k">MA50 偏离</div>
            <div className={`v ${stats.ma50Dev >= 0 ? "pos" : "neg"}`}>
              {fmtPct(stats.ma50Dev * 100)}
            </div>
          </div>
          <div className="stat">
            <div className="k">MA200 偏离</div>
            <div className={`v ${stats.ma200Dev >= 0 ? "pos" : "neg"}`}>
              {fmtPct(stats.ma200Dev * 100)}
            </div>
          </div>
        </div>
      )}

      <div className="card">
        <div className="toolbar">
          {INTERVALS.map((i) => (
            <button
              key={i}
              className={i === tf ? "primary" : ""}
              onClick={() => setTf(i)}
            >
              {i}
            </button>
          ))}
          <span style={{ flex: 1 }} />
          {total > 0 && (
            <span className="muted">
              服务端缓存 {total} 根{last ? `，最新 ${fmtDate(last.open_time)}` : ""}
            </span>
          )}
          <button onClick={loadEarlier} disabled={loading}>
            {loading ? "加载中..." : "加载更早"}
          </button>
        </div>
        {err && <div className="error">{err}</div>}
        <CandleChart klines={klines} height={460} />
      </div>
    </div>
  );
}
