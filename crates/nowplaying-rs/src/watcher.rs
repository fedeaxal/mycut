use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::providers::default_providers;
use crate::types::{NowPlayingInfo, ProviderMsg, RawSample, WatcherOptions};
use crate::util::{now_ms, to_info, StaleFilter};

type UpdateCb = Box<dyn FnMut(Option<NowPlayingInfo>) + Send>;
type ProviderCb = Box<dyn FnMut(&str) + Send>;
type ErrorCb = Box<dyn FnMut(&str) + Send>;

struct Shared {
    raw: Mutex<Option<RawSample>>,
    provider: Mutex<Option<String>>,
    update_cb: Mutex<Option<UpdateCb>>,
    provider_cb: Mutex<Option<ProviderCb>>,
    error_cb: Mutex<Option<ErrorCb>>,
}

/// 跨平台「正在播放」监听器。
/// 按平台选择数据源并按顺序降级；数据源行到达时仅更新采样，
/// 由固定间隔的节拍统一触发 update 回调（进度实时外推，
/// 因此事件驱动型的数据源也能按节拍刷新进度）。
/// 播放器停止后会话可能滞留（仍上报 playing 但进度停滞/振荡），
/// 这类假播放样本会被滑动窗口进度检测过滤为 None。
pub struct NowPlayingWatcher {
    interval_ms: u64,
    shared: Arc<Shared>,
    ctrl: Option<Sender<()>>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl NowPlayingWatcher {
    pub fn new(opts: WatcherOptions) -> Self {
        NowPlayingWatcher {
            interval_ms: opts.interval_ms.max(200),
            shared: Arc::new(Shared {
                raw: Mutex::new(None),
                provider: Mutex::new(None),
                update_cb: Mutex::new(None),
                provider_cb: Mutex::new(None),
                error_cb: Mutex::new(None),
            }),
            ctrl: None,
            handle: None,
        }
    }

    /// 每次采样都会触发（无媒体播放时 info 为 None）
    pub fn on_update(&mut self, f: impl FnMut(Option<NowPlayingInfo>) + Send + 'static) -> &mut Self {
        *self.shared.update_cb.lock().unwrap() = Some(Box::new(f));
        self
    }

    /// 数据源切换时触发
    pub fn on_provider(&mut self, f: impl FnMut(&str) + Send + 'static) -> &mut Self {
        *self.shared.provider_cb.lock().unwrap() = Some(Box::new(f));
        self
    }

    /// 所有数据源都不可用时触发（仅一次）
    pub fn on_error(&mut self, f: impl FnMut(&str) + Send + 'static) -> &mut Self {
        *self.shared.error_cb.lock().unwrap() = Some(Box::new(f));
        self
    }

    /// 启动监听（幂等）
    pub fn start(&mut self) {
        if self.ctrl.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel::<()>();
        let interval = self.interval_ms;
        let shared = self.shared.clone();
        let handle = std::thread::Builder::new()
            .name("nowplaying-watcher".into())
            .spawn(move || run(shared, interval, rx))
            .expect("启动 watcher 线程失败");
        self.ctrl = Some(tx);
        self.handle = Some(handle);
    }

    /// 停止监听并回收数据源子进程
    pub fn stop(&mut self) {
        self.ctrl.take();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }

    /// 最近一条采样外推到当前时刻的信息；无媒体时为 None
    pub fn get_now(&self) -> Option<NowPlayingInfo> {
        let raw = self.shared.raw.lock().unwrap().clone();
        raw.map(|r| to_info(&r, now_ms()))
    }

    /// 当前生效的数据源名称（尚未命中时为 None）
    pub fn current_provider(&self) -> Option<String> {
        self.shared.provider.lock().unwrap().clone()
    }

    /// 阻塞直到 stop() 被调用
    pub fn wait(&mut self) {
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for NowPlayingWatcher {
    fn drop(&mut self) {
        self.stop();
    }
}

fn fire_update(shared: &Shared, info: Option<NowPlayingInfo>) {
    let cb = shared.update_cb.lock().unwrap().take();
    if let Some(mut f) = cb {
        f(info);
        *shared.update_cb.lock().unwrap() = Some(f);
    }
}

fn run(shared: Arc<Shared>, interval_ms: u64, ctrl: Receiver<()>) {
    let mut providers = default_providers(interval_ms);
    let mut stale = StaleFilter::new();
    let mut fatal_reasons: Vec<String> = Vec::new();
    let mut idx = 0usize;
    // (当前激活的数据源下标, 事件接收端)
    let mut active: Option<(usize, Receiver<ProviderMsg>)> = None;
    let mut exhausted = false;
    let interval = Duration::from_millis(interval_ms);
    let mut next_tick = Instant::now() + interval;

    loop {
        // 1. 非阻塞吸收当前数据源的事件
        if let Some((i, rx)) = active.as_ref() {
            loop {
                match rx.try_recv() {
                    Ok(ProviderMsg::Sample(sample)) => {
                        let name = providers[*i].name();
                        {
                            let mut cur = shared.provider.lock().unwrap();
                            if cur.as_deref() != Some(name) {
                                *cur = Some(name.to_string());
                                drop(cur);
                                let cb = shared.provider_cb.lock().unwrap().take();
                                if let Some(mut f) = cb {
                                    f(name);
                                    *shared.provider_cb.lock().unwrap() = Some(f);
                                }
                            }
                        }
                        let keep = sample.as_ref().map(|s| stale.check(s)).unwrap_or(false);
                        *shared.raw.lock().unwrap() =
                            sample.filter(|_| keep);
                    }
                    Ok(ProviderMsg::Fatal(msg)) => {
                        fatal_reasons.push(format!("{}({})", providers[*i].name(), msg));
                        providers[*i].stop();
                        active = None;
                        break;
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        // 事件端全部关闭（异常退出）：视同 fatal，切换下一个
                        fatal_reasons.push(format!("{}(子进程意外结束)", providers[*i].name()));
                        providers[*i].stop();
                        active = None;
                        break;
                    }
                }
            }
        }

        // 2. 无激活数据源则按顺序拉起下一个
        if active.is_none() && !exhausted {
            if idx < providers.len() {
                let (tx, rx) = mpsc::channel::<ProviderMsg>();
                providers[idx].start(tx);
                active = Some((idx, rx));
                idx += 1;
            } else {
                exhausted = true;
                let detail = if fatal_reasons.is_empty() {
                    String::new()
                } else {
                    format!("：{}", fatal_reasons.join("；"))
                };
                let cb = shared.error_cb.lock().unwrap().take();
                if let Some(mut f) = cb {
                    f(&format!("本机没有任何可用的媒体数据源{detail}"));
                    *shared.error_cb.lock().unwrap() = Some(f);
                }
            }
        }

        // 3. 节拍触发 update（进度实时外推）
        let now = Instant::now();
        if now >= next_tick {
            let info = shared
                .raw
                .lock()
                .unwrap()
                .clone()
                .map(|r| to_info(&r, now_ms()));
            fire_update(&shared, info);
            next_tick = if next_tick + interval > now {
                next_tick + interval
            } else {
                now + interval
            };
        }

        // 4. 等待到下一拍或停止信号
        let timeout = next_tick.saturating_duration_since(Instant::now());
        match ctrl.recv_timeout(timeout) {
            Ok(()) | Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {}
        }
    }

    if let Some((i, _)) = active.take() {
        providers[i].stop();
    }
}
