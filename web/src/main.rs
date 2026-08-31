//! quantkit WebUI 入口：`quantkit-web [--port N] [--config PATH]`

mod api;
mod factors;

use quantkit_app::config::load_config;

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let get = |key: &str| -> Option<String> {
        args.iter()
            .position(|a| a == key)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let mut cfg = load_config(get("--config").as_deref());
    if let Some(v) = get("--data") {
        cfg.data_dir = v;
    }
    if let Some(v) = get("--port").and_then(|v| v.parse().ok()) {
        cfg.web_port = v;
    }
    api::serve(cfg).await;
}
