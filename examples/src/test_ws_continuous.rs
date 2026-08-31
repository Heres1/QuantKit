//! 持续接收 WebSocket 数据测试
//!
//! 用法: cargo run --bin test_ws_continuous

use quantkit_exchanges::binance_ws::{BinanceWsClient, WsConfig};

#[tokio::main]
async fn main() {
    env_logger::builder()
        .filter_level(log::LevelFilter::Info)
        .init();

    println!("🧪 持续接收 WebSocket 数据测试\n");

    let symbols = vec!["BTCUSDT".to_string(), "ETHUSDT".to_string()];
    let interval = "1m";

    println!("📡 订阅品种: {:?}", symbols);
    println!("⏱️  K线周期: {}", interval);
    println!("🔗 正在连接 Binance WebSocket...\n");

    // 创建客户端，接收所有K线（包括未闭合的）
    let config = WsConfig {
        only_closed_bars: false,  // 接收实时更新
        ..WsConfig::default()
    };
    let client = BinanceWsClient::new(config, 1024);
    let mut rx = client.subscribe();

    let symbols_vec = symbols.to_vec();
    let interval_str = interval.to_string();

    tokio::spawn(async move {
        let _ = client.run(&symbols_vec, &interval_str).await;
    });

    println!("✅ WebSocket 已连接，开始持续接收数据...");
    println!("📊 接收模式: 实时推送（包括未闭合K线）\n");
    println!("按 Ctrl+C 停止测试\n");

    let mut count = 0u64;
    let start_time = std::time::Instant::now();

    loop {
        match rx.recv().await {
            Ok((symbol, kline)) => {
                count += 1;
                let _elapsed = start_time.elapsed();
                println!(
                    "[{}] {} 收盘: {:.2} | 最高: {:.2} | 最低: {:.2} | 成交量: {:.4}",
                    count,
                    symbol,
                    kline.close,
                    kline.high,
                    kline.low,
                    kline.volume
                );
            }
            Err(e) => {
                eprintln!("❌ 接收错误: {}", e);
                break;
            }
        }
    }
}
