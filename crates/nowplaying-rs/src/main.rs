//! nowplaying CLI：跨平台获取系统正在播放的媒体名称与播放进度（macOS / Windows）。
//!
//! 用法：
//!   nowplaying                 只输出正在播放的媒体，停止/暂停后无任何输出
//!   nowplaying --json          每秒输出一行 JSON（含暂停状态，供程序消费）
//!   nowplaying --interval 500  采样间隔（毫秒，最小 200）
//!   nowplaying --dump-smtc     dump Windows SMTC PowerShell 脚本（调试用）
//!   nowplaying --dump-title    dump Windows 窗口标题 PowerShell 脚本（调试用）
//!   nowplaying --dump-watch    dump macOS JXA watch 脚本（调试用）
//!   nowplaying --dump-app      dump macOS Spotify/Music JXA 脚本（调试用）

use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nowplaying::providers::{macos, windows, windows_title};
use nowplaying::types::NowPlayingInfo;
use nowplaying::NowPlayingWatcher;

fn fmt_time(ms: i64) -> String {
    let total = (ms.max(0) / 1000) as u64;
    let h = total / 3600;
    let m = (total % 3600) / 60;
    let s = total % 60;
    let mm = if h > 0 {
        format!("{}:{:02}", h, m)
    } else {
        format!("{}", m)
    };
    format!("{}:{:02}", mm, s)
}

/// 与 JSON.stringify(info) 一致的字段顺序（供 --json 模式输出）
fn to_json(info: &NowPlayingInfo) -> String {
    let esc = |s: &str| serde_json::to_string(s).unwrap_or_else(|_| "null".into());
    let opt = |v: &Option<String>| match v {
        Some(s) => esc(s),
        None => "null".into(),
    };
    format!(
        "{{\"title\":{},\"artist\":{},\"album\":{},\"app\":{},\"appId\":{},\"playing\":{},\"positionMs\":{},\"durationMs\":{},\"updatedAt\":{}}}",
        esc(&info.title),
        opt(&info.artist),
        opt(&info.album),
        opt(&info.app),
        opt(&info.app_id),
        info.playing,
        info.position_ms,
        match info.duration_ms {
            Some(d) => d.to_string(),
            None => "null".into(),
        },
        info.updated_at,
    )
}

/// 人类可读模式下用于变化检测：秒级进度相同则不重复输出
fn fingerprint(info: &NowPlayingInfo) -> String {
    format!(
        "{}|{}|{}|{}",
        info.title,
        info.artist.as_deref().unwrap_or("null"),
        info.app.as_deref().unwrap_or("null"),
        info.position_ms / 1000
    )
}

struct Args {
    json: bool,
    interval_ms: u64,
    dump: Option<&'static str>,
}

fn parse_args() -> Args {
    let mut args = Args { json: false, interval_ms: 1000, dump: None };
    let mut argv = std::env::args().skip(1);
    while let Some(a) = argv.next() {
        match a.as_str() {
            "--json" => args.json = true,
            "--interval" => {
                // 与原版一致：Number(v) || 1000 —— 0 与非数字均回退 1000，再下限 200
                let v = argv.next().unwrap_or_default();
                let n = match v.parse::<u64>() {
                    Ok(0) | Err(_) => 1000,
                    Ok(n) => n,
                };
                args.interval_ms = n.max(200);
            }
            "--dump-smtc" => args.dump = Some("smtc"),
            "--dump-title" => args.dump = Some("title"),
            "--dump-watch" => args.dump = Some("watch"),
            "--dump-app" => args.dump = Some("app"),
            "--help" | "-h" => {
                print_usage();
                std::process::exit(0);
            }
            _ => {}
        }
    }
    args
}

fn print_usage() {
    eprintln!("用法: nowplaying [--json] [--interval <ms>] [--dump-smtc|--dump-title|--dump-watch|--dump-app]");
}

fn main() {
    let args = parse_args();

    if let Some(dump) = args.dump {
        let interval = args.interval_ms;
        let text = match dump {
            "smtc" => windows::render_smtc_script(interval),
            "title" => windows_title::TITLE_SCRIPT
                .replace("__INTERVAL_MS__", &interval.max(200).to_string()),
            "watch" => macos::render_watch_script(interval),
            _ => macos::render_app_script(interval),
        };
        println!("{}", text.trim_start_matches('\n'));
        return;
    }

    let watcher = Arc::new(Mutex::new(NowPlayingWatcher::new(
        nowplaying::types::WatcherOptions { interval_ms: args.interval_ms },
    )));

    // CLI 输出与原版一致：stderr 提示数据源与错误，stdout 只输出正在播放的媒体
    {
        let mut w = watcher.lock().unwrap();
        w.on_provider(|name| eprintln!("[nowplaying] 数据源: {}", name));
        w.on_error(|msg| {
            eprintln!("[nowplaying] {}", msg);
            std::process::exit(1);
        });
    }

    let json_mode = args.json;
    let last_line = Arc::new(Mutex::new(String::new()));
    {
        let mut w = watcher.lock().unwrap();
        let dedup = last_line.clone();
        w.on_update(move |info| {
            // 只展示正在播放的媒体；停止/暂停后不输出任何内容
            let info = match info {
                Some(i) if i.playing => i,
                _ => {
                    // 重置去重状态，下次播放同一曲目也能重新输出
                    *dedup.lock().unwrap() = String::new();
                    return;
                }
            };
            if json_mode {
                println!("{}", to_json(&info));
                let _ = std::io::stdout().flush();
                return;
            }
            let fp = fingerprint(&info);
            {
                let mut last = dedup.lock().unwrap();
                if *last == fp {
                    return;
                }
                *last = fp;
            }
            let progress = match info.duration_ms {
                Some(d) => format!("[{} / {}]", fmt_time(info.position_ms), fmt_time(d)),
                None => format!("[{}]", fmt_time(info.position_ms)),
            };
            let mut line = format!("{}", info.title);
            if let Some(artist) = &info.artist {
                line.push_str(&format!(" — {}", artist));
            }
            line.push(' ');
            line.push_str(&progress);
            if let Some(app) = &info.app {
                line.push_str(&format!(" ({})", app));
            }
            println!("{}", line);
            let _ = std::io::stdout().flush();
        });
    }

    // Ctrl+C：先回收数据源子进程再退出
    {
        let w2 = watcher.clone();
        ctrlc::set_handler(move || {
            if let Ok(mut w) = w2.lock() {
                w.stop();
            }
            std::process::exit(0);
        })
        .ok();
    }

    watcher.lock().unwrap().start();
    eprintln!("[nowplaying] 正在监听系统正在播放的媒体… (Ctrl+C 退出)");

    // 主线程挂起（不持有锁，便于 Ctrl+C 处理器获取锁执行 stop）
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}
