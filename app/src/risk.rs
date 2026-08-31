//! 前置风控闸门（pre-trade gate）。
//!
//! 与旧版"事后告警"（OrderPlaced 之后才检查）不同，这里在下单前评估：
//! 所有输入来自已持久化的状态（成交流水 / 权益曲线），重启不丢失；
//! 只拦「开新仓」，永不拦卖出回笼资金。三级闸门：
//! 1. 账户级（[`gate_account`]）：回撤 / 连续亏损冷却 / 单日成交笔数
//! 2. 品种级（[`gate_symbol`]）：黑名单 / 24h 极端涨跌幅
//! 3. 订单级（[`gate_order`]）：单笔名义价值 / 总敞口 / 单品种数量

use std::collections::BTreeMap;

use quantkit_core::types::{Position, Side};

use crate::config::RiskConfig;
use crate::live::{EquitySnap, LiveFill};

const MS_PER_DAY: u64 = 86_400_000;
/// 浮点比较的数量下限（低于此视为 0，防灰尘尾差）
const QTY_EPS: f64 = 1e-12;

/// 闸门上下文：全部取自持久化状态，跨重启有效
pub struct GateContext<'a> {
    pub now_ms: u64,
    pub positions: &'a [Position],
    pub fills: &'a [LiveFill],
    pub equity_history: &'a [EquitySnap],
    /// 当前持仓市值（USDT，最近一次权益快照的盯市值）
    pub exposure: f64,
}

/// 一次完整回合（买入批次被卖出闭合）的已实现盈亏
#[derive(Debug, Clone, PartialEq)]
pub struct RoundTrip {
    pub symbol: String,
    /// 已实现盈亏（卖出回款 - 买入成本，均摊手续费）
    pub pnl: f64,
    /// 闭合时间（该笔卖出的成交时间）
    pub close_ts: u64,
}

/// 窗口内峰值回撤（0-1）：峰值只取窗口内的点，随窗口前滚自愈。
/// 无数据 / 峰值非正时返回 0（不拦截）。
pub fn drawdown_from_peak(history: &[EquitySnap], window_ms: u64, now_ms: u64) -> f64 {
    let cutoff = now_ms.saturating_sub(window_ms);
    let mut peak = f64::NEG_INFINITY;
    let mut current: Option<f64> = None;
    for s in history {
        if s.ts < cutoff {
            continue;
        }
        peak = peak.max(s.total);
        current = Some(s.total);
    }
    let Some(current) = current else {
        return 0.0;
    };
    if peak <= 0.0 {
        return 0.0;
    }
    ((peak - current) / peak).max(0.0)
}

/// 当日（UTC 日界，与策略收盘时点一致）成交笔数
pub fn daily_trade_count(fills: &[LiveFill], now_ms: u64) -> u32 {
    let day_start = now_ms - now_ms % MS_PER_DAY;
    fills.iter().filter(|f| f.ts >= day_start).count() as u32
}

/// 由成交流水按 FIFO 重建已闭合回合的盈亏（手续费按数量均摊）。
/// 输入需按时间序追加（实盘流水天然如此）；无持仓批次可卖的
/// 卖出（如场外先卖后补记）不产生回合。
pub fn round_trips(fills: &[LiveFill]) -> Vec<RoundTrip> {
    // 品种 -> FIFO 未闭合批次（剩余数量, 含费单位成本）
    let mut open: BTreeMap<String, Vec<(f64, f64)>> = BTreeMap::new();
    let mut trips = Vec::new();
    for f in fills {
        if f.quantity <= QTY_EPS {
            continue;
        }
        match f.side {
            Side::Buy => {
                let unit_cost = f.price + f.fee / f.quantity;
                open.entry(f.symbol.clone())
                    .or_default()
                    .push((f.quantity, unit_cost));
            }
            Side::Sell => {
                let unit_proceeds = f.price - f.fee / f.quantity;
                let mut remaining = f.quantity;
                let lots = open.entry(f.symbol.clone()).or_default();
                while remaining > QTY_EPS {
                    let Some((lot_qty, unit_cost)) = lots.first_mut() else {
                        break; // 没有可闭合的批次
                    };
                    let take = remaining.min(*lot_qty);
                    trips.push(RoundTrip {
                        symbol: f.symbol.clone(),
                        pnl: (unit_proceeds - *unit_cost) * take,
                        close_ts: f.ts,
                    });
                    *lot_qty -= take;
                    remaining -= take;
                    if *lot_qty <= QTY_EPS {
                        lots.remove(0);
                    }
                }
            }
        }
    }
    trips
}

/// 从最近回合倒数的连续亏损次数，及最近一次亏损闭合时间
pub fn consecutive_losses(trips: &[RoundTrip]) -> (u32, Option<u64>) {
    let mut n = 0;
    let mut last_ts = None;
    for t in trips.iter().rev() {
        if t.pnl < 0.0 {
            n += 1;
            if last_ts.is_none() {
                last_ts = Some(t.close_ts);
            }
        } else {
            break;
        }
    }
    (n, last_ts)
}

/// 账户级闸门：返回拦截全部开仓的原因（空 = 放行）
pub fn gate_account(rc: &RiskConfig, ctx: &GateContext) -> Vec<String> {
    let mut reasons = Vec::new();
    if !rc.enabled {
        return reasons;
    }

    if rc.max_drawdown > 0.0 {
        let dd = drawdown_from_peak(
            ctx.equity_history,
            rc.drawdown_window_days * MS_PER_DAY,
            ctx.now_ms,
        );
        if dd >= rc.max_drawdown {
            reasons.push(format!(
                "权益回撤 {:.1}% ≥ {:.1}%（近 {} 日峰值），暂停开新仓",
                dd * 100.0,
                rc.max_drawdown * 100.0,
                rc.drawdown_window_days
            ));
        }
    }

    if rc.max_consecutive_losses > 0 {
        let trips = round_trips(ctx.fills);
        let (n, last_ts) = consecutive_losses(&trips);
        if n >= rc.max_consecutive_losses {
            let until = last_ts.unwrap_or(0) + rc.loss_cooldown_days * MS_PER_DAY;
            if ctx.now_ms < until {
                let days_left = (until - ctx.now_ms) as f64 / MS_PER_DAY as f64;
                reasons.push(format!(
                    "连续 {n} 次回合亏损，冷却中（约 {days_left:.1} 天后自动恢复）"
                ));
            }
        }
    }

    if rc.max_daily_trades > 0 {
        let count = daily_trade_count(ctx.fills, ctx.now_ms);
        if count >= rc.max_daily_trades {
            reasons.push(format!(
                "今日成交 {} 笔 ≥ 上限 {}，暂停开新仓",
                count, rc.max_daily_trades
            ));
        }
    }

    reasons
}

/// 品种级闸门（数量未定阶段）：黑名单 / 极端涨跌幅。
/// `volatility` 为 |24h 涨跌幅|（0-1）；None = 数据缺失，不因缺数据拦截。
pub fn gate_symbol(rc: &RiskConfig, symbol: &str, volatility: Option<f64>) -> Vec<String> {
    let mut reasons = Vec::new();
    if !rc.enabled {
        return reasons;
    }
    if rc.blacklist.iter().any(|b| b == symbol) {
        reasons.push("品种在黑名单中".to_string());
    }
    if rc.max_volatility > 0.0 {
        if let Some(v) = volatility {
            if v > rc.max_volatility {
                reasons.push(format!(
                    "|24h 涨跌幅| {:.1}% > {:.1}%，跳过极端行情",
                    v * 100.0,
                    rc.max_volatility * 100.0
                ));
            }
        }
    }
    reasons
}

/// 订单级闸门（数量已定）：单笔名义价值 / 总敞口 / 单品种数量
pub fn gate_order(rc: &RiskConfig, ctx: &GateContext, symbol: &str, price: f64, qty: f64) -> Vec<String> {
    let mut reasons = Vec::new();
    if !rc.enabled {
        return reasons;
    }
    let notional = qty * price;
    if rc.max_order_value > 0.0 && notional > rc.max_order_value {
        reasons.push(format!(
            "单笔名义 {:.2} USDT > 上限 {:.2}",
            notional, rc.max_order_value
        ));
    }
    if rc.max_total_exposure > 0.0 && ctx.exposure + notional > rc.max_total_exposure {
        reasons.push(format!(
            "总持仓市值将达 {:.2} > 上限 {:.2} USDT",
            ctx.exposure + notional,
            rc.max_total_exposure
        ));
    }
    if let Some(&cap) = rc.max_position_qty.get(symbol) {
        let held = ctx
            .positions
            .iter()
            .find(|p| p.symbol == symbol)
            .map(|p| p.quantity)
            .unwrap_or(0.0);
        if held + qty > cap {
            reasons.push(format!("持仓量将达 {:.8} > 上限 {cap:.8}", held + qty));
        }
    }
    reasons
}

/// 已启用规则的一句话摘要（启动日志用）
pub fn describe(rc: &RiskConfig) -> String {
    if !rc.enabled {
        return "已停用（risk.enabled = false）".to_string();
    }
    let mut parts = Vec::new();
    if rc.max_order_value > 0.0 {
        parts.push(format!("单笔≤{:.0} USDT", rc.max_order_value));
    }
    if rc.max_daily_trades > 0 {
        parts.push(format!("日成交≤{} 笔", rc.max_daily_trades));
    }
    if rc.max_drawdown > 0.0 {
        parts.push(format!(
            "回撤≥{:.0}% 停开仓（{} 日窗口）",
            rc.max_drawdown * 100.0,
            rc.drawdown_window_days
        ));
    }
    if rc.max_volatility > 0.0 {
        parts.push(format!("|24h 涨跌|>{:.0}% 跳过", rc.max_volatility * 100.0));
    }
    if rc.max_consecutive_losses > 0 {
        parts.push(format!(
            "连亏 {} 次停 {} 天",
            rc.max_consecutive_losses, rc.loss_cooldown_days
        ));
    }
    if !rc.blacklist.is_empty() {
        parts.push(format!("黑名单 {:?}", rc.blacklist));
    }
    if rc.max_total_exposure > 0.0 {
        parts.push(format!("总敞口≤{:.0} USDT", rc.max_total_exposure));
    }
    if parts.is_empty() {
        "已启用，但未配置具体规则".to_string()
    } else {
        parts.join(" | ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(ts: u64, total: f64) -> EquitySnap {
        EquitySnap {
            ts,
            total,
            usdt: total,
            positions_value: 0.0,
        }
    }

    fn fill(ts: u64, symbol: &str, side: Side, qty: f64, price: f64, fee: f64) -> LiveFill {
        LiveFill {
            ts,
            symbol: symbol.to_string(),
            side,
            quantity: qty,
            price,
            fee,
            reason: String::new(),
        }
    }

    fn default_rc() -> RiskConfig {
        RiskConfig::default()
    }

    #[test]
    fn drawdown_empty_or_trivial() {
        assert_eq!(drawdown_from_peak(&[], MS_PER_DAY, 100), 0.0);
        // 单点无回撤
        assert_eq!(drawdown_from_peak(&[snap(10, 100.0)], MS_PER_DAY, 10), 0.0);
        // 峰值非正不拦
        assert_eq!(drawdown_from_peak(&[snap(10, 0.0), snap(20, -5.0)], MS_PER_DAY, 20), 0.0);
    }

    #[test]
    fn drawdown_within_window() {
        let h = vec![snap(10, 100.0), snap(20, 120.0), snap(30, 90.0)];
        // 峰值 120 → 现值 90：回撤 25%
        let dd = drawdown_from_peak(&h, 1000, 30);
        assert!((dd - 0.25).abs() < 1e-9);
    }

    #[test]
    fn drawdown_old_peak_rolls_out() {
        // 窗口外的历史高点不参与：窗口 100，now=200 → cutoff=100，仅剩最后一点
        let h = vec![snap(10, 1000.0), snap(150, 100.0)];
        let dd = drawdown_from_peak(&h, 100, 200);
        assert_eq!(dd, 0.0);
    }

    #[test]
    fn daily_count_utc_boundary() {
        let day = 86_400_000u64;
        let fills = vec![
            fill(day - 1, "BTCUSDT", Side::Buy, 1.0, 1.0, 0.0), // 昨日
            fill(day, "BTCUSDT", Side::Sell, 1.0, 1.0, 0.0),    // 今日起点
            fill(day + 10, "BTCUSDT", Side::Buy, 1.0, 1.0, 0.0),
        ];
        assert_eq!(daily_trade_count(&fills, day + 100), 2);
    }

    #[test]
    fn round_trip_profit_and_loss_with_fees() {
        let fills = vec![
            fill(1, "BTCUSDT", Side::Buy, 2.0, 100.0, 2.0),   // 成本 202
            fill(2, "BTCUSDT", Side::Sell, 2.0, 110.0, 2.2),  // 回款 217.8
        ];
        let trips = round_trips(&fills);
        assert_eq!(trips.len(), 1);
        assert!((trips[0].pnl - 15.8).abs() < 1e-9);
        assert_eq!(trips[0].close_ts, 2);
    }

    #[test]
    fn round_trip_fifo_partial_lots() {
        let fills = vec![
            fill(1, "ETHUSDT", Side::Buy, 1.0, 100.0, 0.0),
            fill(2, "ETHUSDT", Side::Buy, 1.0, 200.0, 0.0),
            fill(3, "ETHUSDT", Side::Sell, 1.5, 150.0, 0.0), // 闭合第一批 1.0 + 第二批 0.5
        ];
        let trips = round_trips(&fills);
        assert_eq!(trips.len(), 2);
        assert!((trips[0].pnl - 50.0).abs() < 1e-9);  // (150-100)*1
        assert!((trips[1].pnl + 25.0).abs() < 1e-9);  // (150-200)*0.5
        // 剩余 0.5 @ 200 未闭合，不计入
    }

    #[test]
    fn round_trip_sell_without_open_ignored() {
        let fills = vec![fill(1, "BTCUSDT", Side::Sell, 1.0, 100.0, 0.0)];
        assert!(round_trips(&fills).is_empty());
    }

    #[test]
    fn consecutive_losses_counts_from_tail() {
        let trips = vec![
            RoundTrip { symbol: "A".into(), pnl: -1.0, close_ts: 10 },
            RoundTrip { symbol: "A".into(), pnl: 5.0, close_ts: 20 },
            RoundTrip { symbol: "B".into(), pnl: -2.0, close_ts: 30 },
            RoundTrip { symbol: "B".into(), pnl: -3.0, close_ts: 40 },
        ];
        let (n, last) = consecutive_losses(&trips);
        assert_eq!(n, 2);
        assert_eq!(last, Some(40));
    }

    #[test]
    fn consecutive_losses_zero_after_win() {
        let trips = vec![
            RoundTrip { symbol: "A".into(), pnl: -1.0, close_ts: 10 },
            RoundTrip { symbol: "A".into(), pnl: 2.0, close_ts: 20 },
        ];
        assert_eq!(consecutive_losses(&trips), (0, None));
    }

    fn ctx<'a>(
        now: u64,
        fills: &'a [LiveFill],
        history: &'a [EquitySnap],
        positions: &'a [Position],
    ) -> GateContext<'a> {
        GateContext {
            now_ms: now,
            positions,
            fills,
            equity_history: history,
            exposure: 0.0,
        }
    }

    #[test]
    fn gate_account_drawdown_blocks() {
        let rc = default_rc();
        let h = vec![snap(10, 200.0), snap(20, 140.0)]; // 回撤 30% > 25%
        let reasons = gate_account(&rc, &ctx(20, &[], &h, &[]));
        assert_eq!(reasons.len(), 1);
        assert!(reasons[0].contains("权益回撤"));
    }

    #[test]
    fn gate_account_drawdown_recovers_after_window() {
        let mut rc = default_rc();
        rc.drawdown_window_days = 1;
        let old_peak = 100;
        let now = old_peak + 2 * MS_PER_DAY;
        let h = vec![snap(old_peak, 200.0), snap(now, 140.0)]; // 峰值滚出窗口
        assert!(gate_account(&rc, &ctx(now, &[], &h, &[])).is_empty());
    }

    #[test]
    fn gate_account_daily_trades_blocks() {
        let mut rc = default_rc();
        rc.max_daily_trades = 2;
        let day = 86_400_000u64;
        let fills = vec![
            fill(day + 1, "A", Side::Buy, 1.0, 1.0, 0.0),
            fill(day + 2, "A", Side::Sell, 1.0, 1.0, 0.0),
        ];
        let reasons = gate_account(&rc, &ctx(day + 3, &fills, &[], &[]));
        assert_eq!(reasons.len(), 1);
        assert!(reasons[0].contains("今日成交"));
    }

    #[test]
    fn gate_account_consecutive_loss_cooldown() {
        let mut rc = default_rc();
        rc.max_consecutive_losses = 2;
        rc.loss_cooldown_days = 7;
        let fills = vec![
            fill(1, "A", Side::Buy, 1.0, 100.0, 0.0),
            fill(2, "A", Side::Sell, 1.0, 90.0, 0.0),
            fill(3, "A", Side::Buy, 1.0, 90.0, 0.0),
            fill(4, "A", Side::Sell, 1.0, 80.0, 0.0),
        ];
        // 冷却期内（刚亏完）→ 拦
        let reasons = gate_account(&rc, &ctx(10, &fills, &[], &[]));
        assert_eq!(reasons.len(), 1);
        assert!(reasons[0].contains("连续"));
        // 冷却期满 → 放
        assert!(gate_account(&rc, &ctx(4 + 8 * MS_PER_DAY, &fills, &[], &[])).is_empty());
    }

    #[test]
    fn gate_account_disabled_passes_everything() {
        let mut rc = default_rc();
        rc.enabled = false;
        let h = vec![snap(10, 200.0), snap(20, 1.0)]; // 回撤 99.5%
        assert!(gate_account(&rc, &ctx(20, &[], &h, &[])).is_empty());
    }

    #[test]
    fn gate_symbol_blacklist_and_volatility() {
        let mut rc = default_rc();
        rc.blacklist = vec!["DOGEUSDT".into()];
        // 黑名单 + 极端涨幅同时命中
        let reasons = gate_symbol(&rc, "DOGEUSDT", Some(0.30));
        assert_eq!(reasons.len(), 2);
        // 波动数据缺失不拦
        assert!(gate_symbol(&rc, "BTCUSDT", None).is_empty());
        // 正常波动放行
        assert!(gate_symbol(&rc, "BTCUSDT", Some(0.05)).is_empty());
        // 阈值内边界：等于阈值不拦（只拦超过）
        assert!(gate_symbol(&rc, "BTCUSDT", Some(0.25)).is_empty());
    }

    #[test]
    fn gate_order_value_and_exposure_and_qty() {
        let mut rc = default_rc();
        rc.max_order_value = 500.0;
        rc.max_total_exposure = 600.0;
        rc.max_position_qty = BTreeMap::from([("BTCUSDT".to_string(), 0.01)]);
        let positions = vec![Position {
            symbol: "BTCUSDT".into(),
            quantity: 0.005,
            avg_entry_price: 50_000.0,
        }];
        let mut c = ctx(1, &[], &[], &positions);
        c.exposure = 250.0;
        // 0.008 BTC @ 50000 = 400 USDT：单笔 OK，但敞口 650 > 600，且持仓 0.013 > 0.01
        let reasons = gate_order(&rc, &c, "BTCUSDT", 50_000.0, 0.008);
        assert_eq!(reasons.len(), 2);
        // 小额全放行
        assert!(gate_order(&rc, &c, "ETHUSDT", 3_000.0, 0.05).is_empty());
    }

    #[test]
    fn describe_lists_active_rules() {
        let s = describe(&default_rc());
        assert!(s.contains("单笔≤500"));
        assert!(s.contains("回撤≥25%"));
        let mut off = default_rc();
        off.enabled = false;
        assert!(describe(&off).contains("停用"));
    }
}
