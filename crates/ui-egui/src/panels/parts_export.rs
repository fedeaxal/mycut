//! Quick Export ▸「匹配源 – 分P导出」的导出弹窗。
//!
//! 功能对照 opencut 的导出对话框:头部是画布信息;格式(MP4·H.264/H.265)、
//! 画质(与源相同码率/低/中/高/极高/自定义码率)、包含音频;「整段导出」
//! 与「分P导出」两张可同时勾选的卡片——分P按时长或数量拆分,实时预览文
//! 件名与起止,末P过短自动并入上一P;封面(选择/编辑/清除,编辑器复用
//! Reaction 的 reaction);投稿模板(标题/标签/简介 → `<名>.biliup.json`);
//! 输出文件夹。点「开始导出」弹窗立即关闭:进度与重试在导出模式的导出
//! 队列里看,封面与投稿模板由 `settle_in_flight` 在全部终态后落盘。
//!
//! 两张卡片同时勾选时不渲染两遍:整段只导出一次(字幕、效果、画质全在
//! 里面),每个分P再用引擎的 `export.split`(流复制,不重编码)从整段
//! 产物里切出来——见 `engine::video_split`。只勾分P仍按旧路逐P渲染。
//!
//! 分段算法是 opencut `export/segments.ts` 的帧对齐移植:边界落在整帧
//! 上,`duration` 模式每P固定帧数、末P短于 max(5s, 每P×25%) 时并入上一
//! P,`count` 模式均分、余数摊给前面的P。
//!
//! 导出本身走引擎现成的队列:每个渲染输出一次 `export.queue.add`,一次
//! `export.queue.start`;`settle_in_flight` 每帧轮询 `export.queue.list`
//! 与切分作业,全部终态后复制封面、写投稿模板。设置以导出界面的当前设
//! 置为基础——效果、硬件编码、GPU 渲染、元数据等自动跟随导出界面,字幕
//! 烧录恒开(见 `settings_for`),弹窗只覆盖格式、画质、音频与「匹配源」。
//!
//! Automation ids: `partsExport.format.<h264|hevc>`, `partsExport.quality.<k>`,
//! `partsExport.bitrate`, `partsExport.audio`, `partsExport.whole`,
//! `partsExport.parts`, `partsExport.prefix`, `partsExport.separator`,
//! `partsExport.splitMode.<k>`, `partsExport.perPart`, `partsExport.partCount`,
//! `partsExport.cover.pick`, `partsExport.cover.edit`, `partsExport.cover.clear`,
//! `partsExport.title`, `partsExport.tags`, `partsExport.desc`,
//! `partsExport.location`, `partsExport.go`, `partsExport.cancel`。

use std::path::PathBuf;

use egui::{Align2, Color32, Sense, pos2, vec2};
use filmcraft_engine::export::{AudioCodec, AudioSettings, BitrateMode, ExportSettings, Format, HardwareEncoding, Multiplexer};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::FilmcraftApp;
use crate::native_dialogs::FileDialog;
// 导出模式的 UI 套件(登记、带标签的行、下拉、拖值、勾选)——分P弹窗
// 与它同款,这是"复用剪辑界面 UI"的实体。
use crate::panels::export_mode::{DragOptions, Reg, check, combo, default_output_dir, drag, opts, row, text};
use crate::theme::Tokens;

/// Quick Export 列表里这个入口的名字。
pub const PARTS_ENTRY: &str = "Match Source – Parts Export";

/// 每个 P 的最短时长(秒)。
pub const MIN_PART_SECONDS: f64 = 1.0;
/// 末P短于该秒数视为过短。
const MIN_LAST_PART_SECONDS: f64 = 5.0;
/// 末P短于每P目标时长的该比例时同样视为过短。
const MIN_LAST_PART_FRACTION: f64 = 0.25;
/// 标签最多几个(biliup 的上限)。
pub const MAX_TAGS: usize = 10;
/// 标题最长(biliup 的上限,字符数)。
const MAX_TITLE_CHARS: usize = 80;
/// 弹窗底栏(汇总 + 取消/开始)占的高度,滚动区从内容区里先把它让出来。
const FOOTER_HEIGHT: f32 = 44.0;

/// 一段分P(秒,含头含尾)。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Part {
    /// 1 起始的序号。
    pub index: usize,
    pub start: f64,
    pub end: f64,
}

/// 拆分校验的错误种类;文案在绘制处经 i18n 展开。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SplitError {
    TooShort,
    LongerThanVideo,
    MinTwo,
    TooMany(usize),
}

/// 一次导出排进队列的一个输出。
#[derive(Clone, Debug)]
pub struct PartsItem {
    /// `export.queue` 的条目 id。
    pub id: u64,
    /// 显示用文件名。
    pub file: String,
    /// 输出绝对路径。
    pub path: String,
}

/// 「整段 + 分P」合并路里将要切出的一个分P:时间范围(秒,含头含尾)
/// 加输出文件——整段渲染完成后交 `export.split` 流复制切出。
#[derive(Clone, Debug)]
pub struct PlannedPart {
    pub part: Part,
    /// 输出文件名(`前缀_PN.<ext>`)与绝对路径。
    pub item: PartsItem,
}

/// 已解码的封面缩略图,连同它来自的路径——路径没变就不重读。
#[derive(Clone)]
struct CoverThumb {
    path: String,
    tex: egui::TextureHandle,
}

/// 分P导出的片头(第一个分P):场景在 reaction 里配置的片头视频,以它
/// 自己的文件名直拷为第一个分P,不参与 前缀_PN 编号、正片从 P1 开始
/// (opencut 的 introVideoPath 同款;直拷不渲染)。复制在后台线程,完成
/// 槽由 `settle_in_flight` 轮询。
#[derive(Clone)]
pub struct IntroPart {
    /// 片头源文件的绝对路径。
    pub source: String,
    /// 输出文件名(源文件自己的文件名)。
    pub file: String,
    /// 复制结果:`None` = 在途,`Some(Ok)` = 完成,`Some(Err)` = 失败文案
    /// (已本地化)。
    pub slot: std::sync::Arc<std::sync::Mutex<Option<Result<(), String>>>>,
}

/// 片头复制的当前结果(在途为 `None`);锁毒化也取出已存的结果——
/// 复制线程在持锁时崩溃不该把导出卡在"在途"上。
fn intro_copy_state(intro: &IntroPart) -> Option<Result<(), String>> {
    match intro.slot.lock() {
        Ok(slot) => slot.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    }
}

/// 分P导出弹窗的状态(嵌在 [`crate::panels::export_mode::ExportUi`] 里,
/// 跟随它持久化;一次导出的在飞标记与队列条目不持久化)。
#[derive(Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PartsExportUi {
    pub open: bool,
    /// `h264` | `hevc`(都是 MP4 容器)。
    pub format: String,
    /// `source`(与源视频相同码率,缺省)| `low` | `medium` | `high` |
    /// `veryHigh` | `custom`。
    pub quality: String,
    /// 自定义码率(Mbps,`quality == custom` 时生效)。
    pub custom_mbps: f64,
    pub include_audio: bool,
    pub whole_enabled: bool,
    pub parts_enabled: bool,
    pub prefix: String,
    /// 前缀与 P 号之间的连接符(≤6 字符)。
    pub separator: String,
    /// `duration` | `count`。
    pub split_mode: String,
    /// 每P时长(分钟)。
    pub per_part_minutes: f64,
    pub part_count: f64,
    pub cover_path: Option<String>,
    pub bili_title: String,
    pub bili_tags: Vec<String>,
    pub bili_desc: String,
    pub location: String,
    /// 「与源视频相同码率」成为默认画质的一次性迁移标记:旧状态里的
    /// `quality == "high"`(当时的缺省)在弹窗第一次打开时翻成 `source`
    /// 并置位,之后用户的手选不再被迁移打扰。
    #[serde(default)]
    pub source_quality_migrated: bool,
    #[serde(skip)]
    pub items: Vec<PartsItem>,
    /// 导出已在飞(点「开始导出」时置位,弹窗同时关闭):`show()` 每帧
    /// 借它继续收尾——队列全部到终态、整段产物也切分完(如走合并路)、
    /// 片头也拷完后落封面与投稿模板。
    #[serde(skip)]
    pub in_flight: bool,
    /// 封面与投稿模板的文件名基名(整段的文件名,或无整段时 P1 的)。
    #[serde(skip)]
    pub base_name: String,
    /// 「整段 + 分P」合并路的切分作业(`export.split` 的 job id):整段
    /// 渲染完成后由 `settle_in_flight` 起一次,分P从整段产物里流复制切出。
    #[serde(skip)]
    pub split_job: Option<u64>,
    /// 合并路里将要切出的分P(范围与输出文件)。整段渲染成功才切分;
    /// 模板把它们与队列项、片头一起列出。
    #[serde(skip)]
    pub planned_parts: Vec<PlannedPart>,
    /// 切分作业没能启动(命令层错误,极少见):记下文案,收尾按失败报。
    #[serde(skip)]
    pub split_error: Option<String>,
    /// 标签输入框的草稿(逗号/回车提交)。
    #[serde(skip)]
    pub tag_draft: String,
    /// 用户在本会话亲手编辑过的投稿模板字段([标题, 简介, 标签]):
    /// 迟到的 AI 元数据不覆盖已编辑字段(opencut 的 biliUserEditedRef
    /// 同一约定)。新一段录制跳进编辑界面时复位。
    #[serde(skip)]
    pub bili_edited: [bool; 3],
    /// 投稿模板旁车的加载状态:`(项目路径, 旁车内容)`。None = 本会话
    /// 还没为任何项目找过旁车;`Some((路径, None))` = 找过、该项目目录
    /// 没有。弹窗首次为某项目绘制(或项目已换)时加载一次,之后不再
    /// 碰盘(绘制路径不做 FS/stat);模板三字段与旁车不同就整份回写。
    #[serde(skip)]
    pub publish_sidecar: Option<(String, Option<reaction::ai_commentary::RecordingMetadata>)>,
    /// 本项目场景的片头(分P导出的第一个分P);`None` = 项目目录里没有
    /// 片头旁车(或片头文件已不在)。
    #[serde(skip)]
    pub intro: Option<IntroPart>,
    /// 片头旁车已为哪个项目找过(只在项目变化时读一次盘)。
    #[serde(skip)]
    pub intro_loaded_for: Option<String>,
    /// 时间线源视频的视频轨码率(kbps,「与源视频相同码率」用;纯内存,
    /// 来自导入时探测的 [`filmcraft_media::MediaInfo`])。`None` = 没有
    /// 带码率的视频源(回落 High 档)。
    #[serde(skip)]
    pub source_kbps: Option<u32>,
    /// 码率缓存所属的(序列, 项目);变了才重算。
    #[serde(skip)]
    pub source_kbps_for: Option<(u64, Option<String>)>,
    /// 投稿模板合并目标的覆盖(测试隔离用;`None` = 探测 biliup-app 的
    /// config.json)。
    #[serde(skip)]
    pub biliup_config_override: Option<PathBuf>,
    /// 弹窗上一帧的矩形:模态阻挡层把这里留空(四条边带),弹窗内容
    /// 永远点得到——整块大遮罩会被「点击外部」顶到弹窗上面,把窗口
    /// 点死(实测踩过)。
    #[serde(skip)]
    pub window_rect: Option<egui::Rect>,
    #[serde(skip)]
    cover_tex: Option<CoverThumb>,
}

impl Default for PartsExportUi {
    fn default() -> Self {
        Self {
            open: false,
            format: "h264".into(),
            quality: "source".into(),
            custom_mbps: 16.0,
            include_audio: true,
            whole_enabled: true,
            parts_enabled: true,
            prefix: String::new(),
            separator: "_".into(),
            split_mode: "duration".into(),
            per_part_minutes: 2.0,
            part_count: 4.0,
            cover_path: None,
            bili_title: String::new(),
            bili_tags: Vec::new(),
            bili_desc: String::new(),
            location: String::new(),
            source_quality_migrated: false,
            in_flight: false,
            items: Vec::new(),
            base_name: String::new(),
            split_job: None,
            planned_parts: Vec::new(),
            split_error: None,
            tag_draft: String::new(),
            bili_edited: [false; 3],
            publish_sidecar: None,
            intro: None,
            intro_loaded_for: None,
            source_kbps: None,
            source_kbps_for: None,
            biliup_config_override: None,
            window_rect: None,
            cover_tex: None,
        }
    }
}

impl PartsExportUi {
    /// 迟到的 AI 元数据(标题/简介/标签)回填投稿模板:只填用户没动过的
    /// 字段,空值不覆盖——AI 迟到时用户可能已经开弹窗手填了一半。
    pub fn apply_ai_metadata(&mut self, meta: &reaction::ai_commentary::RecordingMetadata) {
        if !self.bili_edited[0] && !meta.title.is_empty() {
            self.bili_title = meta.title.clone();
        }
        if !self.bili_edited[1] && !meta.description.is_empty() {
            self.bili_desc = meta.description.clone();
        }
        if !self.bili_edited[2] && !meta.tags.is_empty() {
            self.bili_tags = meta.tags.clone();
        }
    }

    /// 新一段录制跳进编辑界面:投稿模板回到「等待本场 AI 预填」的状态
    /// ——上一场的编辑标记不再拦住本场的元数据。
    pub fn reset_bili_edits(&mut self) {
        self.bili_edited = [false; 3];
    }

    /// 项目目录旁车里的投稿模板灌回弹窗(重开项目后首次开弹窗):整体
    /// 覆盖、编辑标记复位——换了项目上下文,不是「迟到的 AI 回填」。
    pub fn apply_publish(&mut self, meta: &reaction::ai_commentary::RecordingMetadata) {
        self.bili_title = meta.title.clone();
        self.bili_desc = meta.description.clone();
        self.bili_tags = meta.tags.clone();
        self.bili_edited = [false; 3];
    }
}

impl std::fmt::Debug for PartsExportUi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PartsExportUi")
            .field("open", &self.open)
            .field("format", &self.format)
            .field("quality", &self.quality)
            .field("coverPath", &self.cover_path)
            .field("inFlight", &self.in_flight)
            .finish_non_exhaustive()
    }
}

// -------------------------------------------------------------------------------------------------
// 分段与文件名
// -------------------------------------------------------------------------------------------------

/// 计算分P边界(帧对齐)。`mode` 为 `duration`(每P `per_part_sec` 秒)或
/// `count`(`count` 等长P);合并规则见模块文档。
pub fn compute_parts(duration_sec: f64, fps: f64, mode: &str, per_part_sec: f64, count: u32) -> Vec<Part> {
    if !(duration_sec > 0.0 && fps > 0.0) {
        return Vec::new();
    }
    let total_frames = (duration_sec * fps).floor() as u64;
    if total_frames == 0 {
        return Vec::new();
    }
    let at = |frame: u64| frame as f64 / fps;
    let mut parts = Vec::new();
    if mode == "count" {
        let count = count.max(1) as u64;
        let base = total_frames / count;
        if base == 0 {
            return Vec::new();
        }
        let remainder = total_frames % count;
        let mut frame = 0u64;
        for i in 0..count {
            let length = base + u64::from(i < remainder);
            parts.push(Part { index: parts.len() + 1, start: at(frame), end: at(frame + length) });
            frame += length;
        }
    } else {
        let frames_per_part = (per_part_sec * fps).round().max(1.0) as u64;
        let min_last_frames = (MIN_LAST_PART_SECONDS * fps).max(frames_per_part as f64 * MIN_LAST_PART_FRACTION);
        let mut frame = 0u64;
        while frame < total_frames {
            let end = (frame + frames_per_part).min(total_frames);
            parts.push(Part { index: parts.len() + 1, start: at(frame), end: at(end) });
            frame = end;
        }
        // 末P过短并入上一P,免出一个几秒的尾巴P。
        let last_frames = (parts.last().map(|p| p.end - p.start).unwrap_or(0.0) * fps).round();
        if parts.len() > 1 && last_frames < min_last_frames {
            parts.pop();
        }
    }
    // 最后一段的右端贴回真实时长(帧取整丢掉的零头不丢)。
    if let Some(last) = parts.last_mut() {
        last.end = duration_sec;
    }
    parts
}

/// 校验拆分参数;`None` 为可用。
pub fn validate_split(duration_sec: f64, mode: &str, per_part_sec: f64, count: u32) -> Option<SplitError> {
    if mode == "count" {
        if count < 2 {
            return Some(SplitError::MinTwo);
        }
        let max = (duration_sec / MIN_PART_SECONDS).floor() as usize;
        if count as usize > max {
            return Some(SplitError::TooMany(max));
        }
        return None;
    }
    if per_part_sec < MIN_PART_SECONDS {
        return Some(SplitError::TooShort);
    }
    if per_part_sec >= duration_sec {
        return Some(SplitError::LongerThanVideo);
    }
    None
}

/// 替换各平台文件名的非法字符(opencut 同款:替换为 `-`),两端空白去掉。
pub fn sanitize_name(name: &str) -> String {
    name.chars().map(|c| if "<>:\"/\\|?*".contains(c) { '-' } else { c }).collect::<String>().trim().to_string()
}

/// 分P文件名:`前缀<连接符>P<序号>.<ext>`;前缀为空时 `P<序号>.<ext>`。
pub fn part_file_name(prefix: &str, separator: &str, index: usize, ext: &str) -> String {
    let safe = sanitize_name(prefix);
    let link = sanitize_name(separator);
    if safe.is_empty() {
        format!("P{index}.{ext}")
    } else {
        format!("{safe}{link}P{index}.{ext}")
    }
}

/// 导出文件名的基名:序列名剥掉媒体扩展名再过一遍非法字符替换——
/// 从录制新建的序列直接以录制文件命名(`日常.mp4`),不剥就会导出成
/// `日常.mp4.mp4`。
pub fn sequence_base_name(name: &str) -> String {
    sanitize_name(&filmcraft_engine::export_tools::strip_media_extension(name))
}

/// `mm:ss`(超过一小时 `h:mm:ss`)。
fn clock(sec: f64) -> String {
    let s = sec.round().max(0.0) as u64;
    if s >= 3600 { format!("{}:{:02}:{:02}", s / 3600, s % 3600 / 60, s % 60) } else { format!("{:02}:{:02}", s / 60, s % 60) }
}

// -------------------------------------------------------------------------------------------------
// 导出设置与投稿模板
// -------------------------------------------------------------------------------------------------

const QUALITIES: [(&str, f32); 4] = [("low", 0.05), ("medium", 0.1), ("high", 0.2), ("veryHigh", 0.4)];

/// 「匹配源」基调的设置:以导出界面的当前设置(`ExportUi.settings`)为
/// 基础——效果、GPU 渲染、元数据等自动跟随导出界面,不用在分P弹窗里
/// 再配一遍——再叠加弹窗自己的选择:格式(固定 MP4 封装)、画质、是否
/// 包含音频(音频 AAC 320k);帧尺寸与帧率留空=匹配序列。
/// 字幕烧录恒开:分P导出的交付(整段与每个分P)都该带字幕,不要求用户
/// 先去导出界面把「烧录字幕」打开;时间线上没有字幕轨时烧录是空操作。
/// 硬件编码恒为 Auto(导出界面「使用硬件编码」同款:系统有 VideoToolbox
/// / NVENC 就用,没有自动回落软件编码)——录制产物动辄数十分钟,分P导
/// 出以速度为先,不跟随导出界面的手动开关。
/// `source_kbps` 是时间线源视频的视频轨码率(`quality == source` 时作
/// 目标码率;未知则回落 High 档的自适应码率)。
pub fn settings_for(p: &PartsExportUi, base: &ExportSettings, source_kbps: Option<u32>) -> ExportSettings {
    let mut s = base.clone();
    // 弹窗自己的选择盖在导出界面之上;其余(效果/GPU 渲染等)原样带走。
    s.format = if p.format == "hevc" { Format::Hevc } else { Format::H264 };
    s.multiplexer = Multiplexer::Mp4;
    s.bitrate_mode = BitrateMode::Vbr1Pass;
    s.bitrate_kbps = 10_000;
    s.max_bitrate_kbps = None;
    s.adaptive_bitrate = None;
    s.burn_captions = true;
    s.hardware_encoding = HardwareEncoding::Auto;
    s.audio = AudioSettings { codec: AudioCodec::Aac, sample_rate: Some(48_000), channels: 2, bitrate_kbps: 320, bits: 16, ..Default::default() };
    s.include_audio = p.include_audio;
    // 「匹配源」:序列自己的帧尺寸与帧率;每个队列项自带 path 与 range,
    // 导出界面里的那两份不带走(queue.add 会按参数逐项覆盖)。
    s.frame_size = None;
    s.frame_rate = None;
    s.scale = 1.0;
    s.range = None;
    s.path.clear();
    match p.quality.as_str() {
        "source" if source_kbps.is_some() => {
            // 与源视频相同码率:目标=源视频轨码率,上限取 1.5 倍。
            let kbps = source_kbps.unwrap_or(0).max(100);
            s.bitrate_kbps = kbps;
            s.max_bitrate_kbps = Some(kbps * 3 / 2);
        }
        "custom" => {
            // 固定目标码率,上限取 1.5 倍。
            s.bitrate_kbps = (p.custom_mbps * 1000.0).round().clamp(100.0, 800_000.0) as u32;
            s.max_bitrate_kbps = Some(s.bitrate_kbps * 3 / 2);
        }
        // source 档拿不到源码率时回落 High;其余四档按 bits-per-pixel。
        quality => {
            let bpp = QUALITIES.iter().find(|(key, _)| *key == quality).map_or(0.2, |(_, bpp)| *bpp);
            s.adaptive_bitrate = Some(bpp);
        }
    }
    s
}

/// 时间线源视频的视频轨码率(kbps):逐个剪辑取导入时探测的
/// `MediaInfo.video.bitrate`,按时间线时长加权平均;剪辑没有码率时用
/// 文件大小 ÷ 时长兜底(文件总码率,含音频)。纯内存——信息来自项目
/// 里已存的 MediaInfo,不碰文件系统。没有任何视频源时返回 `None`。
pub fn source_video_bitrate(app: &FilmcraftApp, seq: filmcraft_project::ItemId) -> Option<u32> {
    let q = app.session.project.sequence(seq)?;
    let mut parts: Vec<(f64, Option<u64>)> = Vec::new(); // (时长秒, 视频轨码率 bits/s 或文件兜底)
    let mut has_video = false;
    for track in &q.video_tracks {
        for clip in &track.items {
            if !clip.enabled || clip.duration.0 <= 0 {
                continue;
            }
            let Some(media) = media_clip_of(&app.session.project, clip.item) else { continue };
            let seconds = clip.duration.0 as f64 / filmcraft_time::TICKS_PER_SECOND as f64;
            has_video = true;
            let bitrate = media.info.video.as_ref().and_then(|v| v.bitrate).or_else(|| {
                // 兜底:文件总码率(大小 ÷ 时长)。音频码率占比小,误差可接受。
                let ticks = media.info.duration.0;
                media
                    .info
                    .file_size
                    .filter(|_| ticks > 0)
                    .map(|size| (size as f64 * 8.0 * filmcraft_time::TICKS_PER_SECOND as f64 / ticks as f64) as u64)
            });
            parts.push((seconds, bitrate));
        }
    }
    if !has_video {
        return None;
    }
    weighted_bitrate(&parts)
}

/// 剪辑引用的媒体条目(子剪辑跟到父媒体)。
fn media_clip_of(project: &filmcraft_project::Project, item: filmcraft_project::ItemId) -> Option<&filmcraft_project::MediaClip> {
    let it = project.item(item)?;
    match &it.kind {
        filmcraft_project::ItemKind::Media(clip) => Some(clip),
        filmcraft_project::ItemKind::Subclip { parent, .. } => project.item(*parent)?.as_media(),
        _ => None,
    }
}

/// 源码率缓存(序列或项目变了才重算一次;计算本身纯内存)。
fn sync_source_bitrate(app: &mut FilmcraftApp, seq: filmcraft_project::ItemId) {
    let key = Some((seq.0, app.session.path.clone()));
    if app.ui.export.parts.source_kbps_for == key {
        return;
    }
    app.ui.export.parts.source_kbps = source_video_bitrate(app, seq);
    app.ui.export.parts.source_kbps_for = key;
}

/// (时长秒, 码率) 的加权平均:有码率的按时长加权;没码率的部分不参与
/// (它没有信息,不该拉低或抬高平均)。没有可用码率返回 `None`。
fn weighted_bitrate(parts: &[(f64, Option<u64>)]) -> Option<u32> {
    let (mut known_weight, mut known_sum) = (0.0f64, 0.0f64);
    for &(seconds, bitrate) in parts {
        if let Some(b) = bitrate {
            known_weight += seconds;
            known_sum += b as f64 * seconds;
        }
    }
    if known_weight <= 0.0 {
        return None;
    }
    Some((known_sum / known_weight / 1000.0).round().clamp(50.0, 800_000.0) as u32)
}

/// biliup-app 兼容的投稿模板 JSON(字段与 opencut `export/biliup.ts` 的
/// 构造一一对应;`videos[].path` 填导出视频的绝对路径,`videos[].title`
/// 由调用方给——分P是文件名去扩展名,片头固定「片头」)。封面不进模板
/// (opencut 恒为空串,封面 PNG 只复制到输出目录);`videos[].id` 用文件
/// 名——biliup-app 拿它作全局上传任务键,不同模板共用任务队列,必须唯一。
pub fn biliup_json(title: &str, desc: &str, tags: &[String], videos: &[(String, String, String)]) -> Value {
    json!({
        "copyright": 1,
        "source": "",
        "tid": 21,
        "tid_v2": 0,
        "title": title,
        "desc": desc,
        "tag": tags.join(","),
        "cover": "",
        "dtime": null,
        "dynamic": "",
        "videos": videos
            .iter()
            .map(|(filename, path, part_title)| {
                json!({
                    "id": filename,
                    "filename": filename,
                    "title": part_title,
                    "desc": "",
                    "path": path,
                    "cid": 0,
                    "finished_at": 0,
                    "encoding_status": 0,
                    "status_desc": "",
                })
            })
            .collect::<Vec<_>>(),
        "desc_v2": null,
        "subtitle": { "open": 0, "lan": "" },
        "open_subtitle": false,
        "interactive": 0,
        "mission_id": null,
        "topic_id": null,
        "topic_name": null,
        "season_id": null,
        "section_id": null,
        "dolby": 0,
        "lossless_music": 0,
        "no_reprint": 0,
        "open_elec": 0,
        "no_disturbance": 0,
        "aid": null,
        "up_selection_reply": 0,
        "up_close_reply": 0,
        "up_close_danmu": 0,
        "atomic_int": 0,
        "is_only_self": 0,
        "watermark": 0,
        "is_360": -1,
        "staff": null,
        "state": null,
        "state_desc": null,
    })
}

// -------------------------------------------------------------------------------------------------
// biliup-app 配置合并
// -------------------------------------------------------------------------------------------------

/// biliup-app 自己的 config.json(它只从这里加载模板)。macOS
/// `~/Library/Application Support/biliup`、Windows `%APPDATA%\biliup`、
/// 其余 `~/.config/biliup`;环境变量缺失时返回 None。测试经
/// `PartsExportUi.biliup_config_override` 指到别处,不碰用户机器。
fn biliup_config_path() -> Option<PathBuf> {
    let env = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    let dir = if cfg!(target_os = "macos") {
        env("HOME").map(|h| h.join("Library/Application Support/biliup"))
    } else if cfg!(windows) {
        env("APPDATA").map(|h| h.join("biliup"))
    } else {
        env("XDG_CONFIG_HOME").map(|d| d.join("biliup")).or_else(|| env("HOME").map(|h| h.join(".config/biliup")))
    };
    dir.map(|d| d.join("config.json"))
}

/// 把模板按模板名合并进 biliup-app 的 config.json(opencut 桌面端
/// `biliup:merge-template` 的移植):模板 = `config[uid].templates[名]`,
/// 全量读-改-写,`user_order` 里已登录的账号各写一份,含 cookie 的其余
/// 字段原样保留;tmp+rename 原子落盘,防止写坏这份配置。成功返回 true
/// (模板进了 biliup-app,不再需要独立文件),失败文案经 `errors` 带回。
fn merge_biliup_template(config: &std::path::Path, name: &str, template: &Value, errors: &mut Vec<String>) -> bool {
    let bytes = match std::fs::read(config) {
        Ok(bytes) => bytes,
        Err(_) => {
            errors.push(tl!("biliup config.json not found; sign in to biliup-app first").to_string());
            return false;
        }
    };
    let mut root: Value = match serde_json::from_slice(&bytes) {
        Ok(root) => root,
        Err(e) => {
            errors.push(tlf!("biliup config.json parse failed: {e}", e = e));
            return false;
        }
    };
    let ordered: Vec<String> = root
        .get("user_order")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default();
    let Some(configs) = root.get_mut("config").and_then(Value::as_object_mut) else {
        errors.push(tl!("biliup config.json is malformed (missing config); sign in to biliup-app first").to_string());
        return false;
    };
    // user_order 里的才是已登录账号;它缺着/空着就退回全部账号。
    let mut targets: Vec<String> = ordered.into_iter().filter(|uid| configs.contains_key(uid.as_str())).collect();
    if targets.is_empty() {
        targets = configs.keys().cloned().collect();
    }
    if targets.is_empty() {
        errors.push(tl!("no account is signed in to biliup-app; sign in first").to_string());
        return false;
    }
    let now = filmcraft_engine::autosave::unix_now();
    for uid in &targets {
        let user = &mut configs[uid];
        // 缺了就补,但已有的对象/数组原样保留;不是对象/数组的畸形值
        // 一并重置,免得下面的索引写恐慌。
        if !user.get("templates").is_some_and(Value::is_object) {
            user["templates"] = json!({});
        }
        user["templates"][name] = template.clone();
        if !user.get("template_order").is_some_and(Value::is_array) {
            user["template_order"] = json!([]);
        }
        if let Some(order) = user["template_order"].as_array_mut()
            && !order.iter().any(|v| v.as_str() == Some(name))
        {
            order.push(json!(name));
        }
        if !user.get("template_updated_at").is_some_and(Value::is_object) {
            user["template_updated_at"] = json!({});
        }
        user["template_updated_at"][name] = json!(now);
    }
    let mut tmp = config.as_os_str().to_os_string();
    tmp.push(".tmp");
    let write = serde_json::to_string_pretty(&root)
        .map(|text| std::fs::write(&tmp, text))
        .and_then(|wrote| wrote.map_err(serde_json::Error::io))
        .and_then(|()| std::fs::rename(&tmp, config).map_err(serde_json::Error::io));
    match write {
        Ok(()) => {
            log::info!("biliup template merged into {}", config.display());
            true
        }
        Err(e) => {
            errors.push(tlf!("write biliup config.json failed: {e}", e = e));
            false
        }
    }
}

// -------------------------------------------------------------------------------------------------
// 弹窗
// -------------------------------------------------------------------------------------------------

const QUALITY_LABELS: [(&str, &str); 6] = [
    ("source", "Same as Source Bitrate"),
    ("low", "Low"),
    ("medium", "Medium"),
    ("high", "High"),
    ("veryHigh", "Very High"),
    ("custom", "Custom Bitrate"),
];

/// 闭包外算好的只读上下文,配置视图要显示的每一样。
struct Ctx {
    width: u32,
    height: u32,
    fps: f64,
    duration: f64,
    seq_name: String,
    ext: String,
    whole_file: String,
    /// 空目录时显示的默认导出目录。
    default_dir: String,
    parts: Vec<Part>,
    split_error: Option<SplitError>,
    /// 时间线源视频的视频轨码率 kbps(「与源视频相同码率」旁显)。
    source_kbps: Option<u32>,
    can_go: bool,
}

/// 每帧画分P导出弹窗(开着才画)。入口在 Quick Export 的列表里。
pub fn show(app: &mut FilmcraftApp, ctx: &egui::Context) {
    // 点「开始导出」时弹窗就关了;导出在飞期间这里继续收尾:队列全部
    // 到终态、片头也拷完后,落封面与投稿模板(进度与重试在导出模式的
    // 导出队列里)。
    if app.ui.export.parts.in_flight {
        settle_in_flight(app, ctx);
    }
    if !app.ui.export.parts.open {
        return;
    }
    let Some(seq) = app.session.state.active_sequence else {
        app.ui.export.parts.open = false;
        app.ui.status = tl!("Parts Export: open a sequence first").into();
        return;
    };
    sync_publish_sidecar(app);
    sync_intro(app);
    sync_source_bitrate(app, seq);
    let t = app.tokens;
    let mut reg = Reg::default();
    let mut window_open = true;
    // 模态阻挡层:弹窗开着时吃掉其余界面的点击与拖动——分P弹窗之外什
    // 么都不该能操作,关掉弹窗才放行。**弹窗矩形本身留空**(上一帧的
    // 窗口位置四周四条边带):整块大遮罩会被「点击弹窗外」顶到弹窗层
    // 之上,之后弹窗就点不到了(实测踩过);边带各自独立成层,永远不
    // 盖住弹窗内容,焦点在谁身上都好使。画在弹窗之前,弹窗仍在其上。
    let screen = ctx.content_rect();
    if let Some(rect) = app.ui.export.parts.window_rect {
        let strips = [
            egui::Rect::from_min_max(screen.min, egui::pos2(screen.max.x, rect.top())),
            egui::Rect::from_min_max(egui::pos2(screen.min.x, rect.bottom()), screen.max),
            egui::Rect::from_min_max(egui::pos2(screen.min.x, rect.top()), egui::pos2(rect.left(), rect.bottom())),
            egui::Rect::from_min_max(egui::pos2(rect.right(), rect.top()), egui::pos2(screen.max.x, rect.bottom())),
        ];
        for (index, strip) in strips.into_iter().enumerate() {
            if strip.width() <= 0.0 || strip.height() <= 0.0 {
                continue;
            }
            egui::Area::new(egui::Id::new(("parts-export-modal-block", index)))
                .order(egui::Order::Middle)
                .fixed_pos(strip.min)
                .default_size(strip.size())
                .constrain_to(strip)
                .interactable(true)
                .show(ctx, |ui| {
                    ui.allocate_rect(ui.max_rect(), egui::Sense::click_and_drag());
                });
        }
    }
    // 按钮信号:闭包里只借弹窗状态,动 app 的都在闭包外。
    let (mut pick_cover, mut pick_folder, mut edit_cover, mut go) = (false, false, false, false);

    let Some(q) = app.session.active_sequence().cloned() else { return }; // checked above
    let rate = q.settings.frame_rate;
    let fps = rate.num as f64 / rate.den as f64;
    let duration = q.duration().max(rate.frame_duration()).seconds();
    let p = &mut app.ui.export.parts;
    // 「与源视频相同码率」成为缺省的一次性迁移(见字段注释)。
    if !p.source_quality_migrated {
        p.source_quality_migrated = true;
        if p.quality == "high" {
            p.quality = "source".into();
        }
    }
    let (source_kbps, per_part_sec, count) = (p.source_kbps, p.per_part_minutes * 60.0, p.part_count.floor().max(1.0) as u32);
    let settings = settings_for(p, &app.ui.export.settings, source_kbps);
    let ctx_info = Ctx {
        width: q.settings.width,
        height: q.settings.height,
        fps,
        duration,
        seq_name: app.session.project.item(seq).map(|i| sequence_base_name(&i.name)).unwrap_or_else(|| "Sequence".into()),
        ext: settings.extension().to_string(),
        whole_file: String::new(),
        default_dir: default_output_dir(&app.session).to_string_lossy().to_string(),
        parts: Vec::new(),
        split_error: None,
        source_kbps,
        can_go: false,
    };
    let ctx_info = Ctx {
        whole_file: format!("{}.{}", ctx_info.seq_name, ctx_info.ext),
        parts: if p.parts_enabled {
            compute_parts(duration, fps, &p.split_mode, per_part_sec, count)
        } else {
            Vec::new()
        },
        split_error: if p.parts_enabled { validate_split(duration, &p.split_mode, per_part_sec, count) } else { None },
        can_go: (p.whole_enabled || (p.parts_enabled && parts_valid(p, duration))) && duration > 0.0,
        ..ctx_info
    };

    // 高度跟着屏幕走:初始不超过视口的八成(至多 600),拖动也不越过九
    // 成——内容再多交给滚动条,而不是把窗口顶出屏幕。
    let viewport_h = ctx.content_rect().height();
    let window = egui::Window::new(tl!("Parts Export"))
        .id(egui::Id::unique("parts-export"))
        .open(&mut window_open)
        .collapsible(false)
        .resizable(true)
        .default_size(vec2(700.0, (viewport_h * 0.78).clamp(430.0, 600.0)))
        .min_size(vec2(560.0, 400.0))
        .max_height((viewport_h * 0.92).max(430.0))
        .pivot(Align2::CENTER_CENTER)
        .default_pos(crate::widgets::dialog_center(ctx))
        .show(ctx, |ui| {
            let p = &mut app.ui.export.parts;
            configure_view(ui, &t, &mut reg, p, &ctx_info, &mut pick_cover, &mut pick_folder, &mut edit_cover, &mut go);
        });
    // 记下弹窗矩形给下一帧的边带阻挡层(见上),并把弹窗层重新顶到
    // 最上:边带被点会把自己顶上来(egui 的 Area 行为,API 无开关),
    // 那之后弹窗内就点不到了(实测踩过)——每帧压回一次才是模态的
    // 完整语义:弹窗永远在最上、永远可点。
    if let Some(window) = window.as_ref() {
        app.ui.export.parts.window_rect = Some(window.response.rect);
        ctx.move_to_top(window.response.layer_id);
    }

    // 封面缩略图:路径变了一次性解码(用户点选的那一帧),之后只画纹理。
    if let Some(path) = app.ui.export.parts.cover_path.clone() {
        load_cover_thumb(ctx, &path, &mut app.ui.export.parts);
    }
    if go {
        if let Err(e) = start_export(app) {
            app.ui.status = e;
        }
    }
    if pick_cover {
        app.pick_ui(FileDialog::open_file(tl!("Cover"), &["png", "jpg", "jpeg", "webp"]), |app, paths| {
            if let Some(f) = paths.into_iter().next() {
                app.ui.export.parts.cover_path = Some(f);
                app.ui.export.parts.set_cover_dirty();
            }
        });
    }
    if pick_folder {
        let at = app.ui.export.parts.location.clone();
        app.pick_ui(FileDialog::Folder { at: Some(at) }, |app, paths| {
            if let Some(d) = paths.into_iter().next() {
                app.ui.export.parts.location = d;
            }
        });
    }
    if edit_cover {
        app.ui.export.parts.open = false;
        // 这个视频出自哪个录制场景,封面编辑器就按哪个场景取自动标题的
        // 名字、列哪个场景的图片对;认不出(手建的普通项目)就是 None。
        let recording_scene = app
            .session
            .path
            .as_deref()
            .and_then(crate::panels::export_mode::recording_scene);
        app.cover_editor = Some(crate::panels::cover_editor::open(app, ctx, recording_scene));
    }
    reg.flush(app);
    if !window_open || crate::widgets::escape_closes(ctx) {
        app.ui.export.parts.open = false;
    }
}

/// 片头旁车的单向加载(项目变化才读一次盘):reaction 录制收尾把场景
/// 配置的片头写进项目目录的 `<场景名>.intro.json`,这里找回来作为分P
/// 导出的第一个分P。旁车缺失/损坏/片头文件已不在都当没有片头——导出
/// 其余部分不受影响。
fn sync_intro(app: &mut FilmcraftApp) {
    let project_path = app.session.path.clone();
    if app.ui.export.parts.intro_loaded_for.as_deref() == project_path.as_deref() {
        return;
    }
    app.ui.export.parts.intro = project_path.as_deref().and_then(find_intro_path).map(|source| {
        let file = std::path::Path::new(&source)
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| source.clone());
        IntroPart {
            source,
            file,
            slot: std::sync::Arc::default(),
        }
    });
    app.ui.export.parts.intro_loaded_for = project_path;
}

/// 项目目录里的片头旁车(场景目录一份,按后缀找;找不到/读不了/没配
/// 都返回 `None`)。
fn find_intro_path(project_path: &str) -> Option<String> {
    let dir = std::path::Path::new(project_path).parent()?;
    let entry = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .find(|entry| entry.file_name().to_string_lossy().ends_with(".intro.json"))?;
    let bytes = std::fs::read(entry.path()).ok()?;
    let path = serde_json::from_slice::<Value>(&bytes)
        .ok()?
        .get("introVideoPath")?
        .as_str()?
        .trim()
        .to_string();
    (std::path::Path::new(&path).is_file()).then_some(path)
}

/// 投稿模板旁车与弹窗的双向同步(弹窗打开的每一帧,FS 只在状态变化时
/// 碰一次):项目换了就把编辑标记与字段交给新项目、旁车有内容就整体
/// 灌回——重开项目后打开弹窗就是上次保存的模板;模板三字段与旁车不同
/// 就整份回写(用户编辑与迟到的 AI 预填都算变化)。
fn sync_publish_sidecar(app: &mut FilmcraftApp) {
    let project_path = app.session.path.clone();
    let loaded_for = app.ui.export.parts.publish_sidecar.as_ref().map(|(path, _)| path.clone());
    if loaded_for.as_deref() != project_path.as_deref() {
        // 新项目上下文:上一场的编辑标记复位;真的换过项目时字段也不
        // 残留(本会话首次加载不清——拾取可能刚把迟到的元数据填进来,
        // 旁车又缺着,回写还能把它们补到盘上)。旁车有内容就整体灌回。
        app.ui.export.parts.reset_bili_edits();
        if loaded_for.is_some() {
            app.ui.export.parts.bili_title = String::new();
            app.ui.export.parts.bili_desc = String::new();
            app.ui.export.parts.bili_tags = Vec::new();
        }
        let sidecar = project_path.as_deref().and_then(load_publish_sidecar);
        if let Some(meta) = &sidecar {
            app.ui.export.parts.apply_publish(meta);
        }
        app.ui.export.parts.publish_sidecar = project_path.map(|path| (path, sidecar));
        return;
    }
    // 回写:旁车是磁盘上的真相,重开项目全靠它。全空且本来没有旁车时
    // 不建文件;写失败只记日志,下一次变化还会再试。
    let desired = reaction::ai_commentary::RecordingMetadata {
        title: app.ui.export.parts.bili_title.clone(),
        description: app.ui.export.parts.bili_desc.clone(),
        tags: app.ui.export.parts.bili_tags.clone(),
    };
    let unchanged = app
        .ui
        .export
        .parts
        .publish_sidecar
        .as_ref()
        .is_some_and(|(_, cache)| cache.as_ref() == Some(&desired));
    if unchanged {
        return;
    }
    let untouched = desired.title.is_empty() && desired.description.is_empty() && desired.tags.is_empty();
    if untouched
        && app.ui.export.parts.publish_sidecar.as_ref().is_none_or(|(_, cache)| cache.is_none())
    {
        return;
    }
    if let Some(path) = project_path.as_deref() {
        write_publish_sidecar(path, &desired);
        if let Some((_, cache)) = &mut app.ui.export.parts.publish_sidecar {
            *cache = Some(desired);
        }
    }
}

/// 项目文件旁的投稿模板旁车(reaction 录后 AI 落的 `<场景名>.publish.json`;
/// 项目文件名带时间戳,按目录找最近修改的一份)。
fn find_publish_sidecar(project_path: &str) -> Option<PathBuf> {
    let dir = std::path::Path::new(project_path).parent()?;
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        if !entry.file_name().to_string_lossy().ends_with(".publish.json") {
            continue;
        }
        let modified =
            entry.metadata().and_then(|m| m.modified()).unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        if best.as_ref().is_none_or(|(newest, _)| modified > *newest) {
            best = Some((modified, entry.path()));
        }
    }
    best.map(|(_, path)| path)
}

/// 读旁车(缺文件/损坏都返回 None——旁车只是缓存,不挡弹窗)。
fn load_publish_sidecar(project_path: &str) -> Option<reaction::ai_commentary::RecordingMetadata> {
    let bytes = std::fs::read(find_publish_sidecar(project_path)?).ok()?;
    match serde_json::from_slice(&bytes) {
        Ok(meta) => Some(meta),
        Err(e) => {
            log::warn!("the upload template sidecar is damaged, ignoring it: {e}");
            None
        }
    }
}

/// 模板写回旁车:目录里已有的那份(场景级,reaction 写的)优先,一份
/// 都没有就落到项目文件旁的 `<项目名>.publish.json`。
fn write_publish_sidecar(project_path: &str, meta: &reaction::ai_commentary::RecordingMetadata) {
    let target = find_publish_sidecar(project_path).or_else(|| {
        let path = std::path::Path::new(project_path);
        let stem = path.file_stem()?.to_string_lossy().to_string();
        Some(path.parent()?.join(format!("{stem}.publish.json")))
    });
    let Some(target) = target else { return };
    match serde_json::to_vec_pretty(meta)
        .and_then(|bytes| std::fs::write(&target, bytes).map_err(serde_json::Error::io))
    {
        Ok(()) => {}
        Err(e) => log::warn!("could not write the upload template sidecar: {e}"),
    }
}

fn parts_valid(p: &PartsExportUi, duration: f64) -> bool {
    let per_part_sec = p.per_part_minutes * 60.0;
    let count = p.part_count.floor().max(1.0) as u32;
    validate_split(duration, &p.split_mode, per_part_sec, count).is_none()
}

// -------------------------------------------------------------------------------------------------
// 配置视图
// -------------------------------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn configure_view(
    ui: &mut egui::Ui,
    t: &Tokens,
    reg: &mut Reg,
    p: &mut PartsExportUi,
    c: &Ctx,
    pick_cover: &mut bool,
    pick_folder: &mut bool,
    edit_cover: &mut bool,
    go: &mut bool,
) {
    // 滚动区的高度=弹窗内容区减去底栏,底栏因此永远钉在窗口底部。
    // (在自动测量高度的 `egui::Window` 里嵌套 Panel 不产生约束——内容
    // 会越过窗口底边;filmcraft 自己的弹窗也是"固定高的滚动区+按钮在
    // 后"这一个写法,见 `panels::settings`。)
    let body_h = (ui.available_height() - FOOTER_HEIGHT).max(140.0);
    egui::ScrollArea::vertical()
        .id_salt("parts-config-scroll")
        .max_height(body_h)
        .min_scrolled_height(body_h)
        .auto_shrink([false, false])
        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysVisible)
        .show(ui, |ui| {
            let fps_text = if (c.fps - c.fps.round()).abs() < 0.01 { format!("{}", c.fps.round() as u64) } else { format!("{:.2}", c.fps) };
            ui.label(
                egui::RichText::new(tlf!("{w}×{h} · {fps} fps · {dur}", w = c.width, h = c.height, fps = fps_text, dur = clock(c.duration)))
                    .color(t.text_dim)
                    .size(11.5),
            );
            ui.add_space(6.0);

            // —— 格式 / 画质 / 音频:导出模式的同款行 ——
            row(ui, t, tl!("Format"), |ui| {
                let selected = if p.format == "hevc" { "MP4 · H.265" } else { "MP4 · H.264" };
                if let Some(i) = combo(ui, reg, "partsExport.format", selected, &opts(&["MP4 · H.264", "MP4 · H.265"]), 160.0) {
                    p.format = if i == 1 { "hevc".into() } else { "h264".into() };
                }
            });
            row(ui, t, tl!("Quality"), |ui| {
                let selected = QUALITY_LABELS.iter().find(|(k, _)| *k == p.quality).map_or("High", |(_, l)| *l);
                let labels: Vec<(String, bool)> = QUALITY_LABELS.iter().map(|(_, l)| (l.to_string(), true)).collect();
                if let Some(i) = combo(ui, reg, "partsExport.quality", selected, &labels, 160.0) {
                    p.quality = QUALITY_LABELS[i].0.into();
                }
                // 与源相同码率:把解析出的目标亮出来(拖值/自定义才改)。
                if p.quality == "source"
                    && let Some(kbps) = c.source_kbps
                {
                    let mbps = kbps as f64 / 1000.0;
                    let shown = if mbps >= 10.0 { format!("{:.0}", mbps.round()) } else { format!("{:.1}", (mbps * 10.0).round() / 10.0) };
                    ui.label(egui::RichText::new(tlf!("≈ {n} Mbps", n = shown)).color(t.text_faint).size(11.0));
                }
            });
            if p.quality == "custom" {
                row(ui, t, tl!("Custom Bitrate"), |ui| {
                    let mut mbps = p.custom_mbps;
                    if drag(ui, reg, "partsExport.bitrate", &mut mbps, DragOptions { range: 0.1..=800.0, speed: 0.1, suffix: " Mbps", decimals: 1 }) {
                        p.custom_mbps = mbps;
                    }
                });
            }
            row(ui, t, "", |ui| {
                check(ui, reg, "partsExport.audio", &mut p.include_audio, tl!("Include Audio"));
            });
            ui.add_space(8.0);

            // —— 导出内容 ——
            ui.label(egui::RichText::new(tl!("Export Content")).strong().size(14.0));
            row(ui, t, "", |ui| {
                check(ui, reg, "partsExport.whole", &mut p.whole_enabled, tl!("Export Whole Video"));
                ui.label(egui::RichText::new(c.whole_file.as_str()).color(t.text_dim).size(11.5));
            });
            row(ui, t, "", |ui| {
                check(ui, reg, "partsExport.parts", &mut p.parts_enabled, tl!("Split into Parts"));
                if p.parts_enabled && c.split_error.is_none() {
                    ui.label(egui::RichText::new(tlf!("{n} parts", n = c.parts.len())).color(t.text_dim).size(11.5));
                }
            });
            if p.parts_enabled {
                row(ui, t, tl!("File Name Prefix"), |ui| {
                    text(ui, reg, "partsExport.prefix", &mut p.prefix, &c.seq_name);
                });
                row(ui, t, tl!("Separator"), |ui| {
                    let r = ui.add(egui::TextEdit::singleline(&mut p.separator).desired_width(60.0));
                    reg.add("partsExport.separator", r.rect, "Separator");
                });
                // 连字符至多 6 个字符。
                p.separator = p.separator.chars().take(6).collect();
                // 拆分方式(opencut 的 PillGroup):两个选项始终并排可见,
                // 选中一个 accent 填充;下面再按模式出一行各自的数值。
                row(ui, t, tl!("Split by"), |ui| {
                    if let Some(k) = segmented(
                        ui,
                        t,
                        reg,
                        "partsExport.splitMode",
                        &p.split_mode,
                        &[("duration", "Per-Part Duration"), ("count", "Part Count")],
                    ) {
                        p.split_mode = k;
                    }
                });
                if p.split_mode == "duration" {
                    row(ui, t, tl!("Per-Part Duration"), |ui| {
                        let mut minutes = p.per_part_minutes;
                        if drag(ui, reg, "partsExport.perPart", &mut minutes, DragOptions { range: 0.1..=600.0, speed: 0.1, suffix: " min", decimals: 1 }) {
                            p.per_part_minutes = minutes;
                        }
                        // opencut 在旁显换算秒数。
                        ui.label(egui::RichText::new(tlf!("= {s} s", s = (p.per_part_minutes * 60.0).round())).color(t.text_faint).size(11.0));
                    });
                } else {
                    row(ui, t, tl!("Part Count"), |ui| {
                        let mut n = p.part_count;
                        if drag(ui, reg, "partsExport.partCount", &mut n, DragOptions { range: 2.0..=999.0, speed: 1.0, suffix: "", decimals: 0 }) {
                            p.part_count = n;
                        }
                        if c.split_error.is_none() {
                            ui.label(egui::RichText::new(tlf!("≈ {s} per part", s = clock(c.duration / p.part_count.max(1.0)))).color(t.text_faint).size(11.0));
                        }
                    });
                }
                if let Some(err) = c.split_error {
                    row(ui, t, "", |ui| {
                        ui.label(egui::RichText::new(split_error_text(err)).color(t.danger).size(12.0));
                    });
                } else {
                    // 实时预览:文件名 + 起止 + 时长。场景配了片头时列表
                    // 首行是它(自己的文件名,不编号),正片从 P1 开始。
                    row(ui, t, "", |ui| {
                        ui.vertical(|ui| {
                            if let Some(intro) = &p.intro {
                                ui.horizontal(|ui| {
                                    ui.label(egui::RichText::new(&intro.file).size(11.5));
                                    ui.label(egui::RichText::new(tl!("Intro")).size(11.5).color(t.text_dim));
                                });
                            }
                            for part in c.parts.iter().take(40) {
                                ui.label(
                                    egui::RichText::new(format!(
                                        "{}   {} – {}   · {}",
                                        part_file_name(&p.prefix, &p.separator, part.index, &c.ext),
                                        clock(part.start),
                                        clock(part.end),
                                        clock(part.end - part.start),
                                    ))
                                    .size(11.5),
                                );
                            }
                            if c.parts.len() > 40 {
                                ui.label(egui::RichText::new(tlf!("…and {n} more", n = c.parts.len() - 40)).color(t.text_faint).size(11.5));
                            }
                            if merged_last_note(&c.parts, p) {
                                ui.label(
                                    egui::RichText::new(tlf!(
                                        "The last part is under {s} s and was merged into the one before it",
                                        s = MIN_LAST_PART_SECONDS
                                    ))
                                    .color(t.text_dim)
                                    .size(11.5),
                                );
                            }
                        });
                    });
                }
            }
            ui.add_space(8.0);

            // —— 封面 ——
            ui.label(egui::RichText::new(tl!("Cover")).strong().size(14.0));
            row(ui, t, "", |ui| {
                let r = ui.small_button(tl!("Select Cover…"));
                reg.add("partsExport.cover.pick", r.rect, "Select Cover…");
                *pick_cover = r.clicked();
                let r = ui.small_button(tl!("Edit Cover…"));
                reg.add("partsExport.cover.edit", r.rect, "Edit Cover…");
                *edit_cover = r.clicked();
                let r = ui.small_button(tl!("Clear"));
                reg.add("partsExport.cover.clear", r.rect, "Clear Cover");
                if r.clicked() {
                    p.cover_path = None;
                    p.set_cover_dirty();
                }
            });
            row(ui, t, "", |ui| {
                let (rect, _) = ui.allocate_exact_size(vec2(160.0, 90.0), Sense::hover());
                ui.painter().rect_filled(rect, 4.0, t.field_bg);
                let shown = p.cover_tex.as_ref().filter(|x| x.path == p.cover_path.as_deref().unwrap_or(""));
                if let Some(thumb) = shown {
                    ui.painter().image(thumb.tex.id(), rect, egui::Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
                } else if p.cover_path.is_some() {
                    ui.painter().text(rect.center(), Align2::CENTER_CENTER, "…", Tokens::ui(12.0), t.text_dim);
                } else {
                    ui.painter().text(rect.center(), Align2::CENTER_CENTER, tl!("No cover set"), Tokens::ui(11.0), t.text_faint);
                }
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(tl!("The cover is copied next to the videos as a PNG; Edit Cover arranges it on the Reaction canvas."))
                            .color(t.text_dim)
                            .size(11.5),
                    )
                    .wrap_mode(egui::TextWrapMode::Wrap),
                );
            });
            ui.add_space(8.0);

            // —— 投稿模板 ——
            ui.label(egui::RichText::new(tl!("Upload Template (biliup)")).strong().size(14.0));
            row(ui, t, tl!("Title"), |ui| {
                let r = ui.add(
                    egui::TextEdit::singleline(&mut p.bili_title)
                        .hint_text(c.seq_name.as_str())
                        .desired_width(ui.available_width())
                        .char_limit(MAX_TITLE_CHARS),
                );
                p.bili_edited[0] |= r.changed();
                reg.add("partsExport.title", r.rect, "Title");
            });
            row(ui, t, tl!("Tags"), |ui| {
                tag_input(ui, t, reg, p);
            });
            row(ui, t, tl!("Description"), |ui| {
                let r = ui.add(egui::TextEdit::multiline(&mut p.bili_desc).desired_rows(2).desired_width(ui.available_width()));
                p.bili_edited[1] |= r.changed();
                reg.add("partsExport.desc", r.rect, "Description");
            });
            ui.add_space(8.0);

            // —— 输出文件夹 ——
            row(ui, t, tl!("Output Folder"), |ui| {
                let r = ui.small_button("…");
                reg.add("partsExport.location.browse", r.rect, "Choose folder");
                *pick_folder |= r.clicked();
                let shown = if p.location.trim().is_empty() { c.default_dir.clone() } else { p.location.clone() };
                let r = crate::panels::export_mode::location_link(ui, t, &shown);
                reg.add("partsExport.location", r.rect, "Output Folder");
                *pick_folder |= r.clicked();
                });
        });

    // 底栏:汇总 + 取消/开始(滚动区之外)。
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        let videos = p.whole_enabled as usize
            + if p.parts_enabled && c.split_error.is_none() {
                c.parts.len() + usize::from(p.intro.is_some())
            } else {
                0
            };
        let summary = if videos == 0 {
            tl!("Choose what to export").to_string()
        } else if c.duration <= 0.0 {
            tl!("Timeline is empty").to_string()
        } else {
            tlf!("{n} files: {videos} videos + 1 upload template + 1 cover", n = videos + 2, videos = videos)
        };
        ui.label(egui::RichText::new(summary).color(t.text_dim).size(11.5));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let r = ui.add_enabled(
                c.can_go,
                egui::Button::new(egui::RichText::new(tl!("Start Export")).color(Color32::WHITE)).fill(t.accent).corner_radius(12.0).min_size(vec2(110.0, 28.0)),
            );
            reg.add("partsExport.go", r.rect, "Start Export");
            *go = r.clicked();
            let r = ui.button(tl!("Cancel"));
            reg.add("partsExport.cancel", r.rect, "Cancel");
            if r.clicked() {
                p.open = false;
            }
        });
    });
}

fn split_error_text(err: SplitError) -> String {
    match err {
        SplitError::TooShort => tlf!("Each part must be at least {s} s", s = MIN_PART_SECONDS),
        SplitError::LongerThanVideo => tl!("Per-part duration must be shorter than the video; use Export Whole Video").to_string(),
        SplitError::MinTwo => tl!("Split into at least 2 parts").to_string(),
        SplitError::TooMany(max) => tlf!("Too many parts: at most {n} for this video", n = max),
    }
}

/// 末P并入上一P真的发生了才有提示:并入后末P = 前一段的余量 + 一整段,
/// 必然明显长于每P目标。
fn merged_last_note(parts: &[Part], p: &PartsExportUi) -> bool {
    if parts.len() < 2 || p.split_mode != "duration" {
        return false;
    }
    let target = p.per_part_minutes * 60.0;
    parts.last().is_some_and(|last| last.end - last.start > target * 1.5)
}

// -------------------------------------------------------------------------------------------------
// 启动 / 收尾
// -------------------------------------------------------------------------------------------------

/// 直拷片头一个文件;失败文案按 `template`(`{e}` 占位)展开。
fn copy_intro(source: &str, target: &std::path::Path, template: &str) -> Result<(), String> {
    std::fs::copy(source, target)
        .map(|_| ())
        .map_err(|e| crate::i18n::fmt(template, &[("e", &e.to_string())]))
}

/// 后台线程把片头拷到输出目录(不进导出队列),结果落完成槽。失败文案
/// 的模板在 UI 线程取好——i18n 的当前语言是每线程的,后台线程直取只
/// 会拿到缺省语言。(start_export 起线程前把槽清回 `None`,收尾与重导
/// 都以本批的复制结果为准。)
fn spawn_intro_copy(intro: &IntroPart, dir: &std::path::Path) {
    let template = tl!("Intro video copy failed: {e}").to_string();
    let thread_template = template.clone();
    let thread_source = intro.source.clone();
    let thread_target = dir.join(&intro.file);
    let slot = std::sync::Arc::clone(&intro.slot);
    let run = move || {
        let outcome = copy_intro(&thread_source, &thread_target, &thread_template);
        if let Ok(mut slot) = slot.lock() {
            *slot = Some(outcome);
        }
    };
    // 线程起不来(极端)就就地复制:宁可挡一下 UI,也不把导出卡在"在途"。
    if std::thread::Builder::new()
        .name("parts-intro-copy".into())
        .spawn(run)
        .is_err()
    {
        let outcome = copy_intro(&intro.source, &dir.join(&intro.file), &template);
        if let Ok(mut slot) = intro.slot.lock() {
            *slot = Some(outcome);
        }
    }
}

/// 一次导出的完整计划:要排进导出队列的渲染输出 + 「整段 + 分P」同选时
/// 对整段产物的流复制切分清单。纯函数(落盘之外的决策全在这,可直接测)。
///
/// - 只有整段:队列一个输出,无切分。
/// - 只有分P:每个分P一个渲染输出(各带 custom range),无切分。
/// - 两者同选:**整段只渲染一次**(字幕、效果、画质全在里面),队列里
///   只有整段;分P改为从整段产物里流复制切出(`export.split`),省掉
///   N 次重渲染。切分范围与文件名和逐P渲染完全一致。
#[allow(clippy::too_many_arguments)] // 一条纯决策管道,参数都是标量,打包反而藏形状(同 configure_view)
pub fn plan_outputs(
    whole: bool,
    parts: &[Part],
    seq_name: &str,
    ext: &str,
    dir: &std::path::Path,
    prefix: &str,
    separator: &str,
    settings_json: &Value,
) -> (Vec<(String, Value)>, Vec<(String, String)>) {
    let mut queue: Vec<(String, Value)> = Vec::new();
    let mut split: Vec<(String, String)> = Vec::new();
    let whole_file = format!("{seq_name}.{ext}");
    if whole {
        queue.push((
            whole_file.clone(),
            json!({"path": dir.join(&whole_file).to_string_lossy(), "settings": settings_json, "range": "entire"}),
        ));
    }
    if !whole && !parts.is_empty() {
        for part in parts {
            let file = part_file_name(prefix, separator, part.index, ext);
            queue.push((
                file.clone(),
                json!({
                    "path": dir.join(&file).to_string_lossy(),
                    "settings": settings_json,
                    "range": {"mode": "custom", "startSeconds": part.start, "endSeconds": part.end},
                }),
            ));
        }
    }
    if whole && !parts.is_empty() {
        for part in parts {
            let file = part_file_name(prefix, separator, part.index, ext);
            split.push((file.clone(), dir.join(file).to_string_lossy().to_string()));
        }
    }
    (queue, split)
}

/// 把每个输出排进导出队列并启动。返回 Err 文案(状态栏)。
fn start_export(app: &mut FilmcraftApp) -> Result<(), String> {
    let (seq, rate, duration) = {
        let seq = app.session.state.active_sequence.ok_or_else(|| tl!("Parts Export: open a sequence first").to_string())?;
        let q = app.session.active_sequence().cloned().ok_or("no sequence")?;
        (seq, q.settings.frame_rate, q.duration().max(q.settings.frame_rate.frame_duration()).seconds())
    };
    let fps = rate.num as f64 / rate.den as f64;
    let p = &mut app.ui.export.parts;
    let per_part_sec = p.per_part_minutes * 60.0;
    let count = p.part_count.floor().max(1.0) as u32;
    if let Some(err) = p.parts_enabled.then(|| validate_split(duration, &p.split_mode, per_part_sec, count)).flatten() {
        return Err(split_error_text(err));
    }
    let parts = if p.parts_enabled { compute_parts(duration, fps, &p.split_mode, per_part_sec, count) } else { Vec::new() };
    let dir = {
        let d = p.location.trim();
        if d.is_empty() {
            default_output_dir(&app.session)
        } else {
            PathBuf::from(filmcraft_engine::export_tools::expand_home(d))
        }
    };
    // 设置跟随导出界面(效果/硬件编码等)+ 弹窗自己的选择;`p` 与
    // `settings` 是相邻字段的互斥借用,可以直接同用。
    let settings = settings_for(p, &app.ui.export.settings, p.source_kbps);
    let ext = settings.extension().to_string();
    let settings_json = serde_json::to_value(&settings).unwrap_or_default();
    let seq_name = app.session.project.item(seq).map(|i| sequence_base_name(&i.name)).unwrap_or_else(|| "Sequence".into());
    let whole = p.whole_enabled;

    let (outputs, split) = plan_outputs(whole, &parts, &seq_name, &ext, &dir, &p.prefix, &p.separator, &settings_json);
    if outputs.is_empty() && split.is_empty() {
        return Err(tl!("Choose what to export").to_string());
    }

    p.items.clear();
    p.planned_parts.clear();
    p.split_job = None;
    p.split_error = None;
    for (file, params) in outputs {
        match app.session.execute("export.queue.add", params) {
            Ok(v) => {
                let id = v["added"].as_array().and_then(|a| a.first()).and_then(Value::as_u64).unwrap_or(0);
                p.items.push(PartsItem { id, file, path: String::new() });
            }
            Err(e) => {
                p.items.clear();
                return Err(e.to_string());
            }
        }
    }
    if !split.is_empty() {
        p.planned_parts = split
            .into_iter()
            .zip(parts.iter())
            .map(|((file, path), part)| PlannedPart { part: *part, item: PartsItem { id: 0, file, path } })
            .collect();
    }
    if let Err(e) = app.session.execute("export.queue.start", json!({})) {
        return Err(e.to_string());
    }
    // 分P导出且场景配了片头:以片头自己的文件名直拷为第一个分P(不参与
    // 前缀_PN 编号,正片从 P1 开始;opencut 同款)。复制走后台线程,完成
    // 槽由 `settle_in_flight` 轮询——再大的片头也不冻 UI。上一批留下的
    // 结果先清掉,免得收尾把旧状态当成本批的。
    if p.parts_enabled && let Some(intro) = p.intro.clone() {
        if let Ok(mut slot) = intro.slot.lock() {
            *slot = None;
        }
        // 新的默认目录(`<场景名>_output`)此刻多半还不存在:导出管线
        // 会自建,但片头直拷线程现在就启动,不等它——先建好。
        let _ = std::fs::create_dir_all(&dir);
        spawn_intro_copy(&intro, &dir);
    }
    // 路径与基名:整段优先,否则 P1。
    for it in &mut p.items {
        it.path = dir.join(&it.file).to_string_lossy().to_string();
    }
    for it in &mut p.planned_parts {
        it.item.path = dir.join(&it.item.file).to_string_lossy().to_string();
    }
    p.base_name = if p.whole_enabled {
        seq_name
    } else {
        p.items.first().map(|i| i.file.rsplit_once('.').map(|(stem, _)| stem.to_string()).unwrap_or_else(|| i.file.clone())).unwrap_or(seq_name)
    };
    // 开始导出即关闭弹窗(进度与重试在导出模式的导出队列里):在飞标记
    // 交给 `settle_in_flight`,全部终态后落封面与投稿模板。失败时弹窗
    // 留在配置视图,错误经状态栏显示。
    p.in_flight = true;
    p.open = false;
    Ok(())
}

/// 导出在飞(弹窗已在开始导出时关闭)的每帧收尾:查一次队列,全部到
/// 终态、(合并路)整段产物也切分完、片头复制(如有)也落定后,复制封
/// 面、写投稿模板,把结果放进状态栏;没落定就 150ms 后再看。进度与重
/// 试的完整界面在导出模式的导出队列里。
fn settle_in_flight(app: &mut FilmcraftApp, ctx: &egui::Context) {
    let list: Vec<Value> = app
        .session
        .execute("export.queue.list", json!({}))
        .ok()
        .and_then(|v| v["items"].as_array().cloned())
        .unwrap_or_default();
    let status = |id: u64| {
        list.iter()
            .find(|v| v["id"].as_u64() == Some(id))
            .and_then(|v| v["status"].as_str())
    };
    // 合并路:整段渲染完成才切分(切分作业只起一次),切分完才算收尾完。
    if app.ui.export.parts.split_job.is_none()
        && app.ui.export.parts.split_error.is_none()
        && !app.ui.export.parts.planned_parts.is_empty()
        && app.ui.export.parts.items.iter().all(|it| matches!(status(it.id), Some("done" | "failed" | "cancelled")))
    {
        let whole_done = app.ui.export.parts.items.iter().any(|it| status(it.id) == Some("done"));
        if whole_done {
            spawn_split(app);
        } else {
            // 整段失败/取消:没有可切分的产物,清掉计划,按队列结果收尾。
            app.ui.export.parts.planned_parts.clear();
        }
    }
    let split_state = app.ui.export.parts.split_error.clone().map(Err).or_else(|| split_job_state(app));
    let settled = app.ui.export.parts.items.iter().all(|it| matches!(status(it.id), Some("done" | "failed" | "cancelled")))
        && app.ui.export.parts.planned_parts.iter().all(|_| split_state.is_some())
        && app
            .ui
            .export
            .parts
            .intro
            .as_ref()
            .is_none_or(|intro| intro_copy_state(intro).is_some());
    if !settled {
        ctx.request_repaint_after(std::time::Duration::from_millis(150));
        return;
    }
    // 有一个成功就写封面与模板;全失败则不写。
    let p = &app.ui.export.parts;
    let split_ok = split_state.as_ref().is_some_and(|r| r.is_ok());
    let split_failed = split_state.as_ref().is_some_and(|r| r.is_err());
    let done = p.items.iter().filter(|it| status(it.id) == Some("done")).count()
        + usize::from(split_ok) * p.planned_parts.len()
        + usize::from(p.intro.as_ref().is_some_and(|intro| matches!(intro_copy_state(intro), Some(Ok(())))));
    let failed = p.items.iter().any(|it| status(it.id) == Some("failed"))
        || split_failed
        || p.intro.as_ref().is_some_and(|intro| matches!(intro_copy_state(intro), Some(Err(_))));
    let cancelled = p.items.iter().any(|it| status(it.id) == Some("cancelled"));
    // 片头不是队列项,导出队列面板里看不到它的错误,失败文案带在这里。
    let intro_error = p
        .intro
        .as_ref()
        .and_then(intro_copy_state)
        .and_then(|outcome| outcome.err());
    app.ui.export.parts.in_flight = false;
    // 先报结果(写盘失败会覆盖它——错误比汇总更要紧)。
    app.ui.status = if failed {
        match split_state.and_then(|r| r.err()).or(intro_error) {
            Some(error) => format!("{}: {error}", tl!("Export finished with failures")),
            None => tl!("Export finished with failures").to_string(),
        }
    } else if cancelled {
        tl!("Export cancelled").to_string()
    } else {
        tlf!("{n} videos + 1 upload template + 1 cover", n = done)
    };
    if done > 0 {
        write_cover_and_template(app);
    }
}

/// 起合并路的切分作业(整段渲染已 done):把计划里的每个分P交给引擎的
/// `export.split` 流复制切分。命令层起不来时记下错误,收尾按失败报。
fn spawn_split(app: &mut FilmcraftApp) {
    let source = app.ui.export.parts.items.first().map(|it| it.path.clone()).unwrap_or_default();
    let parts: Vec<Value> = app
        .ui
        .export
        .parts
        .planned_parts
        .iter()
        .map(|it| json!({"startSeconds": it.part.start, "endSeconds": it.part.end, "path": it.item.path}))
        .collect();
    let label = format!("Export split: {}", app.ui.export.parts.base_name);
    match app.session.execute("export.split", json!({"source": source, "parts": parts, "label": label})) {
        Ok(v) => app.ui.export.parts.split_job = v["job"].as_u64(),
        Err(e) => {
            app.ui.export.parts.split_error = Some(e.to_string());
            app.ui.export.parts.split_job = None;
        }
    }
}

/// 合并路切分作业的结果:`None` = 在途(或没有作业),`Some(Ok/Err)` =
/// 终态。作业在 `app.session.jobs` 里,完成即有 result。
fn split_job_state(app: &FilmcraftApp) -> Option<Result<(), String>> {
    let job = app.ui.export.parts.split_job.and_then(|id| app.session.jobs.iter().find(|j| j.id == id))?;
    let res = job.result.lock().unwrap_or_else(|e| e.into_inner()).clone()?;
    Some(res.map(|_| ()))
}

/// 封面复制为 `<基名>_封面.png`,投稿模板优先合并进 biliup-app 的
/// config.json(见 [`merge_biliup_template`]),合并不成再落独立文件。
fn write_cover_and_template(app: &mut FilmcraftApp) {
    let p = &app.ui.export.parts;
    let dir = {
        let d = p.location.trim();
        if d.is_empty() { default_output_dir(&app.session) } else { PathBuf::from(filmcraft_engine::export_tools::expand_home(d)) }
    };
    // 模板名:场景级旁车的文件名就是场景名(reaction 写的 `<场景名>.publish.json`),
    // 没有旁车退回基名(opencut 的 templateName = sceneName ?? exportBaseName)。
    let template_name = app
        .session
        .path
        .as_deref()
        .and_then(find_publish_sidecar)
        .and_then(|sidecar| sidecar.file_stem().map(|stem| stem.to_string_lossy().to_string()))
        .unwrap_or_else(|| p.base_name.clone());
    // 整段条目的标题同投稿标题兜底(opencut whole 模式 title=templateTitle);
    // 分P仍用文件名去扩展名。
    let fallback_title = if p.bili_title.trim().is_empty() { p.base_name.clone() } else { p.bili_title.trim().to_string() };
    // 合并路的分P切分成功才进模板(失败的分P不在盘上)。
    let split_ok = app
        .ui
        .export
        .parts
        .split_error
        .clone()
        .map(Err)
        .or_else(|| split_job_state(app))
        .is_some_and(|r| r.is_ok());
    let videos: Vec<(String, String, String)> = p
        .intro
        .as_ref()
        // 片头只在分P导出里作为第一个分P;本次复制成功才进模板。
        .filter(|_| p.parts_enabled)
        .filter(|intro| matches!(intro_copy_state(intro), Some(Ok(()))))
        .map(|intro| (intro.file.clone(), dir.join(&intro.file).to_string_lossy().to_string(), tl!("Intro").to_string()))
        .into_iter()
        .chain(p.items.iter().map(|it| {
            let stem = it.file.rsplit_once('.').map(|(stem, _)| stem.to_string()).unwrap_or_else(|| it.file.clone());
            let title = if p.whole_enabled && stem == p.base_name { fallback_title.clone() } else { stem };
            (it.file.clone(), it.path.clone(), title)
        }))
        .chain(
            p.planned_parts
                .iter()
                .filter(|_| split_ok)
                .map(|it| (it.item.file.clone(), it.item.path.clone(), it.item.file.rsplit_once('.').map_or(it.item.file.clone(), |(stem, _)| stem.to_string()))),
        )
        .collect();
    let mut errors = Vec::new();
    write_outputs(
        &dir,
        &p.base_name,
        p.cover_path.as_deref(),
        (&p.bili_title, &p.bili_desc, &p.bili_tags),
        &videos,
        &template_name,
        p.biliup_config_override.clone().or_else(biliup_config_path).as_deref(),
        &mut errors,
    );
    if !errors.is_empty() {
        app.ui.status = errors.join("; ");
    }
}

/// [`write_cover_and_template`] 的落盘一半,拆出来好直接测:封面复制、
/// 投稿模板落位——`biliup_config` 给了且合并成功就不写独立文件(biliup-app
/// 只读它自己的 config.json),失败文案经 `errors` 带回(状态栏);
/// `biliup_config` 传 None 时直接写 `<基名>.biliup.json`。
#[allow(clippy::too_many_arguments)]
fn write_outputs(
    dir: &std::path::Path,
    base: &str,
    cover: Option<&str>,
    (title, desc, tags): (&str, &str, &[String]),
    videos: &[(String, String, String)],
    template_name: &str,
    biliup_config: Option<&std::path::Path>,
    errors: &mut Vec<String>,
) {
    if let Some(cover) = cover.filter(|c| !c.is_empty())
        && let Err(e) = std::fs::copy(cover, dir.join(format!("{base}_封面.png")))
    {
        errors.push(tlf!("Cover copy failed: {e}", e = e));
    }
    let title = if title.trim().is_empty() { base.to_string() } else { title.trim().to_string() };
    let json = biliup_json(&title, desc.trim(), tags, videos);
    let merged = biliup_config.is_some_and(|config| merge_biliup_template(config, template_name, &json, errors));
    if !merged {
        let out = dir.join(format!("{base}.biliup.json"));
        if let Ok(bytes) = serde_json::to_vec_pretty(&json)
            && let Err(e) = std::fs::write(&out, bytes)
        {
            errors.push(tlf!("Upload template failed: {e}", e = e));
        }
    }
}

// -------------------------------------------------------------------------------------------------
// 小件
// -------------------------------------------------------------------------------------------------

/// opencut 的 TagInput(export-dialog.tsx):一个输入框模样的容器,
/// chips 在前、输入占满剩余宽度;点容器空白聚焦输入框。逗号/中文逗
/// 号即时分段(前面的段成标签,最后一段留作草稿),Enter 提交,失
/// 焦提交,空草稿退格删最后一个;上限 10 个,重复跳过。
fn tag_input(ui: &mut egui::Ui, t: &Tokens, reg: &mut Reg, p: &mut PartsExportUi) {
    let box_id = egui::Id::unique("partsExport.tags.box");
    let input_id = egui::Id::unique("partsExport.tags");
    // 容器的点击命中区用上一帧的矩形:内容控件在它之后注册、命中优先
    // 级更高,点空白才落到容器上(首帧没有矩形,下一帧起生效)。
    let hit = ui.data(|d| d.get_temp::<egui::Rect>(box_id)).unwrap_or(egui::Rect::NOTHING);
    let box_resp = ui.interact(hit, box_id, egui::Sense::click());

    let shown = egui::Frame::NONE
        .fill(t.field_bg)
        .corner_radius(4.0)
        .stroke(egui::Stroke::new(1.0, t.separator))
        .inner_margin(egui::Margin::symmetric(6, 4))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            let mut removed: Option<usize> = None;
            ui.horizontal_wrapped(|ui| {
                for i in 0..p.bili_tags.len() {
                    if tag_chip(ui, t, &p.bili_tags[i]) {
                        removed = Some(i);
                    }
                }
                // 输入占满当前行剩余宽度,不足最小宽就折到下一行
                // (opencut 的 flex-1 min-w-24)。
                let w = ui.available_width().max(96.0);
                let hint = if p.bili_tags.is_empty() { tl!("Add a tag").to_string() } else { String::new() };
                // 焦点与 Enter 都在 `ui.add` 之前问:egui 的单行输入框在
                // Enter 上自己 `surrender_focus`(回车即"输入结束"),还把
                // 这个 Enter 事件从输入状态里消费掉——等 `add` 回来再问,
                // 焦点已是 false、Enter 也已不在,提交就永远不触发。
                let had_focus = ui.memory(|m| m.has_focus(input_id));
                let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
                let r = ui.add(
                    egui::TextEdit::singleline(&mut p.tag_draft)
                        .id(input_id)
                        .hint_text(hint)
                        .frame(egui::Frame::NONE)
                        .desired_width(w)
                        .char_limit(60),
                );
                reg.add("partsExport.tags", r.rect, "Tags");
                // 逗号(中英)即时分段:前面的段成标签,最后一段留作草稿。
                if r.changed() && p.tag_draft.contains([',', '，']) {
                    let segments: Vec<String> = p.tag_draft.split([',', '，']).map(str::to_string).collect();
                    let last = segments.last().cloned().unwrap_or_default();
                    append_tags(p, &segments[..segments.len().saturating_sub(1)]);
                    p.tag_draft = last;
                    p.bili_edited[2] = true;
                }
                // Enter 提交(opencut 的 preventDefault),失焦也提交(onBlur)。
                let committed_with_enter = had_focus && enter;
                if committed_with_enter || r.lost_focus() && !p.tag_draft.trim().is_empty() {
                    let draft = p.tag_draft.clone();
                    append_tags(p, &[draft]);
                    p.tag_draft.clear();
                    p.bili_edited[2] = true;
                }
                // opencut 的 preventDefault 让输入框留在焦点上(连打几个标
                // 签是常事);egui 的单行框回车会自己放手,这里要回来。
                if committed_with_enter {
                    ui.memory_mut(|m| m.request_focus(input_id));
                }
                // 空草稿退格删最后一个。
                if r.has_focus()
                    && p.tag_draft.is_empty()
                    && !p.bili_tags.is_empty()
                    && ui.input(|i| i.key_pressed(egui::Key::Backspace))
                {
                    p.bili_tags.pop();
                }
            });
            removed
        });
    ui.data_mut(|d| d.insert_temp(box_id, shown.response.rect));
    reg.add("partsExport.tags.box", shown.response.rect, "Tags");
    if box_resp.clicked() {
        ui.memory_mut(|m| m.request_focus(input_id));
    }
    if let Some(i) = shown.inner {
        p.bili_tags.remove(i);
    }
}

/// opencut 的 appendTags:去两端空白,跳过空串与重复(精确比较),
/// 上限 [`MAX_TAGS`]。
fn append_tags(p: &mut PartsExportUi, candidates: &[String]) {
    for candidate in candidates {
        let value = candidate.trim();
        if value.is_empty() || p.bili_tags.iter().any(|x| x == value) || p.bili_tags.len() >= MAX_TAGS {
            continue;
        }
        p.bili_tags.push(value.to_string());
    }
}

/// opencut 的 PillGroup:一条浅色圆角槽里并排两段,选中的一段 accent
/// 填充、白字,未选中的透明底。两段始终可见——下拉框会把其中一个藏起
/// 来,而 opencut 的拆分方式是一眼能看全的两个答案。返回被点的 key。
fn segmented(ui: &mut egui::Ui, t: &Tokens, reg: &mut Reg, id: &str, selected: &str, options: &[(&str, &str)]) -> Option<String> {
    let mut picked = None;
    let shown = egui::Frame::NONE
        .fill(t.field_bg)
        .corner_radius(6.0)
        .stroke(egui::Stroke::new(1.0, t.separator))
        .inner_margin(egui::Margin::same(2))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                for (key, label) in options {
                    let on = *key == selected;
                    let r = ui.add(
                        egui::Button::new(
                            egui::RichText::new(crate::i18n::t(label)).size(12.0).color(if on { Color32::WHITE } else { t.text }),
                        )
                        .fill(if on { t.accent } else { Color32::TRANSPARENT })
                        .corner_radius(4.0)
                        .min_size(vec2(104.0, 22.0)),
                    );
                    reg.add(format!("{id}.{key}"), r.rect, *label);
                    if r.clicked() {
                        picked = Some(key.to_string());
                    }
                }
            });
        });
    reg.add(id, shown.response.rect, selected);
    picked
}

/// 一个标签 chip:浅色圆角底 + 标签文字 + 独立的 ×(悬停变红);返回
/// × 是否被点。
fn tag_chip(ui: &mut egui::Ui, t: &Tokens, tag: &str) -> bool {
    let mut remove = false;
    egui::Frame::NONE
        .fill(t.accent.gamma_multiply(0.15))
        .corner_radius(4.0)
        .inner_margin(egui::Margin::symmetric(6, 2))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                ui.label(egui::RichText::new(tag).size(11.0).color(t.accent));
                let (rect, resp) = ui.allocate_exact_size(vec2(10.0, 12.0), egui::Sense::click());
                let color = if resp.hovered() { t.danger } else { t.accent.gamma_multiply(0.6) };
                ui.painter().text(rect.center(), Align2::CENTER_CENTER, "×", egui::FontId::proportional(11.0), color);
                remove = resp.clicked();
            });
        });
    remove
}

impl PartsExportUi {
    /// 封面文件变了,缩略图作废(解码在下一帧做)。
    pub fn set_cover_dirty(&mut self) {
        self.cover_tex = None;
    }
}

/// 解码封面为缩略图纹理(只在该封面第一次被选中时;同步读盘一次)。
fn load_cover_thumb(ctx: &egui::Context, path: &str, p: &mut PartsExportUi) {
    if p.cover_tex.as_ref().is_some_and(|x| x.path == path) {
        return;
    }
    let loaded = image::open(path)
        .ok()
        .map(|img| img.thumbnail(320, 180))
        .map(|img| {
            let rgba = img.to_rgba8();
            ([rgba.width(), rgba.height()], rgba)
        });
    p.cover_tex = loaded.map(|(size, rgba)| CoverThumb {
        path: path.to_string(),
        tex: ctx.load_texture("parts-cover", egui::ColorImage::from_rgba_unmultiplied([size[0] as usize, size[1] as usize], rgba.as_raw()), egui::TextureOptions::LINEAR),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// duration 模式:每P固定帧数,末P过短(< max(5s, 25%))并入上一P。
    #[test]
    fn duration_mode_merges_a_short_last_part() {
        // 60fps、1250s、每 600s 一P → P1 600s、P2 600s、P3 50s(<150s 并入)。
        let parts = compute_parts(1250.0, 60.0, "duration", 600.0, 0);
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].index, 1);
        assert!((parts[0].start - 0.0).abs() < 1e-9);
        assert!((parts[0].end - 600.0).abs() < 1e-6);
        assert!((parts[1].end - 1250.0).abs() < 1e-9, "末段右端贴回真实时长");
    }

    #[test]
    fn duration_mode_keeps_a_long_enough_last_part() {
        // 末P 500s ≥ max(5s, 600s×25%) 时不并入。
        let parts = compute_parts(1100.0, 30.0, "duration", 600.0, 0);
        assert_eq!(parts.len(), 2);
        assert!((parts[1].end - parts[1].start - 500.0).abs() < 0.1);
    }

    /// count 模式:均分,余数摊给前面的 P。
    #[test]
    fn count_mode_spreads_the_remainder_over_the_first_parts() {
        // 1000s @ 30fps = 30000 帧,3P → 各 10000 帧。
        let parts = compute_parts(1000.0, 30.0, "count", 0.0, 3);
        assert_eq!(parts.len(), 3);
        assert!((parts[0].end - parts[0].start - 10000.0 / 30.0).abs() < 1e-9);
        assert!((parts[2].end - 1000.0).abs() < 1e-9);
        // 10001 帧,2P → 前 5001、后 5000。
        let parts = compute_parts(10001.0 / 30.0, 30.0, "count", 0.0, 2);
        assert_eq!(parts.len(), 2);
        assert!((parts[0].end * 30.0 - 5001.0).abs() < 1e-6);
    }

    #[test]
    fn validation_rules() {
        assert_eq!(validate_split(600.0, "duration", 0.5, 0), Some(SplitError::TooShort));
        assert_eq!(validate_split(600.0, "duration", 600.0, 0), Some(SplitError::LongerThanVideo));
        assert_eq!(validate_split(600.0, "duration", 60.0, 0), None);
        assert_eq!(validate_split(600.0, "count", 0.0, 1), Some(SplitError::MinTwo));
        assert_eq!(validate_split(3.0, "count", 0.0, 4), Some(SplitError::TooMany(3)));
        assert_eq!(validate_split(600.0, "count", 0.0, 4), None);
    }

    #[test]
    fn part_file_names() {
        assert_eq!(part_file_name("我的反应", "_", 1, "mp4"), "我的反应_P1.mp4");
        assert_eq!(part_file_name("", "x", 12, "mp4"), "P12.mp4");
        assert_eq!(part_file_name("a:b", "-", 2, "mp4"), "a-b-P2.mp4");
        assert_eq!(sanitize_name("  "), "");
    }

    /// 模板 JSON 的关键字段:投稿分区、封面恒空(opencut 同款,封面 PNG
    /// 只复制不进模板)、分P 的 id=文件名(biliup-app 的全局任务键)与
    /// 绝对路径;片头(如有)是 videos 首条,分P标题用调用方给的。
    /// 导出计划的三种组合:只有整段、只有分P(逐P渲染)、两者同选
    /// (整段只渲染一次,分P改从整段产物里流复制切出)。
    #[test]
    fn plan_outputs_merges_whole_and_parts_into_one_render() {
        let dir = std::path::Path::new("/out");
        let settings = serde_json::json!({});
        let parts = vec![Part { index: 1, start: 0.0, end: 600.0 }, Part { index: 2, start: 600.0, end: 1250.0 }];

        // 只有整段:一个渲染输出,不切分。
        let (queue, split) = plan_outputs(true, &[], "日常", "mp4", dir, "pfx", "_", &settings);
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].0, "日常.mp4");
        assert_eq!(queue[0].1["path"], json!("/out/日常.mp4"));
        assert_eq!(queue[0].1["range"], json!("entire"));
        assert!(split.is_empty());

        // 只有分P:每个分P各渲染一次(现状路径)。
        let (queue, split) = plan_outputs(false, &parts, "日常", "mp4", dir, "pfx", "_", &settings);
        assert_eq!(queue.len(), 2);
        assert_eq!(queue[0].0, "pfx_P1.mp4");
        assert_eq!(queue[0].1["range"], json!({"mode": "custom", "startSeconds": 0.0, "endSeconds": 600.0}));
        assert!(split.is_empty());

        // 两者同选:队列里只有整段;分P变成对整段产物的切分清单。
        let (queue, split) = plan_outputs(true, &parts, "日常", "mp4", dir, "pfx", "_", &settings);
        assert_eq!(queue.len(), 1, "整段只渲染一次");
        assert_eq!(split.len(), 2);
        assert_eq!(split[0], ("pfx_P1.mp4".into(), "/out/pfx_P1.mp4".into()));
        assert_eq!(split[1], ("pfx_P2.mp4".into(), "/out/pfx_P2.mp4".into()));
    }

    /// 序列名剥媒体扩展名:从录制新建的序列以录制文件命名(带 .mp4),
    /// 不剥就会整段导出成 `日常.mp4.mp4`;其他点号不动。
    #[test]
    fn sequence_base_name_strips_media_extensions() {
        assert_eq!(sequence_base_name("日常.mp4"), "日常");
        assert_eq!(sequence_base_name("Take.MOV"), "Take");
        assert_eq!(sequence_base_name("My Edit"), "My Edit");
        assert_eq!(sequence_base_name("v1.2"), "v1.2", "非媒体扩展名不是扩展名");
        assert_eq!(sequence_base_name("a:b.mp4"), "a-b", "非法字符照旧替换");
    }

    /// 源码率的加权平均:按时间线时长加权;无码率的剪辑不参与;
    /// 全无码率返回 None。
    #[test]
    fn weighted_bitrate_averages_by_timeline_duration() {
        assert_eq!(weighted_bitrate(&[(10.0, Some(4_000_000u64)), (30.0, Some(8_000_000u64))]), Some(7_000), "4×10 + 8×30 ÷ 40 = 7 Mbps");
        assert_eq!(weighted_bitrate(&[(10.0, None), (30.0, Some(8_000_000u64))]), Some(8_000), "无码率的剪辑不参与");
        assert_eq!(weighted_bitrate(&[(10.0, None)]), None);
        assert_eq!(weighted_bitrate(&[]), None);
    }

    #[test]
    fn biliup_template_shape() {
        let v = biliup_json(
            "标题",
            "简介",
            &["a".into(), "b".into()],
            &[("op.mp4".into(), "/out/op.mp4".into(), "片头".into()), ("P1.mp4".into(), "/out/P1.mp4".into(), "P1".into())],
        );
        assert_eq!(v["tid"], json!(21));
        assert_eq!(v["tid_v2"], json!(0));
        assert_eq!(v["tag"], json!("a,b"));
        assert_eq!(v["cover"], json!(""));
        assert_eq!(v["videos"][0]["id"], json!("op.mp4"));
        assert_eq!(v["videos"][0]["filename"], json!("op.mp4"), "片头是第一个分P");
        assert_eq!(v["videos"][0]["title"], json!("片头"));
        assert_eq!(v["videos"][0]["path"], json!("/out/op.mp4"));
        assert_eq!(v["videos"][1]["id"], json!("P1.mp4"));
        assert_eq!(v["videos"][1]["filename"], json!("P1.mp4"));
        assert_eq!(v["videos"][1]["title"], json!("P1"));
        assert_eq!(v["videos"][1]["path"], json!("/out/P1.mp4"));
        assert_eq!(v["copyright"], json!(1));
        assert_eq!(v["aid"], json!(null));
    }

    /// 画质映射:与源相同码率(拿到/拿不到源码率两样)、四档自适应 bpp、
    /// 自定义固定码率,音频 AAC。
    #[test]
    fn settings_map_quality_and_custom_bitrate() {
        let mut p = PartsExportUi::default();
        // 缺省就是「与源视频相同码率」,拿到源码率时目标=源码率、上限 1.5×。
        assert_eq!(p.quality, "source");
        let s = settings_for(&p, &ExportSettings::default(), Some(12_500));
        assert_eq!(s.bitrate_kbps, 12_500);
        assert_eq!(s.max_bitrate_kbps, Some(18_750));
        assert_eq!(s.adaptive_bitrate, None);
        // 拿不到源码率:回落 High 档的自适应码率。
        let s = settings_for(&p, &ExportSettings::default(), None);
        assert_eq!(s.adaptive_bitrate, Some(0.2));
        p.quality = "low".into();
        assert_eq!(settings_for(&p, &ExportSettings::default(), None).adaptive_bitrate, Some(0.05));
        p.quality = "veryHigh".into();
        assert_eq!(settings_for(&p, &ExportSettings::default(), None).adaptive_bitrate, Some(0.4));
        p.quality = "custom".into();
        p.custom_mbps = 20.0;
        let s = settings_for(&p, &ExportSettings::default(), None);
        assert_eq!(s.bitrate_kbps, 20_000);
        assert_eq!(s.max_bitrate_kbps, Some(30_000));
        assert_eq!(s.adaptive_bitrate, None);
        assert_eq!(s.audio.bitrate_kbps, 320);
        assert_eq!(s.format, Format::H264);
    }

    /// 设置跟随导出界面:字幕烧录、效果、硬件编码、GPU 渲染、元数据、
    /// 字幕旁车原样带走;格式/封装/画质/音频由弹窗说了算,「匹配源」与
    /// 逐项的 path/range 不带走(导出界面选的码率模式也不带走)。
    #[test]
    fn parts_settings_follow_the_export_screen() {
        use filmcraft_engine::export::{GpuRendering, HardwareEncoding, Multiplexer};
        use filmcraft_time::{Tick, TimeRange};

        let mut base = ExportSettings::default();
        base.burn_captions = false;
        base.hardware_encoding = HardwareEncoding::Off;
        base.gpu_rendering = GpuRendering::Auto;
        base.effects.image_overlay.enabled = true;
        base.effects.image_overlay.path = "/x/overlay.png".into();
        base.effects.loudness.enabled = true;
        base.metadata.title = "标题".into();
        base.caption_sidecar = Some("srt".into());
        base.frame_size = Some((1920, 1080));
        base.frame_rate = Some(filmcraft_time::FrameRate { num: 60, den: 1 });
        base.scale = 0.5;
        base.multiplexer = Multiplexer::Mov;
        base.bitrate_mode = BitrateMode::Crf;
        base.bitrate_kbps = 99_000;
        base.range = Some(TimeRange::new(Tick(0), Tick(1)));
        base.path = "/somewhere/old.mp4".into();

        let mut p = PartsExportUi::default();
        p.quality = "custom".into();
        p.custom_mbps = 8.0;
        p.include_audio = false;
        let s = settings_for(&p, &base, None);

        // 字幕烧录与硬件编码恒开(分P弹窗自动应用),其余跟随导出界面
        assert!(s.burn_captions);
        assert_eq!(s.hardware_encoding, HardwareEncoding::Auto, "导出界面关着也强制 Auto");
        assert_eq!(s.hardware_encoding, HardwareEncoding::Auto);
        assert_eq!(s.gpu_rendering, GpuRendering::Auto);
        assert!(s.effects.image_overlay.enabled);
        assert_eq!(s.effects.image_overlay.path, "/x/overlay.png");
        assert!(s.effects.loudness.enabled);
        assert_eq!(s.metadata.title, "标题");
        assert_eq!(s.caption_sidecar.as_deref(), Some("srt"));
        // 弹窗自己的选择
        assert_eq!(s.format, Format::H264);
        assert_eq!(s.multiplexer, Multiplexer::Mp4);
        assert_eq!(s.bitrate_mode, BitrateMode::Vbr1Pass);
        assert_eq!(s.bitrate_kbps, 8_000);
        assert_eq!(s.max_bitrate_kbps, Some(12_000));
        assert_eq!(s.adaptive_bitrate, None);
        assert!(!s.include_audio);
        // 匹配源 + 逐项的 path/range
        assert_eq!(s.frame_size, None);
        assert_eq!(s.frame_rate, None);
        assert_eq!(s.scale, 1.0);
        assert_eq!(s.range, None);
        assert_eq!(s.path, "");
    }

    /// 产物落盘:封面复制成 `<基名>_封面.png`,模板标题兜底(空标题用
    /// 基名)、封面字段恒空;没有 biliup 配置可合并时落独立文件。
    #[test]
    fn cover_and_template_land_next_to_the_videos() {
        let dir = std::env::temp_dir().join(format!("parts-export-out-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cover = dir.join("src.png");
        std::fs::write(&cover, b"not-really-a-png-but-copy-does-not-care").unwrap();

        let mut errors = Vec::new();
        write_outputs(
            &dir,
            "我的反应",
            Some(cover.to_str().unwrap()),
            ("", "简介", &["a".into()]),
            &[("P1.mp4".into(), dir.join("P1.mp4").to_string_lossy().to_string(), "P1".into())],
            "我的反应",
            None,
            &mut errors,
        );
        assert!(errors.is_empty(), "{errors:?}");

        let copied = dir.join("我的反应_封面.png");
        assert_eq!(std::fs::read(&copied).unwrap(), b"not-really-a-png-but-copy-does-not-care");
        let template: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("我的反应.biliup.json")).unwrap()).unwrap();
        assert_eq!(template["title"], json!("我的反应"), "空标题用基名兜底");
        assert_eq!(template["cover"], json!(""));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 模板合并进 biliup-app 的 config.json:cookie 原样保留、user_order
    /// 里已登录的账号各写一份(user_order 有而 config 没有的 uid 跳过)、
    /// template_order 去重追加、updated_at 落秒级时间戳;重复合并同名
    /// 模板不产生重复条目。
    #[test]
    fn biliup_template_merges_into_biliup_app_config() {
        let dir = std::env::temp_dir().join(format!("parts-biliup-merge-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let config = dir.join("config.json");
        let template = biliup_json("标题", "简介", &["a".into()], &[]);
        let mut errors = Vec::new();

        // 预置两个登录账号(带假 cookie 字段)和一个 user_order 里的幽灵 uid。
        std::fs::write(
            &config,
            json!({
                "config": {
                    "100": { "access_key": "cookie-a", "cookies": {"SESSDATA": "sa"} },
                    "200": { "access_key": "cookie-b" }
                },
                "user_order": ["100", "ghost", "200"]
            })
            .to_string(),
        )
        .unwrap();

        assert!(merge_biliup_template(&config, "日常", &template, &mut errors), "{errors:?}");
        assert!(errors.is_empty());
        // 第二次合并同名模板:template_order 不得重复。
        assert!(merge_biliup_template(&config, "日常", &template, &mut errors));

        let root: Value = serde_json::from_str(&std::fs::read_to_string(&config).unwrap()).unwrap();
        for uid in ["100", "200"] {
            assert_eq!(root["config"][uid]["templates"]["日常"]["title"], json!("标题"));
            assert_eq!(root["config"][uid]["template_order"], json!(["日常"]), "去重追加");
            assert!(root["config"][uid]["template_updated_at"]["日常"].as_i64().unwrap_or(0) > 0);
        }
        assert_eq!(root["config"]["100"]["access_key"], json!("cookie-a"), "原字段与 cookie 原样保留");
        assert_eq!(root["config"]["200"]["access_key"], json!("cookie-b"));
        assert_eq!(root["config"]["100"]["cookies"]["SESSDATA"], json!("sa"));
        assert!(root["config"]["ghost"].is_null(), "user_order 里不存在的 uid 被跳过");
        // 与 biliup-app 的 serde_json_pretty 同格式:2 空格缩进。
        let text = std::fs::read_to_string(&config).unwrap();
        assert!(text.starts_with("{\n  \""), "落盘应是 2 空格缩进的 pretty JSON");
        assert!(!dir.join("config.json.tmp").exists(), "tmp 文件已原子替换掉");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 合并失败(config 不存在/损坏/结构不对/没有登录账号)都带回可读
    /// 的错误文案;`write_outputs` 拿着坏配置时回落写独立 `.biliup.json`,
    /// 导出产物不丢。
    #[test]
    fn biliup_merge_failures_fall_back_to_the_file() {
        let dir = std::env::temp_dir().join(format!("parts-biliup-fail-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let config = dir.join("config.json");
        let template = biliup_json("标题", "简介", &[], &[]);

        let mut errors = Vec::new();
        assert!(!merge_biliup_template(&config, "日常", &template, &mut errors), "配置不存在");
        assert!(errors[0].contains("not found"), "{errors:?}");

        std::fs::write(&config, b"{not json").unwrap();
        errors.clear();
        assert!(!merge_biliup_template(&config, "日常", &template, &mut errors), "损坏的 JSON");
        assert!(errors[0].contains("parse failed"), "{errors:?}");

        std::fs::write(&config, json!({"user_order": ["100"]}).to_string()).unwrap();
        errors.clear();
        assert!(!merge_biliup_template(&config, "日常", &template, &mut errors), "缺 config 键");
        assert!(errors[0].contains("malformed"), "{errors:?}");

        std::fs::write(&config, json!({"config": {}}).to_string()).unwrap();
        errors.clear();
        assert!(!merge_biliup_template(&config, "日常", &template, &mut errors), "没有登录账号");
        assert!(errors[0].contains("sign in first"), "{errors:?}");

        // write_outputs 拿着坏配置:回落写独立文件,错误文案同时带回。
        let out_dir = dir.join("out");
        std::fs::create_dir_all(&out_dir).unwrap();
        let mut errors = Vec::new();
        write_outputs(
            &out_dir,
            "日常",
            None,
            ("标题", "", &[]),
            &[("P1.mp4".into(), out_dir.join("P1.mp4").to_string_lossy().to_string(), "P1".into())],
            "日常",
            Some(&config),
            &mut errors,
        );
        assert!(errors.iter().any(|e| e.contains("sign in first")), "{errors:?}");
        assert!(out_dir.join("日常.biliup.json").is_file(), "合并失败回落写独立文件");

        let _ = std::fs::remove_dir_all(&dir);
    }
    /// 迟到的 AI 元数据回填:只动没编辑过的字段,空值不覆盖;复位后
    /// 重新可填(opencut 的 biliUserEditedRef 语义)。
    #[test]
    fn ai_metadata_fills_only_untouched_fields() {
        let mut p = PartsExportUi::default();
        p.apply_ai_metadata(&reaction::ai_commentary::RecordingMetadata {
            title: "AI 标题".into(),
            description: "AI 简介".into(),
            tags: vec!["影视".into()],
        });
        assert_eq!(p.bili_title, "AI 标题");
        assert_eq!(p.bili_desc, "AI 简介");
        assert_eq!(p.bili_tags, ["影视"]);

        // 用户改过标题与标签后,新一轮元数据只回填简介。
        p.bili_edited = [true, false, true];
        p.apply_ai_metadata(&reaction::ai_commentary::RecordingMetadata {
            title: "另一个标题".into(),
            description: "另一段简介".into(),
            tags: vec!["生活".into()],
        });
        assert_eq!(p.bili_title, "AI 标题", "用户已编辑的不覆盖");
        assert_eq!(p.bili_desc, "另一段简介");
        assert_eq!(p.bili_tags, ["影视"], "用户已编辑的不覆盖");

        // 新一段录制:复位后全部可回填;空字段不覆盖既有内容。
        p.reset_bili_edits();
        p.apply_ai_metadata(&reaction::ai_commentary::RecordingMetadata {
            title: String::new(),
            description: "新简介".into(),
            tags: vec![],
        });
        assert_eq!(p.bili_title, "AI 标题", "空标题不覆盖");
        assert_eq!(p.bili_desc, "新简介");
        assert_eq!(p.bili_tags, ["影视"], "空标签不覆盖");
    }

    /// 片头旁车的找回:项目目录里的 `<场景名>.intro.json` 给出片头路径;
    /// 没有旁车、旁车损坏、片头文件已不在都当没有片头。
    #[test]
    fn intro_sidecar_is_found_beside_the_project() {
        let dir = std::env::temp_dir().join(format!("parts-intro-find-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let project = dir.join("日常-2026-11-03-12-00-00.fcproj");

        assert_eq!(find_intro_path(project.to_str().unwrap()), None, "没有旁车");

        let intro = dir.join("op.mp4");
        std::fs::write(&intro, b"video bytes").unwrap();
        std::fs::write(
            dir.join("日常.intro.json"),
            serde_json::json!({ "introVideoPath": intro.to_string_lossy() }).to_string(),
        )
        .unwrap();
        assert_eq!(
            find_intro_path(project.to_str().unwrap()).as_deref(),
            Some(intro.to_str().unwrap())
        );

        // 旁车指向已删除的片头:当没有片头,导出其余部分不受影响。
        std::fs::remove_file(&intro).unwrap();
        assert_eq!(find_intro_path(project.to_str().unwrap()), None);

        // 损坏的旁车同样跳过。
        std::fs::write(&intro, b"video bytes").unwrap();
        std::fs::write(dir.join("日常.intro.json"), b"{not json").unwrap();
        assert_eq!(find_intro_path(project.to_str().unwrap()), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 片头直拷:成功落输出目录、失败文案带上下文;复制线程持锁崩溃
    /// (锁毒化)也取出已存的结果,导出不卡在"在途"。
    #[test]
    fn intro_copies_as_the_first_part_and_survives_a_poisoned_slot() {
        let dir = std::env::temp_dir().join(format!("parts-intro-copy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // 源放子目录:源与目标同路径的自拷贝会把文件清空,不是要测的。
        let source_dir = dir.join("picked-from");
        std::fs::create_dir_all(&source_dir).unwrap();
        let source = source_dir.join("op.mp4");
        std::fs::write(&source, b"video bytes").unwrap();

        let intro = IntroPart {
            source: source.to_string_lossy().to_string(),
            file: "op.mp4".into(),
            slot: std::sync::Arc::default(),
        };
        spawn_intro_copy(&intro, &dir);
        for _ in 0..600 {
            if intro_copy_state(&intro).is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(intro_copy_state(&intro), Some(Ok(())));
        assert_eq!(std::fs::read(dir.join("op.mp4")).unwrap(), b"video bytes");

        // 失败:文案带上下文(io 错误在 {e} 里)。
        let missing = IntroPart {
            source: dir.join("gone.mp4").to_string_lossy().to_string(),
            file: "gone.mp4".into(),
            slot: std::sync::Arc::default(),
        };
        spawn_intro_copy(&missing, &dir);
        for _ in 0..600 {
            if intro_copy_state(&missing).is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            intro_copy_state(&missing).unwrap().unwrap_err().contains("Intro video copy failed"),
            "失败文案要带「片头复制失败」的上下文"
        );

        // 毒化的锁:已存的结果照常读出。
        let poisoned = IntroPart {
            source: String::new(),
            file: "op.mp4".into(),
            slot: std::sync::Arc::default(),
        };
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if let Ok(mut slot) = poisoned.slot.lock() {
                *slot = Some(Ok(()));
            }
            panic!("poison the mutex");
        }));
        assert_eq!(intro_copy_state(&poisoned), Some(Ok(())));
        let _ = std::fs::remove_dir_all(&dir);
    }
}