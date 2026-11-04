pub mod macos;
pub mod macos_adapter;
pub mod spawn_line;
pub mod windows;
pub mod windows_title;

use crate::types::Provider;

/// 按平台返回数据源列表（按优先级降级）
pub fn default_providers(interval_ms: u64) -> Vec<Box<dyn Provider>> {
    match std::env::consts::OS {
        "windows" => vec![
            // PowerShell SMTC（标题+进度）→ 窗口标题兜底（零前置条件）
            Box::new(windows::create_windows_provider(interval_ms)),
            Box::new(windows_title::create_window_title_provider(interval_ms)),
        ],
        "macos" => macos::create_mac_providers(interval_ms),
        _ => Vec::new(),
    }
}
