use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;

use crate::types::{Provider, ProviderMsg, RawSample};
use crate::util::map_raw_sample;

/// 「JSON 行 → 采样」映射。None 表示心跳 / 无媒体。
pub type MapLine = Box<dyn FnMut(&Value) -> Option<RawSample> + Send>;

/// 子进程拉起参数（重启线程需要，独立于 Provider 存活期）
struct ChildCfg {
    command: String,
    args: Vec<String>,
    probe_timeout_ms: u64,
    // 仅 Windows 读取（CREATE_NO_WINDOW）
    #[cfg_attr(not(windows), allow(dead_code))]
    hide_window: bool,
}

struct Running {
    stopped: AtomicBool,
    activated: AtomicBool,
    fatal_sent: AtomicBool,
    restarts: AtomicU32,
    child: Mutex<Option<Child>>,
    stderr_tail: Mutex<VecDeque<String>>,
    map_line: Mutex<Option<MapLine>>,
}

pub struct SpawnLineOptions {
    pub command: String,
    pub args: Vec<String>,
    /// 探活超时：子进程在这段时间内没吐出任何 JSON 行则判定不可用
    pub probe_timeout_ms: u64,
    /// 启动前的可用性预检（如检查文件存在），Err 则直接降级
    pub precheck: Option<Box<dyn FnOnce() -> Result<(), String> + Send>>,
    /// 自定义「JSON 行 -> 采样」映射，默认 map_raw_sample
    pub map_line: Option<MapLine>,
    /// Windows 下隐藏子进程窗口
    pub hide_window: bool,
}

/// 以「长驻子进程按行输出 JSON」方式工作的数据源。
/// 约定：子进程输出 JSON 行；
///   - {"ok":true,"empty":true}  表示正常但当前无媒体
///   - {"ok":false,"err":"..."}  表示本机不支持，立即降级到下一个数据源
/// 激活后若子进程意外退出，按指数退避自动重启。
pub struct SpawnLineProvider {
    name: &'static str,
    cfg: Arc<ChildCfg>,
    precheck: Option<Box<dyn FnOnce() -> Result<(), String> + Send>>,
    map_line: Option<MapLine>,
    running: Option<Arc<Running>>,
}

impl SpawnLineProvider {
    pub fn new(name: &'static str, opts: SpawnLineOptions) -> Self {
        SpawnLineProvider {
            name,
            cfg: Arc::new(ChildCfg {
                command: opts.command,
                args: opts.args,
                probe_timeout_ms: opts.probe_timeout_ms,
                hide_window: opts.hide_window,
            }),
            precheck: opts.precheck,
            map_line: opts.map_line,
            running: None,
        }
    }
}

impl Provider for SpawnLineProvider {
    fn name(&self) -> &'static str {
        self.name
    }

    fn start(&mut self, tx: Sender<ProviderMsg>) {
        let run = Arc::new(Running {
            stopped: AtomicBool::new(false),
            activated: AtomicBool::new(false),
            fatal_sent: AtomicBool::new(false),
            restarts: AtomicU32::new(0),
            child: Mutex::new(None),
            stderr_tail: Mutex::new(VecDeque::new()),
            map_line: Mutex::new(self.map_line.take()),
        });
        self.running = Some(run.clone());
        let cfg = self.cfg.clone();
        match self.precheck.take() {
            Some(precheck) => {
                std::thread::Builder::new()
                    .name(format!("{}-precheck", self.name))
                    .spawn(move || {
                        if let Err(e) = precheck() {
                            send_fatal(&run, &tx, format!("{} 预检失败：{}", cfg.command, e));
                            return;
                        }
                        if !run.stopped.load(Ordering::Relaxed) {
                            spawn_child(&cfg, &run, &tx);
                        }
                    })
                    .ok();
            }
            None => spawn_child(&cfg, &run, &tx),
        }
    }

    fn stop(&mut self) {
        if let Some(run) = self.running.take() {
            run.stopped.store(true, Ordering::Relaxed);
            kill_child(&run);
        }
    }
}

fn spawn_child(cfg: &Arc<ChildCfg>, run: &Arc<Running>, tx: &Sender<ProviderMsg>) {
    if run.stopped.load(Ordering::Relaxed) {
        return;
    }
    let mut cmd = Command::new(&cfg.command);
    cmd.args(&cfg.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    if cfg.hide_window {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            send_fatal(run, tx, format!("{}: {}", cfg.command, e));
            return;
        }
    };
    let stderr = child.stderr.take();
    let stdout = child.stdout.take();
    *run.child.lock().unwrap() = Some(child);
    run.stderr_tail.lock().unwrap().clear();

    // stderr 仅保留末尾若干段，用于失败时的诊断信息
    if let Some(stderr) = stderr {
        let run2 = run.clone();
        std::thread::Builder::new()
            .spawn(move || {
                let mut buf = [0u8; 4096];
                let mut f = stderr;
                while let Ok(n) = f.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    let mut tail = run2.stderr_tail.lock().unwrap();
                    tail.push_back(String::from_utf8_lossy(&buf[..n]).into_owned());
                    if tail.len() > 10 {
                        tail.pop_front();
                    }
                }
            })
            .ok();
    }

    // 探活看门狗：超时未吐行则杀掉子进程（由 EOF 路径发送 fatal）
    if !run.activated.load(Ordering::Relaxed) {
        let run2 = run.clone();
        let timeout = cfg.probe_timeout_ms;
        std::thread::Builder::new()
            .spawn(move || {
                std::thread::sleep(Duration::from_millis(timeout));
                if !run2.activated.load(Ordering::Relaxed)
                    && !run2.stopped.load(Ordering::Relaxed)
                    && !run2.fatal_sent.load(Ordering::Relaxed)
                {
                    kill_child(&run2);
                }
            })
            .ok();
    }

    // stdout 读行线程：EOF 后负责收割与退避重启
    let run2 = run.clone();
    let tx2 = tx.clone();
    let cfg2 = cfg.clone();
    std::thread::Builder::new()
        .spawn(move || {
            if let Some(stdout) = stdout {
                let reader = BufReader::new(stdout);
                for line in reader.split(b'\n') {
                    match line {
                        Ok(bytes) => {
                            if !handle_line(&run2, &tx2, bytes) {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
            }
            reap_or_respawn(&cfg2, &run2, &tx2);
        })
        .ok();
}

/// 处理一行输出；返回 false 表示应停止读取（fatal 或已停止）
fn handle_line(run: &Arc<Running>, tx: &Sender<ProviderMsg>, bytes: Vec<u8>) -> bool {
    if run.stopped.load(Ordering::Relaxed) || run.fatal_sent.load(Ordering::Relaxed) {
        return false;
    }
    let s = String::from_utf8_lossy(&bytes);
    let s = s.trim();
    if s.is_empty() {
        return true;
    }
    let obj: Value = match serde_json::from_str(s) {
        Ok(v) => v,
        Err(_) => return true, // 非 JSON 行忽略
    };
    if obj.get("ok") == Some(&Value::Bool(false)) {
        let err = obj
            .get("err")
            .and_then(|e| e.as_str())
            .unwrap_or("unsupported");
        if send_fatal(run, tx, format!("不可用：{err}")) {
            kill_child(run);
        }
        return false;
    }
    run.activated.store(true, Ordering::Relaxed); // 首条有效行即探活通过（看门狗自行失效）
    run.restarts.store(0, Ordering::Relaxed);
    let mapped = {
        let mut guard = run.map_line.lock().unwrap();
        match guard.as_mut() {
            Some(f) => f(&obj),
            None => map_raw_sample(&obj),
        }
    };
    let _ = tx.send(ProviderMsg::Sample(mapped));
    true
}

/// EOF 后：收割子进程；未激活则 fatal，激活过则按指数退避自动重启
fn reap_or_respawn(cfg: &Arc<ChildCfg>, run: &Arc<Running>, tx: &Sender<ProviderMsg>) {
    let exited = run.child.lock().unwrap().take();
    let mut child = match exited {
        Some(c) => c,
        None => return, // 已被 stop / fatal 路径收走
    };
    let code = child.wait().ok().and_then(|st| st.code());
    drop(child);
    if run.stopped.load(Ordering::Relaxed) || run.fatal_sent.load(Ordering::Relaxed) {
        return;
    }
    if !run.activated.load(Ordering::Relaxed) {
        let tail: String = {
            let tail = run.stderr_tail.lock().unwrap();
            tail.iter().cloned().collect::<Vec<_>>().join("").trim().to_string()
        };
        let tail = if tail.chars().count() > 400 {
            tail.chars().skip(tail.chars().count() - 400).collect()
        } else {
            tail
        };
        send_fatal(
            run,
            tx,
            format!(
                "在输出任何数据前退出 (code={}){}",
                code.map(|c| c.to_string()).unwrap_or_else(|| "null".into()),
                if tail.is_empty() { String::new() } else { format!("：{tail}") }
            ),
        );
        return;
    }
    // 激活过说明环境可用，意外退出按退避重启（1s、2s、4s … 封顶 30s）
    let delay = std::cmp::min(30_000, 1000u64 << run.restarts.load(Ordering::Relaxed).min(5));
    run.restarts.fetch_add(1, Ordering::Relaxed);
    std::thread::sleep(Duration::from_millis(delay));
    if !run.stopped.load(Ordering::Relaxed) && !run.fatal_sent.load(Ordering::Relaxed) {
        spawn_child(cfg, run, tx);
    }
}

fn kill_child(run: &Arc<Running>) {
    let child = run.child.lock().unwrap().take();
    if let Some(mut c) = child {
        let _ = c.kill();
        let _ = c.wait();
    }
}

/// 只发送一次 fatal；返回是否为首发送
fn send_fatal(run: &Arc<Running>, tx: &Sender<ProviderMsg>, msg: String) -> bool {
    if run
        .fatal_sent
        .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
        .is_ok()
    {
        let _ = tx.send(ProviderMsg::Fatal(msg));
        true
    } else {
        false
    }
}
