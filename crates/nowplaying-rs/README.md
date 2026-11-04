# nowplaying-rs

在 Rust 中获取 macOS / Windows 系统「正在播放」的媒体名称与实时播放进度。

Rust 移植版，与 [opencut-desktop/desktop-shell/nowplaying](../../../opencut-desktop/desktop-shell/nowplaying)（TypeScript 版）**功能 100% 对应**：
零外部运行时依赖（vendored 的 mediaremote-adapter 已内嵌进二进制），通过平台原生机制读取，无注入、无辅助权限弹窗（AppleScript 兜底路径首次使用时需授权一次）。

## 原理

| 平台 | 数据源（按优先级降级） | 覆盖范围 |
| --- | --- | --- |
| Windows | `windows-smtc`：系统自带 Windows PowerShell 5.1 投影 WinRT SMTC API（自动加载 WinRT 支持程序集，AsTask 反射 + Status 轮询双路径） | SMTC 的所有播放器：PotPlayer（≥240618）、Edge、Spotify、QQ音乐…，含标题+进度 |
| Windows | `windows-title`：枚举窗口标题解析播放器与媒体名（零前置条件保底） | PotPlayer/VLC/mpv/MPC/foobar2000 等桌面播放器，仅标题、无进度 |
| macOS | `macos-mediaremote`：osascript(JXA) 长驻脚本调用私有 `MRNowPlayingController` API（macOS 15.4+）或旧 C API（更早版本） | 所有注册系统"正在播放"的应用 |
| macOS | `macos-adapter`：vendored [mediaremote-adapter](https://github.com/ungive/mediaremote-adapter)（BSD-3-Clause，双架构预编译 framework **内嵌于二进制**，运行时物化到用户临时目录）。经系统 perl 托管调用 MediaRemote，对 com.apple. 前缀客户端放行的机制覆盖**所有 macOS 版本（含 15.4+ / Tahoe）**，并提供来源应用标识 | 同上 |
| macOS | `macos-applescript`：AppleScript 查询 Spotify / Music，始终可用的兜底 | 仅 Spotify 与 Music.app |

所有数据源按序自动探测与降级，任何一层失败都会自动切换到下一层，无需任何手动配置。
Windows 上即使 SMTC 不可用（播放器未启用/系统限制），窗口标题数据源仍能显示 PotPlayer、VLC、
mpv 等桌面播放器正在播放的文件名（无进度）。

### macOS 实现细节（`macos-mediaremote`，默认数据源）

macOS 15.4 起 `mediaremoted` 不再响应传统的 `MRMediaRemoteGetNowPlayingInfo` C API。
本实现参考 [ungive/nowplaying-cli](https://github.com/ungive/nowplaying-cli)（GPL-3.0，仅借鉴技术路径，代码为独立实现）采用系统仍在使用的新接口：

1. 创建隐藏 `NSPanel` 与 WindowServer 建立应用级连接；
2. `dlopen` MediaRemote 框架，使 dyld 共享缓存中的私有 ObjC 类注册进运行时；
3. 每个采样周期新建一次 `MRNowPlayingController`（`singleShot=NO` + `requestPlaybackQueue`，
   destination 快照不会跟随"正在播放"应用切换，因此须重建）；
4. 经 KVC 从 `playbackQueue.contentItems[].metadata` 提取标题/艺术家/专辑；
   播放进度优先取系统实时计算的 `calculatedPlaybackPosition`，回退
   `elapsedTime` + `nowPlayingInfo` extras 中的 `Timestamp`；
   无标题元数据时回退到 `AssetURL` 文件名；
5. macOS < 15.4 上私有类缺失时，自动切换旧 C API（`ObjC.bindFunction` 绑定，回调派发至主队列保证 JXA 线程安全）。

进度推进由上层按「采样时刻 + 播放速率」外推，因此 1 秒采样间隔即可保证任意时刻查询准确。

## 使用

```bash
cargo build --release   # 产物 target/release/nowplaying

# CLI
nowplaying                # 只输出正在播放的媒体，停止/暂停后无任何输出
nowplaying --json         # 每秒输出一行 JSON（含暂停状态，供程序消费）
nowplaying --interval 500 # 采样间隔（毫秒，最小 200）

# 调试：dump 各数据源的子进程脚本，可直接执行观察原始输出
nowplaying --dump-watch   # macOS JXA watch 脚本   → osascript -l JavaScript watch.js
nowplaying --dump-app     # macOS Spotify/Music 脚本
nowplaying --dump-smtc    # Windows SMTC PowerShell 脚本
nowplaying --dump-title   # Windows 窗口标题 PowerShell 脚本

# 调试探针：单独激活 vendored adapter 数据源
cargo run --example adapter_probe 8

# 测试（仅覆盖纯逻辑：规整化/外推/假播放过滤/窗口标题解析/base64）
cargo test
```

输出示例：

```json
{"title":"情书.mkv","artist":null,"album":null,"app":null,"appId":null,"playing":true,"positionMs":57821,"durationMs":6991296,"updatedAt":1789294433428}
```

### 作为库使用

```rust
use std::time::Duration;
use nowplaying::{NowPlayingWatcher, WatcherOptions};

let mut watcher = NowPlayingWatcher::new(WatcherOptions { interval_ms: 1000 });
watcher.on_provider(|name| println!("数据源: {name}"));
watcher.on_update(|info| {
    // info 为 None 表示当前没有媒体在播放
    if let Some(info) = info {
        println!("{} - {} {}/{}ms ({})",
            info.title, info.artist.as_deref().unwrap_or(""),
            info.position_ms, info.duration_ms.unwrap_or(0),
            info.app.as_deref().unwrap_or(""));
    }
});
watcher.on_error(|msg| eprintln!("{msg}"));
watcher.start();

// 也可以随时主动查询（进度已外推到当前时刻）
let now = watcher.get_now();

// 退出前回收子进程
watcher.stop();
```

完整示例：`cargo run --example get_now`。

## 设计说明

- **长驻子进程**：每个数据源只拉起一条子进程按行输出 JSON，读行线程流式解析，
  避免每次轮询反复创建进程的开销；意外退出按指数退避自动重启（1s→30s 封顶）。
- **进度外推**：数据源低频采样（默认 1 秒），上层按「采样时刻 + 播放速率」实时外推进度，
  高频调用 `get_now()` 不增加系统开销。
- **自动降级**：数据源启动时预检 + 探活（超时未吐行判定不可用），失败自动切换下一个，
  切换过程通过 `on_provider` 回调可见。
- **假播放识别**：播放器停止后系统会话可能滞留（仍上报 playing 但进度停滞/振荡），
  滑动窗口进度检测（窗口 ≥2.5s 且净推进 <1.5s 判假）将其过滤为 None；
  seek 大跳变（>5s）与换曲自动重置窗口。进度到片尾同样判定为停止。
- Windows 端通过 `-EncodedCommand` 传递脚本（UTF-16LE base64），UTF-8 输出编码，中文曲名无乱码。
- 线程模型：watcher 1 线程 + 每个激活的数据源 3 线程（读行 / stderr / 探活看门狗），
  无异步运行时依赖，采样间隔可低至 200ms。

## 与 TypeScript 原版的对应关系

| TS | Rust |
| --- | --- |
| `NowPlayingWatcher`（EventEmitter） | `NowPlayingWatcher`（回调 `on_update` / `on_provider` / `on_error` + `get_now()`） |
| `NowPlayingInfo` / `RawSample` | 同名结构体（`app_id`/`position_ms`/`duration_ms`/`updated_at` 蛇形命名，JSON 输出保持原 camelCase 字段） |
| `SpawnLineProvider` | `providers::spawn_line::SpawnLineProvider` |
| `createStaleFilter` / `toInfo` / `mapRawSample` | `util::StaleFilter` / `util::to_info` / `util::map_raw_sample` |
| vendor 目录随包分发 + asar 兜底拷贝 | framework/perl `include_bytes!` 内嵌，运行时物化到 `$TMPDIR/nowplaying-rs-vendor` |

唯一有意差异：fatal 消息不再重复数据源名（原版最终错误形如 `windows-smtc(windows-smtc 不可用：…)`，Rust 版为 `windows-smtc(不可用：…)`）。

## 平台要求

- Windows 10 1809+（SMTC API）；必须使用系统自带 `powershell.exe`（5.1），pwsh 7 不支持 WinRT 投影。
- macOS：系统自带 `osascript` 与 `/usr/bin/perl`，无第三方依赖。
- 其它平台（Linux 等）：无内置数据源，触发 `on_error`（与原版行为一致）。

## 已知限制

- macOS `MRNowPlayingController` 新 API 不暴露来源应用名（`app` 为 `null`）；需要应用名时
  adapter 数据源可提供（`bundleIdentifier`）。
- 私有 API 可能随 macOS 大版本更新变化（参考实现已验证至 macOS 15.6 / Sequoia；本移植在 15.7 实测通过）。
- 直播流等无总时长的媒体 `durationMs` 为 `null`。
- 播放器停止后的滞留会话由假播放识别自动静默（停止后约数秒至十几秒），真播放恢复后自动重新显示。

## 许可

- 本项目代码与 TypeScript 原版一致随宿主项目分发。
- `vendor/macos/`（mediaremote-adapter.pl + MediaRemoteAdapter.framework）来自
  [ungive/mediaremote-adapter](https://github.com/ungive/mediaremote-adapter)，BSD-3-Clause，
  见 `vendor/macos/LICENSE-MediaRemoteAdapter`。
