//! 跨平台「正在播放」库：获取 macOS / Windows 系统正在播放的媒体名称与实时播放进度。
//!
//! 通过平台原生机制读取（长驻子进程按行输出 JSON），自动降级：
//! - Windows：`windows-smtc`（PowerShell 投影 WinRT SMTC）→ `windows-title`（窗口标题兜底）
//! - macOS：`macos-mediaremote`（osascript JXA 调 MRNowPlayingController / 旧 C API）
//!   → `macos-adapter`（vendored mediaremote-adapter，perl 托管）→ `macos-applescript`（Spotify/Music）
//!
//! 用法参见 crate 根 README 或 `NowPlayingWatcher`。

pub mod providers;
pub mod types;
pub mod util;
pub mod watcher;

pub use types::{NowPlayingInfo, ProviderMsg, RawSample, WatcherOptions};
pub use watcher::NowPlayingWatcher;
