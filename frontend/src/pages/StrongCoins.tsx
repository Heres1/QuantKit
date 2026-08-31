// 实时强势货币排行榜：综合评分 = 涨跌幅动量(30%) + 成交额流动性(25%) + 价格位置(20%) + 波动优化(15%) + 绝对价格(10%)

import { useCallback, useEffect, useState } from "react";
import { useNavigate } from "react-router-dom";
import { api } from "../api";
import { fmtNum, fmtVol } from "../fmt";
import type { StrongCoin } from "../types";

const fmtPrice = (p: number) => {
  if (p >= 1000) return p.toFixed(2);
  if (p >= 1) return p.toFixed(4);
  return p.toFixed(6);
};

const rankChangeIcon = (change: number | null) => {
  if (change == null) return <span className="muted">-</span>;
  if (change > 0) return <span className="pos" title={`上升${change}位`}>↑+{change}</span>;
  if (change < 0) return <span className="neg" title={`下降${Math.abs(change)}位`}>↓{change}</span>;
  return <span className="muted">→</span>;
};

const scoreColor = (score: number) => {
  if (score >= 65) return "pos";
  if (score >= 55) return "";
  return "neg";
};

const changeColor = (change: number) => {
  if (change > 5) return "pos";
  if (change < -5) return "neg";
  return change > 0 ? "pos" : change < 0 ? "neg" : "";
};

export default function StrongCoins() {
  const [coins, setCoins] = useState<StrongCoin[]>([]);
  const [total, setTotal] = useState(0);
  const [updatedAt, setUpdatedAt] = useState<number>(0);
  const [cacheHit, setCacheHit] = useState(false);
  const [err, setErr] = useState("");
  const [busy, setBusy] = useState(false);
  const nav = useNavigate();

  const load = useCallback(async () => {
    setBusy(true);
    setErr("");
    try {
      const data = await api.strongCoins();
      setCoins(data.coins);
      setTotal(data.total);
      setUpdatedAt(data.updated_at);
      setCacheHit(data.cache_hit);
    } catch (e) {
      setErr((e as Error).message);
    } finally {
      setBusy(false);
    }
  }, []);

  useEffect(() => {
    load();
    const t = setInterval(load, 30_000); // 30秒自动刷新
    return () => clearInterval(t);
  }, [load]);

  if (err) {
    return (
      <div className="card">
        <h2 style={{ margin: 0 }}>实时强势货币排行榜</h2>
        <div className="muted">{err}</div>
        <button onClick={load} style={{ marginTop: 12 }}>重试</button>
      </div>
    );
  }

  const updateTime = updatedAt ? new Date(updatedAt).toLocaleTimeString("zh-CN") : "-";

  return (
    <div className="card table-wrap">
      <div className="toolbar">
        <h2 style={{ margin: 0 }}>实时强势货币排行榜（Top 50）</h2>
        <div>
          <span className="muted">
            共 {total} 个交易对 · 更新于 {updateTime}
            {cacheHit && " (缓存)"}
          </span>
          <button onClick={load} disabled={busy} style={{ marginLeft: 12 }}>
            {busy ? "刷新中..." : "立即刷新"}
          </button>
        </div>
      </div>
      <p className="muted" style={{ marginBottom: 12 }}>
        综合评分 = 涨跌幅动量(30%) + 成交额流动性(25%) + 价格位置(20%) + 波动优化(15%) + 绝对价格(10%)
      </p>
      <table>
        <thead>
          <tr>
            <th>排名</th>
            <th>变化</th>
            <th>币种</th>
            <th>综合得分</th>
            <th>24h涨跌</th>
            <th>成交额</th>
            <th>最新价</th>
            <th>价格位置</th>
            <th>波动调整</th>
          </tr>
        </thead>
        <tbody>
          {coins.map((c) => {
            const medal = c.rank === 1 ? "🥇" : c.rank === 2 ? "🥈" : c.rank === 3 ? "🥉" : "";
            return (
              <tr key={c.symbol} className="clickable" onClick={() => nav(`/symbol/${c.symbol}`)}>
                <td>
                  {medal}
                  <b>{c.rank}</b>
                </td>
                <td>{rankChangeIcon(c.rank_change)}</td>
                <td>
                  <b>{c.symbol}</b>
                </td>
                <td className={scoreColor(c.score)}>
                  <b>{fmtNum(c.score, 1)}</b>
                </td>
                <td className={changeColor(c.price_change_pct)}>
                  {c.price_change_pct > 0 ? "+" : ""}
                  {fmtNum(c.price_change_pct, 2)}%
                </td>
                <td>{fmtVol(c.quote_volume)}</td>
                <td>{fmtPrice(c.last_price)}</td>
                <td title="接近24h高点说明强势">
                  {c.position_score != null ? fmtNum(c.position_score, 1) : "-"}/20
                </td>
                <td title="适度波动最佳">
                  {c.vol_adjust != null ? fmtNum(c.vol_adjust, 1) : "-"}/15
                </td>
              </tr>
            );
          })}
        </tbody>
      </table>
      {coins.length === 0 && !busy && <div className="muted">无数据</div>}
    </div>
  );
}
