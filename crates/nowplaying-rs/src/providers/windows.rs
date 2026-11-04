use crate::providers::spawn_line::{SpawnLineOptions, SpawnLineProvider};

/**
 * Windows 数据源：通过系统自带的 Windows PowerShell 5.1 投影 WinRT 的
 * GlobalSystemMediaTransportControlsSessionManager（SMTC），可读取系统中
 * 任意注册了系统媒体控件的播放器（Spotify、浏览器、PotPlayer、QQ音乐…）。
 *
 * 脚本以单条长驻进程运行，每个周期输出一行 JSON，避免反复拉起进程的开销。
 * 注意：必须用 powershell.exe（5.1），pwsh 7+ 不支持 ContentType=WindowsRuntime 投影。
 * 需要 Windows 10 1809+。
 */
pub const SMTC_SCRIPT: &str = r##"
$ErrorActionPreference = 'SilentlyContinue'
try { [Console]::OutputEncoding = New-Object System.Text.UTF8Encoding($false) } catch {}

# 初始化各步骤失败时收集原因，fatal 时一并上报，便于远程诊断
$initErr = @()

# WinRT 的 CLR 支持程序集：IAsyncOperation 的投影（Status/GetResults）依赖它。
# 多数环境碰巧已加载，部分机器必须显式加载。三重策略确保成功。
$rtLoaded = $false
try { Add-Type -AssemblyName System.Runtime.WindowsRuntime; $rtLoaded = $true } catch {
  $initErr += 'add-type: ' + $_.Exception.Message
}
if (-not $rtLoaded) {
  try {
    $rtDir = [System.Runtime.InteropServices.RuntimeEnvironment]::GetRuntimeDirectory()
    [void][System.Reflection.Assembly]::LoadFrom((Join-Path $rtDir 'System.Runtime.WindowsRuntime.dll'))
    $rtLoaded = $true
  } catch {
    $initErr += 'load-from: ' + $_.Exception.Message
  }
}
if ($rtLoaded) {
  try { [void][System.WindowsRuntimeSystemExtensions] } catch {
    $initErr += 'projection-type: ' + $_.Exception.Message
    $rtLoaded = $false
  }
}
if (-not $rtLoaded) { $initErr += 'WinRT runtime assembly unavailable' }

try {
  [void][Windows.Media.Control.GlobalSystemMediaTransportControlsSessionManager, Windows.Media.Control, ContentType = WindowsRuntime]
} catch {
  $initErr += 'winrt-type-load: ' + $_.Exception.Message
}

# 异步等待：优先 AsTask 反射（社区标准模式，支持程序集加载后即可用），
# 回退轮询 Status。AsyncStatus: Started/Completed/Canceled/Error
$script:AsTaskGeneric = $null
try {
  $script:AsTaskGeneric = ([System.WindowsRuntimeSystemExtensions].GetMethods() |
    Where-Object { $_.Name -eq 'AsTask' -and $_.GetParameters().Count -eq 1 -and $_.GetParameters()[0].ParameterType.Name -eq 'IAsyncOperation`1' })[0]
} catch {
  $initErr += 'as-task-reflection: ' + $_.Exception.Message
}

function AwaitOp($op, $resultType, $timeoutMs) {
  if (-not $op) { throw 'null-async-op' }
  if ($script:AsTaskGeneric) {
    $task = $script:AsTaskGeneric.MakeGenericMethod($resultType).Invoke($null, @($op))
    if (-not $task.Wait($timeoutMs)) { throw 'task-timeout' }
    return $task.Result
  }
  $st = "$($op.Status)"
  if ($st -eq '') { throw 'status-unreadable (WinRT projection unavailable)' }
  $sw = [System.Diagnostics.Stopwatch]::StartNew()
  while ($st -eq 'Started' -and $sw.ElapsedMilliseconds -lt $timeoutMs) {
    Start-Sleep -Milliseconds 30
    $st = "$($op.Status)"
  }
  if ($st -ne 'Completed') { throw "async-status=$st" }
  $op.GetResults()
}

$mgr = $null
try {
  $mgr = AwaitOp ([Windows.Media.Control.GlobalSystemMediaTransportControlsSessionManager]::RequestAsync()) ([Windows.Media.Control.GlobalSystemMediaTransportControlsSessionManager]) 10000
} catch {
  $initErr += 'request-async: ' + $_.Exception.Message
}
if (-not $mgr) {
  ConvertTo-Json -InputObject @{ ok = $false; err = ('smtc-init-failed: ' + ($initErr -join '; ')) } -Compress
  exit 1
}

while ($true) {
  try {
    $best = $null
    foreach ($s in $mgr.GetSessions()) {
      if ($s.GetPlaybackInfo().PlaybackStatus -eq 'Playing') { $best = $s; break }
      if (-not $best) { $best = $s }
    }
    $emitted = $false
    if ($best) {
      $mp = AwaitOp ($best.TryGetMediaPropertiesAsync()) ([Windows.Media.Control.GlobalSystemMediaTransportControlsSessionMediaProperties]) 5000
      if ($mp.Title -and $mp.Title.Trim()) {
        $tl = $best.GetTimelineProperties()
        $pi = $best.GetPlaybackInfo()
        $status = '' + $pi.PlaybackStatus
        $rate = [double]$pi.PlaybackRate
        if ($rate -le 0 -or $rate -gt 16) { $rate = 1 }
        $pos = [int64]$tl.Position.TotalMilliseconds
        $dur = [int64]$tl.EndTime.TotalMilliseconds
        $artist = $mp.Artist
        if (-not $artist) { $artist = $mp.AlbumArtist }
        # 播放器播完后会话常滞留且状态仍上报 Playing，到达结尾视为已停止
        if ($status -eq 'Playing' -and $dur -gt 0 -and $pos -ge ($dur - 800)) {
          ConvertTo-Json -InputObject @{ ok = $true; empty = $true } -Compress
          $emitted = $true
        } else {
          [ordered]@{
            ok         = $true
            title      = $mp.Title
            artist     = $artist
            album      = $mp.AlbumTitle
            app        = $best.SourceAppUserModelId
            playing    = ($status -eq 'Playing')
            rate       = $rate
            positionMs = $pos
            durationMs = $dur
            at         = $tl.LastUpdatedTime.ToUnixTimeMilliseconds()
          } | ConvertTo-Json -Compress
          $emitted = $true
        }
      }
    }
    if (-not $emitted) {
      ConvertTo-Json -InputObject @{ ok = $true; empty = $true } -Compress
    }
  } catch {
    # 单次查询失败（如应用退出竞态）：发心跳保活，下个周期重试
    ConvertTo-Json -InputObject @{ ok = $true; empty = $true } -Compress
  }
  Start-Sleep -Milliseconds __INTERVAL_MS__
}
"##;

/// 导出仅供诊断脚本使用：dump 成 .ps1 文件直接执行观察原始行为
pub fn render_smtc_script(interval_ms: u64) -> String {
    SMTC_SCRIPT.replace("__INTERVAL_MS__", &interval_ms.max(200).to_string())
}

fn utf16le_base64(s: &str) -> String {
    let mut bytes = Vec::with_capacity(s.len() * 2);
    for unit in s.encode_utf16() {
        bytes.push(unit as u8);
        bytes.push((unit >> 8) as u8);
    }
    base64_encode(&bytes)
}

/// PowerShell -EncodedCommand 参数：UTF-16LE 的 base64
pub fn encoded_ps_command(script: &str) -> String {
    utf16le_base64(script)
}

const B64_ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// 标准 base64（含 padding），等价于 Buffer#toString('base64')
pub fn base64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(B64_ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(B64_ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { B64_ALPHABET[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { B64_ALPHABET[n as usize & 63] as char } else { '=' });
    }
    out
}

pub fn create_windows_provider(interval_ms: u64) -> SpawnLineProvider {
    SpawnLineProvider::new(
        "windows-smtc",
        SpawnLineOptions {
            command: "powershell.exe".into(),
            args: vec![
                "-NoProfile".into(),
                "-NonInteractive".into(),
                "-ExecutionPolicy".into(),
                "Bypass".into(),
                "-EncodedCommand".into(),
                utf16le_base64(&render_smtc_script(interval_ms)),
            ],
            probe_timeout_ms: 15_000, // PowerShell 冷启动 + WinRT 初始化
            precheck: None,
            map_line: None,
            hide_window: true,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn utf16le_base64_matches_node_buffer() {
        // Buffer.from('hi', 'utf16le').toString('base64') === 'aABpAA=='
        assert_eq!(utf16le_base64("hi"), "aABpAA==");
        // Buffer.from('中', 'utf16le') = <2d 4e> → 'LU4='
        assert_eq!(utf16le_base64("中"), "LU4=");
    }
}
