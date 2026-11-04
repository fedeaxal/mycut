use std::sync::mpsc::Sender;

/// 对外暴露的「正在播放」信息（进度已外推到查询时刻）
#[derive(Debug, Clone, PartialEq)]
pub struct NowPlayingInfo {
    /// 曲目名
    pub title: String,
    /// 艺术家
    pub artist: Option<String>,
    /// 专辑
    pub album: Option<String>,
    /// 来源应用（友好的显示名，如 "Spotify"）
    pub app: Option<String>,
    /// 来源应用原始标识（bundle id / AUMID）
    pub app_id: Option<String>,
    /// 是否正在播放
    pub playing: bool,
    /// 当前播放进度（毫秒）。基于最后一次采样 + 播放速率实时外推
    pub position_ms: i64,
    /// 总时长（毫秒），未知时为 None（如直播流）
    pub duration_ms: Option<i64>,
    /// 本次查询时间（epoch 毫秒）
    pub updated_at: i64,
}

/// 各平台数据源输出的原始采样（未经外推）
#[derive(Debug, Clone, PartialEq)]
pub struct RawSample {
    pub title: String,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub app: Option<String>,
    pub app_id: Option<String>,
    pub playing: bool,
    /// 播放速率，暂停时为 0
    pub rate: f64,
    /// 采样时刻的进度（毫秒）
    pub position_ms: f64,
    /// 总时长（毫秒），未知为 None
    pub duration_ms: Option<f64>,
    /// positionMs 对应的采样时刻（epoch 毫秒）
    pub at: i64,
}

/// 数据源事件：采样到达，或确定自身不可用（由上层切换到下一个数据源）
#[derive(Debug)]
pub enum ProviderMsg {
    /// 与原版一致：None 表示「正常但当前无媒体」（心跳行 / 空标题）
    Sample(Option<RawSample>),
    Fatal(String),
}

/// 平台数据源接口
pub trait Provider: Send {
    fn name(&self) -> &'static str;
    /// 启动数据源；此后事件经 `tx` 持续送达（Sample / Fatal）
    fn start(&mut self, tx: Sender<ProviderMsg>);
    /// 停止并回收子进程
    fn stop(&mut self);
}

#[derive(Debug, Clone, Copy)]
pub struct WatcherOptions {
    /// 采样间隔（毫秒），默认 1000，最小 200
    pub interval_ms: u64,
}

impl Default for WatcherOptions {
    fn default() -> Self {
        WatcherOptions { interval_ms: 1000 }
    }
}
