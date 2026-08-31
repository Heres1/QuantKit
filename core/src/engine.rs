//! 统一回测引擎。
//!
//! 撮合时序（杜绝未来函数）：
//! 第 t 根K线收盘 -> 喂给策略 -> 产出订单
//! 第 t+1 根K线开盘 -> 按开盘价撮合 -> 成交入账
//!
//! 同一引擎换注入的执行方式即可用于模拟/实盘（Phase 3+）。

use std::collections::BTreeMap;

use crate::executor::{ExecutionAccount, FillModel, FillModelSyncExecutor};
use crate::metrics::{compute_metrics, BacktestMetrics, EquityPoint};
use crate::portfolio::Portfolio;
use crate::strategy::{Strategy, StrategyContext};
use crate::types::{Kline, Order, Position, Side};

/// 订单撮合价格假设。
///
/// 信号统一在第 t 根K线收盘后产生，区别只在成交价：
/// - [`FillPrice::NextBarOpen`]（默认）：第 t+1 根开盘价。贴近实盘
///   “收盘确认信号、次根下单”的保守假设，严格无未来函数
/// - [`FillPrice::SameBarClose`]：第 t 根收盘价。贴近“收盘时刻市价下单”，
///   信号只使用本根及更早数据，同样无未来函数；用于对标老回测系统口径
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FillPrice {
    #[default]
    NextBarOpen,
    SameBarClose,
}

/// 回测配置
#[derive(Debug, Clone)]
pub struct BacktestConfig {
    /// 初始资金（计价资产）
    pub initial_cash: f64,
    /// 成交模型（滑点 + 手续费）
    pub model: FillModel,
    /// 策略可见的最大历史K线数
    pub max_history: usize,
    /// 撮合价格假设（默认下一根开盘价）
    pub fill_price: FillPrice,
    /// 组合回撤熔断：权益自峰值回撤 >= 该值时全清仓并进入冷却（<=0 禁用）
    pub circuit_breaker_pct: f64,
    /// 熔断后冷却时长（毫秒）：冷却期内拦截一切买单；结束后峰值重置为当前权益
    pub circuit_breaker_cooldown_ms: u64,
    /// 信号起始时间戳：早于该时刻的K线只用于喂养策略历史窗口（指标热身），
    /// 不下单、不记账、不计入权益曲线；到达后策略以完整初始资金正常开始交易。
    ///
    /// 0（默认）= 从第一根K线就开始交易。
    /// 滚动前进验证的测试窗必须用它：否则策略在测试窗内热身都来不及完成，
    /// 90 日动量这类策略会一笔不交易，样本外收益被系统性低估为 0。
    pub signal_start_ts: u64,
}

impl Default for BacktestConfig {
    fn default() -> Self {
        Self {
            initial_cash: 10_000.0,
            model: FillModel::new(0.0, 0.001),
            max_history: 400,
            fill_price: FillPrice::default(),
            circuit_breaker_pct: 0.0,
            circuit_breaker_cooldown_ms: 0,
            signal_start_ts: 0,
        }
    }
}

/// 回测结果
#[derive(Debug)]
pub struct BacktestResult {
    pub strategy_name: String,
    pub metrics: BacktestMetrics,
    pub equity_curve: Vec<EquityPoint>,
    /// 已平仓回合明细（入场均价 -> 出场价，净盈亏已扣手续费）
    pub trades: Vec<crate::types::Trade>,
    /// 期末现金（含已扣手续费）
    pub final_cash: f64,
    /// 期末未平仓持仓
    pub final_positions: Vec<Position>,
    /// 回测结束时仍在队列、无后续bar可撮合而作废的订单数。
    ///
    /// 这是每次回测都可能出现的正常收尾情况（策略在最后一根bar发出的信号没有
    /// 下一根可成交），因此由调用方决定是否展示——引擎无条件打印会让 sweep /
    /// walkforward 刷出成百上千行无用告警。
    pub dropped_tail_orders: usize,
}

/// 引擎错误
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("K线数据为空")]
    EmptyData,
}

/// 运行回测。
///
/// data: symbol -> 按时间升序的K线。品种之间按 open_time 对齐；
/// 某时间戳缺失的品种跳过（不发信号、不撮合）。
pub fn run_backtest(
    strategy: &mut dyn Strategy,
    data: &BTreeMap<String, Vec<Kline>>,
    config: &BacktestConfig,
) -> Result<BacktestResult, EngineError> {
    // 借用原始K线切片 + 每品种游标：输入已按时间升序，无需复制一份索引。
    // 参数寻优会在同一份数据上跑成百上千次回测，这里省下的拷贝与分配是主要热点。
    let series: Vec<(&String, &[Kline])> =
        data.iter().map(|(sym, ks)| (sym, ks.as_slice())).collect();

    // 时间轴并集：Vec 排序去重比逐个插入 BTreeSet 少掉每元素一次节点分配
    let mut timeline: Vec<u64> = data
        .values()
        .flat_map(|ks| ks.iter().map(|k| k.open_time))
        .collect();
    timeline.sort_unstable();
    timeline.dedup();
    if timeline.is_empty() {
        return Err(EngineError::EmptyData);
    }

    let mut ctx = StrategyContext::new(config.max_history);
    let mut portfolio = Portfolio::new(config.initial_cash);
    let mut executor = FillModelSyncExecutor::new(config.model.clone());
    let mut pending: Vec<Order> = Vec::new();
    let mut curve: Vec<EquityPoint> = Vec::new();
    // 组合回撤熔断状态：权益峰值与冷却截止时间（0 = 非冷却中）
    let mut peak_equity = config.initial_cash;
    let mut cooldown_until: u64 = 0;
    // 每品种在自己K线序列中的推进位置
    let mut cursors = vec![0usize; series.len()];
    // 本时间戳收盘的 (品种, K线) 借用；每轮复用同一块内存
    let mut current: Vec<(&String, &Kline)> = Vec::with_capacity(series.len());

    for ts in timeline {
        // 推进各品种游标到当前时间戳，收集本时间戳收盘的K线（借用，不拷贝）
        current.clear();
        for (i, (sym, ks)) in series.iter().enumerate() {
            while cursors[i] < ks.len() && ks[cursors[i]].open_time < ts {
                cursors[i] += 1;
            }
            if let Some(k) = ks.get(cursors[i]).filter(|k| k.open_time == ts) {
                current.push((sym, k));
            }
        }

        // 1) 热身阶段：让策略照常"运行"以推进其内部跨 bar 状态，但丢弃订单、
        // 不撮合、不记账、不记录权益。
        //
        // 必须真的调用 on_bars：策略的跨 bar 状态（动量轮动的 bars_seen 计数、
        // 调仓计时锚点、追踪止损峰值）都在 on_bars 内部推进。只往 ctx 里塞历史
        // 而不调用策略，策略会永远认为自己还在热身期，整个测试窗一笔不交易。
        if ts < config.signal_start_ts {
            for (sym, k) in &current {
                ctx.push_bar(sym, (*k).clone());
            }
            if !current.is_empty() {
                let bars: BTreeMap<String, Kline> = current
                    .iter()
                    .map(|(sym, k)| ((*sym).clone(), (*k).clone()))
                    .collect();
                ctx.sync_account(portfolio.positions.clone(), portfolio.cash);
                // 订单丢弃：热身期不产生任何成交，账户到信号起始时仍是满额空仓
                let _ = strategy.on_bars(&ctx, &bars);
            }
            continue;
        }

        // 2) 上一根bar产生的订单，在本bar开盘价撮合（NextBarOpen 模式）
        if config.fill_price == FillPrice::NextBarOpen {
            execute_pending_orders(ts, &current, &mut executor, &mut portfolio, &mut pending, |k| k.open);
        }

        // 3) 本时间戳无任何品种收盘：不决策、不记录
        if current.is_empty() {
            continue;
        }

        // 4) 喂给策略的 bars 与历史窗口（trait 约定为持有型，此处必须拷贝）
        let mut bars: BTreeMap<String, Kline> = BTreeMap::new();
        for (sym, k) in &current {
            bars.insert((*sym).clone(), (*k).clone());
            ctx.push_bar(sym, (*k).clone());
        }

        // 5) 同步账户快照 -> 策略决策 -> 订单进入待撮合队列；
        // 熔断冷却期内拦截一切买单（风控优先于策略信号）
        ctx.sync_account(portfolio.positions.clone(), portfolio.cash);
        let mut orders = strategy.on_bars(&ctx, &bars);
        if cooldown_until > 0 && ts < cooldown_until {
            orders.retain(|o| !matches!(o.side, Side::Buy));
        }
        pending.extend(dedupe_orders(orders));

        // SameBarClose 模式：信号以本根收盘为据，立即按本根收盘价撮合
        if config.fill_price == FillPrice::SameBarClose {
            execute_pending_orders(ts, &current, &mut executor, &mut portfolio, &mut pending, |k| k.close);
        }

        // 6) 记录本bar收盘权益，随后做组合回撤熔断检查（只用已收盘数据，无未来函数）
        let prices: BTreeMap<String, f64> =
            bars.iter().map(|(s, k)| (s.clone(), k.close)).collect();
        let equity = portfolio.equity(&prices);
        curve.push(EquityPoint {
            timestamp: ts,
            equity,
        });

        // 7) 熔断：权益自峰值回撤超限 -> 注入全持仓卖单 + 冷却禁买。
        // 卖单随下一撮合点执行（NextBarOpen 为次根开盘）；
        // 冷却结束后峰值重置为当时权益，避免旧峰值导致反复触发。
        if config.circuit_breaker_pct > 0.0 {
            if cooldown_until > 0 && ts >= cooldown_until {
                peak_equity = equity;
                cooldown_until = 0;
            }
            peak_equity = peak_equity.max(equity);
            if cooldown_until == 0 && !portfolio.positions.is_empty() && peak_equity > 0.0 {
                let dd = (peak_equity - equity) / peak_equity;
                if dd >= config.circuit_breaker_pct {
                    pending.retain(|o| !matches!(o.side, Side::Buy));
                    for (sym, pos) in &portfolio.positions {
                        pending.push(Order::market_sell(sym.clone(), pos.quantity));
                    }
                    cooldown_until = ts + config.circuit_breaker_cooldown_ms;
                    log_skip(
                        "*",
                        ts,
                        &format!(
                            "组合回撤熔断: 回撤 {:.1}% >= {:.1}%，全清仓并冷却 {} 天",
                            dd * 100.0,
                            config.circuit_breaker_pct * 100.0,
                            config.circuit_breaker_cooldown_ms / 86_400_000,
                        ),
                    );
                }
            }
        }
    }

    let dropped_tail_orders = pending.len();

    let metrics = compute_metrics(
        config.initial_cash,
        &curve,
        &portfolio.trades,
        portfolio.total_fees,
    );
    Ok(BacktestResult {
        strategy_name: strategy.name().to_string(),
        metrics,
        equity_curve: curve,
        trades: portfolio.trades.clone(),
        final_cash: portfolio.cash,
        final_positions: portfolio.positions.values().cloned().collect(),
        dropped_tail_orders,
    })
}

/// 撮合待执行订单队列。
///
/// 卖单先于买单执行：同bar换仓时，卖出回笼的现金才能用于买入。
/// `price_of` 决定撮合价取本根开盘还是收盘（见 [`FillPrice`]）。
fn execute_pending_orders(
    ts: u64,
    current: &[(&String, &Kline)],
    executor: &mut FillModelSyncExecutor,
    portfolio: &mut Portfolio,
    pending: &mut Vec<Order>,
    price_of: fn(&Kline) -> f64,
) {
    if pending.is_empty() {
        return;
    }
    let mut orders = std::mem::take(pending);
    orders.sort_by_key(|o| o.side == Side::Buy);
    for order in orders {
        // 品种数量在个位到几十之间，线性查找快于维护一份索引结构
        let price = match current.iter().find(|(sym, _)| **sym == order.symbol) {
            Some((_, k)) => price_of(k),
            None => {
                log_skip(&order.symbol, ts, "本时间戳无K线，订单作废");
                continue;
            }
        };
        let account = ExecutionAccount {
            cash: portfolio.cash,
            assets: portfolio.assets_snapshot(),
            timestamp: ts,
            allow_partial: true,
        };
        match executor.execute_sync(&order, price, &account) {
            Ok(fill) => {
                portfolio.apply_fill(&fill);
            }
            Err(e) => {
                log_skip(&order.symbol, ts, &format!("订单被拒: {e}"));
            }
        }
    }
}

/// 同一品种同方向只保留最后一笔订单（避免同bar重复信号叠加）
fn dedupe_orders(orders: Vec<Order>) -> Vec<Order> {
    let mut map: BTreeMap<(String, bool), Order> = BTreeMap::new();
    for o in orders {
        let is_buy = matches!(o.side, Side::Buy);
        map.insert((o.symbol.clone(), is_buy), o);
    }
    map.into_values().collect()
}

fn log_skip(symbol: &str, ts: u64, reason: &str) {
    // 走 stderr：不污染 stdout 的指标输出，同时订单作废/被拒可追溯
    eprintln!("[engine] 跳过: {symbol}@{ts}: {reason}");
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: u64 = 86_400_000;

    fn kline(open_time: u64, open: f64, close: f64) -> Kline {
        Kline {
            open_time,
            open,
            high: open.max(close),
            low: open.min(close),
            close,
            volume: 1.0,
            close_time: open_time + 1000,
        }
    }

    /// 假策略：收盘 > 105 市价全仓买入；持仓且收盘 < 95 全部卖出。
    struct ThresholdStrategy;

    impl Strategy for ThresholdStrategy {
        fn name(&self) -> &str {
            "threshold-test"
        }
        fn on_bars(
            &mut self,
            ctx: &StrategyContext,
            bars: &BTreeMap<String, Kline>,
        ) -> Vec<Order> {
            let k = match bars.get("TEST") {
                Some(k) => k,
                None => return vec![],
            };
            let held = ctx.position("TEST").map(|p| p.quantity).unwrap_or(0.0);
            if held == 0.0 && k.close > 105.0 {
                let qty = (ctx.cash() * 0.99) / k.close;
                return vec![Order::market_buy("TEST", qty)];
            }
            if held > 0.0 && k.close < 95.0 {
                return vec![Order::market_sell("TEST", held)];
            }
            vec![]
        }
    }

    #[test]
    fn test_fill_timing_uses_next_bar_open() {
        // bar1 收盘 110 触发买入；必须在 bar2 开盘价 100 成交，而不是 110。
        let mut data = BTreeMap::new();
        data.insert(
            "TEST".to_string(),
            vec![
                kline(1, 100.0, 110.0),
                kline(2, 100.0, 90.0),
                kline(3, 90.0, 90.0),
            ],
        );
        let config = BacktestConfig {
            initial_cash: 10_000.0,
            model: FillModel::new(0.0, 0.0),
            max_history: 50,
        fill_price: FillPrice::default(),
            ..Default::default()
        };
        let mut s = ThresholdStrategy;
        let result = run_backtest(&mut s, &data, &config).unwrap();
        // 买入 9900/110 = 90 份 @100，花费 9000，现金 1000
        // bar3 收盘 90：权益 = 1000 + 90*90 = 9100
        let last = result.equity_curve.last().unwrap();
        assert!((last.equity - 9100.0).abs() < 1e-6, "equity={}", last.equity);
    }

    #[test]
    fn test_same_bar_close_fill_mode() {
        // SameBarClose：bar1 收盘 110 触发买入，必须按 110（本根收盘）成交
        let mut data = BTreeMap::new();
        data.insert(
            "TEST".to_string(),
            vec![kline(1, 100.0, 110.0), kline(2, 100.0, 90.0), kline(3, 90.0, 90.0)],
        );
        let config = BacktestConfig {
            initial_cash: 10_000.0,
            model: FillModel::new(0.0, 0.0),
            max_history: 50,
            fill_price: FillPrice::SameBarClose,
            ..Default::default()
        };
        let mut s = ThresholdStrategy;
        let result = run_backtest(&mut s, &data, &config).unwrap();
        // 买入 9900/110 = 90 份 @110，剩余现金 100；bar3 收盘 90：权益 = 90*90 + 100 = 8200
        let last = result.equity_curve.last().unwrap();
        assert!((last.equity - 8200.0).abs() < 1e-6, "equity={}", last.equity);
    }

    #[test]
    fn test_fee_reduces_net_profit() {
        // 0.1% 双边手续费：买卖价格相同也应亏损（净手续费）
        let mut data = BTreeMap::new();
        data.insert(
            "TEST".to_string(),
            vec![
                kline(1, 100.0, 110.0), // 收盘 110 触发买入
                kline(2, 100.0, 90.0), // 开盘 100 买入成交；收盘 90 触发卖出
                kline(3, 100.0, 100.0), // 开盘 100 卖出成交（买卖同价，毛利为 0）
                kline(4, 100.0, 100.0),
            ],
        );
        let config = BacktestConfig {
            initial_cash: 10_000.0,
            model: FillModel::new(0.0, 0.001),
            max_history: 50,
        fill_price: FillPrice::default(),
            ..Default::default()
        };
        let mut s = ThresholdStrategy;
        let result = run_backtest(&mut s, &data, &config).unwrap();
        // 买卖均价相同，毛利为 0，净利润应为负（手续费）
        assert!(result.metrics.total_fees > 0.0);
        let last = result.equity_curve.last().unwrap();
        assert!(
            last.equity < 10_000.0,
            "净利润口径下应亏损于手续费: {}",
            last.equity
        );
        assert_eq!(result.metrics.num_round_trips, 1);
        assert!(result.metrics.win_rate_pct < 1.0);
    }

    /// 同一根bar同时卖出A、买入B：验证卖单先执行，买单能用卖出回笼的现金
    struct SellBuySameBar;

    impl Strategy for SellBuySameBar {
        fn name(&self) -> &str {
            "sellbuy-same-bar"
        }
        fn on_bars(
            &mut self,
            ctx: &StrategyContext,
            _bars: &BTreeMap<String, Kline>,
        ) -> Vec<Order> {
            let n = ctx.history("A", 10).len();
            if n == 1 {
                // 第一根：全仓买 A
                return vec![Order::market_buy("A", ctx.cash() * 0.99 / 100.0)];
            }
            if n == 2 {
                // 第二根：卖 A 同时买 B（若买单先执行会因现金为0被削减）
                let mut v = Vec::new();
                if let Some(p) = ctx.position("A") {
                    v.push(Order::market_sell("A", p.quantity));
                }
                v.push(Order::market_buy("B", 50.0));
                return v;
            }
            vec![]
        }
    }

    #[test]
    fn test_sell_before_buy_in_same_bar() {
        let mut data = BTreeMap::new();
        data.insert(
            "A".to_string(),
            vec![kline(DAY, 100.0, 100.0), kline(2 * DAY, 100.0, 100.0), kline(3 * DAY, 100.0, 100.0)],
        );
        data.insert(
            "B".to_string(),
            vec![kline(DAY, 10.0, 10.0), kline(2 * DAY, 10.0, 10.0), kline(3 * DAY, 10.0, 10.0)],
        );
        let config = BacktestConfig {
            initial_cash: 10_000.0,
            model: FillModel::new(0.0, 0.0),
            max_history: 50,
        fill_price: FillPrice::default(),
            ..Default::default()
        };
        let mut s = SellBuySameBar;
        let result = run_backtest(&mut s, &data, &config).unwrap();
        // 第3根开盘：卖A(99份@100)回笼9900 -> 买B 50份@10=500
        // 最终持有 B 50 份；若买单先执行，B 会被削减为 0
        let last = result.equity_curve.last().unwrap();
        // 权益 = 现金(约9400) + 50*10 = 约 9900
        assert!(last.equity > 9_800.0, "equity={}", last.equity);
        assert!(result.metrics.num_round_trips == 1);
    }

    #[test]
    fn test_sell_clamped_to_position() {
        // 卖出超持仓量时应被削减，而不是报错或产生负持仓
        let mut executor = FillModelSyncExecutor::new(FillModel::new(0.0, 0.0));
        let mut assets = BTreeMap::new();
        assets.insert("TEST".to_string(), 5.0);
        let account = ExecutionAccount {
            cash: 0.0,
            assets,
            timestamp: 1,
            allow_partial: true,
        };
        let fill = executor
            .execute_sync(&Order::market_sell("TEST", 100.0), 10.0, &account)
            .unwrap();
        assert_eq!(fill.quantity, 5.0);
    }

    #[test]
    fn test_buy_clamped_to_cash() {
        let mut executor = FillModelSyncExecutor::new(FillModel::new(0.0, 0.001));
        let account = ExecutionAccount {
            cash: 100.0,
            assets: BTreeMap::new(),
            timestamp: 1,
            allow_partial: true,
        };
        // 单价10 + 0.1% 费 => 每份成本 10.01，最多买 9.99 份
        let fill = executor
            .execute_sync(&Order::market_buy("TEST", 50.0), 10.0, &account)
            .unwrap();
        assert!(fill.quantity <= 100.0 / 10.01 + 1e-9);
        assert!(fill.quantity * 10.01 <= 100.0 + 1e-9);
    }

    #[test]
    fn test_slippage_direction() {
        let model = FillModel::new(0.01, 0.0); // 1% 滑点
        let buy = model.fill(&Order::market_buy("X", 1.0), 100.0, 1);
        let sell = model.fill(&Order::market_sell("X", 1.0), 100.0, 1);
        assert!(buy.price > 100.0, "买入滑点应向上");
        assert!(sell.price < 100.0, "卖出滑点应向下");
        assert!(matches!(buy.side, Side::Buy));
    }

    #[test]
    fn test_metrics_drawdown_and_annualized() {
        use crate::metrics::compute_metrics;
        let curve = vec![
            EquityPoint { timestamp: 0, equity: 100.0 },
            EquityPoint { timestamp: DAY, equity: 120.0 },
            EquityPoint { timestamp: 2 * DAY, equity: 90.0 },
            EquityPoint { timestamp: 365 * DAY, equity: 121.0 },
        ];
        let m = compute_metrics(100.0, &curve, &[], 0.0);
        assert!((m.total_return_pct - 21.0).abs() < 1e-6);
        // 峰值 120 回撤到 90 => 25%
        assert!((m.max_drawdown_pct - 25.0).abs() < 1e-6);
        // 一年约 21% => 年化约 21%
        assert!((m.annualized_return_pct - 21.0).abs() < 2.0);
    }

    /// 熔断回归数据：买入后深度回撤触发熔断清仓，冷却期内禁止重新入场。
    /// ThresholdStrategy：收盘>105 且空仓则买；持仓且收盘<95 则卖。
    fn breaker_data() -> BTreeMap<String, Vec<Kline>> {
        let mut data = BTreeMap::new();
        data.insert(
            "TEST".to_string(),
            vec![
                kline(DAY, 100.0, 110.0), // bar1: 买入信号；bar2 开盘 100 成交 90 份，现金余 1000
                kline(2 * DAY, 100.0, 100.0), // 权益 10000（峰值）
                kline(3 * DAY, 100.0, 96.0), // 权益 9640，回撤 3.6%：浅回撤（不触发策略自身 <95 卖出）
                kline(4 * DAY, 96.0, 110.0), // 开盘 96：熔断场景下清仓成交；收盘 110 买信号在冷却中被拦
                kline(5 * DAY, 110.0, 110.0),
                kline(6 * DAY, 110.0, 110.0),
            ],
        );
        data
    }

    #[test]
    fn test_circuit_breaker_liquidates_and_blocks_reentry() {
        // 熔断 3% + 长冷却：清仓后不得重入，期末空仓、资金锁定在清仓所得 9640 附近（无费模型）
        let config = BacktestConfig {
            initial_cash: 10_000.0,
            model: FillModel::new(0.0, 0.0),
            max_history: 50,
            fill_price: FillPrice::default(),
            circuit_breaker_pct: 0.03,
            circuit_breaker_cooldown_ms: 100 * DAY,
            signal_start_ts: 0,
        };
        let mut s = ThresholdStrategy;
        let result = run_backtest(&mut s, &breaker_data(), &config).unwrap();
        assert_eq!(result.metrics.num_round_trips, 1, "熔断清仓应形成一次完整回合");
        assert!(result.final_positions.is_empty(), "冷却期内不得重新持仓");
        let last = result.equity_curve.last().unwrap();
        assert!((last.equity - 9_640.0).abs() < 1e-6, "equity={}", last.equity);
    }

    #[test]
    fn test_no_circuit_breaker_by_default() {
        // 同数据不启用熔断：回撤后继续持仓（收盘 110 > 95 不卖），权益随价回升
        let config = BacktestConfig {
            initial_cash: 10_000.0,
            model: FillModel::new(0.0, 0.0),
            max_history: 50,
            fill_price: FillPrice::default(),
            ..Default::default()
        };
        let mut s = ThresholdStrategy;
        let result = run_backtest(&mut s, &breaker_data(), &config).unwrap();
        assert_eq!(result.metrics.num_round_trips, 0);
        assert_eq!(result.final_positions.len(), 1);
        let last = result.equity_curve.last().unwrap();
        // 1000 + 90*110 = 10900
        assert!((last.equity - 10_900.0).abs() < 1e-6, "equity={}", last.equity);
    }

    #[test]
    fn test_signal_start_skips_warmup_trading() {
        // bar1~bar3 收盘均 > 105（本会触发买入），但信号起始设在 bar4：
        // 热身期只喂历史，不得成交、不得记录权益点。
        let mut data = BTreeMap::new();
        data.insert(
            "TEST".to_string(),
            vec![
                kline(DAY, 100.0, 110.0),
                kline(2 * DAY, 100.0, 110.0),
                kline(3 * DAY, 100.0, 110.0),
                kline(4 * DAY, 100.0, 110.0),
                kline(5 * DAY, 100.0, 100.0),
            ],
        );
        let config = BacktestConfig {
            initial_cash: 10_000.0,
            model: FillModel::new(0.0, 0.0),
            max_history: 50,
            fill_price: FillPrice::default(),
            signal_start_ts: 4 * DAY,
            ..Default::default()
        };
        let mut s = ThresholdStrategy;
        let result = run_backtest(&mut s, &data, &config).unwrap();

        assert!(
            result.equity_curve.iter().all(|p| p.timestamp >= 4 * DAY),
            "热身期不应记录权益点: {:?}",
            result.equity_curve.iter().map(|p| p.timestamp).collect::<Vec<_>>()
        );
        assert_eq!(result.equity_curve.len(), 2, "只应记录 bar4/bar5 两点");
        assert!(
            result.trades.iter().all(|t| t.entry_time >= 4 * DAY),
            "热身期不得产生成交"
        );
        // 完整初始资金在信号起始后才动用：bar4 收盘触发，bar5 开盘 100 成交 9900/110=90 份
        let pos = result.final_positions.first().expect("bar5 应已建仓");
        assert!((pos.quantity - 90.0).abs() < 1e-9, "qty={}", pos.quantity);
    }

    #[test]
    fn test_warmup_invokes_strategy_so_internal_counters_advance() {
        // 回归：热身期必须真的调用 on_bars。动量轮动这类策略用内部 bars_seen
        // 计数判断热身是否结束，若热身期跳过调用，策略会永远认为还在热身，
        // 整个测试窗一笔不交易（样本外收益被错报为 0）。
        struct CountingStrategy {
            calls: usize,
            warmup_bars: usize,
        }
        impl Strategy for CountingStrategy {
            fn name(&self) -> &str {
                "counting-test"
            }
            fn on_bars(
                &mut self,
                ctx: &StrategyContext,
                bars: &BTreeMap<String, Kline>,
            ) -> Vec<Order> {
                self.calls += 1;
                // 模拟 momentum_rotation 的内部热身守卫
                if self.calls <= self.warmup_bars {
                    return vec![];
                }
                let Some(bar) = bars.get("TEST") else {
                    return vec![];
                };
                if ctx.position("TEST").is_none() && ctx.cash() > 0.0 {
                    return vec![Order::market_buy("TEST", ctx.cash() * 0.99 / bar.close)];
                }
                vec![]
            }
        }

        let mut data = BTreeMap::new();
        data.insert(
            "TEST".to_string(),
            (1..=10).map(|i| kline(i * DAY, 100.0, 100.0)).collect(),
        );
        let config = BacktestConfig {
            initial_cash: 10_000.0,
            model: FillModel::new(0.0, 0.0),
            max_history: 50,
            fill_price: FillPrice::default(),
            // 前 5 根为热身，第 6 根起开始交易
            signal_start_ts: 6 * DAY,
            ..Default::default()
        };
        let mut s = CountingStrategy { calls: 0, warmup_bars: 4 };
        let result = run_backtest(&mut s, &data, &config).unwrap();

        // 热身期也调用了策略：10 根全部调用到
        assert_eq!(s.calls, 10, "热身期必须照常调用 on_bars");
        // 热身已把计数推过阈值，信号起始后立刻能建仓
        assert!(
            !result.final_positions.is_empty(),
            "热身状态未推进导致测试窗内一笔不交易（正是要修的缺陷）"
        );
        // 权益曲线仍只覆盖信号起始之后
        assert!(result.equity_curve.iter().all(|p| p.timestamp >= 6 * DAY));
    }

    #[test]
    fn test_signal_start_zero_matches_default() {
        // 显式 0 与默认必须逐点一致：新增字段不得改变既有回测路径的任何结果
        let mut data = BTreeMap::new();
        data.insert(
            "TEST".to_string(),
            vec![
                kline(DAY, 100.0, 110.0),
                kline(2 * DAY, 100.0, 90.0),
                kline(3 * DAY, 100.0, 100.0),
                kline(4 * DAY, 100.0, 120.0),
            ],
        );
        let base = BacktestConfig {
            initial_cash: 10_000.0,
            model: FillModel::new(0.0, 0.001),
            max_history: 50,
            fill_price: FillPrice::default(),
            ..Default::default()
        };
        let explicit = BacktestConfig { signal_start_ts: 0, ..base.clone() };

        let mut s1 = ThresholdStrategy;
        let a = run_backtest(&mut s1, &data, &base).unwrap();
        let mut s2 = ThresholdStrategy;
        let b = run_backtest(&mut s2, &data, &explicit).unwrap();

        assert_eq!(a.equity_curve.len(), b.equity_curve.len());
        for (x, y) in a.equity_curve.iter().zip(&b.equity_curve) {
            assert_eq!(x.timestamp, y.timestamp);
            assert!((x.equity - y.equity).abs() < 1e-12);
        }
        assert_eq!(a.metrics.num_round_trips, b.metrics.num_round_trips);
    }

    #[test]
    fn test_dropped_tail_orders_counted() {
        // bar1 收盘 110 触发买入（bar2 开盘成交）；bar3 是最后一根，收盘 90 触发
        // 卖出但已无下一根可撮合 -> 该订单作废并被计数，持仓保留到期末。
        let mut data = BTreeMap::new();
        data.insert(
            "TEST".to_string(),
            vec![
                kline(DAY, 100.0, 110.0),
                kline(2 * DAY, 100.0, 100.0),
                kline(3 * DAY, 100.0, 90.0),
            ],
        );
        let config = BacktestConfig {
            initial_cash: 10_000.0,
            model: FillModel::new(0.0, 0.0),
            max_history: 50,
            fill_price: FillPrice::default(),
            ..Default::default()
        };
        let mut s = ThresholdStrategy;
        let r = run_backtest(&mut s, &data, &config).unwrap();
        assert_eq!(r.dropped_tail_orders, 1, "末根卖单应被计为尾部作废");
        assert_eq!(r.final_positions.len(), 1, "作废的卖单不应改变持仓");

        // SameBarClose 下末根信号当根即成交，不产生尾部作废
        let same_close = BacktestConfig {
            fill_price: FillPrice::SameBarClose,
            ..config.clone()
        };
        let mut s2 = ThresholdStrategy;
        let r2 = run_backtest(&mut s2, &data, &same_close).unwrap();
        assert_eq!(r2.dropped_tail_orders, 0, "当根收盘撮合不应留下尾部订单");
    }

    #[test]
    fn test_empty_data_returns_error() {
        let data = BTreeMap::new();
        let config = BacktestConfig::default();
        let mut s = ThresholdStrategy;
        let result = run_backtest(&mut s, &data, &config);
        assert!(result.is_err());
    }

    #[test]
    fn test_missing_symbol_in_timeline() {
        // 某时间戳只有一个品种有K线，另一个缺失
        let mut data = BTreeMap::new();
        data.insert(
            "A".to_string(),
            vec![kline(DAY, 100.0, 110.0), kline(2 * DAY, 100.0, 100.0)],
        );
        data.insert(
            "B".to_string(),
            vec![kline(2 * DAY, 50.0, 55.0)], // 只在第二个时间戳有数据
        );

        struct SimpleBuy;
        impl Strategy for SimpleBuy {
            fn name(&self) -> &str {
                "simple-buy"
            }
            fn on_bars(
                &mut self,
                ctx: &StrategyContext,
                bars: &BTreeMap<String, Kline>,
            ) -> Vec<Order> {
                if ctx.position("A").is_none() && bars.contains_key("A") {
                    let k = &bars["A"];
                    return vec![Order::market_buy("A", ctx.cash() * 0.99 / k.close)];
                }
                vec![]
            }
        }

        let config = BacktestConfig::default();
        let mut s = SimpleBuy;
        let result = run_backtest(&mut s, &data, &config).unwrap();
        
        // 应该能正常运行，B 在第一个时间戳缺失不影响 A 的交易
        assert!(!result.equity_curve.is_empty());
    }

    #[test]
    fn test_fill_price_enum_default() {
        // 验证 FillPrice 默认值为 NextBarOpen
        let default_fill = FillPrice::default();
        assert_eq!(default_fill, FillPrice::NextBarOpen);
    }

    #[test]
    fn test_backtest_config_default_values() {
        let config = BacktestConfig::default();
        assert!((config.initial_cash - 10_000.0).abs() < 1e-6);
        assert_eq!(config.max_history, 400);
        assert_eq!(config.fill_price, FillPrice::NextBarOpen);
        assert!((config.circuit_breaker_pct - 0.0).abs() < 1e-6);
        assert_eq!(config.circuit_breaker_cooldown_ms, 0);
        assert_eq!(config.signal_start_ts, 0);
    }

    #[test]
    fn test_dedupe_orders_same_symbol_same_side() {
        // 同一品种同方向的多笔订单应只保留最后一笔
        let orders = vec![
            Order::market_buy("BTCUSDT", 1.0),
            Order::market_buy("BTCUSDT", 2.0),
            Order::market_buy("BTCUSDT", 3.0),
            Order::market_sell("ETHUSDT", 10.0),
            Order::market_sell("ETHUSDT", 20.0),
        ];
        
        // 调用私有函数需要通过公开接口间接测试
        // 这里通过回测引擎的行为来验证去重逻辑
        let mut data = BTreeMap::new();
        data.insert(
            "BTCUSDT".to_string(),
            vec![kline(DAY, 60000.0, 61000.0)],
        );
        
        struct MultiOrderStrategy;
        impl Strategy for MultiOrderStrategy {
            fn name(&self) -> &str {
                "multi-order"
            }
            fn on_bars(
                &mut self,
                _ctx: &StrategyContext,
                _bars: &BTreeMap<String, Kline>,
            ) -> Vec<Order> {
                // 返回多笔同方向订单
                vec![
                    Order::market_buy("BTCUSDT", 0.1),
                    Order::market_buy("BTCUSDT", 0.2),
                    Order::market_buy("BTCUSDT", 0.3),
                ]
            }
        }
        
        let config = BacktestConfig {
            initial_cash: 100000.0,
            model: FillModel::new(0.0, 0.0),
            max_history: 50,
            fill_price: FillPrice::SameBarClose, // 当根成交，避免尾部订单
            ..Default::default()
        };
        let mut s = MultiOrderStrategy;
        let result = run_backtest(&mut s, &data, &config).unwrap();
        
        // 去重后应该只有一笔买单成交
        assert_eq!(result.final_positions.len(), 1);
        let pos = &result.final_positions[0];
        // 应该是最后一笔订单的量 0.3
        assert!((pos.quantity - 0.3).abs() < 1e-6, "qty={}", pos.quantity);
    }

    #[test]
    fn test_circuit_breaker_disabled_when_zero() {
        // circuit_breaker_pct = 0 时应完全禁用熔断
        let mut data = BTreeMap::new();
        data.insert(
            "TEST".to_string(),
            vec![
                kline(DAY, 100.0, 110.0), // 触发买入
                kline(2 * DAY, 100.0, 100.0), // 保持持仓（100 > 95，不触发卖出）
                kline(3 * DAY, 100.0, 105.0), // 继续持仓
            ],
        );
        
        let config = BacktestConfig {
            initial_cash: 10_000.0,
            model: FillModel::new(0.0, 0.0),
            max_history: 50,
            fill_price: FillPrice::default(),
            circuit_breaker_pct: 0.0, // 明确禁用
            circuit_breaker_cooldown_ms: 0,
            ..Default::default()
        };
        
        let mut s = ThresholdStrategy;
        let result = run_backtest(&mut s, &data, &config).unwrap();
        
        // 应该正常持仓，没有被强制清仓
        assert_eq!(result.final_positions.len(), 1);
    }

    #[test]
    fn test_log_skip_on_missing_kline() {
        // 订单对应的品种在当前时间戳没有K线，应记录跳过日志
        let mut data = BTreeMap::new();
        data.insert(
            "A".to_string(),
            vec![kline(DAY, 100.0, 110.0)],
        );
        // B 品种没有任何K线
        
        struct BuyMissingSymbol;
        impl Strategy for BuyMissingSymbol {
            fn name(&self) -> &str {
                "buy-missing"
            }
            fn on_bars(
                &mut self,
                _ctx: &StrategyContext,
                _bars: &BTreeMap<String, Kline>,
            ) -> Vec<Order> {
                // 尝试买入不存在的品种
                vec![Order::market_buy("NONEXISTENT", 1.0)]
            }
        }
        
        let config = BacktestConfig {
            initial_cash: 10_000.0,
            model: FillModel::new(0.0, 0.0),
            max_history: 50,
            fill_price: FillPrice::SameBarClose,
            ..Default::default()
        };
        
        let mut s = BuyMissingSymbol;
        let result = run_backtest(&mut s, &data, &config).unwrap();
        
        // 订单应被作废，没有持仓
        assert!(result.final_positions.is_empty());
    }
}
