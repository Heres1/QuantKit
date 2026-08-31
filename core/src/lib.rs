//! quantkit 内核：类型、策略/执行器 trait、统一引擎、记账、指标。
//!
//! 设计原则：本 crate 不依赖 tokio/serde_json 等运行时设施，
//! 只依赖 serde（类型序列化）与 thiserror（错误定义）。

pub mod engine;
pub mod executor;
pub mod interval;
pub mod metrics;
pub mod portfolio;
pub mod strategy;
pub mod types;
