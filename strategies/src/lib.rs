//! 策略集（展示 Strategy trait 的可插拔性）。
//!
//! - [`dca`]：定投（固定周期定额买入，可选低于趋势线加码）
//! - [`grid`]：智能网格（区间自适应的低买高卖，带单边下跌止损）
//! - [`momentum_rotation`]：多品种动量轮动（低频调仓）
//! - [`trailing_trend`]：单品种趋势入场 + 追踪止损
//! - [`ma_cross`]：双均线金叉（也是手写策略的参考模板）

pub mod dca;
pub mod grid;
pub mod ma_cross;
pub mod momentum_rotation;
pub mod trailing_trend;
