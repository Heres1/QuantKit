# QuantKit 部署指南

本文档是唯一的部署/运维参考，涵盖架构、服务器部署、HTTPS、实盘启停与故障排查。

## 1. 架构总览

```
本地                          服务器 43.154.120.27 (ubuntu)
┌─────────────────┐          ┌──────────────────────────┐
│ frontend/       │  HTTPS   │ quantkit-web (端口 8080)  │
│ React+Vite      │ ───────► │ Rust axum REST API       │
│ 端口 5173       │  Nginx   │  ├─ 回测/数据/因子        │
└─────────────────┘  443     │  └─ 实盘引擎 (live)       │
                             └──────────────────────────┘
```

- **唯一前端**：`frontend/`（React + Vite + TypeScript）
- **唯一后端**：`web/`（quantkit-web，axum REST API），依赖 5 个库 crate：core、exchanges、strategies、app、examples
- 实盘必须运行在服务器上（本地网络无法访问 Binance API）

## 2. 服务器信息

| 项目 | 值 |
|---|---|
| 主机 | `ubuntu@43.154.120.27`（注意用户名是 `ubuntu`，不是 root） |
| 代码目录 | `~/quantkit` |
| 后端端口 | 8080 |
| 日志 | `/tmp/quantkit-web.log`（nohup 输出） |
| 状态文件 | `~/quantkit/live_quantkit_state.json` |
| API 令牌 | 存放于 `~/quantkit/.env.server`（`export QUANTKIT_API_TOKEN=...`） |

## 3. 日常部署（推荐方式）

```bash
./scripts/deploy.sh           # 同步代码 + 编译 + 重启后端
./scripts/deploy.sh --live    # 同上，并启动实盘
./scripts/check-live.sh       # 检查实盘状态
```

`deploy.sh` 优先走 git（`server` 远程），否则用 rsync 同步；
然后在服务器上 `cargo build --release` 并重启 quantkit-web。
部署与实盘启动分离：普通代码推送不会自动重启实盘，只有 `--live` 或手动调用 API 才会启动。

**远程仓库结构（两个远程）：**

| 远程 | 地址 | 用途 |
|---|---|---|
| `origin` | `git@github.com:Heres1/QuantKit.git` | GitHub 私有仓库，代码备份与协作 |
| `server` | `ubuntu@43.154.120.27:repos/quantkit.git` | 部署用裸仓库，push 后经 post-receive 钩子自动同步 `~/quantkit` |

日常流程：本地提交后 `git push`（GitHub 备份）+ `./scripts/deploy.sh`（部署到服务器）。

## 4. 手动操作参考

### 启动/停止后端

```bash
ssh ubuntu@43.154.120.27 'cd ~/quantkit && . ./.env.server && \
  pkill -f quantkit-web; sleep 2; \
  nohup ./target/release/quantkit-web --port 8080 > /tmp/quantkit-web.log 2>&1 &'
```

### 启动/停止实盘（需令牌）

```bash
# 启动
curl -X POST -H "X-API-Token: $TOKEN" http://43.154.120.27:8080/api/runs/live/start
# 停止
curl -X POST -H "X-API-Token: $TOKEN" http://43.154.120.27:8080/api/runs/live/stop
# 查看日志
curl -H "X-API-Token: $TOKEN" http://43.154.120.27:8080/api/runs/live/logs
```

未设置 `QUANTKIT_API_TOKEN` 时，所有受保护接口（回测/下载/启停）一律拒绝（fail-closed），公开行情接口仍可用。

### 前端本地运行

```bash
cd frontend && npm install && npm run dev   # http://localhost:5173
```

在「设置」页配置后端地址与 API Token，连接测试通过后即可使用实盘监控等页面。

## 5. HTTPS（Nginx 反代）

服务器已配置 Nginx 自签名证书：

- `https://43.154.120.27`（443）→ 反代到 `localhost:8080`；HTTP 80 自动跳转 HTTPS
- 证书位置：`/etc/nginx/ssl/quantkit.crt` / `quantkit.key`（自签名 365 天，浏览器需手动接受警告）
- 生产建议：购买域名后用 Let's Encrypt（certbot）替换自签名证书，或叠加 Cloudflare

## 6. 故障排查

| 现象 | 原因 | 处理 |
|---|---|---|
| 前端"实盘已停止"、无日志 | 状态文件不存在或服务未启动 | `check-live.sh` 查进程，再按第 4 节启动 |
| 401 / Token 验证失败 | 请求令牌与服务器环境变量不一致 | 确认 `.env.server` 中 `QUANTKIT_API_TOKEN` 与前端设置页一致 |
| `❌ 自检-无法连接交易所` | 本地网络无法访问 Binance | 实盘必须在服务器运行 |
| 总资产/今日盈亏显示异常 | 实盘未启动时接口返回错误，前端显示占位值 | 先启动实盘，接口返回真实数据后即正常 |
| SSH 登录失败 | 用错了用户名 | 用户名是 `ubuntu` 不是 `root` |
