use crate::providers::spawn_line::{MapLine, SpawnLineOptions, SpawnLineProvider};
use crate::types::Provider;

/* ------------------------------------------------------------------ */
/* 数据源 1：JXA 长驻脚本调用 MediaRemote（参考 nowplaying-cli 的技术路径） */
/* ------------------------------------------------------------------ */
/*
 * macOS 15.4+：MediaRemote 守护进程不再响应旧 C API，但 MRNowPlayingController
 * （私有 ObjC 类，位于 dyld 共享缓存）仍然工作。流程：
 *   NSPanel 建立窗口连接 → dlopen 框架注册私有类 → 构造 controller
 *   (singleShot=NO + requestPlaybackQueue) → 主 runloop 泵轮询 response，
 *   经 KVC 提取 metadata 与 nowPlayingInfo extras。
 * macOS < 15.4：controller 类缺失时回退旧 C API（bindFunction 绑定，
 *   回调必须派发到主队列以保证 JXA 引擎线程安全）。
 * 每个采样周期输出一行 JSON；进度基准使用 nowPlayingInfo 的 Timestamp 键，
 * 由上层按播放速率外推。
 */
/// 导出仅为调试用：可 dump 成 .js 文件交给 osascript 直接运行
pub const WATCH_SCRIPT: &str = r##"ObjC.import('Foundation')
ObjC.import('AppKit')

function out(o) {
  var s = JSON.stringify(o) + '\n'
  $.NSFileHandle.fileHandleWithStandardOutput.writeData($(s).dataUsingEncoding($.NSUTF8StringEncoding))
}
function un(v) {
  // 注意：JXA 桥接的无参方法（如 NSArray.count）可能直接返回 JS 原始值，没有 isNil
  if (v === undefined || v === null) return null
  try { if (typeof v.isNil === 'function' && v.isNil()) return null } catch (e) {}
  try { return ObjC.unwrap(v) } catch (e) { return null }
}
function cnt(arr) {
  if (!arr) return 0
  var c = arr.count
  if (typeof c === 'function') c = c()
  c = Number(c)
  return isFinite(c) ? c : 0
}
function kvc(obj, key) {
  try { var v = obj.valueForKey(key); return (v && !v.isNil()) ? v : null } catch (e) { return null }
}
function num(v) { var n = Number(v); return isFinite(n) ? n : null }

var INTERVAL_MS = __INTERVAL_MS__

// NSPanel：与 WindowServer 建立应用级连接（参考系统行为所需）
try {
  $.NSPanel.alloc.initWithContentRectStyleMaskBackingDefer($.NSMakeRect(0, 0, 0, 0), 1, $.NSBackingStoreBuffered, false)
} catch (e) {}

$.NSBundle.bundleWithPath('/System/Library/PrivateFrameworks/MediaRemote.framework').load

var destCls = $.NSClassFromString('MRDestination')
var cfgCls = $.NSClassFromString('MRNowPlayingControllerConfiguration')
var ctlCls = $.NSClassFromString('MRNowPlayingController')

/* ---------------- 公共：把信息规整为输出格式 ---------------- */

function emitSample(d) {
  out({
    ok: true,
    title: d.title,
    artist: d.artist,
    album: d.album,
    app: d.app || null,
    appId: d.appId || null,
    playing: d.rate > 0,
    rate: d.rate,
    positionMs: d.positionMs,
    durationMs: d.durationMs,
    at: d.at
  })
}

function parseTimestamp(v) {
  if (v === undefined || v === null) return null
  var t = typeof v === 'string' ? Date.parse(v) : Number(v) * 1000
  return isFinite(t) && t > 0 ? t : null
}

/* ---------------- 模式 A：MRNowPlayingController（macOS 15.4+） ---------------- */

function createController() {
  try {
    var dest = destCls.userSelectedDestination
    var config = cfgCls.alloc.initWithDestination(dest)
    config.setValueForKey(ObjC.wrap(false), 'singleShot')
    config.setValueForKey(ObjC.wrap(true), 'requestPlaybackState')
    config.setValueForKey(ObjC.wrap(true), 'requestPlaybackQueue')
    var c = ctlCls.alloc.initWithConfiguration(config)
    c.beginLoadingUpdates
    return c
  } catch (e) { return null }
}

function buildSampleFromResponse(resp) {
  var state = num(un(kvc(resp, 'playbackState')))
  var rate = num(un(kvc(resp, 'playbackRate')))
  if (rate === null || rate <= 0) rate = state === 1 ? 1 : 0
  var queue = kvc(resp, 'playbackQueue')
  if (!queue) return null
  var items = kvc(queue, 'contentItems')
  var n = cnt(items)
  if (!n) return null
  var loc = num(un(kvc(queue, 'location')))
  if (loc === null || loc < 0 || loc >= n) loc = 0
  var meta = kvc(items.objectAtIndex(loc), 'metadata')
  if (!meta) return null

  // metadata 顶层字段 + nowPlayingInfo extras 合并（时长/进度/时间戳常在 extras 中）
  var extra = {}
  var eDict = null
  try {
    eDict = meta.valueForKey('nowPlayingInfo')
    if (eDict && !eDict.isNil()) extra = ObjC.deepUnwrap(eDict) || {}
    else eDict = null
  } catch (e) {}

  var title = un(kvc(meta, 'title'))
  if (!title && eDict) {
    // 无标题元数据（如 QuickTime 播放本地文件）时回退到资源 URL 的文件名
    try {
      var url = eDict.objectForKey('kMRMediaRemoteNowPlayingInfoAssetURL')
      if (url && !url.isNil()) {
        var path = un(url.absoluteString) || ''
        var base = decodeURIComponent(String(path).split('/').pop() || '')
        if (base) title = base
      }
    } catch (e2) {}
  }
  if (!title) return null

  var durationS = num(un(kvc(meta, 'duration')))
  if (durationS === null) durationS = num(extra.kMRMediaRemoteNowPlayingInfoDuration)

  // 进度：优先系统实时计算的 calculatedPlaybackPosition（最准），
  // 回退 metadata.elapsedTime / extras（此时 at 取 extras 的 Timestamp 作为基准时刻）
  var posS = num(un(kvc(meta, 'calculatedPlaybackPosition')))
  var atMs = Date.now()
  if (posS === null) {
    posS = num(un(kvc(meta, 'elapsedTime')))
    if (posS === null) posS = num(extra.kMRMediaRemoteNowPlayingInfoElapsedTime)
    if (posS !== null) {
      var ts = parseTimestamp(extra.kMRMediaRemoteNowPlayingInfoTimestamp)
      if (ts !== null) atMs = ts
    }
  }
  var extraRate = num(extra.kMRMediaRemoteNowPlayingInfoPlaybackRate)
  var rateFinal = rate > 0 ? rate : (extraRate !== null && extraRate > 0 ? extraRate : 0)

  var posMs = Math.round((posS || 0) * 1000)
  // 播放器播完后会话常滞留且状态仍上报 Playing，到达结尾视为已停止
  if (rateFinal > 0 && durationS !== null && posMs >= durationS * 1000 - 800) return null

  return {
    title: title,
    artist: un(kvc(meta, 'trackArtistName')) || un(kvc(meta, 'albumArtistName')),
    album: un(kvc(meta, 'albumName')),
    app: null, // 新 API 不暴露来源应用
    appId: null,
    rate: rateFinal,
    positionMs: posMs,
    durationMs: durationS !== null && durationS > 0 ? Math.round(durationS * 1000) : null,
    at: atMs
  }
}

function controllerLoop() {
  while (true) {
    // 每拍新建 controller：destination（userSelectedDestination）是创建时的快照，
    // 长驻会话不会跟随"正在播放"应用切换（与 nowplaying-cli 每次查询新建一致）
    var c = createController()
    if (!c) { out({ ok: false, err: 'controller-init-failed' }); $.exit(2) }
    var t0 = Date.now()
    var resp = null
    while (Date.now() - t0 < INTERVAL_MS) {
      pump(0.1)
      if (!resp) {
        var r = kvc(c, 'response')
        if (r) resp = r
      }
    }
    var sample = resp ? buildSampleFromResponse(resp) : null
    if (sample) emitSample(sample)
    else out({ ok: true, empty: true })
    try { c.endLoadingUpdates } catch (e) {}
  }
}

function pump(seconds) {
  $.NSRunLoop.mainRunLoop.runModeBeforeDate('NSDefaultRunLoopMode', $.NSDate.dateWithTimeIntervalSinceNow(seconds))
}

/* ---------------- 模式 B：旧 C API（macOS < 15.4） ---------------- */

function legacyLoop() {
  ObjC.bindFunction('MRMediaRemoteGetNowPlayingInfo', ['void', ['@', '@?']])
  ObjC.bindFunction('MRMediaRemoteGetNowPlayingApplicationPID', ['void', ['@', '@?']])
  ObjC.bindFunction('dispatch_get_current_queue', ['@', []])
  var queue = $.dispatch_get_current_queue() // 主线程调用即主队列；回调须经主 runloop 派发

  var K_TITLE = 'kMRMediaRemoteNowPlayingInfoTitle'
  var K_ARTIST = 'kMRMediaRemoteNowPlayingInfoArtist'
  var K_ALBUM = 'kMRMediaRemoteNowPlayingInfoAlbum'
  var K_ELAPSED = 'kMRMediaRemoteNowPlayingInfoElapsedTime'
  var K_DURATION = 'kMRMediaRemoteNowPlayingInfoDuration'
  var K_RATE = 'kMRMediaRemoteNowPlayingInfoPlaybackRate'
  var K_TS = 'kMRMediaRemoteNowPlayingInfoTimestamp'
  var K_BUNDLE = 'kMRMediaRemoteNowPlayingInfoApplicationBundleIdentifier'

  var info = null
  var infoBlock = ObjC.block(function (d) { info = d ? ObjC.deepUnwrap(d) : null }, ['void', ['@']])
  var pid = null
  var pidBlock = ObjC.block(function (p) { pid = p }, ['void', ['q']])

  function appInfo(bundleId) {
    var name = null, bundle = un(bundleId)
    try {
      if (bundle) {
        var apps = $.NSRunningApplication.runningApplicationsWithBundleIdentifier($(bundle))
        var n = apps.count
        if (typeof n === 'function') n = n()
        if (n > 0) {
          name = un(apps.objectAtIndex(0).localizedName)
        }
      }
    } catch (e) {}
    if (!name) {
      try {
        pid = null
        $.MRMediaRemoteGetNowPlayingApplicationPID(queue, pidBlock)
        var t1 = Date.now()
        while (pid === null && Date.now() - t1 < 400) pump(0.02)
        var p = Number(pid) & 0xFFFFFFFF
        if (p > 2147483647) p -= 4294967296
        if (p > 0) {
          var app = $.NSRunningApplication.runningApplicationWithProcessIdentifier(p)
          if (app && !app.isNil()) {
            name = un(app.localizedName)
            bundle = bundle || un(app.bundleIdentifier)
          }
        }
      } catch (e2) {}
    }
    return { name: name, bundle: bundle }
  }

  function buildSampleFromLegacy(d) {
    if (!d || !d[K_TITLE]) return null
    var rate = num(d[K_RATE])
    rate = rate !== null && rate > 0 ? rate : 0
    var ai = appInfo(d[K_BUNDLE])
    var atMs = parseTimestamp(d[K_TS]) || Date.now()
    return {
      title: '' + d[K_TITLE],
      artist: d[K_ARTIST] ? '' + d[K_ARTIST] : null,
      album: d[K_ALBUM] ? '' + d[K_ALBUM] : null,
      app: ai.name,
      appId: ai.bundle,
      rate: rate,
      positionMs: Math.round((num(d[K_ELAPSED]) || 0) * 1000),
      durationMs: num(d[K_DURATION]) !== null && num(d[K_DURATION]) > 0 ? Math.round(num(d[K_DURATION]) * 1000) : null,
      at: atMs
    }
  }

  // 启动探活：3 秒内回调不触发说明被系统限制，退出降级
  info = undefined
  $.MRMediaRemoteGetNowPlayingInfo(queue, infoBlock)
  var t0 = Date.now()
  while (info === undefined && Date.now() - t0 < 3000) pump(0.05)
  if (info === undefined) { out({ ok: false, err: 'mediaremote-blocked' }); $.exit(2) }

  while (true) {
    var t1 = Date.now()
    info = undefined
    $.MRMediaRemoteGetNowPlayingInfo(queue, infoBlock)
    while (info === undefined && Date.now() - t1 < 1500) pump(0.02)
    var sample = info !== undefined && info !== null ? buildSampleFromLegacy(info) : null
    if (sample) emitSample(sample)
    else out({ ok: true, empty: true })
    var rest = INTERVAL_MS - (Date.now() - t1)
    if (rest > 0) pump(rest / 1000)
  }
}

/* ---------------- 入口 ---------------- */

if (destCls && cfgCls && ctlCls) {
  controllerLoop()
} else {
  legacyLoop()
}
"##;

/// 把 watch 脚本的采样间隔占位符替换为实际值（毫秒，最小 200）
pub fn render_watch_script(interval_ms: u64) -> String {
    WATCH_SCRIPT.replace("__INTERVAL_MS__", &interval_ms.max(200).to_string())
}

fn create_media_remote_provider(interval_ms: u64) -> SpawnLineProvider {
    SpawnLineProvider::new(
        "macos-mediaremote",
        SpawnLineOptions {
            command: "osascript".into(),
            args: vec![
                "-l".into(),
                "JavaScript".into(),
                "-e".into(),
                render_watch_script(interval_ms),
            ],
            probe_timeout_ms: 10_000,
            precheck: None,
            map_line: None,
            hide_window: false,
        },
    )
}

/* ------------------------------------------------------------------ */
/* 数据源 2：vendored mediaremote-adapter（见 macos_adapter.rs）          */
/* 数据源 3：AppleScript 查询 Spotify / Music（始终可用的兜底）             */
/* ------------------------------------------------------------------ */

const APP_SCRIPT: &str = r##"
ObjC.import('Foundation')
ObjC.import('stdlib')

function out(o) {
  var s = JSON.stringify(o) + '\n'
  $.NSFileHandle.fileHandleWithStandardOutput.writeData($(s).dataUsingEncoding($.NSUTF8StringEncoding))
}
function num(v) { var n = Number(v); return isFinite(n) ? n : null }

function probeSpotify() {
  try {
    var sp = Application('Spotify')
    if (!sp.running()) return null
    var st = '' + sp.playerState()
    if (st === 'stopped') return null
    var t = sp.currentTrack
    var dur = num(t.duration()) // Spotify 返回毫秒
    var pos = num(sp.playerPosition()) || 0
    return {
      ok: true,
      title: '' + t.name(),
      artist: '' + t.artist(),
      album: '' + t.album(),
      app: 'Spotify',
      appId: 'com.spotify.client',
      playing: st === 'playing',
      rate: st === 'playing' ? 1 : 0,
      positionMs: Math.round(pos * 1000),
      durationMs: dur !== null && dur > 0 ? Math.round(dur) : null,
      at: Date.now()
    }
  } catch (e) { return null }
}

function probeMusic() {
  try {
    var mu = Application('Music')
    if (!mu.running()) return null
    var st = '' + mu.playerState()
    if (st === 'stopped') return null
    var t = mu.currentTrack
    var dur = null
    try {
      // Music 的 time 是 "m:ss" / "h:mm:ss" 文本
      var s = '' + t.time()
      if (s && s !== 'missing value') {
        var parts = s.split(':')
        dur = 0
        for (var i = 0; i < parts.length; i++) dur = dur * 60 + Number(parts[i])
        dur = dur * 1000
      }
    } catch (e2) {}
    var pos = num(mu.playerPosition()) || 0
    return {
      ok: true,
      title: '' + t.name(),
      artist: '' + t.artist(),
      album: '' + t.album(),
      app: 'Music',
      appId: 'com.apple.Music',
      playing: st === 'playing',
      rate: st === 'playing' ? 1 : 0,
      positionMs: Math.round(pos * 1000),
      durationMs: dur !== null && dur > 0 ? Math.round(dur) : null,
      at: Date.now()
    }
  } catch (e) { return null }
}

var INTERVAL = __INTERVAL_SEC__
while (true) {
  var info = probeSpotify() || probeMusic()
  if (info) out(info)
  else out({ ok: true, empty: true })
  delay(INTERVAL)
}
"##;

/// 导出仅为调试用：可 dump 成 .js 文件交给 osascript 直接运行
pub fn render_app_script(interval_ms: u64) -> String {
    // JS Number→String：整数不带小数点（1 → "1"，0.5 → "0.5"）
    let interval = (interval_ms as f64 / 1000.0).max(0.2);
    let text = if interval == interval.trunc() && interval.abs() < 1e21 {
        format!("{}", interval as i64)
    } else {
        format!("{}", interval)
    };
    APP_SCRIPT.replace("__INTERVAL_SEC__", &text)
}

fn create_apple_script_provider(interval_ms: u64) -> SpawnLineProvider {
    SpawnLineProvider::new(
        "macos-applescript",
        SpawnLineOptions {
            command: "osascript".into(),
            args: vec![
                "-l".into(),
                "JavaScript".into(),
                "-e".into(),
                render_app_script(interval_ms),
            ],
            probe_timeout_ms: 6_000,
            precheck: None,
            map_line: None,
            hide_window: false,
        },
    )
}

/// macOS 数据源列表（按优先级降级）：JXA 控制器 API → vendored adapter → AppleScript
pub fn create_mac_providers(interval_ms: u64) -> Vec<Box<dyn Provider>> {
    vec![
        Box::new(create_media_remote_provider(interval_ms)), // 主力：JXA 控制器 API，实测 macOS 15.4+ 可用
        crate::providers::macos_adapter::create_adapter_provider(interval_ms), // vendored adapter：perl 托管，全 macOS 版本兼容 + 提供来源应用
        Box::new(create_apple_script_provider(interval_ms)), // 兜底：仅 Spotify / Music
    ]
}

/// 仅供测试引用（保持与原版一致的导出面）
pub type MacMapLine = MapLine;
