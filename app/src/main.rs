//! quantkit CLI 运行器。
//!
//! 子命令：
//! - backtest：单策略回测（--strategy momentum|trend|ma_cross|grid|dca，--json 输出，--start/--end 窗口）
//! - sweep：通用参数网格扫描（--grid 任意维度，--split 训练/测试切分，--json JSONL）
//! - walkforward：滚动前进验证（多折「训练选参 → 紧随其后的测试窗验收」，输出真实样本外表现）
//! - dry-run / live：模拟盘 / 实盘
//! - serve：启动 WebUI 量化交易平台

use quantkit_app::optimize::{
    self, apply_grid_param, enumerate_combos, parse_grid, time_range, Fold, MAX_COMBOS,
};
use quantkit_app::{
    backtest_config, benchmark, build_strategy, config, data, dryrun, history_window, live,
};
use quantkit_core::engine::{run_backtest, BacktestConfig};
use quantkit_core::interval::Interval;
use quantkit_core::metrics::BacktestMetrics;
use quantkit_core::types::Kline;
use std::collections::BTreeMap;
use std::path::PathBuf;

#[tokio::main]
async fn main() {
    env_logger::init();
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(String::as_str).unwrap_or("help");

    let mut cfg = config::load_config(get_arg(&args, "--config").as_deref());
    
    // Debug: print ws_enabled after loading config
    if args.get(1).map(String::as_str) == Some("live") {
        eprintln!("[DEBUG] Config loaded: ws_enabled={}, ws_only_closed_bars={}", cfg.ws_enabled, cfg.ws_only_closed_bars);
    }
    
    let symbols_explicit = args.iter().any(|a| a == "--symbols");
    // 周期只解析一次，全链路复用（非法值直接退出）
    let interval = apply_cli_overrides(&args, &mut cfg);
    // CLI 未显式指定品种、且配置文件也未定义 symbols 时，才自动发现数据目录下的全部品种。
    // 配置显式定义的品种池优先（此前自动发现会静默覆盖配置，导致 trend 等
    // 依赖 symbols.first() 的策略交易到错误品种）。
    if !symbols_explicit && get_arg(&args, "--config").is_none() && !cfg.symbols_from_file {
        match data::discover_symbols(&cfg.data_dir, interval) {
            Ok(found) if !found.is_empty() => cfg.symbols = found,
            _ => {}
        }
    }

    match cmd {
        "backtest" => run_backtest_cmd(&cfg, &args, interval),
        "sweep" => run_sweep(&cfg, &args, interval),
        "walkforward" => run_walkforward(&cfg, &args, interval),
        "dry-run" => dryrun::run(&cfg, interval).await,
        "live" => live::run(&cfg, interval).await,
        "serve" => run_serve(&cfg),
        _ => print_usage(),
    }
}

/// 委托启动独立二进制 quantkit-web（WebUI 是单独 crate，避免循环依赖）
fn run_serve(cfg: &config::AppConfig) {
    let bin = std::env::var("QUANTKIT_WEB_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::current_exe()
                .ok()
                .and_then(|exe| exe.parent().map(|d| d.join("quantkit-web")))
                .filter(|p| p.exists())
                .unwrap_or_else(|| PathBuf::from("./quantkit-web"))
        });
    let status = std::process::Command::new(&bin)
        .arg("--port")
        .arg(cfg.web_port.to_string())
        .arg("--data")
        .arg(&cfg.data_dir)
        .status();
    match status {
        Ok(s) if s.success() => {}
        Ok(s) => eprintln!("quantkit-web 退出: {s}"),
        Err(e) => eprintln!("无法启动 quantkit-web({}): {e}（请先 cargo build --workspace）", bin.display()),
    }
}

fn run_backtest_cmd(cfg: &config::AppConfig, args: &[String], interval: Interval) {
    let json = args.iter().any(|a| a == "--json");
    let mut data = match data::load_data(&cfg.data_dir, &cfg.symbols, interval) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("数据加载失败: {e}");
            return;
        }
    };
    // --start/--end 回测窗口（供样本内外切分与定点回测使用）
    let window = window_from_args(args);
    match window {
        Ok((s, e)) => data::slice_data(&mut data, s, e),
        Err(e) => {
            eprintln!("{e}");
            return;
        }
    }
    let mut strategy = build_strategy(cfg, interval);
    let bt = backtest_config(cfg, interval);
    match run_backtest(strategy.as_mut(), &data, &bt) {
        Ok(r) => {
            // --benchmark 基准对比：加载基准品种同窗口数据构建买入持有曲线；
            // 数据缺失只警告不中断（基准是增强信息，不是回测前置条件）
            let bench_report = get_arg(args, "--benchmark").and_then(|sym| {
                let sym = sym.trim().to_uppercase();
                let klines = match data::load_data(&cfg.data_dir, std::slice::from_ref(&sym), interval) {
                    Ok(mut d) => {
                        if let Ok((s, e)) = window {
                            data::slice_data(&mut d, s, e);
                        }
                        d.remove(&sym)
                    }
                    Err(e) => {
                        eprintln!("[基准] {sym} 数据加载失败，已跳过基准对比: {e}");
                        None
                    }
                };
                match klines {
                    Some(ks) => {
                        let rep = benchmark::build_report(
                            &sym, &ks, &r.equity_curve, &r.metrics, bt.initial_cash,
                        );
                        if rep.is_none() {
                            eprintln!("[基准] {sym} 与回测窗口无重叠数据，已跳过基准对比");
                        }
                        rep
                    }
                    None => None,
                }
            });
            if json {
                let mut out = serde_json::json!({
                    "strategy": r.strategy_name,
                    "symbols": cfg.symbols,
                    "bars": r.equity_curve.len(),
                    "metrics": r.metrics,
                    "equity_curve": r.equity_curve,
                    "trades": r.trades,
                    "final_positions": r.final_positions,
                    "final_cash": r.final_cash,
                });
                if let Some(b) = &bench_report {
                    out["benchmark"] = serde_json::json!(b);
                }
                println!("{}", serde_json::to_string(&out).expect("json 序列化"));
                return;
            }
            println!("策略: {}", r.strategy_name);
            println!("品种: {:?} | 共 {} 根bar", cfg.symbols, r.equity_curve.len());
            print_metrics(&r.metrics);
            if let Some(b) = &bench_report {
                println!("基准({}) 买入持有:", b.symbol);
                println!("  总收益率:     {:.1}%", b.metrics.total_return_pct);
                println!("  年化收益率:   {:.1}%", b.metrics.annualized_return_pct);
                println!("  最大回撤:     {:.1}%", b.metrics.max_drawdown_pct);
                println!("  年化超额:     {:+.1}%", b.excess_annualized_pct);
                println!("  β/相关系数:   {} / {}", opt_txt(b.beta), opt_txt(b.correlation));
                println!("  信息比率:     {}", opt_txt(b.information_ratio));
            }
            if std::env::args().any(|a| a == "--verbose") {
                if r.dropped_tail_orders > 0 {
                    println!();
                    println!(
                        "尾部作废订单: {} 笔（最后一根bar的信号无下一根可成交，属正常收尾）",
                        r.dropped_tail_orders
                    );
                }
                println!();
                println!("逐笔回合（净盈亏已扣手续费）:");
                for (i, t) in r.trades.iter().enumerate() {
                    println!(
                        "  #{} {} {} 入{:.4} -> 出{:.4} 净{:+.2}",
                        i + 1, t.entry_time, t.symbol, t.entry_price, t.exit_price, t.pnl
                    );
                }
            }
        }
        Err(e) => eprintln!("回测失败: {e}"),
    }
}

fn run_sweep(cfg: &config::AppConfig, args: &[String], interval: Interval) {
    let json = args.iter().any(|a| a == "--json");
    let mut data = match data::load_data(&cfg.data_dir, &cfg.symbols, interval) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("数据加载失败: {e}");
            return;
        }
    };
    match window_from_args(args) {
        Ok((s, e)) => data::slice_data(&mut data, s, e),
        Err(e) => {
            eprintln!("{e}");
            return;
        }
    }
    if data.is_empty() {
        eprintln!("回测窗口内无可用数据");
        return;
    }

    // 网格：--grid "momentum_days=30,60;trailing_stop=0,0.08"；未指定时默认网格（向后兼容）
    let grid = match parse_grid(get_arg(args, "--grid").as_deref()) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("网格解析失败: {e}");
            return;
        }
    };
    let total: usize = grid.iter().map(|(_, v)| v.len()).product();
    if total > MAX_COMBOS {
        eprintln!("网格组合数 {total} 超过上限 {MAX_COMBOS}，请减少维度取值");
        return;
    }

    // 训练/测试切分：--split 0.7 按时间前 70% 选参、后 30% 验收，防过拟合

    let split_ms = match get_arg(args, "--split").map(|s| s.parse::<f64>()) {
        Some(Ok(r)) if (0.0..1.0).contains(&r) => {
            let (mn, mx) = time_range(&data);
            Some(mn + ((mx - mn) as f64 * r) as u64)
        }
        Some(_) => {
            eprintln!("--split 应为 (0,1) 之间的小数");
            return;
        }
        None => None,
    };
    let has_split = split_ms.is_some();
    let mut train = data.clone();
    data::slice_data(&mut train, None, split_ms);
    let mut test = data.clone();
    data::slice_data(&mut test, split_ms, None);

    let dims: Vec<String> = grid.iter().map(|(k, _)| k.clone()).collect();

    // 先枚举全部组合（混合进制进位遍历），再并行评估：
    // 枚举本身极快，回测才是热点，拆开后才能把 CPU 全部用上。
    let mut combos: Vec<(config::AppConfig, serde_json::Map<String, serde_json::Value>)> =
        Vec::with_capacity(total);
    let mut idx = vec![0usize; grid.len()];
    for _ in 0..total {
        let mut c = cfg.clone();
        let mut params = serde_json::Map::new();
        for (gi, (dim, values)) in grid.iter().enumerate() {
            let v = values[idx[gi]];
            apply_grid_param(&mut c, dim, v);
            params.insert(dim.clone(), serde_json::json!(v));
        }
        combos.push((c, params));
        // 混合进制进位递增遍历所有组合（mixed-radix carry）
        for gi in (0..grid.len()).rev() {
            idx[gi] += 1;
            if idx[gi] < grid[gi].1.len() {
                break;
            }
            idx[gi] = 0;
        }
    }

    // 并行评估：共享游标动态领取任务。静态等分会被最慢的核（能效核）拖住整体耗时，
    // 且不同参数组合的热身期长度不同、工作量本就不均等。
    // 各线程收集 (索引, 结果)，汇总后按索引排序 —— 输出与串行版本逐行一致。
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .min(combos.len().max(1));
    if !json {
        eprintln!("[sweep] {} 个参数组合 / {} 线程并行评估", total, threads);
    }
    let cursor = std::sync::atomic::AtomicUsize::new(0);
    let combos_ref = &combos;
    let mut indexed: Vec<(usize, serde_json::Value)> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                let (train, test, cursor) = (&train, &test, &cursor);
                scope.spawn(move || {
                    let mut out = Vec::new();
                    loop {
                        let i = cursor.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let Some((c, params)) = combos_ref.get(i) else {
                            break;
                        };
                        out.push((i, eval_combo(c, params, train, test, has_split, interval)));
                    }
                    out
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().expect("sweep 线程 panic"))
            .collect()
    });
    indexed.sort_by_key(|(i, _)| *i);
    let mut rows: Vec<serde_json::Value> = indexed.into_iter().map(|(_, r)| r).collect();

    // 排名：切分模式按训练集年化、否则按全期年化（降序）
    rows.sort_by(|a, b| {
        rank_key(b).partial_cmp(&rank_key(a)).unwrap_or(std::cmp::Ordering::Equal)
    });

    if json {
        for r in &rows {
            println!("{}", serde_json::to_string(r).expect("json 序列化"));
        }
        return;
    }
    print_sweep_table(&dims, &rows, has_split);
}

/// 选参指标：决定「训练窗上哪组参数算最优」。
///
/// 默认按年化收益选参本身就是过拟合陷阱（最高收益往往来自极端参数），
/// 所以允许按夏普/Calmar/Sortino 这类风险调整后指标选参。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RankMetric {
    Annualized,
    Sharpe,
    Calmar,
    Sortino,
}

impl RankMetric {
    fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "annualized" | "annual" => Ok(RankMetric::Annualized),
            "sharpe" => Ok(RankMetric::Sharpe),
            "calmar" => Ok(RankMetric::Calmar),
            "sortino" => Ok(RankMetric::Sortino),
            other => Err(format!(
                "不支持的选参指标 '{other}'，可选：annualized / sharpe / calmar / sortino"
            )),
        }
    }

    fn label(self) -> &'static str {
        match self {
            RankMetric::Annualized => "年化%",
            RankMetric::Sharpe => "夏普",
            RankMetric::Calmar => "Calmar",
            RankMetric::Sortino => "Sortino",
        }
    }

    /// 从指标中取值；无定义（如无回撤时的 Calmar）按最差处理，避免被选中
    fn value(self, m: &BacktestMetrics) -> f64 {
        match self {
            RankMetric::Annualized => m.annualized_return_pct,
            RankMetric::Sharpe => m.sharpe_ratio,
            RankMetric::Calmar => m.calmar_ratio.unwrap_or(f64::MIN),
            RankMetric::Sortino => m.sortino_ratio.unwrap_or(f64::MIN),
        }
    }
}

/// 滚动前进验证：多折「训练窗选参 → 紧随其后的测试窗验收」。
///
/// 为什么需要它：`sweep --split` 只切一次，选出的参数可能只是在那一段行情里
/// 运气好。滚动多折会反复检验「按这套流程定期重新调参」在**从未参与选参的
/// 数据**上表现如何——这才是接近实盘的估计。折数越多、每折测试窗越短，
/// 结论越接近「参数是否稳定可用」而非「参数是否恰好拟合了这段历史」。
fn run_walkforward(cfg: &config::AppConfig, args: &[String], interval: Interval) {
    let json = args.iter().any(|a| a == "--json");
    let mut data = match data::load_data(&cfg.data_dir, &cfg.symbols, interval) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("数据加载失败: {e}");
            return;
        }
    };
    match window_from_args(args) {
        Ok((s, e)) => data::slice_data(&mut data, s, e),
        Err(e) => {
            eprintln!("{e}");
            return;
        }
    }
    if data.is_empty() {
        eprintln!("回测窗口内无可用数据");
        return;
    }

    let folds_n = match get_arg(args, "--windows").map(|s| s.parse::<usize>()) {
        Some(Ok(n)) if n >= 1 => n,
        Some(_) => {
            eprintln!("--windows 应为 >= 1 的整数");
            return;
        }
        None => 5,
    };
    let train_ratio = match get_arg(args, "--train-ratio").map(|s| s.parse::<f64>()) {
        Some(Ok(r)) => r,
        Some(_) => {
            eprintln!("--train-ratio 应为 (0,1) 之间的小数");
            return;
        }
        None => 0.7,
    };
    let anchored = args.iter().any(|a| a == "--anchored");
    let rank = match get_arg(args, "--rank").map(|s| RankMetric::parse(&s)) {
        Some(Ok(r)) => r,
        Some(Err(e)) => {
            eprintln!("{e}");
            return;
        }
        None => RankMetric::Annualized,
    };

    let grid = match parse_grid(get_arg(args, "--grid").as_deref()) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("网格解析失败: {e}");
            return;
        }
    };
    let total: usize = grid.iter().map(|(_, v)| v.len()).product();
    if total > MAX_COMBOS {
        eprintln!("网格组合数 {total} 超过上限 {MAX_COMBOS}，请减少维度取值");
        return;
    }

    let (span_start, span_end) = time_range(&data);
    let folds = match optimize::fold_windows(span_start, span_end, folds_n, train_ratio, anchored) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("窗口划分失败: {e}");
            return;
        }
    };
    let combos = enumerate_combos(cfg, &grid);
    let dims: Vec<String> = grid.iter().map(|(k, _)| k.clone()).collect();

    if !json {
        eprintln!(
            "[walkforward] {} 折 / {} 窗 / 训练占比 {:.0}% / 按 {} 选参 / 每折 {} 组合",
            folds_n,
            if anchored { "扩张" } else { "滚动" },
            train_ratio * 100.0,
            rank.label(),
            total
        );
    }

    let mut rows: Vec<serde_json::Value> = Vec::with_capacity(folds.len());
    for (i, fold) in folds.iter().enumerate() {
        let Some(result) = eval_fold(&combos, &dims, &data, fold, rank, interval) else {
            eprintln!(
                "[walkforward] 第 {} 折数据不足（训练 {}~{} / 测试 {}~{}），已跳过",
                i + 1,
                data::format_date(fold.train_start),
                data::format_date(fold.train_end),
                data::format_date(fold.test_start),
                data::format_date(fold.test_end),
            );
            continue;
        };
        rows.push(result);
    }

    if rows.is_empty() {
        eprintln!("所有折均无有效结果：请缩小折数或放宽回测窗口");
        return;
    }

    let summary = walkforward_summary(&rows, rank, &folds);
    if json {
        for r in &rows {
            println!("{}", serde_json::to_string(r).expect("json 序列化"));
        }
        println!("{}", serde_json::to_string(&summary).expect("json 序列化"));
        return;
    }
    print_walkforward_table(&rows, rank);
    print_walkforward_summary(&summary, rank);
}

/// 评估一折：训练窗并行跑全网格取最优参数，再用该参数在测试窗上验收。
/// 训练窗或测试窗回测不出结果（数据不足）时返回 None。
fn eval_fold(
    combos: &[(config::AppConfig, Vec<(String, f64)>)],
    dims: &[String],
    data: &BTreeMap<String, Vec<Kline>>,
    fold: &Fold,
    rank: RankMetric,
    interval: Interval,
) -> Option<serde_json::Value> {
    let mut train = data.clone();
    data::slice_data(&mut train, Some(fold.train_start), Some(fold.train_end));
    if train.is_empty() {
        return None;
    }

    // 训练窗：并行评估全部组合
    let train_metrics: Vec<Option<BacktestMetrics>> = optimize::par_map(combos, |(c, _)| {
        let bt = backtest_config(c, interval);
        let mut s = build_strategy(c, interval);
        run_backtest(s.as_mut(), &train, &bt).ok().map(|r| r.metrics)
    });

    // 取训练窗最优（指标无定义的组合已在 RankMetric::value 中垫底）
    let (best_i, best_train) = train_metrics
        .iter()
        .enumerate()
        .filter_map(|(i, m)| m.as_ref().map(|m| (i, m)))
        .max_by(|(_, a), (_, b)| {
            rank.value(a)
                .partial_cmp(&rank.value(b))
                .unwrap_or(std::cmp::Ordering::Equal)
        })?;

    // 测试窗：只用训练窗选出的这一组参数，绝不回头看测试结果再改选择
    let (best_cfg, best_params) = &combos[best_i];

    // 测试窗必须带上前置热身历史：否则策略在测试窗内热身都来不及完成，
    // 90 日动量这类长回看策略会一笔不交易，样本外收益被系统性低估为 0。
    // signal_start_ts 让引擎区分两段——热身段只喂历史、不交易、不计入权益曲线，
    // 因此指标仍只反映测试窗内、且以完整初始资金起步的表现。
    // 热身长度按**选中的那组参数**算：不同参数的回看需求不同。
    // 训练窗不作此处理：它本身足够长，且要与 sweep 的口径保持一致。
    let warmup_ms = history_window(best_cfg, interval) as u64 * interval.ms();
    let mut test = data.clone();
    data::slice_data(
        &mut test,
        Some(fold.test_start.saturating_sub(warmup_ms)),
        Some(fold.test_end),
    );
    if test.is_empty() {
        return None;
    }
    let bt = BacktestConfig {
        signal_start_ts: fold.test_start,
        ..backtest_config(best_cfg, interval)
    };
    let mut s = build_strategy(best_cfg, interval);
    let test_m = run_backtest(s.as_mut(), &test, &bt).ok().map(|r| r.metrics)?;

    let params: serde_json::Map<String, serde_json::Value> = dims
        .iter()
        .zip(best_params.iter().map(|(_, v)| *v))
        .map(|(d, v)| (d.clone(), serde_json::json!(v)))
        .collect();
    Some(serde_json::json!({
        "train_start": data::format_date(fold.train_start),
        "train_end": data::format_date(fold.train_end),
        "test_start": data::format_date(fold.test_start),
        "test_end": data::format_date(fold.test_end),
        "params": params,
        "train": best_train,
        "test": test_m,
    }))
}

/// 汇总各折的样本外表现。
///
/// 核心口径：把每折测试窗的总收益按顺序复利，等价于「每到窗口边界就按最近
/// 训练窗重新调参，资金连续滚动」的真实过程。这是本命令唯一值得看的收益数字。
fn walkforward_summary(
    rows: &[serde_json::Value],
    rank: RankMetric,
    folds: &[Fold],
) -> serde_json::Value {
    let mut compounded = 1.0_f64;
    let mut test_vals = Vec::new();
    let mut train_vals = Vec::new();
    let mut positive = 0usize;
    for r in rows {
        let ret = num(&r["test"], "total_return_pct") / 100.0;
        compounded *= 1.0 + ret;
        if ret > 0.0 {
            positive += 1;
        }
        test_vals.push(num_by_metric(&r["test"], rank));
        train_vals.push(num_by_metric(&r["train"], rank));
    }
    let oos_total_pct = (compounded - 1.0) * 100.0;

    // 样本外年化：按各折测试窗合计跨度折算
    let oos_days: f64 = folds
        .iter()
        .take(rows.len())
        .map(|f| (f.test_end.saturating_sub(f.test_start)) as f64 / 86_400_000.0)
        .sum();
    let oos_annualized_pct = if oos_days > 1.0 && compounded > 0.0 {
        (compounded.powf(365.0 / oos_days) - 1.0) * 100.0
    } else {
        0.0
    };

    let mean = |v: &[f64]| -> f64 {
        if v.is_empty() {
            0.0
        } else {
            v.iter().sum::<f64>() / v.len() as f64
        }
    };
    let train_mean = mean(&train_vals);
    let test_mean = mean(&test_vals);
    serde_json::json!({
        "summary": true,
        "folds": rows.len(),
        "oos_total_return_pct": oos_total_pct,
        "oos_annualized_pct": oos_annualized_pct,
        "oos_days": oos_days,
        "positive_folds": positive,
        "rank_metric": rank.label(),
        "train_mean": train_mean,
        "test_mean": test_mean,
        // 过拟合差距：训练窗选参时看到的指标 与 样本外实际拿到的指标 之差
        "overfit_gap": train_mean - test_mean,
    })
}

/// 按选参指标从 JSON 指标块取值（与 RankMetric::value 保持一致的字段名）
fn num_by_metric(m: &serde_json::Value, rank: RankMetric) -> f64 {
    let key = match rank {
        RankMetric::Annualized => "annualized_return_pct",
        RankMetric::Sharpe => "sharpe_ratio",
        RankMetric::Calmar => "calmar_ratio",
        RankMetric::Sortino => "sortino_ratio",
    };
    m.get(key).and_then(|v| v.as_f64()).unwrap_or(0.0)
}

fn print_walkforward_table(rows: &[serde_json::Value], rank: RankMetric) {
    println!();
    println!(
        "{:<4} {:<24} {:<24} {:<34} {:>10} {:>10} {:>10}",
        "折",
        "训练区间（选参）",
        "测试区间（样本外）",
        "选中参数",
        format!("训练{}", rank.label()),
        format!("测试{}", rank.label()),
        "测试收益%",
    );
    println!("{}", "-".repeat(122));
    for (i, r) in rows.iter().enumerate() {
        let params = r["params"]
            .as_object()
            .map(|o| {
                o.iter()
                    .map(|(k, v)| format!("{k}={}", trim_num(v.as_f64().unwrap_or(0.0))))
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .unwrap_or_default();
        println!(
            "{:<4} {:<24} {:<24} {:<34} {:>10.2} {:>10.2} {:>10.1}",
            i + 1,
            format!(
                "{}~{}",
                r["train_start"].as_str().unwrap_or("-"),
                r["train_end"].as_str().unwrap_or("-")
            ),
            format!(
                "{}~{}",
                r["test_start"].as_str().unwrap_or("-"),
                r["test_end"].as_str().unwrap_or("-")
            ),
            params,
            num_by_metric(&r["train"], rank),
            num_by_metric(&r["test"], rank),
            num(&r["test"], "total_return_pct"),
        );
    }
}

fn print_walkforward_summary(s: &serde_json::Value, rank: RankMetric) {
    let folds = s["folds"].as_u64().unwrap_or(0);
    let positive = s["positive_folds"].as_u64().unwrap_or(0);
    println!();
    println!("样本外汇总（净利润口径，逐折复利）");
    println!(
        "  样本外总收益: {:.1}%（覆盖 {:.0} 天）",
        s["oos_total_return_pct"].as_f64().unwrap_or(0.0),
        s["oos_days"].as_f64().unwrap_or(0.0)
    );
    println!(
        "  样本外年化:   {:.1}%",
        s["oos_annualized_pct"].as_f64().unwrap_or(0.0)
    );
    println!(
        "  测试窗胜率:   {}/{} 折为正",
        positive, folds
    );
    let gap = s["overfit_gap"].as_f64().unwrap_or(0.0);
    println!(
        "  过拟合差距:   训练{} 均值 {:.2} − 测试均值 {:.2} = {:+.2}",
        rank.label(),
        s["train_mean"].as_f64().unwrap_or(0.0),
        s["test_mean"].as_f64().unwrap_or(0.0),
        gap
    );
    // 差距越大说明选出的参数越依赖训练窗的具体行情，实盘越可能失效
    if gap > 0.0 {
        println!("  ↑ 差距为正属正常（选参必然占训练窗便宜）；差距越大，参数越可能只是拟合了历史");
    }
}

/// 去掉整数值多余的小数位，让参数列更好读（30 而不是 30.0）
fn trim_num(v: f64) -> String {
    if (v - v.round()).abs() < 1e-9 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

/// 评估单个参数组合：跑训练集（切分模式下再跑测试集），组装为一行结果。
///
/// 只读借用行情数据，无共享可变状态 —— 因此可以在并行块中直接调用。
fn eval_combo(
    c: &config::AppConfig,
    params: &serde_json::Map<String, serde_json::Value>,
    train: &std::collections::BTreeMap<String, Vec<quantkit_core::types::Kline>>,
    test: &std::collections::BTreeMap<String, Vec<quantkit_core::types::Kline>>,
    has_split: bool,
    interval: Interval,
) -> serde_json::Value {
    let bt = backtest_config(c, interval);
    let mut s = build_strategy(c, interval);
    let train_m = run_backtest(s.as_mut(), train, &bt).ok().map(|r| r.metrics);
    let test_m = if has_split {
        let mut s2 = build_strategy(c, interval);
        run_backtest(s2.as_mut(), test, &bt).ok().map(|r| r.metrics)
    } else {
        None
    };
    let mut row = serde_json::json!({ "params": params });
    match train_m {
        Some(m) if has_split => {
            row["train"] = serde_json::json!(m);
            row["test"] = serde_json::json!(test_m);
        }
        Some(m) => {
            row["metrics"] = serde_json::json!(m);
        }
        None => {
            row["error"] = serde_json::json!("backtest failed");
        }
    }
    row
}

fn print_metrics(m: &BacktestMetrics) {
    println!("  总收益率(净): {:.1}%", m.total_return_pct);
    println!("  年化收益率:   {:.1}%", m.annualized_return_pct);
    println!("  最大回撤:     {:.1}%", m.max_drawdown_pct);
    println!("  夏普比率:     {:.2}", m.sharpe_ratio);
    println!("  Sortino比率:  {}", opt_txt(m.sortino_ratio));
    println!("  年化波动率:   {:.1}%", m.annualized_volatility_pct);
    println!("  最长回撤期:   {:.0} 天", m.max_drawdown_duration_days);
    println!("  Calmar比率:   {}", opt_txt(m.calmar_ratio));
    println!("  Profit Factor: {}", opt_txt(m.profit_factor));
    println!("  胜率/盈亏比:  {:.1}% / {}", m.win_rate_pct, opt_txt(m.payoff_ratio));
    println!("  暴露率:       {:.1}%", m.exposure_pct);
    println!("  交易回合:     {}", m.num_round_trips);
    println!("  累计手续费:   {:.2}", m.total_fees);
}

fn opt_txt(v: Option<f64>) -> String {
    v.map(|x| format!("{:.2}", x)).unwrap_or_else(|| "-".into())
}

/// 由 --start/--end 解析回测窗口（YYYY-MM-DD，UTC）
fn window_from_args(args: &[String]) -> Result<(Option<u64>, Option<u64>), String> {
    let start = get_arg(args, "--start")
        .map(|s| data::parse_date_ms(&s))
        .transpose()?;
    let end = get_arg(args, "--end")
        .map(|s| data::parse_date_ms(&s))
        .transpose()?;
    Ok((start, end))
}

/// 文本表格输出（非 --json 模式）：维度列 + 指标列；切分模式输出训练/测试双段对比
fn print_sweep_table(dims: &[String], rows: &[serde_json::Value], has_split: bool) {
    let mut head = String::new();
    for d in dims {
        head.push_str(&format!("{:>14}", d));
    }
    if has_split {
        head.push_str(&format!(
            " {:>9} {:>9} {:>9} {:>8} {:>6}",
            "tr_ann%", "te_ann%", "te_maxDD%", "te_shrp", "te_tr"
        ));
    } else {
        head.push_str(&format!(
            " {:>9} {:>9} {:>8} {:>7} {:>6} {:>7} {:>6}",
            "total%", "annual%", "maxDD%", "sharpe", "PF", "calmar", "win%"
        ));
    }
    println!("{}", head);
    for row in rows {
        let mut line = String::new();
        if let Some(p) = row.get("params").and_then(|p| p.as_object()) {
            for d in dims {
                let v = p.get(d).and_then(|x| x.as_f64()).unwrap_or(0.0);
                line.push_str(&format!("{:>14}", v));
            }
        }
        if row.get("error").is_some() {
            println!("{}  backtest failed", line);
            continue;
        }
        if has_split {
            let tr = &row["train"];
            let te = &row["test"];
            let te_tr = te
                .get("num_round_trips")
                .and_then(|v| v.as_u64())
                .map(|v| v.to_string())
                .unwrap_or_else(|| "-".into());
            line.push_str(&format!(
                " {:>9.1} {:>9.1} {:>9.1} {:>8.2} {:>6}",
                num(tr, "annualized_return_pct"),
                num(te, "annualized_return_pct"),
                num(te, "max_drawdown_pct"),
                num(te, "sharpe_ratio"),
                te_tr
            ));
        } else {
            let m = &row["metrics"];
            line.push_str(&format!(
                " {:>9.1} {:>9.1} {:>8.1} {:>7.2} {:>6} {:>7} {:>6.1}",
                num(m, "total_return_pct"),
                num(m, "annualized_return_pct"),
                num(m, "max_drawdown_pct"),
                num(m, "sharpe_ratio"),
                opt_str(m, "profit_factor"),
                opt_str(m, "calmar_ratio"),
                num(m, "win_rate_pct")
            ));
        }
        println!("{}", line);
    }
}

fn num(v: &serde_json::Value, key: &str) -> f64 {
    v.get(key).and_then(|x| x.as_f64()).unwrap_or(f64::NAN)
}

/// Option 字段（profit_factor/calmar 等）无值时输出 -
fn opt_str(v: &serde_json::Value, key: &str) -> String {
    match v.get(key).and_then(|x| x.as_f64()) {
        Some(x) => format!("{:.2}", x),
        None => "-".to_string(),
    }
}

/// 排名依据：切分模式取训练集年化、否则取全期年化；失败组合垫底
fn rank_key(row: &serde_json::Value) -> f64 {
    let sec = if row.get("train").is_some_and(|v| !v.is_null()) {
        &row["train"]
    } else {
        &row["metrics"]
    };
    sec.get("annualized_return_pct")
        .and_then(|v| v.as_f64())
        .unwrap_or(f64::MIN)
}

fn get_arg(args: &[String], key: &str) -> Option<String> {
    args.iter()
        .position(|a| a == key)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

/// 应用 CLI 覆盖并返回解析后的周期（`--interval` 也在此覆盖，故周期必须在覆盖之后解析）
fn apply_cli_overrides(args: &[String], cfg: &mut config::AppConfig) -> Interval {
    if let Some(v) = get_arg(args, "--data") {
        cfg.data_dir = v;
    }
    if let Some(v) = get_arg(args, "--strategy") {
        cfg.strategy = v;
    }
    if let Some(v) = get_arg(args, "--interval") {
        cfg.interval = v;
    }
    if let Some(v) = get_arg(args, "--cash") {
        if let Ok(v) = v.parse() {
            cfg.initial_cash = v;
        }
    }
    if let Some(v) = get_arg(args, "--symbols") {
        cfg.symbols = v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    }
    if let Some(v) = get_arg(args, "--momentum-days") {
        if let Ok(v) = v.parse() {
            cfg.momentum_days = v;
        }
    }
    if let Some(v) = get_arg(args, "--ma-days") {
        if let Ok(v) = v.parse() {
            cfg.ma_days = v;
        }
    }
    if let Some(v) = get_arg(args, "--rebalance-days") {
        if let Ok(v) = v.parse() {
            cfg.rebalance_days = v;
        }
    }
    if let Some(v) = get_arg(args, "--trailing-stop") {
        if let Ok(v) = v.parse() {
            cfg.trailing_stop_pct = v;
            cfg.trailing_stop_enabled = v > 0.0;
        }
    }
    if args.iter().any(|a| a == "--no-trailing-stop") {
        cfg.trailing_stop_enabled = false;
    }
    if let Some(v) = get_arg(args, "--fill") {
        cfg.fill_mode = v;
    }
    if let Some(v) = get_arg(args, "--fee") {
        if let Ok(v) = v.parse() {
            cfg.fee_rate = v;
        }
    }
    if let Some(v) = get_arg(args, "--cooldown-days") {
        if let Ok(v) = v.parse() {
            cfg.cooldown_days = v;
        }
    }
    if let Some(v) = get_arg(args, "--top-n") {
        if let Ok(v) = v.parse::<usize>() {
            cfg.top_n = v.max(1);
        }
    }
    if let Some(v) = get_arg(args, "--regime-ma") {
        if let Ok(v) = v.parse() {
            cfg.regime_ma_days = v;
        }
    }
    if let Some(v) = get_arg(args, "--regime-breadth") {
        if let Ok(v) = v.parse() {
            cfg.regime_min_breadth = v;
        }
    }
    if let Some(v) = get_arg(args, "--ma-fast") {
        if let Ok(v) = v.parse() {
            cfg.ma_cross_fast = v;
        }
    }
    if let Some(v) = get_arg(args, "--ma-slow") {
        if let Ok(v) = v.parse() {
            cfg.ma_cross_slow = v;
        }
    }
    if let Some(v) = get_arg(args, "--grid-levels") {
        if let Ok(v) = v.parse() {
            cfg.grid_levels = v;
        }
    }
    if let Some(v) = get_arg(args, "--grid-lookback-days") {
        if let Ok(v) = v.parse() {
            cfg.grid_lookback_days = v;
        }
    }
    if let Some(v) = get_arg(args, "--grid-stop-loss") {
        if let Ok(v) = v.parse() {
            cfg.grid_stop_loss_pct = v;
        }
    }
    if let Some(v) = get_arg(args, "--grid-budget") {
        if let Ok(v) = v.parse() {
            cfg.grid_budget_per_symbol = v;
        }
    }
    if let Some(v) = get_arg(args, "--dca-amount") {
        if let Ok(v) = v.parse() {
            cfg.dca_amount = v;
        }
    }
    if let Some(v) = get_arg(args, "--dca-interval-days") {
        if let Ok(v) = v.parse() {
            cfg.dca_interval_days = v;
        }
    }
    if let Some(v) = get_arg(args, "--dca-ma-days") {
        if let Ok(v) = v.parse() {
            cfg.dca_ma_days = v;
        }
    }
    if let Some(v) = get_arg(args, "--dca-dip-multiplier") {
        if let Ok(v) = v.parse() {
            cfg.dca_dip_multiplier = v;
        }
    }
    if let Some(v) = get_arg(args, "--circuit-breaker") {
        if let Ok(v) = v.parse() {
            cfg.circuit_breaker_pct = v;
        }
    }
    if let Some(v) = get_arg(args, "--circuit-cooldown") {
        if let Ok(v) = v.parse() {
            cfg.circuit_breaker_cooldown_days = v;
        }
    }
    if let Some(v) = get_arg(args, "--port") {
        if let Ok(v) = v.parse() {
            cfg.web_port = v;
        }
    }
    let interval = cfg.interval_or_exit();
    if cfg.symbols.is_empty() {
        cfg.symbols = match data::discover_symbols(&cfg.data_dir, interval) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("品种发现失败: {e}");
                Vec::new()
            }
        };
    }
    interval
}

fn print_usage() {
    println!("quantkit - Rust 通用加密货币量化交易平台");
    println!();
    println!("用法:");
    println!("  quantkit backtest [--data DIR] [--strategy momentum|trend|ma_cross|grid|dca] [--symbols A,B]");
    println!("                    [--interval 1d]  # K线周期: 5m / 15m / 30m / 1h / 4h / 12h / 1d / 1w");
    println!("                    [--cash N] [--momentum-days N] [--ma-days N] [--rebalance-days N]");
    println!("                    [--ma-fast N] [--ma-slow N]  # ma_cross 模板策略快/慢线周期");
    println!("                    [--grid-levels 10] [--grid-lookback-days 30] [--grid-stop-loss 0.15] [--grid-budget 0]  # grid 智能网格");
    println!("                    [--dca-amount 100] [--dca-interval-days 7] [--dca-ma-days 200] [--dca-dip-multiplier 2]  # dca 定投");
    println!("                    [--top-n N] [--regime-ma N] [--regime-breadth 0.5]  # 分散与市场状态过滤");
    println!("                    [--circuit-breaker 0.3] [--circuit-cooldown 30]  # 组合回撤熔断");
    println!("                    [--start YYYY-MM-DD] [--end YYYY-MM-DD]  # 回测窗口");
    println!("                    [--benchmark SYM]  # 基准品种买入持有对比（α/β/IR）");
    println!("                    [--json]  # 结构化输出（WebUI 使用）");
    println!("  quantkit sweep    [--data DIR] [--symbols A,B] [--interval 1d] [--grid \"dim=v1,v2;dim2=v3\"]");
    println!("                    [--split 0.7]  # 训练/测试切分（前70%选参，后30%验收）");
    println!("                    [--json]  # JSONL 结构化输出（每行一个参数组合）");
    println!("                    支持维度: momentum_days ma_days rebalance_days trailing_stop cooldown_days");
    println!("                              top_n regime_ma regime_breadth circuit_breaker");
    println!("                              ma_fast ma_slow grid_levels grid_lookback_days grid_stop_loss");
    println!("                              dca_interval_days dca_ma_days dca_dip_multiplier");
    println!("  quantkit walkforward [--data DIR] [--symbols A,B] [--interval 1d] [--grid \"dim=v1,v2\"]");
    println!("                    [--windows 5]        # 折数：历史切成几段滚动验证");
    println!("                    [--train-ratio 0.7]  # 每折训练窗占总跨度的比例");
    println!("                    [--anchored]         # 扩张窗（训练起点固定）；默认滚动窗");
    println!("                    [--rank annualized|sharpe|calmar|sortino]  # 训练窗选参依据");
    println!("                    [--json]             # 每折一行 JSONL + 末行汇总");
    println!("                    与 sweep --split 的区别: 多折「训练窗选参 → 紧随其后的测试窗验收」，");
    println!("                    给出真实样本外收益与过拟合差距，用于判断参数是否只是拟合了历史");
    println!("  quantkit dry-run");
    println!("  quantkit live");
    println!("  quantkit serve    [--port N]  # 启动 WebUI 量化交易平台");
    println!();
    println!("配置: 当前目录的 quantkit.toml（可选，所有字段有默认值），CLI 参数优先");
}
