//! 关键事件通知（Telegram Bot）。
//!
//! 实盘关键事件（启动、真实成交、下单失败、对账漂移）推送到 Telegram，
//! 避免"只打在日志里没人看"。配置 `telegram_bot_token` + `telegram_chat_id`
//! 启用；未配置时所有发送为 no-op。
//!
//! 设计约束：通知失败只告警，绝不阻塞或影响交易主流程（5s 超时）。

/// Telegram 通知器。未配置时 `enabled()` 为 false，`send()` 立即返回。
pub struct Notifier {
    token: Option<String>,
    chat_id: Option<String>,
    http: reqwest::Client,
}

impl Notifier {
    /// 从配置值构造；空字符串视为未配置。
    pub fn new(token: &str, chat_id: &str) -> Self {
        Self {
            token: (!token.is_empty()).then(|| token.to_string()),
            chat_id: (!chat_id.is_empty()).then(|| chat_id.to_string()),
            // 通知专用客户端：短超时，防止网络故障拖累交易循环
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(5))
                .connect_timeout(std::time::Duration::from_secs(3))
                .build()
                .expect("构建通知 HTTP 客户端"),
        }
    }

    /// 是否已启用（token 与 chat_id 都配置）
    pub fn enabled(&self) -> bool {
        self.token.is_some() && self.chat_id.is_some()
    }

    /// 发送一条文本消息。失败只打印告警（通知是旁路，不是交易链路）。
    pub async fn send(&self, text: &str) {
        let (Some(token), Some(chat_id)) = (&self.token, &self.chat_id) else {
            return;
        };
        let url = format!("https://api.telegram.org/bot{token}/sendMessage");
        let body = serde_json::json!({ "chat_id": chat_id, "text": text });
        match self.http.post(&url).json(&body).send().await {
            Ok(resp) if resp.status().is_success() => {}
            Ok(resp) => eprintln!("[notify] Telegram 推送失败: HTTP {}", resp.status()),
            Err(e) => eprintln!("[notify] Telegram 推送失败: {e}"),
        }
    }
}

/// 实盘成交消息（统一格式，含方向/数量/价格/手续费/原因，日志与推送共用）
pub fn fill_message(symbol: &str, side: &str, qty: f64, price: f64, fee: f64, reason: &str) -> String {
    format!(
        "[quantkit 实盘] {} {} 数量{:.8} 价格{:.6} 手续费{:.6}（{reason}）",
        side, symbol, qty, price, fee
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_notifier_enabled() {
        assert!(!Notifier::new("", "").enabled());
        assert!(!Notifier::new("token", "").enabled());
        assert!(!Notifier::new("", "chat").enabled());
        assert!(Notifier::new("token", "chat").enabled());
    }

    #[test]
    fn test_fill_message() {
        let m = fill_message("BTCUSDT", "买入", 0.01, 60_000.5, 0.45, "信号调仓：买入新目标");
        assert!(m.contains("BTCUSDT") && m.contains("买入") && m.contains("信号调仓"));
    }
}
