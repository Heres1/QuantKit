//! 历史数据加载（JSON 格式，`{SYMBOL}_{interval}.json`：snake_case 字段，多余字段忽略）
//!
//! 另提供回测窗口切分：按 --start/--end 日期裁剪，供训练/测试集划分使用。

use quantkit_core::interval::Interval;
use quantkit_core::types::Kline;
use std::collections::BTreeMap;
use std::path::Path;

/// 按品种列表加载指定周期的K线数据
pub fn load_data(
    dir: &str,
    symbols: &[String],
    interval: Interval,
) -> Result<BTreeMap<String, Vec<Kline>>, String> {
    let mut data = BTreeMap::new();
    for sym in symbols {
        let path = Path::new(dir).join(format!("{}_{}.json", sym, interval.as_str()));
        let text = std::fs::read_to_string(&path).map_err(|e| {
            format!(
                "读取 {} 失败: {}（{} 周期数据可能尚未下载）",
                path.display(),
                e,
                interval.as_str()
            )
        })?;
        let klines: Vec<Kline> = serde_json::from_str(&text)
            .map_err(|e| format!("解析 {} 失败: {}", path.display(), e))?;
        if klines.is_empty() {
            return Err(format!("{} 无K线数据", path.display()));
        }
        data.insert(sym.clone(), klines);
    }
    Ok(data)
}

/// 日期字符串 "YYYY-MM-DD" → UTC 毫秒时间戳（与交易所 K 线 open_time 同基准）
pub fn parse_date_ms(s: &str) -> Result<u64, String> {
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() != 3 {
        return Err(format!("日期格式应为 YYYY-MM-DD，实际为 '{s}'"));
    }
    let (y, m, d): (i64, u64, u64) = (
        parts[0].parse().map_err(|_| format!("非法年份 '{}'", parts[0]))?,
        parts[1].parse().map_err(|_| format!("非法月份 '{}'", parts[1]))?,
        parts[2].parse().map_err(|_| format!("非法日期 '{}'", parts[2]))?,
    );
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return Err(format!("非法日期 '{s}'"));
    }
    // Hinnant 公历算法：1970-01-01 起的天数
    let yy = if m <= 2 { y - 1 } else { y };
    let era = if yy >= 0 { yy } else { yy - 399 } / 400;
    let yoe = yy - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy as i64;
    let days = era * 146097 + doe - 719468;
    Ok((days * 86_400_000) as u64)
}

/// UTC 毫秒时间戳 → "YYYY-MM-DD"（[`parse_date_ms`] 的逆运算，Hinnant 公历算法）
pub fn format_date(ms: u64) -> String {
    let days = (ms / 86_400_000) as i64;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{:04}-{:02}-{:02}", y, m, d)
}

/// 已知的美元稳定币基础币：与 USDT 组成的交易对价格恒为 1。
///
/// 这类品种永远不可能有动量，却会污染因子排名；更隐蔽的危害是在市场状态过滤的
/// 「广度」统计里充当噪声分母——它恒在均线附近抖动，随机地被计为站上/跌破均线，
/// 让熊市判断失真。用显式清单而非「波动率过低」这类启发式：后者会误杀
/// 真实存在但当期恰好横盘的币种。
const STABLE_BASES: &[&str] = &[
    "USDC", "FDUSD", "TUSD", "BUSD", "DAI", "USDP", "USDD", "PYUSD", "EURI",
];

/// 是否为「稳定币兑 USDT」交易对（如 USDCUSDT）
fn is_stable_pair(symbol: &str) -> bool {
    symbol
        .strip_suffix("USDT")
        .is_some_and(|base| STABLE_BASES.contains(&base))
}

/// 扫描目录中指定周期的 `{SYMBOL}_{interval}.json`，返回品种列表。
/// 稳定币兑 USDT 的交易对会被剔除（见 [`STABLE_BASES`]）。
pub fn discover_symbols(dir: &str, interval: Interval) -> Result<Vec<String>, String> {
    let entries =
        std::fs::read_dir(dir).map_err(|e| format!("读取目录 {} 失败: {}", dir, e))?;
    let suffix = format!("_{}.json", interval.as_str());
    let mut symbols = Vec::new();
    let mut skipped = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if let Some(sym) = name.strip_suffix(&suffix) {
            if is_stable_pair(sym) {
                skipped.push(sym.to_string());
                continue;
            }
            symbols.push(sym.to_string());
        }
    }
    symbols.sort();
    if !skipped.is_empty() {
        skipped.sort();
        eprintln!("[数据] 已剔除稳定币交易对（价格恒为 1，无动量且会干扰广度统计）: {}", skipped.join(", "));
    }
    if symbols.is_empty() {
        return Err(format!(
            "目录 {} 中没有 *{} 数据文件（{} 周期数据可能尚未下载）",
            dir,
            suffix,
            interval.as_str()
        ));
    }
    Ok(symbols)
}

/// 按日期窗口裁剪数据：保留 open_time ∈ [start, end)；两端均 None 时不做任何操作。
/// 窗口内无数据的品种会被移除并提示。
pub fn slice_data(
    data: &mut BTreeMap<String, Vec<Kline>>,
    start_ms: Option<u64>,
    end_ms: Option<u64>,
) {
    if start_ms.is_none() && end_ms.is_none() {
        return;
    }
    for klines in data.values_mut() {
        klines.retain(|k| {
            start_ms.is_none_or(|s| k.open_time >= s)
                && end_ms.is_none_or(|e| k.open_time < e)
        });
    }
    data.retain(|sym, klines| {
        if klines.is_empty() {
            eprintln!("[数据] {sym} 在指定窗口内无K线，已剔除");
            false
        } else {
            true
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kline(day: u64) -> Kline {
        Kline {
            open_time: day * 86_400_000,
            open: 1.0,
            high: 1.0,
            low: 1.0,
            close: 1.0,
            volume: 1.0,
            close_time: (day + 1) * 86_400_000 - 1,
        }
    }

    #[test]
    fn test_parse_date_ms_epoch() {
        assert_eq!(parse_date_ms("1970-01-01").unwrap(), 0);
        // 2024-01-01 = 第 19723 天（含闰年规则）
        assert_eq!(parse_date_ms("2024-01-01").unwrap(), 1_704_067_200_000);
        assert!(parse_date_ms("2024/01/01").is_err());
        assert!(parse_date_ms("2024-13-01").is_err());
    }

    #[test]
    fn test_format_date_roundtrip() {
        for s in [
            "1970-01-01",
            "2000-02-29", // 闰年（世纪闰年）
            "2024-02-29", // 闰年
            "2023-02-28",
            "2024-12-31",
            "2025-01-01",
            "2024-01-01",
            "2026-08-30",
        ] {
            let ms = parse_date_ms(s).unwrap();
            assert_eq!(format_date(ms), s, "往返不一致: {s}");
        }
        // 当天任意时刻都应格式化为同一天（日内偏移不进位）
        let day = parse_date_ms("2024-06-15").unwrap();
        assert_eq!(format_date(day), "2024-06-15");
        assert_eq!(format_date(day + 86_399_999), "2024-06-15");
        assert_eq!(format_date(day + 86_400_000), "2024-06-16");
    }

    #[test]
    fn test_is_stable_pair() {
        for s in ["USDCUSDT", "FDUSDUSDT", "DAIUSDT", "TUSDUSDT", "BUSDUSDT"] {
            assert!(is_stable_pair(s), "{s} 应判为稳定币对");
        }
        for s in ["BTCUSDT", "ETHUSDT", "SOLUSDT", "LINKUSDT", "DOGEUSDT"] {
            assert!(!is_stable_pair(s), "{s} 不应被误判为稳定币对");
        }
        // 只按完整基础币名匹配：名字里含 USDC 但基础币不在清单内的不误杀
        assert!(!is_stable_pair("USDCOINUSDT"), "基础币 USDCOIN 不在清单内");
        // 非 USDT 计价的不在本项目范围内，不判定
        assert!(!is_stable_pair("BTCUSDC"));
    }

    #[test]
    fn test_slice_data_window() {
        let mut data = BTreeMap::new();
        data.insert("A".into(), (0..10).map(kline).collect::<Vec<_>>());
        data.insert("B".into(), (0..2).map(kline).collect::<Vec<_>>());
        // 窗口 [第2天, 第5天)：A 剩 3 根；B 无数据被剔除（下行为补齐的测试断言）
        slice_data(&mut data, Some(2 * 86_400_000), Some(5 * 86_400_000));
        assert_eq!(data.len(), 1);
        let a = data.get("A").unwrap();
        assert_eq!(a.len(), 3);
        assert_eq!(a[0].open_time, 2 * 86_400_000);
        assert_eq!(a[2].open_time, 4 * 86_400_000);
    }
}
