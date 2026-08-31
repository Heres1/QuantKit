// 仪表板：策略/费率/资金/运行状态总览 + 模拟盘快照

import { useCallback, useEffect, useState } from "react";
import { api } from "../api";
import { fmtDate, fmtDateTime, fmtNum, fmtPrice } from "../fmt";
import type { Dashboard as DashboardData } from "../types";

export default function Dashboard() {
  const [dash, setDash] = useState<DashboardData | null>(null);
  const [err, setErr] = useState("");

  const load = useCallback(async () => {
    try {
      setDash(await api.dashboard());
      setErr("");
    } catch (e) {
      setErr((e as Error).message);
    }
  }, []);

  useEffect(() => {
    load();
    const t = setInterval(load, 10000);
    return () => clearInterval(t);
  }, [load]);

  const dry = dash?.dry_state;
  const dryRunning = dash?.runs.dryrun.running ?? false;
  const liveRunning = dash?.runs.live.running ?? false;

  return (
    <div>
      <h2>仪表板</h2>
      {err && <div className="error">{err}</div>}
      {!dash && !err && <div className="muted">加载中...</div>}
      {dash && (
        <>
          <div className="card">
            <h2>系统配置</h2>
            <div className="grid">
              <div className="stat">
                <div className="k">版本</div>
                <div className="v">{dash.version}</div>
              </div>
              <div className="stat">
                <div className="k">代码版本</div>
                <div className="v">
                  <code>{dash.git_commit ?? "unknown"}</code>
                </div>
              </div>
              <div className="stat">
                <div className="k">策略</div>
                <div className="v">{dash.strategy}</div>
              </div>
              <div className="stat">
                <div className="k">单边费率</div>
                <div className="v">{fmtNum(dash.fee_rate * 100, 4)}%</div>
              </div>
              <div className="stat">
                <div className="k">数据目录</div>
                <div className="v" style={{ fontSize: 12, wordBreak: "break-all" }}>
                  {dash.data_dir}
                </div>
              </div>
              <div className="stat">
                <div className="k">缓存品种</div>
                <div className="v">{dash.symbols.length}</div>
              </div>
              <div className="stat">
                <div className="k">实盘门禁</div>
                <div className="v">
                  {dash.live_enabled && dash.api_keys_set ? (
                    <span className="badge on">已开启</span>
                  ) : (
                    <span className="badge off">关闭</span>
                  )}
                </div>
              </div>
            </div>
          </div>

          <div className="card">
            <h2>运行状态</h2>
            <div className="grid">
              <div className="stat">
                <div className="k">模拟盘</div>
                <div className="v">
                  <span className={`badge ${dryRunning ? "on" : "off"}`}>
                    {dryRunning ? "运行中" : "已停止"}
                  </span>
                </div>
              </div>
              <div className="stat">
                <div className="k">实盘</div>
                <div className="v">
                  <span className={`badge ${liveRunning ? "on" : "off"}`}>
                    {liveRunning ? "运行中" : "已停止"}
                  </span>
                </div>
              </div>
            </div>
          </div>

          {dry && (
            <div className="card">
              <h2>模拟盘快照</h2>
              <div className="grid">
                <div className="stat">
                  <div className="k">总权益（净值口径）</div>
                  <div className="v">{fmtNum(dry.equity ?? 0)}</div>
                </div>
                <div className="stat">
                  <div className="k">现金</div>
                  <div className="v">{fmtNum(dry.cash ?? 0)}</div>
                </div>
                <div className="stat">
                  <div className="k">已平仓回合</div>
                  <div className="v">{dry.trades?.length ?? 0}</div>
                </div>
                <div className="stat">
                  <div className="k">最后处理K线</div>
                  <div className="v">{fmtDate(dry.last_bar_ts ?? 0)}</div>
                </div>
                <div className="stat">
                  <div className="k">快照时间</div>
                  <div className="v" style={{ fontSize: 13 }}>
                    {fmtDateTime(dry.updated_at_ms ?? 0)}
                  </div>
                </div>
              </div>
              {dry.positions && dry.positions.length > 0 && (
                <>
                  <h2 style={{ marginTop: 16 }}>当前持仓</h2>
                  <div className="table-wrap">
                    <table>
                      <thead>
                        <tr>
                          <th>品种</th>
                          <th>数量</th>
                          <th>平均入场价</th>
                        </tr>
                      </thead>
                      <tbody>
                        {dry.positions.map((p) => (
                          <tr key={p.symbol}>
                            <td>{p.symbol}</td>
                            <td>{fmtNum(p.quantity, 6)}</td>
                            <td>{fmtPrice(p.avg_entry_price)}</td>
                          </tr>
                        ))}
                      </tbody>
                    </table>
                  </div>
                </>
              )}
              {(!dry.positions || dry.positions.length === 0) && (
                <div className="muted" style={{ marginTop: 12 }}>
                  当前空仓
                </div>
              )}
            </div>
          )}
        </>
      )}
    </div>
  );
}
