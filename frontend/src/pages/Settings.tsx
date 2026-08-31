// 设置：后端地址运行时切换 + 令牌（X-API-Token）管理 + 连接测试

import { useState } from "react";
import { api, getApiBase, getToken, setApiBase, setToken } from "../api";

// 预设后端：本地开发 / 实盘服务器（与服务器 start.sh 的 8080 端口对应）
const PRESETS = [
  { label: "本地", base: "http://127.0.0.1:8080" },
  { label: "实盘服务器", base: "http://43.154.120.27:8080" },
];

export default function Settings() {
  const [baseInput, setBaseInput] = useState(getApiBase());
  const [baseSaved, setBaseSaved] = useState(false);
  const [input, setInput] = useState(getToken());
  const [saved, setSaved] = useState(false);
  const [testResult, setTestResult] = useState<
    { ok: boolean; msg: string } | null
  >(null);
  const [testing, setTesting] = useState(false);

  const save = () => {
    setToken(input.trim());
    setSaved(true);
    setTimeout(() => setSaved(false), 2000);
  };

  const test = async () => {
    setTesting(true);
    setTestResult(null);
    try {
      const h = await api.health();
      const d = await api.dashboard();
      setTestResult({
        ok: true,
        msg: `连接成功：${h.name} v${h.version}，缓存品种 ${d.symbols.length} 个`,
      });
    } catch (e) {
      setTestResult({ ok: false, msg: (e as Error).message });
    } finally {
      setTesting(false);
    }
  };

  const saveBase = () => {
    setApiBase(baseInput);
    setBaseSaved(true);
    setTimeout(() => setBaseSaved(false), 2000);
    // 切换后立即验证新地址可达性（令牌随 localStorage 自动携带）
    void test();
  };

  return (
    <div>
      <h2>设置</h2>

      <div className="card">
        <h2>后端连接</h2>
        <p className="muted" style={{ marginTop: 0 }}>
          当前生效：<code>{getApiBase()}</code>
          。地址仅存于本浏览器，切换后无需重新构建前端；
          本地与服务器同时运行时优先连服务器，避免重复启动本地后端。
        </p>
        <div className="toolbar">
          <input
            style={{ minWidth: 320 }}
            placeholder="http://host:port"
            value={baseInput}
            onChange={(e) => setBaseInput(e.target.value)}
          />
          <button className="primary" onClick={saveBase}>
            保存地址
          </button>
          {baseSaved && <span className="pos">已保存</span>}
          {PRESETS.map((p) => (
            <button
              key={p.base}
              onClick={() => setBaseInput(p.base)}
              style={getApiBase() === p.base ? { borderColor: "#26a69a", color: "#26a69a" } : undefined}
            >
              {p.label}
            </button>
          ))}
          <button onClick={test} disabled={testing}>
            {testing ? "测试中..." : "测试连接"}
          </button>
        </div>
        {testResult && (
          <div className={testResult.ok ? "warn" : "error"} style={testResult.ok ? { color: "#26a69a", borderColor: "rgba(38,166,154,.3)", background: "rgba(38,166,154,.08)" } : undefined}>
            {testResult.msg}
          </div>
        )}
      </div>

      <div className="card">
        <h2>管理令牌（X-API-Token）</h2>
        <p className="muted" style={{ marginTop: 0 }}>
          受保护接口（回测、参数扫描、数据下载、模拟/实盘启停）需要令牌，
          必须与服务端环境变量 QUANTKIT_API_TOKEN 一致。令牌仅保存在本浏览器
          localStorage，不会上传任何第三方。
        </p>
        <div className="toolbar">
          <input
            type="password"
            style={{ minWidth: 320 }}
            placeholder="填写令牌；留空保存即清除"
            value={input}
            onChange={(e) => setInput(e.target.value)}
          />
          <button className="primary" onClick={save}>
            保存
          </button>
          {saved && <span className="pos">已保存</span>}
        </div>
      </div>
    </div>
  );
}
