//! K线周期：多周期回测的统一口径。
//!
//! 三个职责：
//! 1. 数据文件命名（`{SYMBOL}_{interval}.json`）与交易所接口参数用同一套字符串；
//! 2. 把用户面向的「天」换算为策略回看的「根」（`bars_per_day`）——
//!    投资者按天思考（90 日动量），策略按根计算，换周期不需要重算参数；
//! 3. 指标年化的每年周期数（`periods_per_year`），使夏普在任意周期下口径一致。

use std::fmt;

/// 支持的K线周期（与 Binance interval 字符串一致）
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum Interval {
    M5,
    M15,
    M30,
    H1,
    H4,
    H12,
    #[default]
    D1,
    W1,
}

/// 加密市场全年无休：年化一律按 365 天
pub const DAYS_PER_YEAR: f64 = 365.0;
const MS_PER_MINUTE: u64 = 60_000;
const MS_PER_DAY: u64 = 86_400_000;

impl Interval {
    /// 全部周期（前端下拉与 CLI 帮助共用同一份来源）
    pub const ALL: [Interval; 8] = [
        Interval::M5,
        Interval::M15,
        Interval::M30,
        Interval::H1,
        Interval::H4,
        Interval::H12,
        Interval::D1,
        Interval::W1,
    ];

    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "5m" => Ok(Interval::M5),
            "15m" => Ok(Interval::M15),
            "30m" => Ok(Interval::M30),
            "1h" => Ok(Interval::H1),
            "4h" => Ok(Interval::H4),
            "12h" => Ok(Interval::H12),
            "1d" => Ok(Interval::D1),
            "1w" => Ok(Interval::W1),
            other => Err(format!(
                "不支持的周期 '{other}'，可选：{}",
                Interval::ALL
                    .iter()
                    .map(|i| i.as_str())
                    .collect::<Vec<_>>()
                    .join(" / ")
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Interval::M5 => "5m",
            Interval::M15 => "15m",
            Interval::M30 => "30m",
            Interval::H1 => "1h",
            Interval::H4 => "4h",
            Interval::H12 => "12h",
            Interval::D1 => "1d",
            Interval::W1 => "1w",
        }
    }

    /// 一根K线的毫秒跨度
    pub fn ms(self) -> u64 {
        match self {
            Interval::M5 => 5 * MS_PER_MINUTE,
            Interval::M15 => 15 * MS_PER_MINUTE,
            Interval::M30 => 30 * MS_PER_MINUTE,
            Interval::H1 => 60 * MS_PER_MINUTE,
            Interval::H4 => 4 * 60 * MS_PER_MINUTE,
            Interval::H12 => 12 * 60 * MS_PER_MINUTE,
            Interval::D1 => MS_PER_DAY,
            Interval::W1 => 7 * MS_PER_DAY,
        }
    }

    /// 每天多少根（1w 为 1/7，小于 1）
    pub fn bars_per_day(self) -> f64 {
        MS_PER_DAY as f64 / self.ms() as f64
    }

    /// 每年多少根：指标年化用（夏普 × sqrt(periods_per_year)）
    pub fn periods_per_year(self) -> f64 {
        DAYS_PER_YEAR * self.bars_per_day()
    }

    /// 「天」→「根」：用户按天配置回看窗口，策略按根取历史。
    ///
    /// 日线下恒等（bars_per_day = 1），换周期时窗口的真实时间跨度保持不变。
    /// 结果至少为 1 根（否则回看窗口退化为空，策略永不触发）；
    /// `days` 为 0 表示“禁用”，原样返回 0。
    pub fn days_to_bars(self, days: usize) -> usize {
        if days == 0 {
            return 0;
        }
        let bars = (days as f64 * self.bars_per_day()).round() as usize;
        bars.max(1)
    }
}

impl fmt::Display for Interval {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 从时间戳序列推断每年周期数：取相邻间隔的中位数，对数据缺口稳健。
///
/// 指标层不知道回测用的是什么周期（权益曲线只有时间戳），靠这个函数
/// 自适应年化；样本不足或间隔异常时退回日线口径。
pub fn periods_per_year_from_timestamps(timestamps: &[u64]) -> f64 {
    if timestamps.len() < 3 {
        return DAYS_PER_YEAR;
    }
    let mut diffs: Vec<u64> = timestamps
        .windows(2)
        .map(|w| w[1].saturating_sub(w[0]))
        .filter(|d| *d > 0)
        .collect();
    if diffs.is_empty() {
        return DAYS_PER_YEAR;
    }
    diffs.sort_unstable();
    let median = diffs[diffs.len() / 2];
    if median == 0 {
        return DAYS_PER_YEAR;
    }
    DAYS_PER_YEAR * MS_PER_DAY as f64 / median as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_roundtrip() {
        for i in Interval::ALL {
            assert_eq!(Interval::parse(i.as_str()).unwrap(), i);
        }
        assert_eq!(Interval::parse("1D").unwrap(), Interval::D1);
        assert!(Interval::parse("3d").is_err());
    }

    #[test]
    fn test_bars_per_day_and_annualization() {
        assert_eq!(Interval::D1.bars_per_day(), 1.0);
        assert_eq!(Interval::H4.bars_per_day(), 6.0);
        assert_eq!(Interval::H1.bars_per_day(), 24.0);
        assert!((Interval::W1.bars_per_day() - 1.0 / 7.0).abs() < 1e-12);
        assert_eq!(Interval::D1.periods_per_year(), 365.0);
        assert_eq!(Interval::H4.periods_per_year(), 365.0 * 6.0);
    }

    #[test]
    fn test_days_to_bars_identity_on_daily() {
        // 日线下「天」与「根」必须完全等价：老参数换周期后语义不漂移
        for d in [1usize, 7, 30, 50, 90, 200] {
            assert_eq!(Interval::D1.days_to_bars(d), d);
        }
        // 4h：90 天 = 540 根；1w：90 天 ≈ 13 根
        assert_eq!(Interval::H4.days_to_bars(90), 540);
        assert_eq!(Interval::W1.days_to_bars(90), 13);
        // 0 表示禁用，保持 0；不足一根的窗口至少给 1 根
        assert_eq!(Interval::D1.days_to_bars(0), 0);
        assert_eq!(Interval::W1.days_to_bars(1), 1);
    }

    #[test]
    fn test_periods_per_year_inference() {
        let daily: Vec<u64> = (0..10).map(|i| i * MS_PER_DAY).collect();
        assert!((periods_per_year_from_timestamps(&daily) - 365.0).abs() < 1e-9);

        let four_hour: Vec<u64> = (0..10).map(|i| i * Interval::H4.ms()).collect();
        assert!((periods_per_year_from_timestamps(&four_hour) - 365.0 * 6.0).abs() < 1e-9);

        // 中位数对缺口稳健：单个大跳空不改变推断结果
        let mut gappy = daily.clone();
        gappy.push(gappy.last().unwrap() + 30 * MS_PER_DAY);
        assert!((periods_per_year_from_timestamps(&gappy) - 365.0).abs() < 1e-9);

        // 样本不足退回日线口径
        assert_eq!(periods_per_year_from_timestamps(&[0, MS_PER_DAY]), 365.0);
    }

    #[test]
    fn test_parse_case_insensitive_and_whitespace() {
        // 大小写不敏感
        assert_eq!(Interval::parse("5M").unwrap(), Interval::M5);
        assert_eq!(Interval::parse("15M").unwrap(), Interval::M15);
        assert_eq!(Interval::parse("1H").unwrap(), Interval::H1);
        assert_eq!(Interval::parse("1D").unwrap(), Interval::D1);
        assert_eq!(Interval::parse("1W").unwrap(), Interval::W1);

        // 前后空格容忍
        assert_eq!(Interval::parse(" 5m ").unwrap(), Interval::M5);
        assert_eq!(Interval::parse("  1h  ").unwrap(), Interval::H1);
    }

    #[test]
    fn test_parse_invalid_intervals() {
        // 不支持的周期
        assert!(Interval::parse("2m").is_err());
        assert!(Interval::parse("3d").is_err());
        assert!(Interval::parse("2w").is_err());
        assert!(Interval::parse("invalid").is_err());
        assert!(Interval::parse("").is_err());
        assert!(Interval::parse("   ").is_err());
    }

    #[test]
    fn test_ms_values() {
        // 验证各周期的毫秒值计算正确
        assert_eq!(Interval::M5.ms(), 5 * 60_000);
        assert_eq!(Interval::M15.ms(), 15 * 60_000);
        assert_eq!(Interval::M30.ms(), 30 * 60_000);
        assert_eq!(Interval::H1.ms(), 60 * 60_000);
        assert_eq!(Interval::H4.ms(), 4 * 60 * 60_000);
        assert_eq!(Interval::H12.ms(), 12 * 60 * 60_000);
        assert_eq!(Interval::D1.ms(), 86_400_000);
        assert_eq!(Interval::W1.ms(), 7 * 86_400_000);
    }

    #[test]
    fn test_bars_per_day_all_intervals() {
        // 验证所有周期的 bars_per_day 计算
        assert!((Interval::M5.bars_per_day() - 288.0).abs() < 1e-9); // 24*60/5
        assert!((Interval::M15.bars_per_day() - 96.0).abs() < 1e-9); // 24*60/15
        assert!((Interval::M30.bars_per_day() - 48.0).abs() < 1e-9); // 24*60/30
        assert!((Interval::H1.bars_per_day() - 24.0).abs() < 1e-9); // 24*60/60
        assert!((Interval::H4.bars_per_day() - 6.0).abs() < 1e-9); // 24/4
        assert!((Interval::H12.bars_per_day() - 2.0).abs() < 1e-9); // 24/12
        assert!((Interval::D1.bars_per_day() - 1.0).abs() < 1e-9);
        assert!((Interval::W1.bars_per_day() - 1.0 / 7.0).abs() < 1e-12);
    }

    #[test]
    fn test_periods_per_year_all_intervals() {
        // 验证年化周期数
        assert!((Interval::M5.periods_per_year() - 365.0 * 288.0).abs() < 1e-6);
        assert!((Interval::M15.periods_per_year() - 365.0 * 96.0).abs() < 1e-6);
        assert!((Interval::M30.periods_per_year() - 365.0 * 48.0).abs() < 1e-6);
        assert!((Interval::H1.periods_per_year() - 365.0 * 24.0).abs() < 1e-6);
        assert!((Interval::H4.periods_per_year() - 365.0 * 6.0).abs() < 1e-6);
        assert!((Interval::H12.periods_per_year() - 365.0 * 2.0).abs() < 1e-6);
        assert!((Interval::D1.periods_per_year() - 365.0).abs() < 1e-6);
        assert!((Interval::W1.periods_per_year() - 365.0 / 7.0).abs() < 1e-6);
    }

    #[test]
    fn test_days_to_bars_edge_cases() {
        // 零值保持为零（禁用）
        for interval in Interval::ALL {
            assert_eq!(interval.days_to_bars(0), 0);
        }

        // 小数值至少返回 1（避免空窗口）
        assert_eq!(Interval::W1.days_to_bars(1), 1); // 1天 ≈ 0.14根 → 向上取整到1
        assert_eq!(Interval::D1.days_to_bars(1), 1);

        // 大数值精确转换
        assert_eq!(Interval::H1.days_to_bars(30), 720); // 30天 * 24根/天
        assert_eq!(Interval::M5.days_to_bars(1), 288); // 1天 * 288根/天
    }

    #[test]
    fn test_days_to_bars_rounding_behavior() {
        // W1: 1周 = 7天，但 bars_per_day = 1/7
        // days_to_bars(7) = round(7 * 1/7) = round(1.0) = 1
        assert_eq!(Interval::W1.days_to_bars(7), 1);
        
        // days_to_bars(14) = round(14 * 1/7) = round(2.0) = 2
        assert_eq!(Interval::W1.days_to_bars(14), 2);

        // H12: 12小时 = 0.5天，bars_per_day = 2
        // days_to_bars(1) = round(1 * 2) = 2
        assert_eq!(Interval::H12.days_to_bars(1), 2);
        assert_eq!(Interval::H12.days_to_bars(7), 14);
    }

    #[test]
    fn test_display_trait() {
        // 验证 Display trait 与 as_str 一致
        for interval in Interval::ALL {
            assert_eq!(format!("{}", interval), interval.as_str());
        }
    }

    #[test]
    fn test_interval_ordering() {
        // 验证 Interval 的 Ord 实现（按定义顺序）
        assert!(Interval::M5 < Interval::M15);
        assert!(Interval::M15 < Interval::M30);
        assert!(Interval::M30 < Interval::H1);
        assert!(Interval::H1 < Interval::H4);
        assert!(Interval::H4 < Interval::H12);
        assert!(Interval::H12 < Interval::D1);
        assert!(Interval::D1 < Interval::W1);
    }

    #[test]
    fn test_interval_default() {
        // 默认周期为 D1
        assert_eq!(Interval::default(), Interval::D1);
    }

    #[test]
    fn test_period_inference_empty_and_single_timestamp() {
        // 空数组
        assert_eq!(periods_per_year_from_timestamps(&[]), DAYS_PER_YEAR);
        
        // 单个时间戳
        assert_eq!(periods_per_year_from_timestamps(&[MS_PER_DAY]), DAYS_PER_YEAR);
        
        // 两个相同时间戳（间隔为0）
        assert_eq!(periods_per_year_from_timestamps(&[MS_PER_DAY, MS_PER_DAY]), DAYS_PER_YEAR);
    }

    #[test]
    fn test_period_inference_irregular_intervals() {
        // 不规则间隔：使用 M30 周期，但有缺失
        let irregular: Vec<u64> = vec![
            0,
            30 * 60_000,      // 30分钟
            60 * 60_000,      // 60分钟（跳过一根）
            90 * 60_000,      // 90分钟
            120 * 60_000,     // 120分钟（正常间隔）
        ];
        
        // 中位数间隔应为 30分钟
        let inferred = periods_per_year_from_timestamps(&irregular);
        let expected = DAYS_PER_YEAR * 48.0; // 365 * (24*60/30)
        assert!((inferred - expected).abs() < 1e-6);
    }

    #[test]
    fn test_period_inference_large_gaps() {
        // 大量缺失数据：只有几根K线，间隔很大
        let sparse: Vec<u64> = vec![
            0,
            10 * MS_PER_DAY,  // 10天后
            20 * MS_PER_DAY,  // 20天后
            30 * MS_PER_DAY,  // 30天后
        ];
        
        // 中位数间隔 = 10天
        let inferred = periods_per_year_from_timestamps(&sparse);
        let expected = DAYS_PER_YEAR / 10.0; // 365 / 10
        assert!((inferred - expected).abs() < 1e-6);
    }

    #[test]
    fn test_period_inference_mixed_intervals_with_duplicates() {
        // 混合间隔 + 重复时间戳
        let mixed: Vec<u64> = vec![
            0,
            MS_PER_DAY,       // 1天
            MS_PER_DAY,       // 重复（间隔0，应被过滤）
            2 * MS_PER_DAY,   // 1天
            3 * MS_PER_DAY,   // 1天
            10 * MS_PER_DAY,  // 7天跳空
        ];
        
        // 有效间隔: [1天, 1天, 1天, 7天]，排序后中位数为 1天
        let inferred = periods_per_year_from_timestamps(&mixed);
        assert!((inferred - DAYS_PER_YEAR).abs() < 1e-6);
    }

    #[test]
    fn test_ms_overflow_protection() {
        // 验证大时间戳不会溢出（使用 saturating_sub）
        let timestamps = vec![u64::MAX - 1000, u64::MAX];
        let result = periods_per_year_from_timestamps(&timestamps);
        // 样本不足，应退回日线口径
        assert_eq!(result, DAYS_PER_YEAR);
    }

    #[test]
    fn test_all_constants_consistency() {
        // 验证常量定义的一致性
        assert_eq!(MS_PER_MINUTE, 60_000);
        assert_eq!(MS_PER_DAY, 24 * 60 * MS_PER_MINUTE);
        assert_eq!(DAYS_PER_YEAR, 365.0);
        
        // 验证 W1 的 ms 计算
        assert_eq!(Interval::W1.ms(), 7 * MS_PER_DAY);
    }
}
