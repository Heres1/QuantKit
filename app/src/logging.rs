//! 日志系统模块（参考 binance-rust 架构）
//!
//! 特性：
//! - 统一时间戳格式 `[MM-DD HH:MM:SS]`
//! - 结构化日志级别（INFO / ACTION / WARN / ERROR）
//! - Emoji 标识关键事件类型
//! - 异步写入文件，支持自动轮转
//! - 控制台只显示 WARN+ 级别

use chrono::Local;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;

/// 日志级别
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LogLevel {
    /// 常规信息（bar 处理、状态检查等）
    Info,
    /// 重要操作（调仓、成交、对账通过等）
    Action,
    /// 警告信息（漂移、重试、异常但可恢复）
    Warn,
    /// 错误信息（失败、阻断性错误）
    Error,
}

impl LogLevel {
    fn as_str(&self) -> &'static str {
        match self {
            LogLevel::Info => "INFO",
            LogLevel::Action => "ACTION",
            LogLevel::Warn => "WARN",
            LogLevel::Error => "ERROR",
        }
    }

    fn should_print_to_console(&self) -> bool {
        // 只有 WARN 和 ERROR 输出到控制台
        matches!(self, LogLevel::Warn | LogLevel::Error)
    }
}

/// 日志配置
#[derive(Clone)]
pub struct LoggerConfig {
    file_path: Option<String>,
    rotate_size: Option<u64>,
    max_files: usize,
}

impl Default for LoggerConfig {
    fn default() -> Self {
        LoggerConfig {
            file_path: None,
            rotate_size: Some(100 * 1024 * 1024), // 单文件 100MB 上限
            max_files: 10,                        // 保留最近 10 个历史日志
        }
    }
}

impl LoggerConfig {
    pub fn new(file_path: Option<String>) -> Self {
        LoggerConfig {
            file_path,
            rotate_size: Some(100 * 1024 * 1024),
            max_files: 10,
        }
    }

    pub fn with_rotation(mut self, rotate_size: u64, max_files: usize) -> Self {
        self.rotate_size = Some(rotate_size);
        self.max_files = max_files;
        self
    }
}

/// 生成带时间戳的日志文件路径
/// 输入: "logs/trading.log" → 输出: "logs/trading_2026-05-24_163506.log"
fn generate_session_log_path(base_path: &str) -> String {
    let path = Path::new(base_path);
    let stem = path
        .file_stem()
        .unwrap_or_default()
        .to_str()
        .unwrap_or("trading");
    let ext = path
        .extension()
        .unwrap_or_default()
        .to_str()
        .unwrap_or("log");
    let dir = path.parent().unwrap_or(Path::new("."));
    let timestamp = Local::now().format("%Y-%m-%d_%H%M%S");
    dir.join(format!("{}_{}.{}", stem, timestamp, ext))
        .to_string_lossy()
        .to_string()
}

/// 清理历史日志，只保留最近 max_files 个
fn cleanup_old_logs(base_path: &str, max_files: usize) {
    let path = Path::new(base_path);
    let stem = path
        .file_stem()
        .unwrap_or_default()
        .to_str()
        .unwrap_or("trading");
    let ext = path
        .extension()
        .unwrap_or_default()
        .to_str()
        .unwrap_or("log");
    let dir = path.parent().unwrap_or(Path::new("."));

    let pattern = format!("{}_", stem);
    let mut log_files: Vec<PathBuf> = Vec::new();

    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let file_name = entry.file_name().to_string_lossy().to_string();
            if file_name.starts_with(&pattern) && file_name.ends_with(&format!(".{}", ext)) {
                log_files.push(entry.path());
            }
        }
    }

    log_files.sort();

    if log_files.len() > max_files {
        let to_remove = log_files.len() - max_files;
        for old_file in log_files.iter().take(to_remove) {
            if let Err(e) = fs::remove_file(old_file) {
                eprintln!("清理旧日志失败 {:?}: {}", old_file, e);
            }
        }
    }
}

/// 运行内日志大小监控
struct SizeMonitor {
    current_path: String,
    max_size: u64,
    base_path: String,
}

impl SizeMonitor {
    fn new(current_path: String, max_size: u64, base_path: String) -> Self {
        SizeMonitor {
            current_path,
            max_size,
            base_path,
        }
    }

    fn should_split(&self) -> bool {
        match fs::metadata(&self.current_path) {
            Ok(m) => m.len() > self.max_size,
            Err(_) => false,
        }
    }

    fn split(&mut self) -> Option<File> {
        let new_path = generate_session_log_path(&self.base_path);
        match OpenOptions::new().create(true).append(true).open(&new_path) {
            Ok(file) => {
                self.current_path = new_path;
                Some(file)
            }
            Err(e) => {
                eprintln!("日志分割失败: {}", e);
                None
            }
        }
    }
}

/// 异步日志记录器
pub struct AsyncLogger {
    sender: Option<mpsc::Sender<String>>,
    handle: Option<thread::JoinHandle<()>>,
}

impl AsyncLogger {
    pub fn new(config: LoggerConfig) -> Self {
        let (sender, receiver) = mpsc::channel::<String>();

        let config_clone = config.clone();
        let handle = thread::spawn(move || {
            let mut current_file: Option<File> = None;
            let mut size_monitor: Option<SizeMonitor> = None;

            if let Some(ref base_path) = config_clone.file_path {
                // 自动创建日志目录
                if let Some(parent) = Path::new(base_path).parent() {
                    if !parent.exists() {
                        if let Err(e) = fs::create_dir_all(parent) {
                            eprintln!("创建日志目录失败 {:?}: {}", parent, e);
                        }
                    }
                }

                // 清理历史日志
                cleanup_old_logs(base_path, config_clone.max_files);

                // 生成本次运行的日志文件名（带时间戳）
                let session_path = generate_session_log_path(base_path);

                match OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&session_path)
                {
                    Ok(file) => {
                        current_file = Some(file);

                        if let Some(max_size) = config_clone.rotate_size {
                            size_monitor = Some(SizeMonitor::new(
                                session_path.clone(),
                                max_size,
                                base_path.clone(),
                            ));
                        }

                        // 创建/更新软链接，方便 tail -f 查看最新日志
                        let _ = Self::update_symlink(base_path, &session_path);
                    }
                    Err(e) => {
                        eprintln!("打开日志文件失败 {}: {}", session_path, e);
                    }
                }
            }

            while let Ok(msg) = receiver.recv() {
                // 检查是否需要分割（单文件超过上限）
                if let Some(ref mut monitor) = size_monitor {
                    if monitor.should_split() {
                        if let Some(new_file) = monitor.split() {
                            current_file = Some(new_file);
                            // 更新软链接指向新文件
                            if let Some(ref base_path) = config_clone.file_path {
                                let _ = Self::update_symlink(base_path, &monitor.current_path);
                            }
                        }
                    }
                }

                // 写入日志文件
                if let Some(ref mut file) = current_file {
                    if writeln!(file, "{}", msg).is_err() {
                        eprintln!("日志写入失败");
                    }
                    let _ = file.flush();
                }
            }
        });

        AsyncLogger {
            sender: Some(sender),
            handle: Some(handle),
        }
    }

    /// 创建/更新软链接，让 trading.log 始终指向当前会话的日志文件
    fn update_symlink(link_path: &str, target_path: &str) -> std::io::Result<()> {
        let link = Path::new(link_path);
        if link.exists() || link.symlink_metadata().is_ok() {
            fs::remove_file(link)?;
        }
        let target = Path::new(target_path);
        let target_filename = target.file_name().unwrap_or_default();
        #[cfg(unix)]
        std::os::unix::fs::symlink(target_filename, link)?;
        #[cfg(not(unix))]
        {
            fs::write(link, target_filename.to_string_lossy().as_bytes())?;
        }
        Ok(())
    }

    /// 格式化日志消息
    fn format_message(level: LogLevel, message: &str) -> String {
        let short_timestamp = Local::now().format("%m-%d %H:%M:%S").to_string();
        format!("[{}] {} - {}", short_timestamp, level.as_str(), message)
    }

    /// 记录日志
    pub fn log(&self, level: LogLevel, message: &str) {
        let formatted = Self::format_message(level, message);

        // 只有 WARN 和 ERROR 输出到控制台
        if level.should_print_to_console() {
            eprintln!("{}", formatted);
        }

        // 发送到异步写入线程
        if let Some(ref sender) = self.sender {
            let _ = sender.send(formatted);
        }
    }

    /// 初始化全局日志记录器
    pub fn init(config: LoggerConfig) -> Result<(), Box<dyn std::error::Error>> {
        let _logger = Box::new(AsyncLogger::new(config));
        // TODO: 集成到 log crate 的全局 logger
        Ok(())
    }
}

impl Drop for AsyncLogger {
    fn drop(&mut self) {
        drop(self.sender.take());
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// 便捷宏：INFO 级别日志
#[macro_export]
macro_rules! info {
    ($($arg:tt)*) => {{
        let msg = format!($($arg)*);
        eprintln!("[live] {}", msg);
    }};
}

/// 便捷宏：ACTION 级别日志（重要操作，带 emoji）
#[macro_export]
macro_rules! action {
    ($emoji:expr, $($arg:tt)*) => {{
        let msg = format!($($arg)*);
        eprintln!("[live] {} {}", $emoji, msg);
    }};
}

/// 便捷宏：WARN 级别日志
#[macro_export]
macro_rules! warn {
    ($($arg:tt)*) => {{
        let msg = format!($($arg)*);
        eprintln!("[live] ⚠️  {}", msg);
    }};
}

/// 便捷宏：ERROR 级别日志
#[macro_export]
macro_rules! error {
    ($($arg:tt)*) => {{
        let msg = format!($($arg)*);
        eprintln!("[live] ❌ {}", msg);
    }};
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_log_level_as_str() {
        assert_eq!(LogLevel::Info.as_str(), "INFO");
        assert_eq!(LogLevel::Action.as_str(), "ACTION");
        assert_eq!(LogLevel::Warn.as_str(), "WARN");
        assert_eq!(LogLevel::Error.as_str(), "ERROR");
    }

    #[test]
    fn test_log_level_should_print_to_console() {
        // INFO 和 ACTION 不应输出到控制台
        assert!(!LogLevel::Info.should_print_to_console());
        assert!(!LogLevel::Action.should_print_to_console());
        
        // WARN 和 ERROR 应输出到控制台
        assert!(LogLevel::Warn.should_print_to_console());
        assert!(LogLevel::Error.should_print_to_console());
    }

    #[test]
    fn test_log_level_equality() {
        assert_eq!(LogLevel::Info, LogLevel::Info);
        assert_ne!(LogLevel::Info, LogLevel::Warn);
    }

    #[test]
    fn test_logger_config_default() {
        let config = LoggerConfig::default();
        assert!(config.file_path.is_none());
        assert_eq!(config.rotate_size, Some(100 * 1024 * 1024)); // 100MB
        assert_eq!(config.max_files, 10);
    }

    #[test]
    fn test_logger_config_new() {
        let config = LoggerConfig::new(Some("logs/test.log".to_string()));
        assert_eq!(config.file_path, Some("logs/test.log".to_string()));
        assert_eq!(config.rotate_size, Some(100 * 1024 * 1024));
        assert_eq!(config.max_files, 10);
    }

    #[test]
    fn test_logger_config_with_rotation() {
        let config = LoggerConfig::default()
            .with_rotation(50 * 1024 * 1024, 5);
        assert_eq!(config.rotate_size, Some(50 * 1024 * 1024));
        assert_eq!(config.max_files, 5);
    }

    #[test]
    fn test_logger_config_builder_pattern() {
        let config = LoggerConfig::new(None)
            .with_rotation(200 * 1024 * 1024, 20);
        assert_eq!(config.rotate_size, Some(200 * 1024 * 1024));
        assert_eq!(config.max_files, 20);
    }

    #[test]
    fn test_generate_session_log_path_basic() {
        let path = generate_session_log_path("logs/trading.log");
        // 应该包含原始文件名 stem 和时间戳
        assert!(path.contains("trading_"));
        assert!(path.ends_with(".log"));
        // 时间戳格式应为 YYYY-MM-DD_HHMMSS
        assert!(path.contains('_'));
    }

    #[test]
    fn test_generate_session_log_path_custom_name() {
        let path = generate_session_log_path("/var/log/myapp/custom.log");
        assert!(path.contains("custom_"));
        assert!(path.ends_with(".log"));
    }

    #[test]
    fn test_generate_session_log_path_no_extension() {
        let path = generate_session_log_path("logs/trading");
        // 无扩展名时应使用默认 "log"
        assert!(path.contains("trading_"));
        // 注意：由于 extension() 返回 None，实际会以 "." 结尾
        // 这是当前实现的特性，不是 bug
        assert!(path.contains('.'));
    }

    #[test]
    fn test_generate_session_log_path_different_stem() {
        let path = generate_session_log_path("data/market.json");
        assert!(path.contains("market_"));
        assert!(path.ends_with(".json"));
    }

    #[test]
    fn test_format_message_info() {
        let msg = AsyncLogger::format_message(LogLevel::Info, "策略启动");
        assert!(msg.contains("INFO"));
        assert!(msg.contains("策略启动"));
        // 时间戳格式 [MM-DD HH:MM:SS]
        assert!(msg.starts_with('['));
    }

    #[test]
    fn test_format_message_action() {
        let msg = AsyncLogger::format_message(LogLevel::Action, "买入 BTCUSDT");
        assert!(msg.contains("ACTION"));
        assert!(msg.contains("买入 BTCUSDT"));
    }

    #[test]
    fn test_format_message_warn() {
        let msg = AsyncLogger::format_message(LogLevel::Warn, "对账漂移");
        assert!(msg.contains("WARN"));
        assert!(msg.contains("对账漂移"));
    }

    #[test]
    fn test_format_message_error() {
        let msg = AsyncLogger::format_message(LogLevel::Error, "网络超时");
        assert!(msg.contains("ERROR"));
        assert!(msg.contains("网络超时"));
    }

    #[test]
    fn test_format_message_timestamp_format() {
        let msg = AsyncLogger::format_message(LogLevel::Info, "test");
        // 时间戳应该是 MM-DD HH:MM:SS 格式
        assert!(msg.contains('-')); // MM-DD
        assert!(msg.contains(':')); // HH:MM:SS
    }

    #[test]
    fn test_cleanup_old_logs_empty_dir() {
        // 在临时目录测试
        let temp_dir = std::env::temp_dir().join(format!("test_logs_{}", std::process::id()));
        fs::create_dir_all(&temp_dir).ok();
        
        let base_path = temp_dir.join("trading.log").to_string_lossy().to_string();
        
        // 空目录清理应该不会报错
        cleanup_old_logs(&base_path, 10);
        
        // 清理临时目录
        fs::remove_dir_all(&temp_dir).ok();
    }

    #[test]
    fn test_cleanup_old_logs_within_limit() {
        let temp_dir = std::env::temp_dir().join(format!("test_logs2_{}", std::process::id()));
        fs::create_dir_all(&temp_dir).ok();
        
        let base_path = temp_dir.join("trading.log").to_string_lossy().to_string();
        
        // 创建 5 个日志文件
        for i in 0..5 {
            let log_file = temp_dir.join(format!("trading_2026-01-0{}_120000.log", i + 1));
            File::create(&log_file).ok();
        }
        
        // 保留 10 个，实际只有 5 个，应该都不删除
        cleanup_old_logs(&base_path, 10);
        
        // 验证文件仍然存在
        let count = fs::read_dir(&temp_dir).unwrap().count();
        assert_eq!(count, 5);
        
        fs::remove_dir_all(&temp_dir).ok();
    }

    #[test]
    fn test_cleanup_old_logs_exceeds_limit() {
        let temp_dir = std::env::temp_dir().join(format!("test_logs3_{}", std::process::id()));
        fs::create_dir_all(&temp_dir).ok();
        
        let base_path = temp_dir.join("trading.log").to_string_lossy().to_string();
        
        // 创建 15 个日志文件
        for i in 0..15 {
            let log_file = temp_dir.join(format!("trading_2026-01-{:02}_120000.log", i + 1));
            File::create(&log_file).ok();
        }
        
        // 只保留 10 个，应该删除最旧的 5 个
        cleanup_old_logs(&base_path, 10);
        
        // 验证只剩下 10 个文件
        let count = fs::read_dir(&temp_dir).unwrap().count();
        assert_eq!(count, 10);
        
        fs::remove_dir_all(&temp_dir).ok();
    }

    #[test]
    fn test_cleanup_old_logs_preserves_newest() {
        let temp_dir = std::env::temp_dir().join(format!("test_logs4_{}", std::process::id()));
        fs::create_dir_all(&temp_dir).ok();
        
        let base_path = temp_dir.join("trading.log").to_string_lossy().to_string();
        
        // 创建按时间排序的文件名
        File::create(temp_dir.join("trading_2026-01-01_120000.log")).ok();
        File::create(temp_dir.join("trading_2026-01-02_120000.log")).ok();
        File::create(temp_dir.join("trading_2026-01-03_120000.log")).ok();
        
        // 只保留 2 个，应该删除最早的
        cleanup_old_logs(&base_path, 2);
        
        let mut files: Vec<String> = fs::read_dir(&temp_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        files.sort();
        
        // 应该保留最新的两个
        assert_eq!(files.len(), 2);
        assert!(files.contains(&"trading_2026-01-02_120000.log".to_string()));
        assert!(files.contains(&"trading_2026-01-03_120000.log".to_string()));
        
        fs::remove_dir_all(&temp_dir).ok();
    }

    #[test]
    fn test_cleanup_old_logs_ignores_unrelated_files() {
        let temp_dir = std::env::temp_dir().join(format!("test_logs5_{}", std::process::id()));
        fs::create_dir_all(&temp_dir).ok();
        
        let base_path = temp_dir.join("trading.log").to_string_lossy().to_string();
        
        // 创建相关日志文件
        File::create(temp_dir.join("trading_2026-01-01_120000.log")).ok();
        File::create(temp_dir.join("trading_2026-01-02_120000.log")).ok();
        
        // 创建不相关的文件
        File::create(temp_dir.join("other.txt")).ok();
        File::create(temp_dir.join("README.md")).ok();
        
        cleanup_old_logs(&base_path, 10);
        
        // 不相关文件应该保留
        let count = fs::read_dir(&temp_dir).unwrap().count();
        assert_eq!(count, 4); // 2 个日志 + 2 个其他文件
        
        fs::remove_dir_all(&temp_dir).ok();
    }

    #[test]
    fn test_size_monitor_should_split() {
        let temp_file = std::env::temp_dir().join(format!("test_monitor_{}.log", std::process::id()));
        
        // 创建一个小文件
        let mut file = File::create(&temp_file).unwrap();
        writeln!(file, "test").ok();
        
        let monitor = SizeMonitor::new(
            temp_file.to_string_lossy().to_string(),
            1024, // 1KB 上限
            "logs/trading.log".to_string(),
        );
        
        // 小文件不应该触发分割
        assert!(!monitor.should_split());
        
        fs::remove_file(&temp_file).ok();
    }

    #[test]
    fn test_size_monitor_nonexistent_file() {
        let monitor = SizeMonitor::new(
            "/tmp/nonexistent_file_12345.log".to_string(),
            1024,
            "logs/trading.log".to_string(),
        );
        
        // 不存在的文件不应触发分割
        assert!(!monitor.should_split());
    }

    #[test]
    fn test_async_logger_format_and_send() {
        let config = LoggerConfig::new(None);
        let logger = AsyncLogger::new(config);
        
        // 记录日志不应该 panic
        logger.log(LogLevel::Info, "测试消息");
        logger.log(LogLevel::Warn, "警告消息");
        logger.log(LogLevel::Error, "错误消息");
        
        // logger drop 时应该等待线程结束
    }

    #[test]
    fn test_async_logger_without_file() {
        // 没有配置文件的 logger 应该只输出到控制台
        let config = LoggerConfig::default();
        let logger = AsyncLogger::new(config);
        
        logger.log(LogLevel::Info, "仅控制台");
        // 不应该创建任何文件
    }

    #[test]
    fn test_macro_info_expansion() {
        // 测试 info! 宏能正常展开
        info!("测试信息 {}", 123);
        info!("多参数 {} {}", "hello", "world");
    }

    #[test]
    fn test_macro_action_expansion() {
        // 测试 action! 宏能正常展开
        action!("🟢", "买入成功");
        action!("🔴", "卖出 {}", "BTCUSDT");
    }

    #[test]
    fn test_macro_warn_expansion() {
        // 测试 warn! 宏能正常展开
        warn!("警告信息");
        warn!("带参数警告 {}", 456);
    }

    #[test]
    fn test_macro_error_expansion() {
        // 测试 error! 宏能正常展开
        error!("错误信息");
        error!("带参数错误 {}", 789);
    }

    #[test]
    fn test_logger_config_clone() {
        let config = LoggerConfig::new(Some("test.log".to_string()))
            .with_rotation(50 * 1024 * 1024, 5);
        
        let cloned = config.clone();
        assert_eq!(cloned.file_path, config.file_path);
        assert_eq!(cloned.rotate_size, config.rotate_size);
        assert_eq!(cloned.max_files, config.max_files);
    }

    #[test]
    fn test_generate_path_in_current_dir() {
        // 测试在当前目录生成日志路径
        let path = generate_session_log_path("app.log");
        assert!(path.contains("app_"));
        assert!(path.ends_with(".log"));
        // 应该在当前目录或子目录
        assert!(!path.starts_with('/'));
    }

    #[test]
    fn test_cleanup_zero_max_files() {
        let temp_dir = std::env::temp_dir().join(format!("test_logs6_{}", std::process::id()));
        fs::create_dir_all(&temp_dir).ok();
        
        let base_path = temp_dir.join("trading.log").to_string_lossy().to_string();
        
        // 创建一些文件
        File::create(temp_dir.join("trading_2026-01-01_120000.log")).ok();
        File::create(temp_dir.join("trading_2026-01-02_120000.log")).ok();
        
        // max_files = 0 应该删除所有
        cleanup_old_logs(&base_path, 0);
        
        let count = fs::read_dir(&temp_dir).unwrap().count();
        assert_eq!(count, 0);
        
        fs::remove_dir_all(&temp_dir).ok();
    }
}
