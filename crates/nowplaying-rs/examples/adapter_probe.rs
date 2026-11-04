//! 调试探针：直接激活 vendored mediaremote-adapter 数据源，观察其原始采样。
//! 用法：cargo run --example adapter_probe [秒数，默认 8]

use std::sync::mpsc;
use std::time::Duration;

use nowplaying::providers::macos_adapter::create_adapter_provider;
use nowplaying::types::ProviderMsg;

fn main() {
    let secs: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(8);
    let mut provider = create_adapter_provider(1000);
    let (tx, rx) = mpsc::channel();
    provider.start(tx);
    let deadline = std::time::Instant::now() + Duration::from_secs(secs);
    while std::time::Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(ProviderMsg::Sample(s)) => println!("{:?}", s),
            Ok(ProviderMsg::Fatal(e)) => {
                eprintln!("fatal: {e}");
                std::process::exit(1);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(_) => break,
        }
    }
    provider.stop();
}
