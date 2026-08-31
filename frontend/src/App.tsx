import { useEffect, useState } from "react";
import { HashRouter, NavLink, Route, Routes } from "react-router-dom";
import { api, getToken } from "./api";
import Market from "./pages/Market";
import StrongCoins from "./pages/StrongCoins";
import Factors from "./pages/Factors";
import SymbolPage from "./pages/Symbol";
import Backtest from "./pages/Backtest";
import Data from "./pages/Data";
import Runs from "./pages/Runs";
import Dashboard from "./pages/Dashboard";
import Settings from "./pages/Settings";

// 左侧边栏布局（参考 QuantConnect / 聚宽等主流量化平台）：
// 侧边栏 = 品牌 + 图标导航 + 底部系统状态；右侧内容区独立滚动。
// HashRouter：纯静态托管无需服务端路由重写，任意部署环境可用。
const NAV: { to: string; label: string; end?: boolean; d: string }[] = [
  { to: "/", label: "市场总览", end: true, d: "M2 12l4-5 3 2 5-6M12 3h2v2" },
  { to: "/strong", label: "强势币种", d: "M8 1l3 6 5 1-3 5 1 6-6-2-6 2 1-6-3-5 5-1z" },
  { to: "/factors", label: "因子选股", d: "M1.5 2.5h13L9.5 8.7V13l-3 1.8V8.7L1.5 2.5z" },
  { to: "/backtest", label: "回测中心", d: "M2 13c2-9 4 4 6-4 1.5-6 3 5 6-3" },
  {
    to: "/data",
    label: "数据中心",
    d: "M3 4c0-1.1 2.2-2 5-2s5 .9 5 2v8c0 1.1-2.2 2-5 2s-5-.9-5-2V4zm0 0c0 1.1 2.2 2 5 2s5-.9 5-2M3 8c0 1.1 2.2 2 5 2s5-.9 5-2",
  },
  { to: "/runs", label: "模拟·实盘", d: "M5 3l8 5-8 5V3z" },
  { to: "/dashboard", label: "仪表板", d: "M3 3h4v4H3zM9 3h4v4H9zM3 9h4v4H3zM9 9h4v4H9z" },
  {
    to: "/settings",
    label: "设置",
    d: "M8 5.5a2.5 2.5 0 110 5 2.5 2.5 0 010-5zM8 1.5v2M8 12.5v2M1.5 8h2M12.5 8h2M3.4 3.4l1.4 1.4M11.2 11.2l1.4 1.4M12.6 3.4l-1.4 1.4M4.8 11.2l-1.4 1.4",
  },
];

export default function App() {
  // 后端健康状态：30s 轮询，显示在侧边栏底部（量化平台的常驻状态条惯例）
  const [online, setOnline] = useState<boolean | null>(null);

  useEffect(() => {
    const check = () =>
      api
        .health()
        .then(() => setOnline(true))
        .catch(() => setOnline(false));
    check();
    const t = setInterval(check, 30_000);
    return () => clearInterval(t);
  }, []);

  return (
    <HashRouter>
      <div className="app">
        <aside className="sidebar">
          <div className="brand">
            Quant<span>Kit</span>
          </div>
          <nav>
            {NAV.map((n) => (
              <NavLink key={n.to} to={n.to} end={n.end}>
                <svg
                  width="16"
                  height="16"
                  viewBox="0 0 16 16"
                  fill="none"
                  stroke="currentColor"
                  strokeWidth="1.3"
                  strokeLinecap="round"
                  strokeLinejoin="round"
                >
                  <path d={n.d} />
                </svg>
                <span>{n.label}</span>
              </NavLink>
            ))}
          </nav>
          <div className="sidebar-foot">
            <span>
              <span className={`dot ${online === true ? "on" : online === false ? "off" : ""}`} />
              {online === null ? "连接后端中…" : online ? "后端在线" : "后端离线"}
            </span>
            <span>管理令牌：{getToken() ? "已配置" : "未配置"}</span>
          </div>
        </aside>
        <div className="main-col">
          <main className="content">
            <Routes>
              <Route path="/" element={<Market />} />
              <Route path="/strong" element={<StrongCoins />} />
              <Route path="/factors" element={<Factors />} />
              <Route path="/symbol/:sym" element={<SymbolPage />} />
              <Route path="/backtest" element={<Backtest />} />
              <Route path="/data" element={<Data />} />
              <Route path="/runs" element={<Runs />} />
              <Route path="/dashboard" element={<Dashboard />} />
              <Route path="/settings" element={<Settings />} />
            </Routes>
          </main>
        </div>
      </div>
    </HashRouter>
  );
}
