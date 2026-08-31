//! 测试 WebSocket 真实连接
//!
//! 用法: cargo run --bin test_ws

use quantkit_exchanges::binance_ws;

#[tokio::main]
async fn main() {
    // 初始化日志
    env_logger::builder()
        .filter_level(log::LevelFilter::Info)
        .init();

    println!("🧪 测试 Binance WebSocket 真实连接\n");

    // 测试多品种订阅（合并流）
    let symbols = vec!["BTCUSDT".to_string(), "ETHUSDT".to_string(), "SOLUSDT".to_string()];
    let interval = "1m";

    println!("📡 订阅品种: {:?}", symbols);
    println!("⏱️  K线周期: {}", interval);
    println!("🔗 正在连接 Binance WebSocket...\n");

    // 启动 WebSocket 客户端
    let mut rx = binance_ws::quick_start_ws(&symbols, interval).await;

    println!("✅ WebSocket 已连接，等待接收数据...\n");

    // 接收并显示前 10 条消息
    let mut count = 0;
    while let Ok((symbol, kline)) = rx.recv().await {
        count += 1;
        println!(
            "[{}] {} 收盘: {:.2} | 最高: {:.2} | 最低: {:.2} | 成交量: {:.4}",
            count,
            symbol,
            kline.close,
            kline.high,
            kline.low,
            kline.volume
        );

        if count >= 10 {
            println!("\n✅ 成功接收 10 条消息，测试完成！");
            break;
        }
    }
}
