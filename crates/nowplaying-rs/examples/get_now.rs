//! 库用法演示：随时主动查询（进度已外推到当前时刻）。
//! 用法：cargo run --example get_now

use std::time::Duration;

use nowplaying::{NowPlayingWatcher, WatcherOptions};

fn main() {
    let mut watcher = NowPlayingWatcher::new(WatcherOptions { interval_ms: 1000 });
    watcher.on_provider(|name| eprintln!("数据源: {name}"));
    watcher.start();

    for _ in 0..10 {
        match watcher.get_now() {
            Some(info) => println!(
                "{} - {} {}/{}ms ({})",
                info.title,
                info.artist.as_deref().unwrap_or("未知艺术家"),
                info.position_ms,
                info.duration_ms.map_or(-1, |d| d),
                info.app.as_deref().unwrap_or("未知应用")
            ),
            None => println!("（当前没有媒体在播放）"),
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    watcher.stop();
}
