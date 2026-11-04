// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer and Reacut contributors

//! The phone's decoders, over the studio's own clean-room codecs — the same
//! `filmcraft-h264` and `filmcraft-opus` the editor ships, so a phone camera
//! adds no second codec stack. Push decoders both: the session thread hands
//! one access unit (or one Opus packet) in, and finished pictures come back
//! out.
//!
//! H.264 in software, multithreaded (`filmcraft-h264`'s `threads` feature):
//! a phone's 720p stream is a light load, and the output arrives as plain
//! 4:2:0 planes — exactly what the studio's NV12 upload wants, with no
//! colour conversion anywhere on this path.

use std::sync::{Arc, Mutex};

use filmcraft_h264::Decoder as H264Decoder;
use filmcraft_opus::StreamDecoder;

use crate::{DecodedPicture, VideoSink};

static EMITTED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
use std::sync::atomic::Ordering;

// ── video ────────────────────────────────────────────────────────────────

/// The phone's H.264: Annex-B access units in, 4:2:0 pictures out to the
/// sink.
/// A size change takes this many consecutive frames to count. At the
/// phone's 30 fps that is half a second: long enough that an encoder
/// momentarily changing its mind costs nothing downstream, short enough
/// that a deliberate turn of the phone lands before anyone notices.
const SIZE_STABLE_FRAMES: u32 = 15;

/// The decoder draws on this many worker threads.
///
/// The phone's stream is 720p-class at 30 fps — a couple of milliseconds of
/// CPU a frame — and every thread here is a thread the compositor and the
/// encoders sharing this process are not running on. Two is headroom, not
/// a bottleneck.
const DECODE_THREADS: usize = 2;

pub struct MediaH264Decoder {
    decoder: H264Decoder,
    /// Set once the first frame's size has been logged (see `emit`).
    said_size: bool,
    /// The size a size change is being counted against, and how many
    /// consecutive frames have agreed with it. `frame_size` — and with it
    /// every downstream reaction to a new size (the engine waking, the
    /// Canvas rearranging, the project writing) — is only updated once the
    /// count fills. A size that flickers never lands anywhere.
    pending: (u32, u32),
    stable: u32,
    /// Frames that failed to decode — the session reacts with a PLI when
    /// this moves.
    bad: u32,
    /// Loss recovery: drop access units until an IDR arrives.
    skip_until_idr: bool,
    video: Arc<dyn VideoSink>,
    frame_size: Arc<Mutex<Option<(u32, u32)>>>,
}

impl MediaH264Decoder {
    pub fn new(
        video: Arc<dyn VideoSink>,
        frame_size: Arc<Mutex<Option<(u32, u32)>>>,
    ) -> Result<Self, String> {
        Ok(MediaH264Decoder {
            decoder: H264Decoder::with_threads(DECODE_THREADS),
            said_size: false,
            pending: (0, 0),
            stable: 0,
            bad: 0,
            skip_until_idr: false,
            video,
            frame_size,
        })
    }

    /// Decode one Annex-B access unit and send every picture it yields.
    /// `pts` is the RTP timestamp in the 90 kHz clock, carried through to
    /// the picture.
    pub fn push(&mut self, au: &[u8], pts: u32) -> Result<(), String> {
        // Loss recovery: an AU without an IDR carries P-frames that decode
        // against a broken reference — drop until the recovery keyframe.
        if self.skip_until_idr {
            if !au_has_idr(au) {
                return Ok(());
            }
            self.skip_until_idr = false;
        }
        match self.decoder.decode(au, i64::from(pts)) {
            Ok(pictures) => {
                for picture in pictures {
                    self.emit(picture);
                }
                Ok(())
            }
            Err(error) => {
                self.bad += 1;
                Err(format!("h264 decode: {error}"))
            }
        }
    }

    /// One decoded picture → the sink, planes as they came out.
    fn emit(&mut self, picture: filmcraft_h264::Picture) {
        let (width, height) = (picture.width, picture.height);
        if width == 0 || height == 0 {
            return;
        }
        // 第一帧报一次尺寸：画布比例问题的第一线索就是它——手机实际
        // 发来的分辨率与方向都在这里。
        if !self.said_size {
            self.said_size = true;
            log::info!("phone video: first frame {width}x{height}");
        }
        if debounce_size(&mut self.pending, &mut self.stable, width, height) {
            if !self.said_size {
                self.said_size = true;
                log::info!("phone video: first frame {width}x{height}");
            }
            let mut size = self.frame_size.lock().unwrap_or_else(|e| e.into_inner());
            *size = Some((width, height));
            drop(size);
            // The published size is what the Canvas rearranges around: one
            // wake per publication, exactly when every reader can see the
            // value — a wake on the first frame lands before this gate and
            // would find nothing.
            self.video.size_published(width, height);
        }
        // 每 150 帧报一次投递速率：静帧时区分"解码停了"和"投递后停了"。
        let emitted = EMITTED.fetch_add(1, Ordering::Relaxed) + 1;
        if emitted % 150 == 0 {
            log::info!("phone video: {emitted} frames decoded and delivered");
        }
        self.video.send(DecodedPicture {
            width,
            height,
            y: &picture.y,
            y_stride: picture.y_stride,
            u: &picture.u,
            v: &picture.v,
            uv_stride: picture.uv_stride,
        });
    }

    fn reset(&mut self) {
        // 丢弃整个解码器而非续用：包丢失后参考链已断，而 SPS/PPS 本就
        // 随每个恢复关键帧重发，新解码器在下一个 IDR 即可继续。与构造
        // 同一个线程数——丢包重置可能连续发生，每次都起一个满核线程池
        // 正是它本该省下的东西。
        self.decoder = H264Decoder::with_threads(DECODE_THREADS);
        self.skip_until_idr = true;
    }

    fn skip_until_idr(&mut self) {
        self.skip_until_idr = true;
    }
}

/// The size-stability gate behind [`MediaH264Decoder::emit`]: a picture
/// size only counts once it has held for `SIZE_STABLE_FRAMES` consecutive
/// frames. Returns whether `frame_size` is to be published now.
///
/// 抖动过滤：编码器一时改主意（丢包后的重协商、码率挤压下的降档）不该
/// 每次都惊动下游——下游的每一次反应都是唤醒引擎、重排画布、写一次
/// 数据库。Pure so the gate itself is testable without a bitstream.
fn debounce_size(pending: &mut (u32, u32), stable: &mut u32, width: u32, height: u32) -> bool {
    if (*pending) != (width, height) {
        *pending = (width, height);
        *stable = 0;
        return false;
    }
    *stable += 1;
    *stable == SIZE_STABLE_FRAMES
}

impl super::phone_decode::PhoneDecoder for MediaH264Decoder {
    fn push(&mut self, au: &[u8], pts: u32) -> Result<(), String> {
        MediaH264Decoder::push(self, au, pts)
    }

    fn bad_frames(&self) -> u32 {
        self.bad
    }

    fn reset(&mut self) {
        MediaH264Decoder::reset(self)
    }

    fn skip_until_idr(&mut self) {
        MediaH264Decoder::skip_until_idr(self)
    }
}

/// The video AU's IDR check: NAL type 7 (SPS) or 5 (IDR) anywhere in the
/// access unit. A parameter set rides every recovery keyframe the phone's
/// encoder sends.
fn au_has_idr(au: &[u8]) -> bool {
    let mut i = 0usize;
    while i + 4 <= au.len() {
        if au[i] == 0 && au[i + 1] == 0 && au[i + 2] == 0 && au[i + 3] == 1 {
            let nal_type = au[i + 4] & 0x1F;
            if nal_type == 5 || nal_type == 7 {
                return true;
            }
            i += 4;
        }
        i += 1;
    }
    false
}

// ── audio ────────────────────────────────────────────────────────────────

/// The phone's Opus: RTP payloads pushed straight to the studio's Opus
/// decoder, which hands back interleaved stereo — the AudioSink's own
/// layout, so the packet goes to the mixer with no repacking at all.
pub struct MediaOpusDecoder {
    decoder: StreamDecoder,
    scratch: Vec<f32>,
}

impl MediaOpusDecoder {
    pub fn new() -> Result<Self, String> {
        Ok(MediaOpusDecoder {
            decoder: StreamDecoder::new(48_000, 2)
                .map_err(|error| format!("opus decoder: {error}"))?,
            scratch: Vec::new(),
        })
    }

    /// Decodes one RTP payload into `buf` as interleaved stereo f32,
    /// returning the number of *frames* (stereo pairs) written — the same
    /// contract the session loop's `decode_f32` expects.
    pub fn decode_f32(&mut self, payload: &[u8], buf: &mut [f32]) -> Result<usize, String> {
        if buf.len() < 2 {
            return Ok(0);
        }
        self.scratch.clear();
        let samples = self
            .decoder
            .decode_interleaved(Some(payload), &mut self.scratch)
            .map_err(|error| format!("opus decode: {error}"))?;
        let pairs = samples.min(buf.len() / 2);
        buf[..pairs * 2].copy_from_slice(&self.scratch[..pairs * 2]);
        Ok(pairs)
    }
}

#[cfg(test)]
mod tests {
    use super::{SIZE_STABLE_FRAMES, debounce_size};

    /// A size that holds is published exactly once, on the frame the count
    /// fills: first sight resets the count, and `SIZE_STABLE_FRAMES` of
    /// agreement later it lands — once.
    #[test]
    fn a_stable_size_is_published_once_after_the_count_fills() {
        let mut pending = (0u32, 0u32);
        let mut stable = 0u32;
        assert!(!debounce_size(&mut pending, &mut stable, 1280, 720));
        for _ in 0..SIZE_STABLE_FRAMES - 1 {
            assert!(!debounce_size(&mut pending, &mut stable, 1280, 720));
        }
        assert!(debounce_size(&mut pending, &mut stable, 1280, 720));
        // Held further: silence — one publication per size, whoever reads it.
        assert!(!debounce_size(&mut pending, &mut stable, 1280, 720));
        assert!(!debounce_size(&mut pending, &mut stable, 1280, 720));
    }

    /// The failure this gate exists for: an encoder flickering between two
    /// sizes never publishes either — the downstream world never hears of
    /// the flicker at all.
    #[test]
    fn a_flickering_size_is_never_published() {
        let mut pending = (0u32, 0u32);
        let mut stable = 0u32;
        for _ in 0..SIZE_STABLE_FRAMES * 4 {
            assert!(!debounce_size(&mut pending, &mut stable, 1280, 720));
            assert!(!debounce_size(&mut pending, &mut stable, 1280, 720));
            assert!(!debounce_size(&mut pending, &mut stable, 640, 360));
            assert!(!debounce_size(&mut pending, &mut stable, 640, 360));
        }
    }

    /// A deliberate change — the phone turned sideways — lands after its
    /// own count, starting from the change.
    #[test]
    fn a_turn_of_the_phone_lands_after_its_own_count() {
        let mut pending = (0u32, 0u32);
        let mut stable = 0u32;
        assert!(!debounce_size(&mut pending, &mut stable, 720, 1280));
        for _ in 0..SIZE_STABLE_FRAMES - 1 {
            assert!(!debounce_size(&mut pending, &mut stable, 720, 1280));
        }
        assert!(debounce_size(&mut pending, &mut stable, 720, 1280));
        // The turn: the count starts over for the new size.
        assert!(!debounce_size(&mut pending, &mut stable, 1280, 720));
        for _ in 0..SIZE_STABLE_FRAMES - 1 {
            assert!(!debounce_size(&mut pending, &mut stable, 1280, 720));
        }
        assert!(debounce_size(&mut pending, &mut stable, 1280, 720));
    }
}
