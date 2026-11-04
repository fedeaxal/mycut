//! H.264 decode for the PhoneCamera WebRTC source, over the studio's own
//! `filmcraft-h264`: one decoder on the session thread, Annex-B access units
//! in, 4:2:0 pictures out to the sink.

use std::sync::{Arc, Mutex};

use crate::decoders::MediaH264Decoder;
use crate::VideoSink;

/// One decoder for the phone's H.264 track. Fed Annex-B access units on the
/// session thread; decoded pictures land in the video sink.
///
/// Not `Send`: the decoder is single-threaded state (its own worker pool is
/// internal) and lives on the session thread for its whole life. Driving it
/// from one thread is all this crate ever does.
pub trait PhoneDecoder {
    /// Decode one complete access unit. `pts` is the RTP timestamp in the
    /// 90 kHz media clock — carried through to the picture.
    fn push(&mut self, au: &[u8], pts: u32) -> Result<(), String>;

    /// Count of bad frames since the last check: when this moves, the
    /// picture is frozen on the last good frame until a keyframe arrives —
    /// the phone layer reacts with a PLI + decoder reset.
    fn bad_frames(&self) -> u32 {
        0
    }

    /// Discard the decoder's internal state and stop accepting frames until
    /// the next keyframe. Call together with a keyframe request: after
    /// packet loss, P-frames decode against a broken reference chain.
    fn reset(&mut self) {}

    /// Drop every access unit until the next keyframe (loss recovery gate).
    fn skip_until_idr(&mut self) {}
}

pub fn create_decoder(
    video: Arc<dyn VideoSink>,
    frame_size: Arc<Mutex<Option<(u32, u32)>>>,
) -> Result<Box<dyn PhoneDecoder>, String> {
    Ok(Box::new(MediaH264Decoder::new(video, frame_size)?))
}
