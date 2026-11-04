//! PhoneCamera: the phone's camera becomes a live source over the LAN.
//!
//! A phone scans a QR code, opens `https://<desktop-ip>:48210/`, and streams
//! H.264 + Opus over WebRTC straight into the compositor. The whole thing is
//! zero-dependency infrastructure — no tokio, no axum:
//!
//! * **Signaling** — a minimal HTTPS server (rustls + a self-signed cert
//!   from rcgen — browsers require a secure context for `getUserMedia`; the
//!   certificate's SANs cover the LAN IP the phone scans). One accept thread
//!   plus one thread per connection; the phone is the offerer (no trickle
//!   ICE): `POST /offer` (202, immediately — the handler must never block
//!   the session thread), poll `GET /status`, then `GET /answer`.
//! * **WebRTC** — `rtc` 0.20's sans-I/O core (`RTCPeerConnection` implements
//!   `Protocol`): the session thread drives `poll_write`/`poll_event`/
//!   `poll_read`/`poll_timeout`/`handle_timeout`/`handle_read` over a single
//!   plain `UdpSocket`. No dedicated media ports, no threads.
//! * **Congestion control** — no TWCC: the answer SDP carries no
//!   transport-cc (rtcp-fb or extmap), so Chrome's bandwidth estimator falls
//!   back to REMB, which the session feeds every 500ms straight through
//!   `receiver.write_rtcp` (a real measured link rate × headroom).
//! * **Media** — H.264 payloads go through [`H264Depacketizer`] → the
//!   studio's own `filmcraft-h264` decoder (see `phone_decode`) →
//!   [`VideoSink`]; Opus payloads are Opus packets already, decoded by the
//!   studio's own `filmcraft-opus` to interleaved stereo f32 and pushed
//!   into the [`AudioSink`].
//!
//! Session lifetime: one phone at a time, on one process-wide signaling
//! server (see `phone::Shared`): an incoming offer goes to the most recently
//! started waiting session — the Source the user added last and scanned
//! from — and a scan that lands while a session is on the air replaces its
//! stream instead of being refused. A connection whose phone stops sending
//! is torn down by the session itself within seconds, so the pairing code
//! comes back without waiting for the ICE agent to call the silent peer
//! failed. A failed or stopped session drops its peer connection and the
//! server loops back to waiting for a new offer (the phone reconnects by
//! re-running its page flow).
//!
//! The crate owns no capture or audio-engine state: the caller injects a
//! [`VideoSink`] (decoded pictures land there as bare 4:2:0 planes — the
//! studio converts to NV12 in the one copy a frame costs) and an
//! [`AudioSink`] (interleaved stereo 48 kHz f32). On desktop these are the
//! reaction studio's compositor input and its audio-mixer branch.

mod decoders;
mod h264_depacketizer;
mod phone;
mod phone_decode;

pub use decoders::{MediaH264Decoder, MediaOpusDecoder};
pub use h264_depacketizer::{DepackOutcome, H264Depacketizer};
pub use phone::{
    PhoneSession, SessionStatus, SIGNALING_PORT, default_route_ip, pairing_url, phone_session_id,
    session_status, set_pairing_candidates, take_connected, take_failed,
};
pub use phone_decode::{PhoneDecoder, create_decoder};

/// One decoded picture, as the decoder hands it over: three 8-bit 4:2:0
/// planes and the row strides they sit on. Borrowed, not owned — the sink
/// does its one copying pass straight from these.
pub struct DecodedPicture<'a> {
    pub width: u32,
    pub height: u32,
    /// Luma, `y_stride` bytes per row.
    pub y: &'a [u8],
    pub y_stride: usize,
    /// Blue-difference chroma, `uv_stride` bytes per row.
    pub u: &'a [u8],
    /// Red-difference chroma, same layout as `u`.
    pub v: &'a [u8],
    pub uv_stride: usize,
}

/// Where decoded phone video goes. The caller owns the receiving side
/// (on desktop: the reaction studio's compositor input, fed NV12).
pub trait VideoSink: Send + Sync {
    /// Publish one decoded picture. Latest-wins semantics are the caller's.
    fn send(&self, picture: DecodedPicture<'_>);

    /// The debounced picture size was published — fired once per size, at
    /// the moment the value is readable (see the decoder's stability gate).
    /// The desktop sink wakes its engine with it, which is what rearranges
    /// the Canvas around the real size; a wake on the first frame cannot do
    /// that job, because it lands before the gate publishes anything.
    fn size_published(&self, _width: u32, _height: u32) {}
}

/// Where decoded phone audio goes. Interleaved stereo 48 kHz f32 (the
/// decoder outputs interleaved directly — see `filmcraft-opus`'s
/// `StreamDecoder`).
pub trait AudioSink: Send + Sync {
    fn push(&self, samples: &[f32]);
}
