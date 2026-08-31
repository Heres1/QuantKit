# quantkit

Rust 通用交易框架：对标 [freqtrade](https://www.freqtrade.io/) 的五个核心抽象——
**策略可插拔、交易所抽象、统一回测/实盘引擎、dry-run 模拟、参数寻优**——从零独立实现。

现货、单交易所（Binance）、多周期（5m / 15m / 30m / 1h / 4h / 12h / 1d / 1w）。每一行代码都能讲清楚。
前后端分离的 Web 平台：`quantkit-web` 纯 REST API + `frontend/`（React + Vite + TS）——
市场总览（全币种实时行情）/ 蜡烛图 / 回测中心 / 数据中心 / 模拟·实盘监控。

策略库五个，覆盖从「不看盘」到「主动轮动」：
**定投（dca）** / **智能网格（grid）** / 双均线（ma_cross）/ 趋势追踪（trend）/ 动量轮动（momentum）。

## 架构

```
                    ┌────────────────────────────────────────────┐
  K线（历史/实时）──▶│ Engine（唯一主循环：回测/模拟/实盘同一份代码） │
                    │   每根bar收盘 → strategy.on_bars(ctx, bars) │
                    │   订单 ──────────────▶ OrderExecutor（可注入）│
                    │   成交 ◀────────────── Fill                │
                    │   记账 ──▶ Portfolio（现金/持仓/手续费/净值）  │
                    │   指标 ──▶ Metrics（年化/回撤/夏普/胜率/    │
                    │      PF/盈亏比/Calmar/暴露率，均净利润口径） │
                    └────────────────────────────────────────────┘
                                    ▲
        ┌───────────────────────────┼───────────────────────────┐
  BacktestExecutor             PaperExecutor                LiveExecutor
  下一根bar开盘撮合            真实行情价即时模拟            Binance 真实下单
  （无未来函数）               （状态原子持久化）            （签名/限频/精度）
```

关键点：**策略只返回订单，不接触执行器**。订单一律经引擎执行，
所以换执行方式（回测→模拟→实盘）不需要改一行策略代码。

## 项目结构

| crate | 职责 |
|---|---|
| `core` | 零外部依赖内核：类型、周期抽象（Interval）、Strategy/OrderExecutor trait、引擎、Portfolio 记账、指标 |
| `exchanges` | 交易所抽象（MarketData + Broker trait）+ 从零实现的 Binance REST 客户端 |
| `strategies` | 五个策略：定投（可逢跌加码）、智能网格（区间自适应+单边下跌止损）、双均线、趋势+追踪止损、动量轮动（Top-N 分散/市场状态过滤/逐品种追踪止损） |
| `app` | CLI：backtest / sweep / walkforward / dry-run / live / serve；`optimize` 模块承载寻优内核（网格解析、并行评估、滚动折窗划分，均有单测） |
| `web` | Web API 服务：axum 纯 REST（行情代理 / K线缓存 / 回测与进程编排），前后端分离 |
| `frontend` | React + Vite + TS 前端（纯静态产物，任意静态托管部署） |
| `examples` | 最小可运行示例（内联策略 + 合成数据） |

## 五分钟上手

### 1. 构建与测试

```bash
cargo build --release
cargo test          # 全部单测
cargo clippy --all-targets
```

### 2. 跑第一个回测

历史数据为 JSON K线（与 binance-rust 相同格式），放在 `data_dir` 指向的目录，
每个品种每周期一个文件（如 `BTCUSDT_1d.json`、`BTCUSDT_4h.json`）。
默认路径 `../binance-rust/data/history`。

未指定 `--symbols` 时自动发现数据目录下的品种，并**剔除稳定币兑 USDT 的交易对**
（USDCUSDT / FDUSDUSDT / DAIUSDT 等）：它们价格恒为 1，不可能有动量，
还会在市场状态过滤的「广度」统计里充当噪声分母，让熊市判断失真。

```bash
# 未指定 --symbols 时自动发现数据目录下的全部品种
cargo run -p quantkit-app -- backtest

# 显式指定策略/品种/参数
cargo run -p quantkit-app -- backtest \
    --strategy momentum --symbols BTCUSDT,ETHUSDT,SOLUSDT \
    --momentum-days 90 --rebalance-days 30

# 指定回测窗口（YYYY-MM-DD，UTC）
cargo run -p quantkit-app -- backtest --start 2024-01-01 --end 2025-01-01

# 基准对比：基准品种买入持有曲线 + 超额/β/相关系数/信息比率
cargo run -p quantkit-app -- backtest --benchmark BTCUSDT

# 逐笔回合明细
cargo run -p quantkit-app -- backtest --verbose

# 风控与分散（二期）：动量前2等额持仓 + 市场状态过滤 + 组合回撤熔断
cargo run -p quantkit-app -- backtest --strategy momentum \
    --symbols BTCUSDT,ETHUSDT,SOLUSDT \
    --top-n 2 --regime-ma 120 --regime-breadth 0.5 \
    --circuit-breaker 0.3 --circuit-cooldown 30

# 双均线模板策略（手写策略参考）：快线上穿慢线买入、下穿卖出
cargo run -p quantkit-app -- backtest --strategy ma_cross \
    --symbols BTCUSDT,ETHUSDT --ma-fast 10 --ma-slow 30

# 定投（dca）：每 7 天买 100 USDT；收盘价低于 200 日均线时加倍买入
cargo run -p quantkit-app -- backtest --strategy dca --symbols BTCUSDT \
    --dca-amount 100 --dca-interval-days 7 --dca-ma-days 200 --dca-dip-multiplier 2

# 智能网格（grid）：近 60 天高低点自动定区间、10 格低买高卖，跌破下界 15% 清仓止损
cargo run -p quantkit-app -- backtest --strategy grid --symbols BTCUSDT \
    --grid-levels 10 --grid-lookback-days 60 --grid-stop-loss 0.15
```

新策略参数语义：
- **dca 定投**：`--dca-amount` 每期每品种买入金额、`--dca-interval-days` 定投间隔（7 = 周投）；
  `--dca-ma-days` 趋势均线（0 = 关闭加码）、`--dca-dip-multiplier` 收盘价低于该均线时的加码倍数。
  定投只买不卖——收益来自长期持有，卖点由你自己决定（或叠加引擎层回撤熔断）
- **grid 智能网格**：`--grid-levels` 格数、`--grid-lookback-days` 用最近多少天的最高/最低价定区间
  （区间宽度不足 1% 时不布网，避免只付手续费）、`--grid-budget` 每品种预算（0 = 按品种数均分现金）；
  `--grid-stop-loss` 是网格唯一的致命失效模式（单边下跌一路买入）的兜底：
  跌破区间下界该比例即清仓并停止本轮，价格回到区间上方才以新区间重开。
  **向上突破会重建区间，向下跌破绝不重建**——否则下界跟着新低一路下移，止损线永远追不上价格

### 多周期（--interval）

```bash
# 同一策略换周期：只改 --interval，参数不用重算
cargo run -p quantkit-app -- backtest --strategy grid --symbols BTCUSDT --interval 4h
```

可选 `5m / 15m / 30m / 1h / 4h / 12h / 1d`（默认）`/ 1w`，对应数据文件 `{SYMBOL}_{interval}.json`。
换周期时三类参数的处理方式不同（这是「参数不用重算」的原因）：

| 参数类别 | 例子 | 换周期时 |
|---|---|---|
| 以「天」表达的回看窗口 | `--momentum-days` `--ma-days` `--regime-ma` `--grid-lookback-days` `--dca-ma-days` | **按周期自动换算成根数**：90 日动量在 4h 上是 540 根，仍是 90 天的动量，只是评估更频繁 |
| 以「天」表达的时间间隔 | `--rebalance-days` `--cooldown-days` `--dca-interval-days` `--circuit-cooldown` | 本身就是墙钟时间，不受周期影响 |
| 均线「周期数」 | `--ma-fast` `--ma-slow` | 按图表惯例即为「根」（4h 上的 MA10 = 10 根 4h） |

指标年化随周期自适应：夏普/Sortino/波动率的年化因子由权益曲线的实际时间间隔推断
（日线得 √365，4h 得 √2190），不再假定「一天一根」——短周期回测的夏普不会被系统性高估。

### 滚动前进验证（walkforward）——判断参数是不是只在拟合历史

`sweep --split` 只切一次，选出的参数可能只是在那一段行情里运气好。
`walkforward` 把历史切成 N 折，每折「用训练窗选参 → 在紧随其后、从未参与选参的测试窗验收」，
反复检验「按这套流程定期重新调参」的真实表现。

```bash
# 5 折滚动、每折训练窗占 60%，按年化选参
cargo run -p quantkit-app -- walkforward --strategy momentum \
    --windows 5 --train-ratio 0.6 --grid "momentum_days=30,60,90;top_n=1,2,3"

# 扩张窗（训练起点固定、历史越用越多）+ 按夏普选参（比按年化选参更抗过拟合）
cargo run -p quantkit-app -- walkforward --anchored --rank sharpe \
    --windows 8 --grid "momentum_days=30,60,90;trailing_stop=0,0.08,0.12"
```

- **滚动窗**（默认）：训练窗定长向前滑动，只用最近一段历史，适合行情结构会变的市场
- **扩张窗**（`--anchored`）：训练起点固定，历史越用越多，适合认为规律长期稳定的场景
- `--rank annualized|sharpe|calmar|sortino`：训练窗上按哪个指标选最优。
  按年化选参本身就是过拟合陷阱（最高收益往往来自极端参数），换成风险调整后指标通常更稳

输出关键字段怎么读：

| 字段 | 含义 |
|---|---|
| 逐折的「训练指标 / 测试指标」 | 同一组参数在选参窗与样本外窗的表现对照 |
| 样本外总收益 | 各折测试窗收益按顺序复利——等价于「每到窗口边界就重新调参、资金连续滚动」的真实过程。**这是唯一值得看的收益数字** |
| 测试窗胜率 | 多少折的样本外为正；只有 1~2 折为正说明结论主要靠运气 |
| **过拟合差距** | 训练指标均值 − 测试指标均值。为正属正常（选参必然占训练窗便宜）；**差距越大，参数越可能只是拟合了历史，实盘越可能失效** |

本机实测（12 个品种日线，动量轮动，5 折 / 训练占比 0.6 / 网格 `momentum_days=30,60,90` × `top_n=1,2,3`）：

```
训练年化均值 46.7%  →  样本外均值 -37.5%
样本外总收益 -42.9%（覆盖 450 天） | 测试窗胜率 1/5 折为正 | 过拟合差距 +84pp
```

这个结论本身就是 walkforward 的价值：**该参数网格在这段历史上是过拟合的，不该直接拿去实盘。**
只看 `sweep` 的训练集排名会得到「年化 129%」的最优参数，而它在样本外是亏的。

实现细节（影响结论正确性）：测试窗会自动往前多取该组参数所需的热身历史，
引擎用 `signal_start_ts` 区分两段——热身段只喂历史给策略推进内部状态、不交易、不计入权益曲线，
因此指标只反映测试窗内、且以完整初始资金起步的表现。
少了这一步，90 日动量这类长回看策略在 90 天的测试窗里热身都来不及完成，
会一笔不交易、样本外收益被错报为 0（开发过程中确实先踩了这个坑）。

风控参数语义：
- `--top-n N`：持仓品种数，动量前 N 等额分散（1 = 老版集中轮动，默认）
- `--regime-ma N`：市场状态过滤均线（0 = 禁用）；池内站上该均线的品种占比
  低于 `--regime-breadth`（默认 0.5）时判为熊市——清仓且不开新仓
- `--circuit-breaker P`（引擎层）：组合权益自峰值回撤 ≥ P 即全清仓，
  `--circuit-cooldown D` 天冷却期内禁止一切买入（0 = 禁用，默认）

### 3. 参数寻优（通用网格 + 样本外验收）

```bash
# 默认网格（动量天数 × 调仓间隔，兼容旧版 9 组）
cargo run -p quantkit-app -- sweep

# 任意维度网格：支持 momentum_days/ma_days/rebalance_days/trailing_stop/cooldown_days
#   以及 top_n/regime_ma/regime_breadth/circuit_breaker（二期风控维度）
cargo run -p quantkit-app -- sweep \
    --grid "momentum_days=30,60,90;trailing_stop=0,0.08,0.12"

# 训练/测试切分：前 70% 选参、后 30% 验收，防过拟合（按训练年化排序）
cargo run -p quantkit-app -- sweep --split 0.7

# JSONL 结构化输出（每行一个参数组合，WebUI 使用）
cargo run -p quantkit-app -- sweep --split 0.7 --json
```

指标均为净利润口径：总/年化收益、最大回撤、夏普、**Sortino**、**年化波动率**、
**最长回撤持续天数**、胜率、profit factor、盈亏比、Calmar、暴露率（已平仓回合持仓时间占比）。

参数寻优为多线程并行（线程数取 CPU 核数，共享游标动态领取任务，避免被最慢的核拖住）：
本机 8 线程实测 729 组合 × 13 品种日线 **2.46s → 0.56s（4.4×）**。
输出按组合索引归位后再排名，与串行版本逐行一致——同样输入永远得到同样结果。

### 4. 最小示例（无需数据文件）

```bash
cargo run -p quantkit-examples --bin quickstart
```

## 撮合口径

- `fill_mode = next_open`（默认）：第 t 根收盘决策，第 t+1 根开盘成交——杜绝未来函数
- `fill_mode = same_close`：当根收盘价成交（对标旧回测系统的口径）
- 手续费与滑点由 `FillModel` 统一建模；**所有收益指标一律按净利润（已扣往返手续费）**
- 手续费用**真实费率**，不虚构：默认 `fee_rate = 0.00075` = Binance 现货 VIP0 taker 0.1%
  启用 BNB 抵扣后的费率（未开抵扣设 0.001）；live 启动时自动拉取账户实际费率（含折扣）覆盖配置
- 年化按 365 天（加密全年无休），夏普/Sortino/波动率的年化周期数由权益曲线实际间隔推断
  （日线 = 365，4h = 2190），且从首次动用资金起算（裁掉前置空仓平坦段）

## 自定义策略指南（手写策略三步注册）

策略 = 实现 `Strategy` trait 的一个结构体，只负责返回订单；撮合/持仓/风控由引擎统一处理。
模板参考 [`strategies/src/ma_cross.rs`](strategies/src/ma_cross.rs)（双均线金叉，全文带注释）：

1. 在 `strategies/src/` 新建文件，实现 `Strategy`（`name()` + `on_bars()`），照抄模板结构；
2. 在 `strategies/src/lib.rs` 添加 `pub mod 文件名;`；
3. 在 `app/src/lib.rs` 的 `build_strategy` 加一个匹配分支，参数加到 `app/src/config.rs`
   （`#[serde(default)]` 字段）与 `apply_cli_overrides`（CLI 参数）。
   之后 `backtest / sweep / dry-run / live / WebUI` 全通道自动可用。

关键约定（避免未来函数）：
- `on_bars` 收到已收盘当根 bar，订单在**下一根开盘**撮合（默认 `next_open`）；
- 信号只能用 `ctx.history(symbol, n)` 与 `bars` 里的已收盘数据；
- 只返回订单，不直接改持仓/现金；持仓查询用 `ctx.position(sym)` / `ctx.positions()`；
- 结构体私有字段即跨 bar 状态，重启后由历史重放自动重建，无需手动持久化。
- 模板策略自带单测（`cargo test -p quantkit-strategies`）：建议每个新策略都写一组构造行情验证信号行为。

## 配置（quantkit.toml，可选，所有字段有默认值，CLI 参数优先）

```toml
data_dir = "../binance-rust/data/history"
initial_cash = 10000.0
fee_rate = 0.00075          # 单边手续费率（真实费率：0.1% 开 BNB 抵扣；未开抵扣设 0.001）
slippage_pct = 0.0
fill_mode = "next_open"     # next_open | same_close
interval = "1d"             # K线周期：5m|15m|30m|1h|4h|12h|1d|1w（--interval），决定数据文件 {SYMBOL}_{interval}.json
symbols = ["BTCUSDT", "ETHUSDT"]
strategy = "momentum"       # momentum | trend | ma_cross | grid | dca
ma_cross_fast = 10           # ma_cross 模板策略：快线周期（--ma-fast）
ma_cross_slow = 30           # ma_cross 模板策略：慢线周期（--ma-slow）
grid_levels = 10                  # grid 网格格数（--grid-levels）
grid_lookback_days = 30           # grid 区间回看天数（--grid-lookback-days）
grid_stop_loss_pct = 0.15         # grid 跌破区间下界该比例即清仓止损（--grid-stop-loss，0=关闭）
grid_budget_per_symbol = 0.0      # grid 每品种预算（--grid-budget，0=按品种数均分现金）
dca_amount = 100.0                # dca 每期每品种买入金额（--dca-amount）
dca_interval_days = 7             # dca 定投间隔天数（--dca-interval-days，7=周投）
dca_ma_days = 200                 # dca 智能加码趋势均线天数（--dca-ma-days，0=关闭加码）
dca_dip_multiplier = 2.0          # dca 低于趋势均线时的加码倍数（--dca-dip-multiplier）
momentum_days = 90
ma_days = 50
rebalance_days = 30
trailing_stop_enabled = true
trailing_stop_pct = 0.12
cooldown_days = 30
top_n = 1                    # 持仓品种数（1=集中轮动；>1=动量前N等额分散）
regime_ma_days = 0           # 市场状态过滤均线天数（0=禁用）
regime_min_breadth = 0.5     # 熊市广度阈值（池内站上均线品种占比低于此值判熊）
circuit_breaker_pct = 0.0    # 组合回撤熔断阈值（<=0 禁用）
circuit_breaker_cooldown_days = 30  # 熔断后冷却天数（期内禁买）
state_file = "quantkit_state.json"   # dry-run 状态快照
poll_secs = 60
live_enabled = false        # live 总开关：默认关闭
live_auto_heal = false      # live 对账自愈：漂移时以交易所真实余额重写本地持仓（默认只告警）
telegram_bot_token = ""     # Telegram Bot 令牌（实盘关键事件推送；空 = 不启用）
telegram_chat_id = ""       # Telegram chat_id（与令牌同时配置才启用）
web_port = 8080             # WebUI 监听端口（quantkit serve）
```

## 实盘部署（live）

实盘与回测/模拟共用同一套策略与决策代码，区别仅在执行层（真实下单）。
启动顺序：**自检（时钟/余额/精度/真实费率）→ 启动对账 → 二次确认（手动输入 YES）→ 收盘信号驱动主循环**。

1. 配置文件开启：`live_enabled = true`；如希望启动对账发现漂移时自动以交易所真实余额重写本地持仓，设 `live_auto_heal = true`（默认只告警不动状态，人工核对）
2. 密钥只从环境变量读取：`export BINANCE_API_KEY=... BINANCE_API_SECRET=...`（权限只需现货交易 + 账户读取，建议不开提币）
3. 启动（交互确认）：
   ```bash
   ./target/release/quantkit live
   ```
   无人值守部署：`nohup ./target/release/quantkit live > logs/live.log 2>&1 < yes.conf &`
   （`yes.conf` 内容为单独一行 `YES`；务必先在模拟盘验证策略后再启用）
4. 安全机制：
   - 幂等下单：每笔订单携带确定性 clientOrderId（bar+品种+方向），网络超时后凭 ID 查回真实成交，绝不盲目重发；同 bar 已处理过则不重复决策，重启不重复下单
   - 启动对账：本地状态持仓与交易所实际可用余额逐项比对（数量偏差/缺失/未记录持仓），漂移即告警；`live_auto_heal` 开启时自愈，交易所存在而状态未记录的仓位只告警不自动接管（避免误动人工仓位）
   - 卖出钳制：卖出数量不超过交易所真实可用余额，状态漂移也不会超卖报错；余额查询失败退回本地数量并告警，不阻断调仓
   - 限频与重连：全局请求限速（≤10 req/s）+ 网络错误/5xx 指数退避重试；行情主域不可达自动切官方镜像并粘性保持（详见上节）
   - 状态原子持久化：`live_{state_file}` 与 dry-run 隔离，成交逐笔落盘可审计；状态文件损坏会明确要求人工核对而非静默继续
5. 日志完整可追溯：每笔真实成交打印品种/数量/价格/手续费，累计成交数与持仓每轮输出；也可经 WebUI「模拟·实盘」页启停并实时查看日志。
6. 告警通知：配置 `telegram_bot_token` + `telegram_chat_id` 后，实盘启动/每笔真实成交（含卖出）/下单失败/对账漂移/整轮失败都会推送 Telegram；未配置时不影响任何交易逻辑。通知为旁路（5s 超时），失败只告警不阻断。
7. 实盘监控（WebUI「模拟·实盘」页）：实时持仓盯市（最新价/市值/浮动盈亏）、总资产权益曲线（实盘进程每轮写入快照）、今日盈亏、成交流水（含信号原因）；数据读自 `live_{state_file}`，实盘进程不在线也可查看。对应接口 `/api/live/positions|equity|fills`（`X-API-Token` 保护，含资金信息）。
8. 服务器部署与代码同步：`scripts/deploy.sh` 一键部署（同步+编译+重启，`--live` 附带启动实盘），`scripts/check-live.sh` 检查状态；详见 [docs/DEPLOYMENT.md](docs/DEPLOYMENT.md)。

## Web 平台（前后端分离）


```
frontend/（React+Vite+TS，构建为纯静态文件，任意静态托管）
   │  HTTP(S)，API 地址由 VITE_API_BASE 配置
   ▼
quantkit-web（纯 REST API，0.0.0.0:8080）
   ├── 公开层：全币种行情 / K线 / 仪表板只读状态（CORS 全开）
   ├── 保护层（X-API-Token）：回测 / 扫描 / 数据下载 / 模拟盘·实盘启停
   ├── Binance 公共行情代理 + 服务端 K线缓存（data_dir/{SYMBOL}_{interval}.json）
   └── 编排 quantkit CLI 子进程（实盘逻辑零改动）
```

### 后端

```bash
export QUANTKIT_API_TOKEN=<你的令牌>     # 受保护接口必需；未设置时保护接口一律拒绝（fail-closed）
./start.sh                               # 编译 + nohup 启动 quantkit-web（PID 文件 + logs/）
./stop.sh                                # 停止
```

环境变量：`QUANTKIT_PORT`（默认 8080）、`QUANTKIT_DATA_DIR`（默认 ./data）、`QUANTKIT_CONFIG`。

| 路由 | 说明 | 鉴权 |
|---|---|---|
| `GET /api/markets` | 全市场 USDT 现货 24h 行情（30s 缓存） | 公开 |
| `GET /api/klines/{symbol}?interval=&limit=&end_time=` | 任意周期 K线（服务端缓存 + 增量补齐 + 向前翻页） | 公开 |
| `GET /api/dashboard` `/api/health` `/api/data` `/api/runs/*/logs` | 只读状态 | 公开 |
| `GET /api/backtests` `/api/backtests/:id` | 回测记录列表（摘要）/详情 | 公开 |
| `GET /api/factors` | 多因子截面快照（本地日K计算：动量/波动/趋势/RSI/量能/回撤/资金流 + 综合打分排名） | 公开 |
| `GET /api/correlation?days=60` | 品种两两日收益率相关矩阵（近 N 交易日，默认 60） | 公开 |
| `GET /api/factor-ic?factor=&horizon=20` | 因子 IC 回测验证（因子截面值与未来 N 日收益的滚动相关：IC均值/ICIR/胜率/序列） | 公开 |
| `POST /api/backtest` `/api/sweep` `/api/walkforward` `/api/download` `/api/runs/*/start\|stop` | 计算重/写操作（回测成功自动存档；`/api/walkforward` 返回逐折结果 + 样本外汇总；`/api/download` 支持 `interval` 字段按周期下载，多周期回测前需先下载对应周期数据） | `X-API-Token` |
| `GET /api/live/positions` `/api/live/equity` `/api/live/fills` | 实盘监控：持仓盯市（最新价/市值/浮盈）/ 权益曲线+今日盈亏 / 成交流水（含信号原因） | `X-API-Token` |
| `DELETE /api/backtests/:id` | 删除回测记录 | `X-API-Token` |

架构（控制面）：web 进程**不内嵌交易循环**，而是编排现有 `quantkit` CLI 子进程
（回测 `--json` 结构化输出；模拟/实盘常驻子进程 + 日志捕获 + 停止控制）。
好处：实盘逻辑零改动、进程隔离。环境变量：`QUANTKIT_WEB_BIN` 指定 CLI 二进制路径
（默认取同目录 `quantkit`）。
行情网络兜底：主域不可达时公共行情自动切官方镜像（data-api.binance.vision）并粘性保持；
显式设置 `BINANCE_BASE_URL` 或 `BINANCE_NO_FALLBACK=1` 可关闭。

### 前端

```bash
cd frontend
npm install
VITE_API_BASE=http://127.0.0.1:8080 npm run dev   # 开发调试（默认即本机 8080）
npm run build                                     # 产出 dist/ 纯静态文件，任意静态托管可部署
```

| 页面 | 能力 |
|---|---|
| 市场总览 | 全币种表格（最新价/24h涨跌幅/24h高低点/成交额），搜索 + 排序，30s 自动刷新 |
| 因子选股 | 多因子截面打分（动量/趋势/资金流/低波动/量能/低回撤加权，z 标准化）：排序/搜索、Top-N 圈选、一键发送回测；因子 IC 回测验证（IC均值/ICIR/胜率 + 滚动 IC 曲线，可选因子与前瞻天数）；相关性矩阵热力图（30/60/120/250 日窗口） |
| 币种详情 | lightweight-charts 蜡烛图，周期切换（1h/4h/1d/1w）、向前翻页；日线统计概览带（365日高低点/距高点回撤/30日年化波动/RSI14/MA50、MA200偏离）；一键加入回测 |
| 回测中心 | 五个策略（定投/智能网格/双均线/趋势/动量）+ K线周期选择（5m~1w）；**参数表单按所选策略动态显示**，每个参数带一句投资者语言的说明，不堆砌无关输入；币种多选、回测窗口（--start/--end）、基准对比（--benchmark：超额/β/相关/IR）、权益曲线（基准叠加）、月度收益与品种归因、逐笔回合；指标含 Sortino/年化波动率/最长回撤持续天数；参数扫描支持自定义网格与训练/测试切分（样本外验收，高亮最优）；**滚动前进验证面板**（折数/训练占比/滚动或扩张窗/选参指标，输出逐折表格与样本外汇总含过拟合差距，每个控件都配一句投资者语言的解释）；回测记录自动存档（列表/查看/删除/多记录归一化曲线对比） |
| 数据中心 | 服务端缓存清单（周期筛选/搜索/概览统计/点击进详情）；按周期批量下载历史K线（进度轮询 + 下载日志） |
| 模拟盘/实盘 | 一键启停、实时日志轮询、实盘门禁状态展示；实盘监控面板：总资产/今日盈亏/浮动盈亏/累计成交概览卡片、实盘权益曲线、持仓盯市表（最新价/市值/盈亏幅度）、成交流水（方向/价格/手续费/信号原因），10s 自动刷新 |
| 仪表板 | 策略/费率/资金/运行状态总览 + 模拟盘快照 |
| 设置 | API 连接测试；管理令牌（X-API-Token）存 localStorage |

部署说明：`frontend/dist` 可放任意静态托管（nginx / Vercel / GitHub Pages）；
构建时用 `VITE_API_BASE` 注入后端公网地址（如 `https://api.example.com`）。

## dry-run（真实行情 + 模拟成交）


```bash
cargo run -p quantkit-app -- dry-run
```

- 每轮拉取**全量历史**已收盘K线（周期由 `interval` 决定，向前分页，上限 2000 根），
  **全新策略实例完整重放**
  得出目标持仓——确定性设计：调仓相位锚定历史起点，等价于一直运行的进程；
  重启后状态由行情自动重建，不丢、不错
- 与本地快照做差量：新回合即"本轮模拟成交"，打日志并原子持久化（临时文件 + rename）

## live（实盘通道，默认禁用）

四道门禁，缺一不启动：

1. 配置文件中显式 `live_enabled = true`
2. 环境变量 `BINANCE_API_KEY` / `BINANCE_API_SECRET`（**只从环境变量读取，不进配置与 Git**）
3. 启动自检：时钟偏移（>2s 拒绝）、USDT 余额、各品种 stepSize/minQty
4. 二次确认：打印风险提示后必须手动输入 `YES`

每轮重放得出目标持仓（只取品种），买入量按真实可用余额 × 0.999 / 现价，
再按 stepSize 取整；状态写 `live_{state_file}`，与 dry-run 隔离；同 bar 不重复下单。
手续费用账户**真实费率**（启动自检时拉取）。每笔订单带确定性 `clientOrderId`
（bar 时间戳+品种+方向）：网络超时后凭此 ID 幂等查询恢复真实成交结果，
上层重试复用同一 ID，杜绝重复下单。

> 网络受限环境可用 `BINANCE_BASE_URL` 覆盖默认域名
> （如公共行情镜像 `https://data-api.binance.vision`，仅支持公共接口）。

## 与 freqtrade 的概念对照

| freqtrade | quantkit | 说明 |
|---|---|---|
| `IStrategy` | `Strategy` trait | `on_bars` 返回 `Vec<Order>`，不接触执行 |
| `Exchange` | `MarketData` + `Broker` trait | 行情与交易分离 |
| 统一引擎 | `run_backtest` + 可注入 `OrderExecutor` | 回测/模拟/实盘同一份引擎代码 |
| dry-run 模式 | `dry-run` 子命令 + `PaperExecutor` 思路 | 真实行情、模拟成交、状态持久化 |
| `hyperopt` | `sweep` + `walkforward` 子命令 | 任意维度参数网格 + 单次切分，以及多折滚动前进验证（无贝叶斯调参） |

明确不做：多交易所、合约、做市、机器学习调参。

## 测试要点

- 撮合时序无未来函数（第 t 根决策 / 第 t+1 根成交）
- 手续费净利润口径、追踪止损边界值
- HMAC 签名确定性（含官方测试向量）、限频窗口、原子状态写入
- 指标计算（profit factor/盈亏比/Calmar/暴露率/Sortino/最长回撤持续天数）
- 周期抽象：「天→根」换算在日线下必须恒等（保证老参数语义不漂移）、
  夏普年化因子随曲线间隔自适应（日线仍为 √365，4h 为 √365×6）
- 滚动折窗划分：训练窗与测试窗严格不重叠、各折测试窗首尾相接铺满尾部区间、
  末折右端对齐数据末尾、单折等价于 `sweep --split`、非法入参（0 折/占比越界/空跨度）报错
- 寻优内核：每个声明支持的扫描维度都必须真的改到配置（防「声明了但空转」）、
  并行评估结果按输入顺序归位（可复现）、日期格式化往返（含世纪闰年）
- 热身段必须真的调用策略以推进其内部 bar 计数，否则依赖内部计数的策略
  会在测试窗内一笔不交易、样本外收益被错报为 0（已有回归测试钉住）
- 定投：按期买入的期数与份额、低于趋势均线时加码买到更多份额、现金耗尽后不再下单且现金不为负
- 网格：格位边界与越界钳制、震荡行情产生完整回合、**单边下跌必须触发止损清仓**
  （回归此前的设计缺陷：跌破下界重建区间会让止损线跟着新低下移，永远无法触发）
- 回测窗口切分（--start/--end 日期解析、闰年、剔除空窗品种）、暴露率区间合并
- 交叉验证：动量轮动 9 组参数网格与旧系统基线全对齐
- 引擎优化与并行寻优不改变任何结果：729 组合的 JSONL 输出哈希在优化前后逐字节一致
