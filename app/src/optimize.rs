//! 参数寻优的可复用内核：网格解析、并行评估、滚动前进验证的窗口划分。
//!
//! 这些逻辑此前散落在 CLI 二进制里，无法被单测覆盖。窗口边界算错会安静地
//! 毁掉整个验证结论（训练窗和测试窗重叠 = 样本外指标其实是样本内的），
//! 所以放在库里并配单测。

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use quantkit_core::types::Kline;

use crate::config::AppConfig;

/// 单次寻优的参数组合数上限（防组合爆炸）
pub const MAX_COMBOS: usize = 10_000;

/// 网格支持的扫描维度（均为 AppConfig 可覆盖字段）
pub const SUPPORTED_DIMS: &[&str] = &[
    "momentum_days",
    "ma_days",
    "rebalance_days",
    "trailing_stop",
    "cooldown_days",
    "top_n",
    "regime_ma",
    "regime_breadth",
    "circuit_breaker",
    "ma_fast",
    "ma_slow",
    "grid_levels",
    "grid_lookback_days",
    "grid_stop_loss",
    "dca_interval_days",
    "dca_ma_days",
    "dca_dip_multiplier",
];

/// 解析网格声明 "dim=v1,v2;dim2=v3,v4"；未指定时返回默认网格（兼容旧版 9 组）
pub fn parse_grid(spec: Option<&str>) -> Result<Vec<(String, Vec<f64>)>, String> {
    let Some(spec) = spec else {
        return Ok(vec![
            ("momentum_days".to_string(), vec![90.0, 60.0, 30.0]),
            ("rebalance_days".to_string(), vec![30.0, 7.0, 3.0]),
        ]);
    };
    let mut grid = Vec::new();
    for part in spec.split(';') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let (dim, vals) = part
            .split_once('=')
            .ok_or_else(|| format!("'{part}' 缺少 '='"))?;
        let dim = dim.trim().to_string();
        if !SUPPORTED_DIMS.contains(&dim.as_str()) {
            return Err(format!("维度 '{dim}' 不支持，可用: {SUPPORTED_DIMS:?}"));
        }
        let mut values = Vec::new();
        for v in vals.split(',') {
            let v = v.trim();
            if v.is_empty() {
                continue;
            }
            values.push(
                v.parse::<f64>()
                    .map_err(|_| format!("维度 {dim} 数值非法 '{v}'"))?,
            );
        }
        if values.is_empty() {
            return Err(format!("维度 {dim} 无取值"));
        }
        grid.push((dim, values));
    }
    if grid.is_empty() {
        return Err("网格声明为空".into());
    }
    Ok(grid)
}

/// 将单个网格维度值应用到配置（trailing_stop=0 表示关闭追踪止损）
pub fn apply_grid_param(cfg: &mut AppConfig, dim: &str, v: f64) {
    match dim {
        "momentum_days" => cfg.momentum_days = v as usize,
        "ma_days" => cfg.ma_days = v as usize,
        "rebalance_days" => cfg.rebalance_days = v as u64,
        "trailing_stop" => {
            cfg.trailing_stop_pct = v;
            cfg.trailing_stop_enabled = v > 0.0;
        }
        "cooldown_days" => cfg.cooldown_days = v as u64,
        "top_n" => cfg.top_n = (v as usize).max(1),
        "regime_ma" => cfg.regime_ma_days = v as usize,
        "regime_breadth" => cfg.regime_min_breadth = v,
        "circuit_breaker" => cfg.circuit_breaker_pct = v,
        "ma_fast" => cfg.ma_cross_fast = (v as usize).max(1),
        "ma_slow" => cfg.ma_cross_slow = (v as usize).max(2),
        "grid_levels" => cfg.grid_levels = (v as usize).max(2),
        "grid_lookback_days" => cfg.grid_lookback_days = v as usize,
        "grid_stop_loss" => cfg.grid_stop_loss_pct = v,
        "dca_interval_days" => cfg.dca_interval_days = (v as u64).max(1),
        "dca_ma_days" => cfg.dca_ma_days = v as usize,
        "dca_dip_multiplier" => cfg.dca_dip_multiplier = v,
        _ => {}
    }
}

/// 枚举网格的全部参数组合（混合进制进位遍历），返回 (配置, 参数快照) 列表
pub fn enumerate_combos(
    base: &AppConfig,
    grid: &[(String, Vec<f64>)],
) -> Vec<(AppConfig, Vec<(String, f64)>)> {
    let total: usize = grid.iter().map(|(_, v)| v.len()).product();
    let mut combos = Vec::with_capacity(total);
    let mut idx = vec![0usize; grid.len()];
    for _ in 0..total {
        let mut cfg = base.clone();
        let mut params = Vec::with_capacity(grid.len());
        for (gi, (dim, values)) in grid.iter().enumerate() {
            let v = values[idx[gi]];
            apply_grid_param(&mut cfg, dim, v);
            params.push((dim.clone(), v));
        }
        combos.push((cfg, params));
        for gi in (0..grid.len()).rev() {
            idx[gi] += 1;
            if idx[gi] < grid[gi].1.len() {
                break;
            }
            idx[gi] = 0;
        }
    }
    combos
}

/// 数据全局时间范围（全品种 open_time 的最小/最大值）
pub fn time_range(data: &BTreeMap<String, Vec<Kline>>) -> (u64, u64) {
    let mut mn = u64::MAX;
    let mut mx = 0u64;
    for klines in data.values() {
        if let Some(f) = klines.first() {
            mn = mn.min(f.open_time);
        }
        if let Some(l) = klines.last() {
            mx = mx.max(l.open_time);
        }
    }
    (mn, mx)
}

/// 并行 map：按 CPU 核数开线程，共享游标动态领取任务，结果按输入顺序返回。
///
/// 用动态领取而非静态等分：性能核/能效核混合的机器上，等分会被最慢的核拖住；
/// 且不同参数组合的回测工作量本就不均等。按索引归位保证输出可复现。
pub fn par_map<T, R, F>(items: &[T], f: F) -> Vec<R>
where
    T: Sync,
    R: Send,
    F: Fn(&T) -> R + Sync,
{
    if items.is_empty() {
        return Vec::new();
    }
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .min(items.len());
    let cursor = AtomicUsize::new(0);
    let mut indexed: Vec<(usize, R)> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                let (cursor, f) = (&cursor, &f);
                scope.spawn(move || {
                    let mut out = Vec::new();
                    loop {
                        let i = cursor.fetch_add(1, Ordering::Relaxed);
                        match items.get(i) {
                            Some(item) => out.push((i, f(item))),
                            None => break,
                        }
                    }
                    out
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().expect("并行任务 panic"))
            .collect()
    });
    indexed.sort_by_key(|(i, _)| *i);
    indexed.into_iter().map(|(_, r)| r).collect()
}

/// 一折的训练窗与测试窗（半开区间 [start, end)，单位为毫秒时间戳）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fold {
    pub train_start: u64,
    pub train_end: u64,
    pub test_start: u64,
    pub test_end: u64,
}

/// 划分滚动前进验证的各折窗口。
///
/// 记总跨度 T = span_end - span_start、训练占比 r、折数 N：
/// 训练窗长 = r·T，测试窗长 = (1-r)·T / N，每折向前推进一个测试窗长，
/// 于是 N 折的测试窗首尾相接、恰好铺满尾部 (1-r)·T，既不重叠也不留空。
///
/// - `anchored = false`（滚动窗）：训练窗定长向前滑动，只用最近 r·T 的历史
/// - `anchored = true`（扩张窗）：训练窗起点固定在最早，历史越用越多
///
/// **训练窗与测试窗严格不重叠**，这是样本外结论成立的前提。
pub fn fold_windows(
    span_start: u64,
    span_end: u64,
    folds: usize,
    train_ratio: f64,
    anchored: bool,
) -> Result<Vec<Fold>, String> {
    if folds == 0 {
        return Err("折数必须 >= 1".into());
    }
    if !(0.0..1.0).contains(&train_ratio) || train_ratio <= 0.0 {
        return Err("训练占比应在 (0,1) 之间".into());
    }
    if span_end <= span_start {
        return Err("数据时间跨度为空".into());
    }
    let total = (span_end - span_start) as f64;
    let train_len = total * train_ratio;
    let test_len = total * (1.0 - train_ratio) / folds as f64;
    if test_len <= 0.0 {
        return Err("测试窗长度为 0，请减少折数或降低训练占比".into());
    }
    let mut out = Vec::with_capacity(folds);
    for i in 0..folds {
        let offset = test_len * i as f64;
        let train_end = span_start as f64 + train_len + offset;
        let train_start = if anchored {
            span_start as f64
        } else {
            span_start as f64 + offset
        };
        // 末折的测试窗右端对齐到数据末尾，避免浮点累积误差留下缝隙
        let test_end = if i + 1 == folds {
            span_end
        } else {
            (train_end + test_len) as u64
        };
        out.push(Fold {
            train_start: train_start as u64,
            train_end: train_end as u64,
            test_start: train_end as u64,
            test_end,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: u64 = 86_400_000;

    #[test]
    fn test_parse_grid_default_and_errors() {
        let d = parse_grid(None).unwrap();
        assert_eq!(d.len(), 2, "默认网格为动量天数 × 调仓间隔");
        assert_eq!(d[0].1.len() * d[1].1.len(), 9, "默认应为 9 组，兼容旧版");

        let g = parse_grid(Some("momentum_days=30,60;trailing_stop=0,0.08")).unwrap();
        assert_eq!(g[0].0, "momentum_days");
        assert_eq!(g[1].1, vec![0.0, 0.08]);

        assert!(parse_grid(Some("momentum_days")).is_err(), "缺少 = 应报错");
        assert!(parse_grid(Some("nope=1")).is_err(), "未知维度应报错");
        assert!(parse_grid(Some("momentum_days=abc")).is_err(), "非法数值应报错");
        assert!(parse_grid(Some("momentum_days=")).is_err(), "无取值应报错");
    }

    #[test]
    fn test_apply_grid_param_covers_all_supported_dims() {
        // 每个声明支持的维度都必须真的改到配置，否则扫描该维度会是无声的空转。
        // 用「两个不同取值产生不同配置」判定，避免与某个维度的默认值碰巧相等导致假阴性。
        let base = AppConfig::default();
        for dim in SUPPORTED_DIMS {
            let mut lo = base.clone();
            let mut hi = base.clone();
            apply_grid_param(&mut lo, dim, 3.0);
            apply_grid_param(&mut hi, dim, 11.0);
            assert_ne!(
                format!("{:?}", lo),
                format!("{:?}", hi),
                "维度 {dim} 的不同取值未对配置产生差异（该维度未接线）"
            );
        }
    }

    #[test]
    fn test_trailing_stop_zero_disables() {
        let mut cfg = AppConfig::default();
        apply_grid_param(&mut cfg, "trailing_stop", 0.0);
        assert!(!cfg.trailing_stop_enabled, "0 应关闭追踪止损而非设为 0%");
        apply_grid_param(&mut cfg, "trailing_stop", 0.1);
        assert!(cfg.trailing_stop_enabled);
    }

    #[test]
    fn test_enumerate_combos_covers_cartesian_product() {
        let grid = vec![
            ("momentum_days".to_string(), vec![30.0, 60.0, 90.0]),
            ("top_n".to_string(), vec![1.0, 2.0]),
        ];
        let combos = enumerate_combos(&AppConfig::default(), &grid);
        assert_eq!(combos.len(), 6, "3 × 2 应枚举 6 组");
        let seen: Vec<(usize, usize)> = combos
            .iter()
            .map(|(c, _)| (c.momentum_days, c.top_n))
            .collect();
        for m in [30usize, 60, 90] {
            for n in [1usize, 2] {
                assert!(seen.contains(&(m, n)), "缺少组合 ({m},{n})");
            }
        }
    }

    #[test]
    fn test_par_map_preserves_order() {
        let items: Vec<usize> = (0..1000).collect();
        let out = par_map(&items, |i| i * 2);
        assert_eq!(out.len(), 1000);
        for (i, v) in out.iter().enumerate() {
            assert_eq!(*v, i * 2, "并行结果必须按输入顺序归位");
        }
        assert!(par_map::<usize, usize, _>(&[], |i| *i).is_empty());
    }

    #[test]
    fn test_fold_windows_tile_without_overlap() {
        // 100 天、5 折、训练占比 0.5：训练窗 50 天，测试窗 (100-50)/5 = 10 天
        let folds = fold_windows(0, 100 * DAY, 5, 0.5, false).unwrap();
        assert_eq!(folds.len(), 5);

        assert_eq!(folds[0].train_start, 0);
        assert_eq!(folds[0].train_end, 50 * DAY);
        assert_eq!(folds[0].test_start, 50 * DAY);
        assert_eq!(folds[0].test_end, 60 * DAY);

        // 逐折向前推进一个测试窗长；训练窗与测试窗不得重叠
        for (i, f) in folds.iter().enumerate() {
            assert!(f.train_end <= f.test_start, "第{i}折训练窗与测试窗重叠");
            assert!(f.train_start < f.train_end, "第{i}折训练窗为空");
            assert!(f.test_start < f.test_end, "第{i}折测试窗为空");
        }
        // 测试窗首尾相接，铺满尾部 50 天
        for w in folds.windows(2) {
            assert_eq!(w[0].test_end, w[1].test_start, "测试窗之间出现缝隙或重叠");
        }
        assert_eq!(folds.last().unwrap().test_end, 100 * DAY, "末折应对齐数据末尾");
    }

    #[test]
    fn test_fold_windows_rolling_vs_anchored() {
        let rolling = fold_windows(0, 100 * DAY, 5, 0.5, false).unwrap();
        let anchored = fold_windows(0, 100 * DAY, 5, 0.5, true).unwrap();
        // 滚动窗：训练起点随折数前移，窗长恒定
        assert_eq!(rolling[0].train_start, 0);
        assert_eq!(rolling[4].train_start, 40 * DAY);
        let len0 = rolling[0].train_end - rolling[0].train_start;
        let len4 = rolling[4].train_end - rolling[4].train_start;
        assert_eq!(len0, len4, "滚动窗训练长度应恒定");
        // 扩张窗：训练起点恒为最早，窗长递增
        assert!(anchored.iter().all(|f| f.train_start == 0));
        assert!(
            anchored[4].train_end - anchored[4].train_start
                > anchored[0].train_end - anchored[0].train_start
        );
        // 两种模式的测试窗必须完全一致（只有训练范围不同）
        for (r, a) in rolling.iter().zip(&anchored) {
            assert_eq!((r.test_start, r.test_end), (a.test_start, a.test_end));
        }
    }

    #[test]
    fn test_fold_windows_rejects_bad_input() {
        assert!(fold_windows(0, 100 * DAY, 0, 0.5, false).is_err(), "0 折应报错");
        assert!(fold_windows(0, 100 * DAY, 5, 0.0, false).is_err(), "占比 0 应报错");
        assert!(fold_windows(0, 100 * DAY, 5, 1.0, false).is_err(), "占比 1 应报错");
        assert!(fold_windows(100, 100, 5, 0.5, false).is_err(), "空跨度应报错");
    }

    #[test]
    fn test_single_fold_equals_simple_split() {
        // 1 折时应完全等价于 sweep --split：前 70% 训练、后 30% 测试
        let f = fold_windows(0, 100 * DAY, 1, 0.7, false).unwrap();
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].train_start, 0);
        assert_eq!(f[0].train_end, 70 * DAY);
        assert_eq!(f[0].test_start, 70 * DAY);
        assert_eq!(f[0].test_end, 100 * DAY);
    }
}
