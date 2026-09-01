# 策略评估脚本（2026-09 实盘篮子决策）

2026-09 三币趋势篮子（BTC+TRX+DOGE）实盘决策所用的评估脚本，从服务器 /tmp 归档（/tmp 重启即丢）。
脚本内容与产出决策数字的版本保持逐字节一致，勿就地修改；如需迭代请另存新版本。

运行环境：HK 服务器（ubuntu@43.154.120.27）。假定 `~/quantkit` 已 `cargo build --release`，
日 K 历史位于 `/home/ubuntu/binance-rust/data/history/`（可用根目录 `scripts/fetch_long_history.py` 更新）。

| 脚本 | 用途 |
|---|---|
| `wf_long.sh` | L1~L7 电池：长历史（2017~）基线、稳健性网格、多组参数族 4 折 walkforward |
| `wf_multi.sh` | 多品种评估：M1 固定参数（ma50/trail0.12）全历史筛查 + M2 逐品种 4 折 walkforward（17 品种） |
| `basket.sh` | M3 等权篮子：各品种用 walkforward 众数参数跑独立资金切片，曲线加总 + 分年度 + 日收益相关性 |
| `bt_summary.py` | 从 stdin 读 `backtest --json`，输出 final_equity/total/annual/maxDD/trades 摘要 |

推荐流程：`wf_multi.sh`（筛查 + 逐品种验收）→ `basket.sh`（组合验证）。
验收门槛（2026-09 预注册）：4 折 walkforward OOS 为正且过拟合差距 < 1。
决策结论记录在项目 memory（quantkit-live-trading-status.md）第 11~12 条。
