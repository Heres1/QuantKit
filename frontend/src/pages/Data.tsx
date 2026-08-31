// 数据中心：服务端已缓存品种清单 + 批量下载（需 Token）+ 下载进度与日志 + 清单筛选浏览

import { useCallback, useEffect, useMemo, useState } from "react";
import { useNavigate } from "react-router-dom";
import { api, getToken } from "../api";
import { fmtDateTime } from "../fmt";
import type { DataFile, DownloadTask } from "../types";

export default function Data() {
  const [files, setFiles] = useState<DataFile[]>([]);
  const [download, setDownload] = useState<DownloadTask | null>(null);
  const [logs, setLogs] = useState<string[]>([]);
  const [input, setInput] = useState("");
  const [maxBars, setMaxBars] = useState(1500);
  const [dlInterval, setDlInterval] = useState("1d");
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState("");
  const [refreshKey, setRefreshKey] = useState(0);
  // 清单浏览：周期筛选 + 品种搜索（日线是回测/因子的主数据，默认只看 1d）
  const [intervalFilter, setIntervalFilter] = useState("1d");
  const [fileQ, setFileQ] = useState("");
  const nav = useNavigate();

  const refresh = () => setRefreshKey((k) => k + 1);

  // 清单 + 下载任务状态
  const load = useCallback(async () => {
    try {
      const d = await api.data();
      setFiles(d.market);
      setDownload(d.download);
      setErr("");
    } catch (e) {
      setErr((e as Error).message);
    }
  }, []);

  useEffect(() => {
    load();
  }, [load, refreshKey]);

  // 下载进行中：轮询进度；同时拉取下载日志（[下载] 前缀）
  const running = download !== null;
  useEffect(() => {
    if (!running) return;
    const t = setInterval(() => {
      load();
      api
        .runLogs("all")
        .then((d) => setLogs(d.lines.filter((l) => l.includes("[下载]"))))
        .catch(() => {});
    }, 2000);
    return () => clearInterval(t);
  }, [running, load]);

  const submit = async () => {
    const symbols = input
      .split(/[\s,]+/)
      .map((s) => s.trim().toUpperCase())
      .filter(Boolean);
    if (symbols.length === 0) return;
    setBusy(true);
    setErr("");
    try {
      await api.download(symbols.join(","), maxBars, dlInterval);
      setInput("");
      await load();
    } catch (e) {
      setErr((e as Error).message);
    } finally {
      setBusy(false);
    }
  };

  // 进度百分比
  let pct = 0;
  if (download && download.symbols.length > 0) {
    pct = Math.round((download.done.length / download.symbols.length) * 100);
  }

  // 清单筛选与概览统计
  const intervals = useMemo(() => [...new Set(files.map((f) => f.interval))].sort(), [files]);
  const shown = useMemo(() => {
    let list = files;
    if (intervalFilter) list = list.filter((f) => f.interval === intervalFilter);
    const kw = fileQ.trim().toUpperCase();
    if (kw) list = list.filter((f) => f.symbol.includes(kw));
    return list;
  }, [files, intervalFilter, fileQ]);
  const daily = files.filter((f) => f.interval === "1d");
  const totalBars = shown.reduce((s, f) => s + f.bars, 0);

  return (
    <div>
      <h2>数据中心</h2>

      <div className="card">
        <h2>批量下载K线数据</h2>
        {!getToken() && (
          <div className="warn">
            下载接口受保护，请先在「设置」页填写 X-API-Token
          </div>
        )}
        <div className="toolbar">
          <input
            style={{ minWidth: 320 }}
            placeholder="品种，逗号或空格分隔，如 BTCUSDT,ETHUSDT"
            value={input}
            onChange={(e) => setInput(e.target.value)}
          />
          <label className="field">
            周期
            <select value={dlInterval} onChange={(e) => setDlInterval(e.target.value)}>
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
            最大根数
            <input
              type="number"
              value={maxBars}
              onChange={(e) => setMaxBars(Number(e.target.value))}
            />
          </label>
          <button
            className="primary"
            disabled={busy || running || !getToken() || !input.trim()}
            onClick={submit}
          >
            {busy ? "提交中..." : running ? "下载进行中" : "开始下载"}
          </button>
          <span style={{ flex: 1 }} />
          <button onClick={refresh}>刷新清单</button>
        </div>
        <div className="muted">
          回测按周期读取对应数据文件，用 4h/1h 等周期回测前需先在此下载该周期；
          日线是因子与市场环境分析的主数据
        </div>
        {err && <div className="error">{err}</div>}
        {download && (
          <div className="toolbar">
            <span className="badge on">进行中</span>
            <span>
              {download.done.length}/{download.symbols.length}
              {download.running ? `（当前 ${download.running}）` : ""} · {pct}%
            </span>
            <span className="muted">
              开始于 {fmtDateTime(download.started_at_ms)}
            </span>
          </div>
        )}
        {logs.length > 0 && (
          <div className="logs">
            {logs.slice(-50).join("\n")}
          </div>
        )}
      </div>

      <div className="card">
        <h2>服务端已缓存品种（{shown.length}）</h2>
        <div className="grid" style={{ marginBottom: 12 }}>
          <div className="stat">
            <div className="k">日线品种</div>
            <div className="v">{daily.length}</div>
          </div>
          <div className="stat">
            <div className="k">当前筛选K线总根数</div>
            <div className="v">{totalBars.toLocaleString()}</div>
          </div>
          <div className="stat">
            <div className="k">覆盖周期</div>
            <div className="v">{intervals.join(", ") || "-"}</div>
          </div>
        </div>
        <div className="toolbar">
          {intervals.map((iv) => (
            <button
              key={iv}
              className={intervalFilter === iv ? "primary" : ""}
              onClick={() => setIntervalFilter(iv)}
            >
              {iv}
            </button>
          ))}
          <button className={intervalFilter === "" ? "primary" : ""} onClick={() => setIntervalFilter("")}>
            全部
          </button>
          <input
            placeholder="搜索品种"
            value={fileQ}
            onChange={(e) => setFileQ(e.target.value)}
            style={{ width: 200 }}
          />
          <span className="muted">点击行可进入币种详情（K线/日线统计）</span>
        </div>
        {files.length === 0 && !err && (
          <div className="muted">暂无缓存，请先下载</div>
        )}
        <div className="table-wrap">
          <table>
            <thead>
              <tr>
                <th>品种</th>
                <th>周期</th>
                <th>根数</th>
                <th>起始日期</th>
                <th>最新日期</th>
              </tr>
            </thead>
            <tbody>
              {shown.map((f) => (
                <tr
                  key={`${f.symbol}_${f.interval}`}
                  className="clickable"
                  onClick={() => nav(`/symbol/${f.symbol}`)}
                >
                  <td>{f.symbol}</td>
                  <td>{f.interval}</td>
                  <td>{f.bars}</td>
                  <td>{f.first}</td>
                  <td>{f.last}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      </div>
    </div>
  );
}
