//! PhoneCamera (M5): the phone's camera becomes a live source over the LAN.
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
//!   back to REMB, which `SessionCtx::maybe_send_remb` feeds every 500ms
//!   straight through `receiver.write_rtcp` (a real measured link rate ×
//!   headroom). Without any feedback Chrome's estimator keeps cutting the
//!   bitrate until the encoder drops resolution — the "phone automatically
//!   lowering quality" symptom. (webrtc-rs's TWCC *receiver* is unusable
//!   here: its per-stream recorder misreports cross-stream loss and the
//!   sender cuts bitrate; and interceptor `bind_remote_stream` never fires —
//!   see `build_peer`.)
//! * **Media** — H.264 payloads go through [`H264Depacketizer`] → the
//!   studio's own `filmcraft-h264` (see `phone_decode`) → the video sink; Opus
//!   payloads are Opus packets already, decoded by the studio's own
//!   `filmcraft-opus` to interleaved stereo f32 and pushed into the audio
//!   sink.
//!
//! Session lifetime: one phone at a time, on one process-wide signaling
//! server (see [`Shared`]) — an incoming offer goes to a waiting session the
//! host named as a pairing candidate (see [`set_pairing_candidates`]: the
//! phone camera Sources of the Scene being shown, which is where the QR the
//! user can be looking at lives), falling back to the most recently started
//! waiting session, and a scan that lands while a session is on the
//! air **replaces** its stream instead of being refused (the phone's stop
//! button only closes its own peer connection, so a re-scan must not be
//! told the port is busy for the half minute an ICE agent takes to call a
//! silent peer failed). The negotiation loop keeps its own watch on a
//! connection whose phone stopped sending, for the same reason. A failed or
//! stopped session drops its peer connection and the server loops back to
//! waiting for a new offer (the phone reconnects by re-running its page
//! flow).

use std::io::{Read, Write};
use std::net::{IpAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rcgen::{CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair};
use rustls::crypto::ring;
use rustls::pki_types::PrivateKeyDer;
use rustls::{ServerConfig, ServerConnection, StreamOwned};

use bytes::BytesMut;
use rtc::interceptor::{Interceptor, Registry};
use rtc::peer_connection::configuration::interceptor_registry::{configure_nack, configure_rtcp_reports};
use rtc::peer_connection::configuration::media_engine::{
    MediaEngine, MIME_TYPE_H264, MIME_TYPE_OPUS, MIME_TYPE_RTX,
};
use rtc::peer_connection::configuration::RTCConfigurationBuilder;
use rtc::peer_connection::event::{RTCPeerConnectionEvent, RTCTrackEvent};
use rtc::peer_connection::message::RTCMessage;
use rtc::peer_connection::sdp::{RTCSessionDescription, RTCSdpType};
use rtc::peer_connection::state::RTCPeerConnectionState;
use rtc::peer_connection::transport::{CandidateConfig, CandidateHostConfig, RTCIceCandidate};
use rtc::peer_connection::RTCPeerConnection;
use rtc::peer_connection::RTCPeerConnectionBuilder;
use rtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use rtc::rtcp::payload_feedbacks::receiver_estimated_maximum_bitrate::ReceiverEstimatedMaximumBitrate;
use rtc::rtp::packet::Packet as RtpPacket;
use rtc::rtp_transceiver::rtp_sender::{RTCRtpCodec, RTCRtpCodecParameters, RtpCodecKind};
use rtc::rtp_transceiver::RTCRtpReceiverId;
use rtc::sansio::Protocol;
use rtc::shared::{TaggedBytesMut, TransportContext, TransportProtocol};

use crate::h264_depacketizer::{DepackOutcome, H264Depacketizer};
use crate::phone_decode::{PhoneDecoder, create_decoder};
use crate::{AudioSink, VideoSink};

/// The LAN port the signaling server + the phone page live on.
pub const SIGNALING_PORT: u16 = 48210;

/// Set once a phone session's peer connection reaches Connected. The UI
/// consumes the edge once (every render tick) to auto-close the pairing
/// popup; a fresh connection (the phone re-scans after closing its tab)
/// sets it again. A single flag is right: the app is one reaction view
/// with one pairing popup, and any phone connecting means "scan worked".
static CONNECTED: AtomicBool = AtomicBool::new(false);

/// Whether a phone connection just established. Consumes the edge: each
/// call reports a connection at most once.
pub fn take_connected() -> bool {
    CONNECTED.swap(false, Ordering::Acquire)
}

/// UI-facing session status (M6): the process-wide mirror of the live
/// session's signal state. One signaling server serves the whole reaction
/// view (single :48210 port), so the process-level mirror matches the
/// actual session; the UI's 100 ms tick reads it for the pairing popup's
/// status line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionStatus {
    /// No phone connected (server up, waiting for an offer).
    Waiting,
    /// The phone's offer landed; negotiation in progress.
    Connecting,
    /// Media flowing.
    Connected,
    /// The session died (with the reason). The pairing popup shows the
    /// reason and suggests a re-scan.
    Failed(String),
}

static STATUS: Mutex<SessionStatus> = Mutex::new(SessionStatus::Waiting);
/// The failure edge — consumed once by the UI tick (`take_failed`), which
/// toasts it (a session fails at most once; `take` gives the once for
/// free). Only genuine failures land here — the phone asking to stop is
/// not an error.
static FAILED: Mutex<Option<String>> = Mutex::new(None);

/// The current session status (the popup's status line).
pub fn session_status() -> SessionStatus {
    STATUS.lock().unwrap().clone()
}

/// Whether the session failed since the last call. Consumes the edge.
pub fn take_failed() -> Option<String> {
    FAILED.lock().unwrap().take()
}

/// Report a genuine session failure: flips the status and arms the
/// one-shot failure edge (the UI toasts it and shows the reason).
fn report_failure(reason: String) {
    *STATUS.lock().unwrap() = SessionStatus::Failed(reason.clone());
    *FAILED.lock().unwrap() = Some(reason);
}

/// The phone page: a self-contained HTML page served at `/`. The phone scans
/// a QR code pointing at `http://<ip>:48210/` and never types anything.
const PHONE_PAGE: &str = include_str!("phone_page.html");

// ---------------------------------------------------------------------------
// Session handle (owned by the source registry, mirrors MediaSession)
// ---------------------------------------------------------------------------

pub struct PhoneSession {
    stop: Arc<AtomicBool>,
    /// The first decoded frame's actual size — the UI adopts it as the
    /// source's natural size (the 1280x720 creation-time guess is wrong for
    /// portrait streams, and the selection box must hug the picture).
    frame_size: Arc<Mutex<Option<(u32, u32)>>>,
    join: Option<JoinHandle<()>>,
}

impl PhoneSession {
    /// Start the signaling server + the session thread. Decoded frames land
    /// in `video` (the caller owns the receiving side — on desktop the
    /// reaction studio's `VideoMailbox`, attach it to the studio to see
    /// them) and decoded audio in `audio` (the caller's capture ring).
    pub fn start(
        id: String,
        video: Arc<dyn VideoSink>,
        audio: Arc<dyn AudioSink>,
    ) -> Result<Self, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let frame_size = Arc::new(Mutex::new(None));
        let join = {
            let video = video.clone();
            let audio = audio.clone();
            let stop = stop.clone();
            let frame_size = frame_size.clone();
            let id = id.clone();
            std::thread::Builder::new()
                .name("phone-session".to_string())
                .spawn(move || run_session(id, video, audio, stop, frame_size))
                .map_err(|e| format!("phone session thread: {e}"))?
        };
        Ok(Self {
            stop,
            frame_size,
            join: Some(join),
        })
    }

    /// The actual decoded frame size, once the first frame has landed
    /// (`None` before the phone connects / while the codec hasn't fired).
    pub fn frame_size(&self) -> Option<(u32, u32)> {
        *self.frame_size.lock().unwrap()
    }

    /// Stop the session. Dropping is what stops it — the flag goes up and
    /// the session thread is joined — so this is only a name for it; the
    /// caller may equally let the value fall out of scope.
    pub fn stop(self) {}
}

impl Drop for PhoneSession {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

// ---------------------------------------------------------------------------
// Signaling state shared with the rouille handler (HTTP happens on the same
// thread, but the two-phase protocol keeps handlers non-blocking anyway)
// ---------------------------------------------------------------------------

#[derive(Default)]
struct SignalInner {
    state: SignalState,
}

#[derive(Default, Clone)]
enum SignalState {
    #[default]
    Idle,
    OfferReady { sdp: String, host_ip: String },
    AnswerReady { sdp: String },
    /// Connected, with the last published answer kept (a phone that
    /// re-fetches `/answer` after `connected` still gets it).
    Connected { sdp: String },
    Failed(String),
}

struct Signaling {
    inner: Mutex<SignalInner>,
    /// Set when the router retires this session's live connection for a
    /// newer scan. A flag of its own because the offer that follows the
    /// retirement overwrites the `Failed` state the retirement wrote — the
    /// negotiation loop reads this one, which survives until the loop is
    /// done with the connection it was driving.
    retire: AtomicBool,
}

impl Default for Signaling {
    fn default() -> Self {
        Self {
            inner: Mutex::new(SignalInner::default()),
            retire: AtomicBool::new(false),
        }
    }
}

impl Signaling {
    /// Accept an offer. Only an idle or failed session takes a new phone
    /// (a live one answers 409 — the phone's page shows "busy, stop first").
    fn set_offer(&self, sdp: String, host_ip: String) -> bool {
        let mut inner = self.inner.lock().unwrap();
        match &inner.state {
            SignalState::Idle | SignalState::Failed(_) => {
                inner.state = SignalState::OfferReady { sdp, host_ip };
                *STATUS.lock().unwrap() = SessionStatus::Connecting;
                true
            }
            _ => false,
        }
    }

    fn take_offer(&self) -> Option<(String, String)> {
        let mut inner = self.inner.lock().unwrap();
        if let SignalState::OfferReady { sdp, host_ip } =
            std::mem::replace(&mut inner.state, SignalState::Idle)
        {
            Some((sdp, host_ip))
        } else {
            None
        }
    }

    fn set_answer(&self, sdp: String) {
        self.inner.lock().unwrap().state = SignalState::AnswerReady { sdp };
    }

    fn set_connected(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.state = match &inner.state {
            SignalState::AnswerReady { sdp } => {
                SignalState::Connected { sdp: sdp.clone() }
            }
            _ => SignalState::Connected { sdp: String::new() },
        };
        // The UI consumes this edge once (see `take_connected`) to close the
        // pairing popup automatically when the phone connects.
        CONNECTED.store(true, Ordering::Release);
        *STATUS.lock().unwrap() = SessionStatus::Connected;
    }

    fn set_failed(&self, err: String) {
        self.inner.lock().unwrap().state = SignalState::Failed(err);
    }

    /// The same, from a negotiation's own teardown. An offer the router
    /// already routed here — a newer scan that displaced the connection this
    /// negotiation is winding down — outranks the failure: it stays for the
    /// loop's next round instead of being wiped by it.
    fn set_failed_keep_offer(&self, err: String) {
        let mut inner = self.inner.lock().unwrap();
        if matches!(inner.state, SignalState::OfferReady { .. }) {
            return;
        }
        inner.state = SignalState::Failed(err);
    }

    /// The phone asked to stop: the running session's event loop checks this
    /// each round and tears down promptly instead of waiting for ICE to time
    /// out.
    fn is_failed(&self) -> bool {
        matches!(&self.inner.lock().unwrap().state, SignalState::Failed(_))
    }

    /// A newer scan displaced this session's connection: the negotiation
    /// loop stands down the same round it sees this, whatever the state
    /// slot now holds.
    fn is_retired(&self) -> bool {
        self.retire.load(Ordering::Acquire)
    }

    /// Stand a live connection down for a newer scan. The `Failed` state
    /// makes the session routable again (`waiting`) and tells the phone on
    /// the air, through `/status`, that it is done; the flag makes the
    /// negotiation loop actually exit, which the state alone cannot — the
    /// offer that follows overwrites it.
    fn retire(&self) {
        self.retire.store(true, Ordering::Release);
        self.set_failed("stopped by phone".to_string());
    }

    fn clear_retire(&self) {
        self.retire.store(false, Ordering::Release);
    }

    /// Whether this session can take a phone — the gate [`Self::set_offer`]
    /// applies. An offer is routed by this: the newest session that answers
    /// `true` becomes the phone's session.
    fn waiting(&self) -> bool {
        matches!(
            &self.inner.lock().unwrap().state,
            SignalState::Idle | SignalState::Failed(_)
        )
    }

    fn status_json(&self) -> serde_json::Value {
        let inner = self.inner.lock().unwrap();
        match &inner.state {
            SignalState::Idle => serde_json::json!({ "state": "waiting" }),
            SignalState::OfferReady { .. } => serde_json::json!({ "state": "offer-received" }),
            SignalState::AnswerReady { .. } => serde_json::json!({ "state": "answer-ready" }),
            SignalState::Connected { .. } => serde_json::json!({ "state": "connected" }),
            SignalState::Failed(e) => {
                serde_json::json!({ "state": "failed", "error": e })
            }
        }
    }

    fn answer(&self) -> Option<RTCSessionDescription> {
        let inner = self.inner.lock().unwrap();
        match &inner.state {
            SignalState::AnswerReady { sdp } | SignalState::Connected { sdp } => {
                RTCSessionDescription::answer(sdp.clone()).ok()
            }
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// The process-wide server: one listener, every session, one phone
// ---------------------------------------------------------------------------

/// Which session a request without one is for. The port and the page URL are
/// the process's, and a phone scan carries no session identity — so the
/// routing is the router's:
///
/// * `POST /offer` goes to the **most recently started** waiting session —
///   the Source the user added last, whose QR they scanned. That is what
///   makes a second phone camera Source, in another Scene, pair instead of
///   the first one — idle, waiting — silently taking the stream.
/// * Everything the phone asks after its offer (`/status`, `/answer`,
///   `/stop`) follows it to the session that accepted the offer.
struct Router {
    /// Registered sessions, newest first, each with the id its `PhoneSession`
    /// started under. Weak: a session that stopped without unregistering is
    /// skipped at the next routing decision rather than being asked about
    /// forever.
    sessions: Vec<(String, Weak<Signaling>)>,
    /// The session the phone is on the air with — the one that accepted the
    /// current offer.
    active: Option<Weak<Signaling>>,
}

impl Default for Router {
    fn default() -> Self {
        Self {
            sessions: Vec::new(),
            active: None,
        }
    }
}

/// The signaling server every `PhoneSession` of this process shares. The
/// listener lives while at least one session is registered; the last one out
/// stops it and forgets it, so a later session binds a fresh server rather
/// than finding the port held by this process's own dead listener.
struct Shared {
    router: Mutex<Router>,
    server: Mutex<Option<HttpsServer>>,
    /// What the accept loop polls between accepts: raised when the last
    /// session unregisters, just before the server is dropped and joined.
    stop: Arc<AtomicBool>,
}

static SHARED: Mutex<Option<Arc<Shared>>> = Mutex::new(None);

/// The session ids the host expects a scan to pair with — the phone camera
/// Sources of the Scene being shown, named by [`phone_session_id`]. The QR
/// the user can be looking at lives in one of these Sources' properties, so
/// an offer goes to a **waiting session on this list first** and only falls
/// back to the newest waiting session elsewhere. Without the preference a
/// scan after a disconnect could re-pair with the session the phone just
/// left — it is as "waiting" as any — and the picture would land in the
/// Scene it was streamed from instead of the one being shown.
static PAIRING_CANDIDATES: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Names the session a phone camera Source runs under, from the pipeline
/// input name the engine opens it against. The same string [`Self::
/// set_pairing_candidates`] lists and the router matches.
pub fn phone_session_id(input_name: &str) -> String {
    format!("phone-camera-{input_name}")
}

/// Replaces the pairing candidates (see [`PAIRING_CANDIDATES`]). Called by
/// the host whenever the Scene being shown changes — the property that
/// decides which Sources a scan can be aimed at.
pub fn set_pairing_candidates(ids: &[String]) {
    let mut candidates = PAIRING_CANDIDATES.lock().unwrap();
    if *candidates != ids {
        *candidates = ids.to_vec();
    }
}

fn pairing_candidates() -> Vec<String> {
    PAIRING_CANDIDATES.lock().unwrap().clone()
}

impl Shared {
    /// The process's server, started by the first session that needs it.
    fn acquire() -> Result<Arc<Shared>, String> {
        let mut guard = SHARED.lock().unwrap();
        if let Some(shared) = guard.as_ref() {
            return Ok(Arc::clone(shared));
        }
        let shared = Arc::new(Shared {
            router: Mutex::new(Router::default()),
            server: Mutex::new(None),
            stop: Arc::new(AtomicBool::new(false)),
        });
        *shared.server.lock().unwrap() = Some(HttpsServer::start(Arc::clone(&shared))?);
        *guard = Some(Arc::clone(&shared));
        Ok(shared)
    }

    /// A session joins: it becomes the one a new scan pairs with.
    fn register(self: &Arc<Self>, id: &str, signaling: &Arc<Signaling>) {
        self.router
            .lock()
            .unwrap()
            .sessions
            .insert(0, (id.to_owned(), Arc::downgrade(signaling)));
    }

    /// A session leaves. The last one out also stops the listener and
    /// forgets it.
    fn unregister(self: &Arc<Self>, signaling: &Signaling) {
        let me = signaling as *const Signaling;
        let was_active;
        let last = {
            let mut router = self.router.lock().unwrap();
            router.sessions.retain(|(_, weak)| {
                weak.upgrade()
                    .is_some_and(|arc| Arc::as_ptr(&arc) as *const Signaling != me)
            });
            was_active = router
                .active
                .as_ref()
                .and_then(|weak| weak.upgrade())
                .is_some_and(|arc| Arc::as_ptr(&arc) as *const Signaling == me);
            if was_active {
                router.active = None;
            }
            router.sessions.is_empty()
        };
        // What was on the air is gone — a source closed while its phone
        // streamed, most often. Nothing is connected any more, whatever the
        // status mirror still says; leaving it at `Connected` would keep the
        // pairing code hidden forever, since only a session's own failure or
        // a phone's own stop ever writes over that state.
        if was_active {
            *STATUS.lock().unwrap() = SessionStatus::Waiting;
        }
        if last {
            // Outside the router lock: dropping the server joins the accept
            // thread, which takes the router lock per request.
            let mut guard = SHARED.lock().unwrap();
            self.stop.store(true, Ordering::Release);
            drop(self.server.lock().unwrap().take());
            if guard.as_ref().is_some_and(|shared| Arc::ptr_eq(shared, self)) {
                *guard = None;
            }
        }
    }
}

/// A registered session, for exception safety: whatever way `run_session`
/// leaves its loop — stop, error, panic — the router hears of it.
struct Registration {
    server: Arc<Shared>,
    signaling: Arc<Signaling>,
}

impl Drop for Registration {
    fn drop(&mut self) {
        self.server.unregister(&self.signaling);
    }
}

// ---------------------------------------------------------------------------
// The session thread: HTTPS signaling on its own accept thread, then the
// WebRTC event loop, then back to waiting for the next phone.
// ---------------------------------------------------------------------------

fn run_session(
    id: String,
    video: Arc<dyn VideoSink>,
    audio: Arc<dyn AudioSink>,
    stop: Arc<AtomicBool>,
    frame_size: Arc<Mutex<Option<(u32, u32)>>>,
) {
    let signaling = Arc::new(Signaling::default());
    // One server per process, not per session: a second phone camera Source
    // cannot bind the port the first one holds, and the phone that scans
    // would reach the first session's listener and stream into the first
    // Source's pipeline — the picture turning up in the Scene it was not
    // paired from. The router sends the offer to the newest waiting session
    // instead, which is the Source added last.
    let server = match Shared::acquire() {
        Ok(server) => server,
        Err(e) => {
            log::error!("phone signaling server on :{SIGNALING_PORT}: {e}");
            report_failure(format!("信令服务器启动失败：{e}"));
            return;
        }
    };
    server.register(&id, &signaling);
    let _registration = Registration {
        server: Arc::clone(&server),
        signaling: Arc::clone(&signaling),
    };
    log::info!("phone camera ready: {id} signaling :{SIGNALING_PORT}");

    loop {
        if stop.load(Ordering::Acquire) {
            return;
        }
        if let Some((offer_sdp, host_ip)) = signaling.take_offer() {
            // One negotiation, one retire: the flag belongs to the
            // connection this offer starts, not to the session.
            signaling.clear_retire();
            // The phone reached us via the Host header, so that IP is a
            // candidate the phone can actually reach; fall back to the
            // default route's IP if no header was sent.
            let host_ip = if host_ip.is_empty() {
                default_route_ip().unwrap_or_default()
            } else {
                host_ip
            };
            // Media socket — one ephemeral UDP port carries ICE + RTP +
            // RTCP. Bound to the host candidate's IP: rtc-ice matches
            // inbound STUN packets against the exact local address, and a
            // 0.0.0.0-bound socket reports local_addr as 0.0.0.0:port,
            // which matches no candidate — every packet is discarded and
            // the connection dies within seconds.
            let Some(ip) = host_ip.parse::<IpAddr>().ok() else {
                log::warn!("phone: unparseable host ip {host_ip:?}; skipping session");
                continue;
            };
            let udp = match UdpSocket::bind((ip, 0)) {
                Ok(s) => s,
                Err(e) => {
                    log::error!("phone media socket on {ip}: {e}");
                    continue;
                }
            };
            // A keyframe burst can land several hundred RTP packets at once;
            // the OS default receive buffer overflows (and *we* lose the
            // packets, not the network) when the session thread is busy.
            // 8 MiB covers a 720p keyframe burst plus margin. (std::net's
            // set_recv_buffer_size was removed in Rust 1.97 — socket2 is
            // already in the tree via rtc.)
            let _ = socket2::SockRef::from(&udp).set_recv_buffer_size(8 * 1024 * 1024);
            let udp_port = udp.local_addr().map(|a| a.port()).unwrap_or(0);
            match run_negotiation(
                &signaling,
                &offer_sdp,
                &host_ip,
                &udp,
                udp_port,
                &video,
                &audio,
                &stop,
                &frame_size,
            ) {
                Ok(()) => {
                    // Only `stop` ends a healthy session.
                    if stop.load(Ordering::Acquire) {
                        return;
                    }
                }
                Err(e) => {
                    log::warn!("phone session ended: {e}");
                    // A pending offer — a newer scan this router already
                    // routed here while the old negotiation was winding
                    // down — outranks the teardown's failure note.
                    signaling.set_failed_keep_offer(e.clone());
                    // The phone leaving on purpose, and the phone that
                    // walked away in silence, are not errors: the status
                    // mirror goes back to waiting and the pairing code
                    // returns. A connection that never came up, or failed
                    // under way, is.
                    if e == "stopped by phone" || e == "the phone stopped sending" {
                        *STATUS.lock().unwrap() = SessionStatus::Waiting;
                    } else {
                        report_failure(e);
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(15));
    }
}

// ---------------------------------------------------------------------------
// One peer connection, from the offer to the media flowing (or failing)
// ---------------------------------------------------------------------------

/// Build the peer connection with the codecs and interceptor chain that
/// keep the phone's encoder at full resolution:
///
/// * H.264 baseline (42e01f, what desktop Chrome sends) + high (640c1f, some
///   Androids) + their RTX, Opus.
/// * `configure_nack` (loss repair) + `configure_rtcp_reports` (SR/RR) +
///   `RembFeedbackInterceptor` (per-sender REMB bandwidth feedback).
///
/// Why REMB and not TWCC: the SDP deliberately negotiates no transport-cc,
/// so Chrome's bandwidth estimator relies on REMB. A sender whose estimator
/// never receives feedback stays at its low starting bitrate (~300 kbps in
/// Chrome), the encoder then drops the resolution to match — observed as the
/// phone "automatically lowering quality". webrtc-rs's TWCC *receiver* would
/// be the alternative, but its per-stream recorder misreports cross-stream
/// loss and makes the sender cut bitrate; REMB is per-sender aggregate, so
/// it carries no such defect.
///
/// Returns the connection (its `create_answer` output already carries the
/// host candidate in valid RFC 4566 order — no manual injection needed).
fn build_peer(host_ip: &str, udp_port: u16) -> Result<RTCPeerConnection<impl Interceptor>, String> {
    let mut media_engine = MediaEngine::default();
    let h264_fmtp = |profile: &str| {
        format!("level-asymmetry-allowed=1;packetization-mode=1;profile-level-id={profile}")
    };
    media_engine
        .register_codec(
            RTCRtpCodecParameters {
                rtp_codec: RTCRtpCodec {
                    mime_type: MIME_TYPE_H264.to_owned(),
                    clock_rate: 90000,
                    channels: 0,
                    sdp_fmtp_line: h264_fmtp("42e01f"),
                    rtcp_feedback: vec![],
                },
                payload_type: 102,
            },
            RtpCodecKind::Video,
        )
        .map_err(|e| e.to_string())?;
    media_engine
        .register_codec(
            RTCRtpCodecParameters {
                rtp_codec: RTCRtpCodec {
                    mime_type: MIME_TYPE_RTX.to_owned(),
                    clock_rate: 90000,
                    channels: 0,
                    sdp_fmtp_line: "apt=102".to_owned(),
                    rtcp_feedback: vec![],
                },
                payload_type: 103,
            },
            RtpCodecKind::Video,
        )
        .map_err(|e| e.to_string())?;
    media_engine
        .register_codec(
            RTCRtpCodecParameters {
                rtp_codec: RTCRtpCodec {
                    mime_type: MIME_TYPE_H264.to_owned(),
                    clock_rate: 90000,
                    channels: 0,
                    sdp_fmtp_line: h264_fmtp("640c1f"),
                    rtcp_feedback: vec![],
                },
                payload_type: 104,
            },
            RtpCodecKind::Video,
        )
        .map_err(|e| e.to_string())?;
    media_engine
        .register_codec(
            RTCRtpCodecParameters {
                rtp_codec: RTCRtpCodec {
                    mime_type: MIME_TYPE_RTX.to_owned(),
                    clock_rate: 90000,
                    channels: 0,
                    sdp_fmtp_line: "apt=104".to_owned(),
                    rtcp_feedback: vec![],
                },
                payload_type: 105,
            },
            RtpCodecKind::Video,
        )
        .map_err(|e| e.to_string())?;
    media_engine
        .register_codec(
            RTCRtpCodecParameters {
                rtp_codec: RTCRtpCodec {
                    mime_type: MIME_TYPE_OPUS.to_owned(),
                    clock_rate: 48000,
                    channels: 2,
                    sdp_fmtp_line: "minptime=10;useinbandfec=1".to_owned(),
                    rtcp_feedback: vec![],
                },
                payload_type: 111,
            },
            RtpCodecKind::Audio,
        )
        .map_err(|e| e.to_string())?;

    // REMB 带宽反馈不挂 interceptor:webrtc-rs 0.20.3 的 `bind_remote_stream`
    // 实际不会被调用(set_remote_description 时 track codec 是
    // Default::default(),deferred bind 又按映射后的 payload type 匹配,
    // 与 RTP 包携带的 offer pt 对不上),interceptor 拿不到远端 SSRC。
    // 改为在媒体循环里直接通过 `receiver.write_rtcp` 注入 REMB(见
    // `SessionCtx::maybe_send_remb`),与 PLI 同一条路径。
    let registry = configure_nack(Registry::new(), &mut media_engine);
    let registry = configure_rtcp_reports(registry);

    let mut pc = RTCPeerConnectionBuilder::new()
        .with_configuration(RTCConfigurationBuilder::new().build())
        .with_media_engine(media_engine)
        .with_interceptor_registry(registry)
        .build()
        .map_err(|e| e.to_string())?;

    let candidate = CandidateHostConfig {
        base_config: CandidateConfig {
            network: "udp".to_owned(),
            address: host_ip.to_owned(),
            port: udp_port,
            component: 1,
            ..Default::default()
        },
        ..Default::default()
    }
    .new_candidate_host()
    .map_err(|e| e.to_string())?;
    // Register the candidate with the ICE checklist; `create_answer`
    // marshals it into the answer SDP itself (in valid RFC 4566 order).
    let init = RTCIceCandidate::from(&candidate)
        .to_json()
        .map_err(|e| e.to_string())?;
    pc.add_local_candidate(init).map_err(|e| e.to_string())?;

    Ok(pc)
}

/// Answer the offer, then drive the connection until the session stops or
/// fails. The answer SDP is published before the event loop runs (the phone
/// polls for it while ICE proceeds), and the loop below runs until `stop`
/// is set or the connection fails.
///
/// Eight parameters is a lot, but every one is a piece of the single-threaded
/// event loop's context (signaling + media socket + media sinks + stop flag);
/// the peer connection's anonymous interceptor type keeps this all in one
/// function.
#[allow(clippy::too_many_arguments)]
fn run_negotiation(
    signaling: &Signaling,
    offer_sdp: &str,
    host_ip: &str,
    udp: &UdpSocket,
    udp_port: u16,
    video: &Arc<dyn VideoSink>,
    audio: &Arc<dyn AudioSink>,
    stop: &AtomicBool,
    frame_size: &Arc<Mutex<Option<(u32, u32)>>>,
) -> Result<(), String>
{
    let mut pc = build_peer(host_ip, udp_port)?;
    pc.set_remote_description(
        RTCSessionDescription::offer(offer_sdp.to_owned()).map_err(|e| format!("offer: {e}"))?,
    )
    .map_err(|e| format!("set_remote_description: {e}"))?;
    let answer = pc
        .create_answer(None)
        .map_err(|e| format!("create_answer: {e}"))?;
    pc.set_local_description(answer.clone())
        .map_err(|e| format!("set_local_description: {e}"))?;
    // 诊断:协商出的 answer 里是否带 transport-cc(rtcp-fb / extmap)。
    // 若带,Chrome 会启用 TWCC 并忽略 REMB,收不到 feedback → 码率清零。
    log::info!("phone answer sdp:\n{}", answer.sdp);
    // The answer's SDP already carries the host candidate — `create_answer`
    // marshals the locally added candidate (a=candidate ... typ host) into
    // each media section, in valid RFC 4566 order (after c=, before the next
    // m=). We must NOT splice the candidate in manually: injecting an
    // a= line directly after m= (before c=) is a media-description order
    // violation that Safari's SDP parser rejects with "Invalid SDP line".
    signaling.set_answer(answer.sdp);

    let mut ctx = SessionCtx::new(audio.clone());
    let mut buf = [0u8; 2048];
    let local_addr = udp.local_addr().map_err(|e| format!("local_addr: {e}"))?;

    // Two watchdogs, because the peer connection's own failure takes its
    // time: an ICE agent calls a silent peer failed only after its 5 s
    // disconnected + 25 s failed timeouts, and until then the session sits
    // `Connected` — the pairing code hidden, the one-phone gate closed.
    // Both watch the same cheap signals the loop already has at hand.
    //
    // * Pairing: a taken offer that never reaches a connection — a phone
    //   that posted its offer and died before the answer, most often —
    //   frees the session after 10 s instead of 30+.
    // * Media silence: a connection over which nothing arrives for 8 s —
    //   the phone's page stopped without telling anyone (its stop button
    //   only closes its own peer connection), a locked screen that killed
    //   it, a phone that left Wi-Fi. A live stream is packets every few
    //   milliseconds and consent checks every 5 s, so 8 s of *nothing* is
    //   a phone that is gone. The session ends as the phone's own stop
    //   does — the status mirror goes back to waiting, the code returns.
    const PAIRING_TIMEOUT: Duration = Duration::from_secs(10);
    const MEDIA_SILENCE: Duration = Duration::from_secs(8);
    let started = Instant::now();
    let mut connected_at: Option<Instant> = None;
    let mut last_packet = Instant::now();

    loop {
        if stop.load(Ordering::Acquire) {
            return Ok(());
        }
        if signaling.is_failed() || signaling.is_retired() {
            return Err("stopped by phone".to_string());
        }
        match connected_at {
            None if started.elapsed() > PAIRING_TIMEOUT => {
                return Err("the phone never completed the connection".to_string());
            }
            Some(_) if last_packet.elapsed() > MEDIA_SILENCE => {
                return Err("the phone stopped sending".to_string());
            }
            _ => {}
        }
        // (Signaling runs on its own accept thread — nothing to poll here.)

        // Outbound (ICE / STUN / RTCP / SRTP data) → the phone's address.
        while let Some(message) = pc.poll_write() {
            udp.send_to(&message.message, message.transport.peer_addr)
                .map_err(|e| format!("send_to: {e}"))?;
        }

        while let Some(event) = pc.poll_event() {
            if let RTCPeerConnectionEvent::OnTrack(track_event) = &event {
                let RTCTrackEvent::OnOpen(init) = track_event else { continue };
                // Learn the negotiated payload types from the receiver,
                // then wire the decoders. The codecs come back with *local*
                // payload types (mapped at negotiation), which is what the
                // RTP packets carry.
                let mut codecs: Vec<(u8, String)> = Vec::new();
                if let Some(mut receiver) = pc.rtp_receiver(init.receiver_id) {
                    for c in &receiver.get_parameters().rtp_parameters.codecs {
                        codecs.push((c.payload_type, c.rtp_codec.mime_type.clone()));
                    }
                }
                // Keep the receiver for RTCP feedback: a dropped or
                // undecodable frame needs a Picture Loss Indication to get a
                // fresh keyframe — without one the video freezes on the last
                // good frame.
                ctx.pli = Some((init.receiver_id, init.ssrc));
                for (pt, mime) in codecs {
                    if ctx.h264_pt.is_none() && mime.eq_ignore_ascii_case("video/H264") {
                        ctx.h264_pt = Some(pt);
                        ctx.decoder = Some(
                            create_decoder(video.clone(), frame_size.clone())
                                .map_err(|e| format!("video decoder: {e}"))?,
                        );
                    } else if ctx.opus_pt.is_none() && mime.eq_ignore_ascii_case("audio/opus") {
                        ctx.opus_pt = Some(pt);
                        ctx.opus = Some(
                            crate::decoders::MediaOpusDecoder::new()
                                .map_err(|e| format!("opus decoder: {e}"))?,
                        );
                    }
                }
                continue;
            }
            match event {
                RTCPeerConnectionEvent::OnConnectionStateChangeEvent(state) => match state {
                    RTCPeerConnectionState::Connected => {
                        signaling.set_connected();
                        // The media-silence watchdog starts from here: until
                        // this, silence is what a phone that is still
                        // gathering and negotiating looks like.
                        connected_at = Some(Instant::now());
                    }
                    RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed => {
                        return Err("peer connection failed".to_string());
                    }
                    _ => {}
                },
                // ICE failure surfaces through the connection state above;
                // per-component ICE detail is not fatal here.
                RTCPeerConnectionEvent::OnIceConnectionStateChangeEvent(_) => {}
                _ => {}
            }
        }

        while let Some(message) = pc.poll_read() {
            // RTCP is handled inside the peer connection (RTCP reports,
            // NACK) — only RTP media reaches here.
            if let RTCMessage::RtpPacket(_track, packet) = message {
                ctx.handle_rtp(&mut pc, &packet);
            }
        }

        // 周期注入 REMB 带宽反馈(write_rtcp 直接送进出站链,不经
        // interceptor 的 bind —— 见 build_peer 注释)。
        ctx.maybe_send_remb(&mut pc);

        // 每 5s 帧率统计：确认 phone 解码投递速率（发送侧已确认 30fps——
        // 若这里低于 30，掉帧在接收链路而非发送侧）。
        if ctx.stats_last.elapsed() >= Duration::from_secs(5) {
            let el = ctx.stats_last.elapsed().as_secs_f64();
            log::info!(
                "phone decode: {:.1} fps AU (5s window)",
                ctx.stats_au as f64 / el
            );
            ctx.stats_au = 0;
            ctx.stats_last = Instant::now();
        }

        // When the sans-I/O core schedules internal work (ICE timeouts,
        // RTCP scheduling), sleep only until then.
        let deadline = pc.poll_timeout();
        let wait = deadline.map_or(Duration::from_millis(100), |d| {
            d.saturating_duration_since(Instant::now())
                .min(Duration::from_millis(100))
        });
        if wait.is_zero() {
            pc.handle_timeout(Instant::now())
                .map_err(|e| format!("timeout: {e}"))?;
        } else {
            udp.set_read_timeout(Some(wait))
                .map_err(|e| format!("set_read_timeout: {e}"))?;
            match udp.recv_from(&mut buf) {
                Ok((n, peer)) => {
                    // Anything inbound counts as the phone being alive —
                    // consent checks as much as media. This is what the
                    // media-silence watchdog reads.
                    last_packet = Instant::now();
                    let mut message = BytesMut::with_capacity(n);
                    message.extend_from_slice(&buf[..n]);
                    pc.handle_read(TaggedBytesMut {
                        now: Instant::now(),
                        transport: TransportContext {
                            local_addr,
                            peer_addr: peer,
                            ecn: None,
                            transport_protocol: TransportProtocol::UDP,
                        },
                        message,
                    })
                    .map_err(|e| format!("handle_read: {e}"))?;
                }
                Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
                Err(e) => return Err(format!("recv_from: {e}")),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Per-connection media state: depacketize → decode → mailbox / ring
// ---------------------------------------------------------------------------

struct SessionCtx {
    h264_pt: Option<u8>,
    opus_pt: Option<u8>,
    depack: H264Depacketizer,
    decoder: Option<Box<dyn PhoneDecoder>>,
    opus: Option<crate::decoders::MediaOpusDecoder>,
    /// 交织立体声解码缓冲（decode_f32 直接写立体声）。
    /// 11520 = 120 ms of 48 kHz stereo (the max Opus frame) × 2 channels.
    opus_buf: [f32; 11_520],
    audio: Arc<dyn AudioSink>,
    /// The video receiver (for RTCP feedback) and its RTP SSRC, set when the
    /// track opens. The PLI's `media_ssrc` names the SSRC we want a keyframe
    /// for.
    pli: Option<(RTCRtpReceiverId, u32)>,
    /// Last time a PLI was sent — the phone reacts to one by emitting an IDR
    /// as fast as the encoder allows, and a burst of PLIs would cascade.
    last_pli: Instant,
    /// Last seen bad-data count from the decoder (see `PhoneDecoder::
    /// bad_frames`). When it moves, FFmpeg hardware decode failed (counted
    /// synchronously by `FfmpegDecoder`) — the picture is frozen on the
    /// last good frame until an IDR arrives.
    last_bad: u32,
    /// REMB 带宽反馈(见 `maybe_send_remb`):上次发送时刻、测速窗口起点、
    /// 窗口内收到的视频 RTP payload 字节数、最近一次实测接收速率(bps,
    /// 0 = 尚无数据)。
    remb_last_send: Instant,
    remb_window_start: Instant,
    remb_window_bytes: u64,
    remb_measured_bps: f32,
    // —— 每 5s 帧率统计（诊断：phone 到达/解码投递速率）——
    stats_last: Instant,
    stats_au: u64,
}

impl SessionCtx {
    fn new(audio: Arc<dyn AudioSink>) -> Self {
        Self {
            h264_pt: None,
            opus_pt: None,
            depack: H264Depacketizer::new(),
            decoder: None,
            opus: None,
            opus_buf: [0.0; 11_520],
            audio,
            pli: None,
            last_pli: Instant::now() - Duration::from_secs(1), // ready on first loss
            last_bad: 0,
            // 首条 REMB 在拿到视频 SSRC 后立刻发出(高带宽值让发送端 ramp)。
            remb_last_send: Instant::now() - Duration::from_millis(500),
            remb_window_start: Instant::now(),
            remb_window_bytes: 0,
            remb_measured_bps: 0.0,
            stats_last: Instant::now(),
            stats_au: 0,
        }
    }

    /// 周期性地向发送端反馈"接收端能收的带宽"(REMB)。Chrome 的带宽估计
    /// 器完全依赖它:没有 transport-cc 协商时收不到任何反馈,它就把码率
    /// 一路降到最低,编码器随之降分辨率/画质。
    ///
    /// REMB 的语义是"上限":发送端只会 ramp 到反馈值附近,永远不会超过
    /// 它。若反馈取实测接收速率,会形成一个闭环 —— 发送端按反馈输出,
    /// 实测 ≈ 输出,反馈再跟着实测走,码率锁死在初始小值(实测观察到
    /// ~1 Mbps)。所以这里固定反馈一个**高于所有档位编码上限**的值
    /// (页面端 maxBitrate 最大 6 Mbps):带宽不是瓶颈,Chrome 按
    /// maxBitrate 全速编码;真发生丢包时 loss-based 控制(NACK 丢包检测)
    /// 仍然会兜底降码率。实测值仅作诊断日志。
    fn maybe_send_remb<I: Interceptor>(&mut self, pc: &mut RTCPeerConnection<I>) {
        const REMB_INTERVAL: Duration = Duration::from_millis(500);
        // 上限。下限与余量：反馈 = 实测接收速率 × 1.5 —— 接收速率是链路
        // 还塞得下的证明，余量给增长空间；丢包让接收速率下降，反馈跟着
        // 下降，编码器让路（maintain-framerate 下先降分辨率），拥塞缓解，
        // 丢包停止。固定 10 Mbps 是这个环路的反面：无论链路多堵，手机
        // 端的估计器都被抬到天上，丢包时一个字节也不少发，NACK/重传把
        // 拥塞推成恶性循环——"一开始流畅、几十秒后变卡"正是它的形状。
        const REMB_CEILING: f32 = 10_000_000.0;
        const REMB_FLOOR: f32 = 500_000.0;
        const REMB_HEADROOM: f32 = 1.5;

        let now = Instant::now();
        if now.duration_since(self.remb_last_send) < REMB_INTERVAL {
            return;
        }
        self.remb_last_send = now;
        let Some((receiver_id, media_ssrc)) = self.pli else { return };
        let Some(mut receiver) = pc.rtp_receiver(receiver_id) else { return };
        let measured = self.remb_measured_bps;
        let bitrate = if measured > 0.0 {
            (measured * REMB_HEADROOM).clamp(REMB_FLOOR, REMB_CEILING)
        } else {
            // 首个 250ms 窗口还没满：上限起步，下一拍就落到实测值。
            REMB_CEILING
        };
        let remb = ReceiverEstimatedMaximumBitrate {
            sender_ssrc: 0,
            bitrate,
            ssrcs: vec![media_ssrc],
        };
        if let Err(e) = receiver.write_rtcp(vec![Box::new(remb)]) {
            log::warn!("phone remb: send failed: {e}");
        } else {
            // 诊断:REMB 是否真正送出、实测接收速率(RUST_LOG=info)。
            // log::info!(
            //     "phone remb: sent {REMB_BITRATE:.0} bps to ssrc {media_ssrc} (measured {:.0} bps)",
            //     self.remb_measured_bps
            // );
        }
    }

    /// Ask the phone for a fresh keyframe. After a dropped or undecodable
    /// frame every subsequent P-frame decodes to garbage and the picture
    /// freezes on the last good frame — a Picture Loss Indication gets the
    /// encoder to send an IDR. Throttled so a loss burst costs one PLI.
    fn request_keyframe<I: Interceptor>(&mut self, pc: &mut RTCPeerConnection<I>) {
        if self.last_pli.elapsed() < Duration::from_millis(500) {
            return;
        }
        self.last_pli = Instant::now();
        let Some((receiver_id, media_ssrc)) = self.pli else { return };
        let Some(mut receiver) = pc.rtp_receiver(receiver_id) else { return };
        let pli = PictureLossIndication { sender_ssrc: 0, media_ssrc };
        if let Err(e) = receiver.write_rtcp(vec![Box::new(pli)]) {
            log::warn!("phone h264 decode: PLI send failed: {e}");
        } else {
            // Diagnostic: the recovery path — the phone's encoder should
            // answer with an IDR. If this never appears while frames are
            // dropped, the PLI never reached the wire (or the receiver
            // handles it silently).
            static SENT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
            if !SENT.swap(true, std::sync::atomic::Ordering::Relaxed) {
                log::info!("phone h264: PLI sent (media_ssrc={media_ssrc}) — waiting for a keyframe");
            }
        }
    }

    fn handle_rtp<I: Interceptor>(&mut self, pc: &mut RTCPeerConnection<I>, packet: &RtpPacket) {
        let pt = packet.header.payload_type;
        // Diagnostic: RTP arrives before the negotiated payload types are
        // known (OnTrack needs the first packet — one round-trip of
        // staleness is expected, and harmless: keyframes repeat). A *steady*
        // stream of these means the PT mapping is off.
        if self.h264_pt.is_none() && self.opus_pt.is_none() {
            static REPORTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
            if !REPORTED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                log::warn!("phone rtp: packets before track open, pt={pt}");
            }
        }
        if self.h264_pt == Some(pt) {
            // 接收速率测量:250ms 窗口累计视频 RTP payload 字节,与当前值
            // 做轻量平滑(新值 30% 权重),供 `maybe_send_remb` 折算带宽。
            self.remb_window_bytes += packet.payload.len() as u64;
            let elapsed = Instant::now().duration_since(self.remb_window_start);
            if elapsed >= Duration::from_millis(250) {
                let rate = self.remb_window_bytes as f32 * 8.0 / elapsed.as_secs_f32();
                self.remb_measured_bps = if self.remb_measured_bps > 0.0 {
                    0.7 * self.remb_measured_bps + 0.3 * rate
                } else {
                    rate
                };
                self.remb_window_start = Instant::now();
                self.remb_window_bytes = 0;
            }
            // FFmpeg 硬件解码失败（receive / transfer 错误）不会让 push
            // 返回 Err——`FfmpegDecoder` 在 push 内同步检测并计入
            // bad_frames。bad_frames 移动时画面停在最后一帧好帧上：
            // 请求 keyframe 直到 IDR 到达才恢复（hw_failed() 内部已 flush
            // 并置 skip-until-IDR 门）。
            let bad = self.decoder.as_ref().map(|d| d.bad_frames()).unwrap_or(0);
            if bad != self.last_bad {
                self.last_bad = bad;
                log::warn!("phone h264 decode: hw decode failed {bad} time(s) — resetting decoder, requesting keyframe");
                if let Some(decoder) = &mut self.decoder {
                    // 硬件失败后解码器已停在 IDR 门上（hw_failed 的 flush +
                    // skip 或重建软件解码器）；reset 再 flush 一次是幂等
                    // 的，配合上面的 keyframe 请求在下一个 IDR 恢复画面。
                    decoder.reset();
                }
                self.request_keyframe(pc);
            }
            match self.depack.push(&packet.payload, packet.header.marker, packet.header.sequence_number) {
                // One complete Annex-B access unit: decode it.
                DepackOutcome::Frame(au) => {
                    if let Some(decoder) = &mut self.decoder
                        && let Err(e) = decoder.push(&au, packet.header.timestamp)
                    {
                        // Bad bitstream (e.g. lost NALs): the decoder is
                        // stuck until a keyframe arrives. 立即重置并挂上
                        // IDR 门——继续喂只会一帧一个错误（参考链断了的
                        // P 帧解不出 gap），每次都是一次注定失败的解码、
                        // 一条日志、一次 keyframe 请求。重置后只等 IDR，
                        // 日志一条，CPU 一拍都不浪费。
                        log::warn!("phone h264 decode: {e} — resetting, waiting for keyframe");
                        decoder.reset();
                        self.request_keyframe(pc);
                    } else {
                        // 解码投递速率统计：完整 AU 成功进解码器（h264_pt
                        // 与 decoder 在 OnTrack 同批建立，此处 decoder 必在）。
                        self.stats_au += 1;
                    }
                }
                // Corrupt frame (packet loss): dropped at the marker. The
                // decoder still has the *previous* frame's reference, so
                // only a keyframe can recover.
                DepackOutcome::Dropped => {
                    // Diagnostic: loss events are rare on a LAN. Reported at
                    // most once per five seconds — a second line means the
                    // link is *still* dropping packets, and each dropped
                    // frame costs a keyframe round-trip (a visible stutter
                    // while the picture waits for the IDR).
                    static LAST_REPORTED: std::sync::atomic::AtomicU64 =
                        std::sync::atomic::AtomicU64::new(0);
                    let now_ms = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as u64)
                        .unwrap_or(0);
                    let last = LAST_REPORTED.load(std::sync::atomic::Ordering::Relaxed);
                    if now_ms.saturating_sub(last) > 5_000 {
                        LAST_REPORTED.store(now_ms, std::sync::atomic::Ordering::Relaxed);
                        log::warn!("phone rtp: dropped a corrupt/out-of-order frame (packet loss?) — requesting keyframe");
                    }
                    // The loss broke the reference chain: skip P-frames
                    // until the recovery keyframe so the picture freezes
                    // briefly instead of decoding to artifacts.
                    if let Some(decoder) = &mut self.decoder {
                        decoder.skip_until_idr();
                    }
                    self.request_keyframe(pc);
                }
                DepackOutcome::None => {}
            }
        } else if self.opus_pt == Some(pt)
            && let Some(decoder) = &mut self.opus
        {
            match decoder.decode_f32(&packet.payload, &mut self.opus_buf) {
                Ok(n) if n > 0 => self.audio.push(&self.opus_buf[..n * 2]),
                _ => {}
            }
        }
    }
}

// ---------------------------------------------------------------------------
// HTTPS signaling server (rustls + a hand-written HTTP/1.1 loop) — handlers
// never block: they only flip shared state.
// ---------------------------------------------------------------------------

/// Minimal TLS HTTP server: one non-blocking accept thread, one thread per
/// connection. Signaling traffic is tiny and rare (the phone polls at
/// ~7 Hz during negotiation), so this model is both simple and cheap. The
/// stop flag is the [`Shared`] one — the server stops when the last session
/// unregisters, which is where the drop below joins the accept thread.
struct HttpsServer {
    join: Option<JoinHandle<()>>,
}

impl Drop for HttpsServer {
    fn drop(&mut self) {
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl HttpsServer {
    fn start(shared: Arc<Shared>) -> Result<Self, String> {
        let listener = TcpListener::bind(("0.0.0.0", SIGNALING_PORT))
            .map_err(|e| format!("bind :{SIGNALING_PORT}: {e}"))?;
        listener.set_nonblocking(true).map_err(|e| e.to_string())?;
        let tls = build_tls_config()?;
        let join = {
            std::thread::Builder::new()
                .name("phone-signaling".to_string())
                .spawn(move || loop {
                    if shared.stop.load(Ordering::Acquire) {
                        return;
                    }
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let tls = tls.clone();
                            let shared = Arc::clone(&shared);
                            // Connections live one request round-trip —
                            // a fresh thread each is fine.
                            let _ = std::thread::Builder::new()
                                .name("phone-signal-conn".to_string())
                                .spawn(move || {
                                    if let Err(e) = handle_connection(stream, tls, &shared) {
                                        log::debug!("phone signaling connection: {e}");
                                    }
                                });
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        Err(e) => {
                            log::error!("phone signaling accept: {e}");
                            return;
                        }
                    }
                })
                .map_err(|e| format!("phone signaling thread: {e}"))?
        };
        Ok(Self { join: Some(join) })
    }
}

/// A self-signed certificate for the signaling TLS. The phone scans
/// `https://<ip>:48210/`, so the SANs cover the desktop's LAN IP (default
/// route — the same IP the QR code embeds) plus localhost for desktop-side
/// testing. Ephemeral: regenerated at every launch, never written to disk.
fn build_tls_config() -> Result<Arc<ServerConfig>, String> {
    let mut sans = vec![
        rcgen::SanType::IpAddress(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
        // rcgen 0.13 wants `Ia5String` for DNS SANs.
        rcgen::SanType::DnsName("localhost".try_into().map_err(|e| format!("san: {e}"))?),
    ];
    if let Some(ip) = default_route_ip()
        && let Ok(addr) = ip.parse::<IpAddr>()
    {
        sans.push(rcgen::SanType::IpAddress(addr));
    }
    let mut params = CertificateParams::new(Vec::<String>::new()).map_err(|e| e.to_string())?;
    params.subject_alt_names = sans;
    params.distinguished_name = DistinguishedName::new();
    params
        .distinguished_name
        .push(DnType::CommonName, "Filmcraft Phone Camera");
    params.is_ca = IsCa::NoCa;
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let key = KeyPair::generate().map_err(|e| e.to_string())?;
    let cert = params.self_signed(&key).map_err(|e| e.to_string())?;
    let config = ServerConfig::builder_with_provider(Arc::new(ring::default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.der().clone()],
            PrivateKeyDer::Pkcs8(key.serialize_der().into()),
        )
        .map_err(|e| e.to_string())?;
    Ok(Arc::new(config))
}

/// Serve one TLS connection: read the request head + body, dispatch, write
/// the response, close. A 15 s read timeout keeps dead connections from
/// pinning a thread forever.
fn handle_connection(
    stream: TcpStream,
    tls: Arc<ServerConfig>,
    shared: &Shared,
) -> std::io::Result<()> {
    // The listener is non-blocking (accept loop polls `stop`), and on Unix
    // accepted sockets inherit O_NONBLOCK — restore blocking mode or every
    // read here would fail instantly with EAGAIN and the TLS handshake
    // would never complete.
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(15))).ok();
    let conn = ServerConnection::new(tls).map_err(|e| std::io::Error::other(e.to_string()))?;
    let mut stream = StreamOwned::new(conn, stream);

    // Request head, plus any body bytes that arrived in the same segment.
    let mut head = Vec::with_capacity(4096);
    let mut buf = [0u8; 4096];
    loop {
        let n = stream.read(&mut buf)?;
        if n == 0 {
            return Ok(());
        }
        head.extend_from_slice(&buf[..n]);
        if head.len() > 16 * 1024 {
            return Ok(()); // oversized request: drop it
        }
        if head.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    let head_end = head.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
    let (method, path, host, content_length) = match parse_head(&head[..head_end]) {
        Some(v) => v,
        None => return Ok(()),
    };

    // Body: whatever already arrived plus the remainder, bounded by the
    // declared Content-Length.
    let mut body = head[head_end..].to_vec();
    while body.len() < content_length {
        let mut chunk = vec![0u8; content_length - body.len()];
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Ok(());
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(content_length);

    let (status, content_type, response) = route_request(method, path, host, &body, shared);
    let reason = match status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        404 => "Not Found",
        409 => "Conflict",
        _ => "Status",
    };
    let header = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        response.len()
    );
    stream.write_all(header.as_bytes())?;
    stream.write_all(&response)?;
    // Graceful TLS close: browsers tolerate an abrupt FIN, but a clean
    // close_notify lets well-behaved clients read to a clean EOF.
    stream.conn.send_close_notify();
    stream.flush()?;
    Ok(())
}

/// A request's essentials. `None` for anything unparsable — the connection
/// is dropped, which is fine for a phone page.
fn parse_head(head: &[u8]) -> Option<(&str, &str, &str, usize)> {
    let head = std::str::from_utf8(head).ok()?;
    let mut lines = head.split("\r\n");
    let mut parts = lines.next()?.split_whitespace();
    let method = parts.next()?;
    let path = parts.next()?.split('?').next()?; // drop the query string
    let mut host = "";
    let mut content_length = 0usize;
    for line in lines {
        if line.is_empty() {
            break;
        }
        let Some((k, v)) = line.split_once(':') else { continue };
        let v = v.trim();
        if k.eq_ignore_ascii_case("content-length") {
            content_length = v.parse().unwrap_or(0);
        } else if k.eq_ignore_ascii_case("host") {
            host = v;
        }
    }
    Some((method, path, host, content_length))
}

/// Which session a request is for, then [`handle_request`]. The offer names
/// no session: it goes to the newest waiting one — the Source the user added
/// last, whose code they scanned — and everything the phone asks after the
/// offer (`/status`, `/answer`, `/stop`) lands on the session that accepted
/// it. One phone at a time holds for the whole process, as it did when the
/// one server was one session's: a session that is mid-pairing refuses a
/// second phone, and a session that is **connected is displaced by a newer
/// scan** — the phone that re-scans (a fresh page, a stopped stream, a
/// second try after the first one wedged) replaces the stream instead of
/// being told the session is busy, which is also what retires a connection
/// whose phone walked away without telling anyone. With no session on the
/// air at all, a throwaway idle one answers: the page, a `waiting` status,
/// an `answer not ready` — the same words a waiting session would say.
fn route_request(
    method: &str,
    path: &str,
    host: &str,
    body: &[u8],
    shared: &Shared,
) -> (u16, &'static str, Vec<u8>) {
    if (method, path) == ("POST", "/offer") {
        let mut router = shared.router.lock().unwrap();
        if let Some(on_the_air) = router.active.as_ref().and_then(|weak| weak.upgrade()) {
            // The state is read out and the lock dropped before acting: the
            // connected arm retires the session, which takes this very lock,
            // and a guard carried into the match would deadlock against it.
            let on_the_air_state = on_the_air.inner.lock().unwrap().state.clone();
            match on_the_air_state {
                // A phone is actively pairing — the same refusal one server
                // per session gave, and for the same reason: the scan that
                // is under way is the one that owns the port.
                SignalState::OfferReady { .. } | SignalState::AnswerReady { .. } => {
                    return (409, "text/plain; charset=utf-8", b"session busy".to_vec());
                }
                // A live connection stands down for a newer scan: the same
                // words the phone's own stop uses, so the negotiation winds
                // down the same way — and a pending offer this router routed
                // to the same session outranks the teardown (see
                // `set_failed_keep_offer`).
                SignalState::Connected { .. } => on_the_air.retire(),
                _ => {}
            }
        }
        // The scan goes to a waiting session the host expects — a Source of
        // the Scene being shown, whose QR the user can be looking at — and
        // only falls back to the newest waiting session anywhere.
        let candidates = pairing_candidates();
        let target = router
            .sessions
            .iter()
            .find_map(|(id, weak)| {
                weak.upgrade()
                    .filter(|signaling| signaling.waiting() && candidates.contains(id))
            })
            .or_else(|| {
                router
                    .sessions
                    .iter()
                    .find_map(|(_, weak)| weak.upgrade().filter(|signaling| signaling.waiting()))
            });
        let Some(signaling) = target else {
            return (409, "text/plain; charset=utf-8", b"session busy".to_vec());
        };
        let answered = handle_request(method, path, host, body, &signaling);
        if answered.0 == 202 {
            // The phone is this session's now; its follow-ups follow it.
            router.active = Some(Arc::downgrade(&signaling));
        }
        return answered;
    }
    let active = shared
        .router
        .lock()
        .unwrap()
        .active
        .as_ref()
        .and_then(|weak| weak.upgrade());
    match active {
        Some(signaling) => handle_request(method, path, host, body, &signaling),
        None => handle_request(method, path, host, body, &Signaling::default()),
    }
}

/// Route one request. Never blocks: it only flips shared state.
#[allow(clippy::needless_return)] // match arms returning tuples read clearer
fn handle_request(
    method: &str,
    path: &str,
    host: &str,
    body: &[u8],
    signaling: &Signaling,
) -> (u16, &'static str, Vec<u8>) {
    match (method, path) {
        ("GET", "/") => (200, "text/html; charset=utf-8", PHONE_PAGE.as_bytes().to_vec()),
        ("GET", "/status") => (
            200,
            "application/json",
            signaling.status_json().to_string().into_bytes(),
        ),
        ("GET", "/answer") => match signaling.answer() {
            Some(answer) => (
                200,
                "application/json",
                serde_json::to_string(&answer).unwrap_or_default().into_bytes(),
            ),
            None => (409, "text/plain; charset=utf-8", b"answer not ready".to_vec()),
        },
        ("POST", "/offer") => match serde_json::from_slice::<RTCSessionDescription>(body) {
            Ok(desc) if desc.sdp_type == RTCSdpType::Offer => {
                // The phone connected to us, so the Host header address is
                // one it can reach — the candidate IP.
                let host_ip = host_ip_from(host).unwrap_or_default();
                if signaling.set_offer(desc.sdp, host_ip) {
                    (202, "text/plain; charset=utf-8", b"accepted".to_vec())
                } else {
                    (409, "text/plain; charset=utf-8", b"session busy".to_vec())
                }
            }
            _ => (400, "text/plain; charset=utf-8", b"bad offer".to_vec()),
        },
        ("POST", "/stop") => {
            signaling.set_failed("stopped by phone".to_string());
            // The phone left on purpose — not an error: back to waiting for
            // the next scan (no failure toast).
            *STATUS.lock().unwrap() = SessionStatus::Waiting;
            (200, "text/plain; charset=utf-8", b"ok".to_vec())
        }
        _ => (404, "text/plain; charset=utf-8", b"not found".to_vec()),
    }
}

/// The desktop IP the phone dialed (from the Host header) — guaranteed
/// reachable, unlike any NIC enumeration.
fn host_ip_from(host: &str) -> Option<String> {
    let host = host.trim();
    if let Some(rest) = host.strip_prefix('[') {
        // "[::1]:48210"
        let ip = rest.split(']').next()?;
        return ip.parse::<IpAddr>().ok().map(|a| a.to_string());
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Some(ip.to_string());
    }
    // "192.168.1.5:48210"
    let host = host.rsplit_once(':').map(|(h, _)| h).unwrap_or(host);
    host.parse::<IpAddr>().ok().map(|a| a.to_string())
}

/// The default route's source IP — a host candidate the phone can usually
/// reach. UDP `connect` sends nothing; it only selects the route. Also used
/// by the studio, which builds the QR code the phone scans from it.
pub fn default_route_ip() -> Option<String> {
    let sock = UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect("8.8.8.8:80").ok()?;
    sock.local_addr().ok().map(|a| a.ip().to_string())
}

/// The URL the studio's QR code carries: the phone page on this machine's
/// LAN address. `None` where there is no route out — no network up, so no
/// address a phone could reach even if it had the code.
///
/// Memoized for a second. The route only moves when the network does, and
/// the studio's properties dock asks for this every frame — each ask is a
/// socket opened and closed otherwise, which is a per-frame syscall for an
/// answer that cannot change under it.
pub fn pairing_url() -> Option<String> {
    static CACHE: std::sync::Mutex<Option<(Instant, Option<String>)>> =
        std::sync::Mutex::new(None);
    let mut cache = CACHE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    match &*cache {
        Some((at, url)) if at.elapsed() < Duration::from_secs(1) => url.clone(),
        _ => {
            let url = default_route_ip().map(|ip| format!("https://{ip}:{SIGNALING_PORT}/"));
            *cache = Some((Instant::now(), url.clone()));
            url
        }
    }
}

/// Insert the host candidate line after every media section header. The
/// `sdp` crate inside rtc regenerates attribute lines on parse, so this is
/// the one place we must patch by hand (add_local_candidate doesn't rewrite
/// the SDP).
#[cfg(test)]
mod tests {
    use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
    use rustls::client::ClientConnection;
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
    use rustls::{ClientConfig, DigitallySignedStruct, SignatureScheme};

    use super::*;

    /// Accepts any certificate (the test only exercises the TLS + HTTP path).
    #[derive(Debug)]
    struct NoVerify;

    impl ServerCertVerifier for NoVerify {
        fn verify_server_cert(
            &self,
            _end_entity: &CertificateDer<'_>,
            _intermediates: &[CertificateDer<'_>],
            _server_name: &ServerName<'_>,
            _ocsp_response: &[u8],
            _now: UnixTime,
        ) -> Result<ServerCertVerified, rustls::Error> {
            Ok(ServerCertVerified::assertion())
        }
        fn verify_tls12_signature(
            &self,
            _message: &[u8],
            _cert: &CertificateDer<'_>,
            _dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            Ok(HandshakeSignatureValid::assertion())
        }
        fn verify_tls13_signature(
            &self,
            _message: &[u8],
            _cert: &CertificateDer<'_>,
            _dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            Ok(HandshakeSignatureValid::assertion())
        }
        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            vec![
                SignatureScheme::ECDSA_NISTP256_SHA256,
                SignatureScheme::ED25519,
                SignatureScheme::RSA_PSS_SHA256,
            ]
        }
    }

    /// The full path a phone takes: TCP → TLS handshake → GET / → 200 + page.
    /// Requires :48210 to be free (a `bind` failure here means the port is
    /// held by a stale process — exactly the failure the phone would see as
    /// "This site can't be reached").
    #[test]
    fn https_server_serves_the_page_end_to_end() {
        let _ = env_logger::builder()
            .filter_level(log::LevelFilter::Trace)
            .is_test(true)
            .try_init();
        let shared = Shared::acquire().expect("signaling server must bind :48210 for this test");
        let signaling = Arc::new(Signaling::default());
        shared.register("first", &signaling);

        let config = ClientConfig::builder_with_provider(Arc::new(ring::default_provider()))
            .with_safe_default_protocol_versions()
            .expect("tls versions")
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerify))
            .with_no_client_auth();
        let name = ServerName::try_from("localhost").expect("server name");
        let tcp = TcpStream::connect(("127.0.0.1", SIGNALING_PORT)).expect("tcp connect");
        let conn =
            ClientConnection::new(Arc::new(config), name).expect("tls client connection");
        let mut stream = StreamOwned::new(conn, tcp);
        stream
            .write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1:48210\r\nConnection: close\r\n\r\n")
            .expect("write request");
        let mut response = Vec::new();
        stream.read_to_end(&mut response).expect("read response");
        let response = String::from_utf8_lossy(&response);
        assert!(response.contains("200 OK"), "status line: {response:?}");
        assert!(response.contains("getUserMedia"), "page body: {response:?}");
        // The last session out stops the listener, so a later test (or a
        // re-run of this one) binds a fresh server.
        shared.unregister(&signaling);
    }

    #[test]
    fn tls_config_builds_a_working_server_connection() {
        // rcgen → rustls 集成：自签证书生成、SAN 注入、ServerConfig 可用。
        let config = build_tls_config().expect("self-signed cert + config");
        let conn = ServerConnection::new(config).expect("TLS server connection");
        drop(conn);
    }

    #[test]
    fn parse_head_extracts_method_path_host_and_length() {
        let head =
            b"POST /offer?res=480 HTTP/1.1\r\nHost: 192.168.1.5:48210\r\nContent-Length: 4\r\n\r\n";
        let (method, path, host, length) = parse_head(head).expect("parses");
        assert_eq!(method, "POST");
        assert_eq!(path, "/offer"); // query string dropped
        assert_eq!(host, "192.168.1.5:48210");
        assert_eq!(length, 4);
    }

    #[test]
    fn root_serves_the_phone_page() {
        let sig = Signaling::default();
        let (status, ct, body) = handle_request("GET", "/", "", b"", &sig);
        assert_eq!(status, 200);
        assert!(ct.starts_with("text/html"));
        assert!(String::from_utf8_lossy(&body).contains("getUserMedia"));
    }

    #[test]
    fn status_reflects_the_waiting_state() {
        let sig = Signaling::default();
        let (status, ct, body) = handle_request("GET", "/status", "", b"", &sig);
        assert_eq!(status, 200);
        assert_eq!(ct, "application/json");
        assert!(String::from_utf8_lossy(&body).contains("waiting"));
    }

    /// Serializes the tests that write the process-wide `STATUS`/`FAILED`
    /// (the offer/stop handlers flip them; the state tests assert them).
    static STATUS_LOCK: Mutex<()> = Mutex::new(());

    /// The state tests are order-independent: each starts by resetting the
    /// process-wide session state (a peer test may have left it Connecting)
    /// and the pairing candidates (a peer test may have named its own
    /// sessions).
    fn reset_session_state() {
        *STATUS.lock().unwrap() = SessionStatus::Waiting;
        *FAILED.lock().unwrap() = None;
        set_pairing_candidates(&[]);
    }

    fn minimal_sdp() -> String {
        "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\n\
         m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n"
            .to_string()
    }

    #[test]
    fn session_status_tracks_negotiation_to_connected() {
        let _g = STATUS_LOCK.lock().unwrap();
        reset_session_state();
        let sig = Signaling::default();
        assert_eq!(session_status(), SessionStatus::Waiting);

        let sdp = minimal_sdp();
        assert!(sig.set_offer(sdp.clone(), "192.168.1.5".into()));
        assert_eq!(session_status(), SessionStatus::Connecting);

        sig.set_answer(sdp);
        sig.set_connected();
        assert_eq!(session_status(), SessionStatus::Connected);
        assert!(take_connected(), "the connected edge fired");
        assert!(take_failed().is_none(), "no failure so far");
    }

    #[test]
    fn a_failed_session_reports_once_and_a_new_scan_resets() {
        let _g = STATUS_LOCK.lock().unwrap();
        reset_session_state();
        let sig = Signaling::default();

        report_failure("ICE failed".into());
        assert!(
            matches!(session_status(), SessionStatus::Failed(reason) if reason == "ICE failed"),
            "the status carries the reason"
        );
        assert_eq!(take_failed().as_deref(), Some("ICE failed"));
        assert_eq!(take_failed(), None, "the failure edge fires once");

        // A new scan leaves the failed state; the phone leaving on purpose
        // is not an error (no failure edge, back to waiting).
        assert!(sig.set_offer(minimal_sdp(), "".into()));
        assert_eq!(session_status(), SessionStatus::Connecting);
        let (status, _, _) = handle_request("POST", "/stop", "", b"", &sig);
        assert_eq!(status, 200);
        assert_eq!(session_status(), SessionStatus::Waiting);
        assert!(take_failed().is_none(), "a phone leaving is not an error");
    }

    #[test]
    fn offer_accepts_then_busy() {
        let _g = STATUS_LOCK.lock().unwrap();
        let sig = Signaling::default();
        // A minimal but syntactically valid SDP (the handler only parses it).
        let sdp = "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\n\
                   m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n";
        let offer =
            serde_json::to_vec(&RTCSessionDescription::offer(sdp.to_owned()).expect("offer"))
                .expect("json");
        let host = "192.168.1.5:48210";
        let (status, _, _) = handle_request("POST", "/offer", host, &offer, &sig);
        assert_eq!(status, 202);
        // A live session rejects the next phone with 409.
        let (status, _, _) = handle_request("POST", "/offer", host, &offer, &sig);
        assert_eq!(status, 409);
        // The Host header became the host candidate IP.
        assert!(
            matches!(&sig.inner.lock().unwrap().state, SignalState::OfferReady { host_ip, .. }
                if host_ip == "192.168.1.5")
        );
    }

    /// The router is what makes a second phone camera Source — in another
    /// Scene, say — the one a scan pairs with: the offer goes to the most
    /// recently started waiting session, and the phone's follow-up requests
    /// follow it. A session that leaves hands the phone to whoever waits.
    #[test]
    fn an_offer_goes_to_the_newest_waiting_session() {
        let _g = STATUS_LOCK.lock().unwrap();
        reset_session_state();
        let shared = Arc::new(Shared {
            router: Mutex::new(Router::default()),
            server: Mutex::new(None),
            stop: Arc::new(AtomicBool::new(false)),
        });
        let first = Arc::new(Signaling::default());
        let second = Arc::new(Signaling::default());
        shared.register("first", &first);
        shared.register("second", &second);

        let offer = serde_json::to_vec(&RTCSessionDescription::offer(minimal_sdp()).expect("offer"))
            .expect("json");
        let (status, _, _) = route_request("POST", "/offer", "192.168.1.5:48210", &offer, &shared);
        assert_eq!(status, 202, "the newest waiting session takes the phone");
        assert!(first.waiting(), "the earlier session keeps waiting");
        assert!(!second.waiting(), "the newest session took the offer");

        // The phone's follow-ups land on the session that accepted it —
        // /status reads its state, not some other session's.
        let (status, _, body) = route_request("GET", "/status", "", b"", &shared);
        assert_eq!(status, 200);
        assert!(
            String::from_utf8_lossy(&body).contains("offer-received"),
            "the follow-up reached the newest session: {}",
            String::from_utf8_lossy(&body)
        );

        // One phone at a time holds for the whole process: while the session
        // on the air holds a phone, a second one is refused even though
        // another session is still waiting.
        let (status, _, _) = route_request("POST", "/offer", "192.168.1.5:48210", &offer, &shared);
        assert_eq!(status, 409, "a second phone waits for the first to leave");
        assert!(first.waiting(), "the refused offer reaches no session");

        // It leaves: the next scan pairs with the one that is left.
        shared.unregister(&second);
        let (status, _, _) = route_request("POST", "/offer", "192.168.1.5:48210", &offer, &shared);
        assert_eq!(status, 202, "the remaining session is next");
        assert!(!first.waiting());
    }

    /// A scan pairs with a waiting session of the Scene being shown — the
    /// host's pairing candidates — even when a session from another Scene
    /// registered more recently and is just as waiting. "Newest" alone would
    /// re-pair a scan with the session the phone just left, and the picture
    /// would land in the Scene it was streamed from instead of the one the
    /// user is looking at.
    #[test]
    fn the_scene_being_shown_wins_over_recency() {
        let _g = STATUS_LOCK.lock().unwrap();
        reset_session_state();
        let shared = Arc::new(Shared {
            router: Mutex::new(Router::default()),
            server: Mutex::new(None),
            stop: Arc::new(AtomicBool::new(false)),
        });
        let scene_two = Arc::new(Signaling::default());
        let scene_one = Arc::new(Signaling::default());
        // The scene-two Source opened first, the scene-one one second — the
        // newest registration is the scene the user just left.
        shared.register("scene-two", &scene_two);
        shared.register("scene-one", &scene_one);
        // The host says the Scene being shown holds the scene-two Source.
        set_pairing_candidates(&["scene-two".to_owned()]);

        let offer = serde_json::to_vec(&RTCSessionDescription::offer(minimal_sdp()).expect("offer"))
            .expect("json");
        let (status, _, _) = route_request("POST", "/offer", "192.168.1.5:48210", &offer, &shared);
        assert_eq!(status, 202);
        assert!(
            matches!(scene_two.inner.lock().unwrap().state, SignalState::OfferReady { .. }),
            "the shown scene's older session takes the scan"
        );
        assert!(scene_one.waiting(), "the other scene's session is untouched");

        // No candidate matches — a Scene with no phone source of its own —
        // and the newest waiting session answers, as before.
        set_pairing_candidates(&["scene-none".to_owned()]);
        let (status, _, _) = route_request("POST", "/offer", "192.168.1.5:48210", &offer, &shared);
        assert_eq!(status, 409, "the scene-two session is mid-pairing");
        shared.unregister(&scene_two);
        let (status, _, _) = route_request("POST", "/offer", "192.168.1.5:48210", &offer, &shared);
        assert_eq!(status, 202, "the fallback is the newest waiting session");
        assert!(matches!(
            scene_one.inner.lock().unwrap().state,
            SignalState::OfferReady { .. }
        ));
    }

    /// A scan that lands while a session is on the air **replaces** the
    /// stream instead of being refused: the connection is retired, and the
    /// new offer waits in the same session for the old negotiation to stand
    /// down. This is what makes a re-scan work at all — the phone's page
    /// that stopped only closed its own peer connection, and a fresh scan
    /// must not be told the port is busy for the half minute an ICE agent
    /// takes to call a silent peer failed.
    #[test]
    fn a_newer_scan_displaces_a_connected_session() {
        let _g = STATUS_LOCK.lock().unwrap();
        reset_session_state();
        let shared = Arc::new(Shared {
            router: Mutex::new(Router::default()),
            server: Mutex::new(None),
            stop: Arc::new(AtomicBool::new(false)),
        });
        let session = Arc::new(Signaling::default());
        shared.register("session", &session);

        // The first phone pairs all the way to connected.
        let offer = serde_json::to_vec(&RTCSessionDescription::offer(minimal_sdp()).expect("offer"))
            .expect("json");
        let (status, _, _) = route_request("POST", "/offer", "192.168.1.5:48210", &offer, &shared);
        assert_eq!(status, 202);
        assert!(session.take_offer().is_some(), "the session thread takes it");
        session.set_answer(minimal_sdp());
        session.set_connected();
        assert_eq!(session_status(), SessionStatus::Connected);

        // A second scan while connected: accepted, the old connection retired
        // under it, the new offer waiting for the old negotiation to leave.
        let (status, _, _) = route_request("POST", "/offer", "192.168.1.5:48210", &offer, &shared);
        assert_eq!(status, 202, "a newer scan replaces the stream");
        assert!(
            session.is_retired(),
            "the negotiation loop is told to stand down"
        );
        let (status, _, body) = route_request("GET", "/status", "", b"", &shared);
        assert_eq!(status, 200);
        assert!(
            String::from_utf8_lossy(&body).contains("offer-received"),
            "the new offer is what /status reports: {}",
            String::from_utf8_lossy(&body)
        );
        // The old negotiation's teardown does not wipe the new offer...
        session.set_failed_keep_offer("peer connection failed".to_string());
        assert!(
            session.take_offer().is_some(),
            "the pending offer survives the teardown"
        );
        // ...and the next negotiation starts clean.
        session.clear_retire();
        assert!(!session.is_retired());
    }

    /// A session that leaves while it was the one on the air puts the status
    /// mirror back to waiting — otherwise a Source closed while its phone
    /// streamed would keep the pairing code hidden forever, since nothing
    /// else writes over `Connected` on that path.
    #[test]
    fn a_session_leaving_the_air_returns_the_status_to_waiting() {
        let _g = STATUS_LOCK.lock().unwrap();
        reset_session_state();
        let shared = Arc::new(Shared {
            router: Mutex::new(Router::default()),
            server: Mutex::new(None),
            stop: Arc::new(AtomicBool::new(false)),
        });
        let session = Arc::new(Signaling::default());
        shared.register("session", &session);
        let offer = serde_json::to_vec(&RTCSessionDescription::offer(minimal_sdp()).expect("offer"))
            .expect("json");
        let (status, _, _) = route_request("POST", "/offer", "192.168.1.5:48210", &offer, &shared);
        assert_eq!(status, 202);
        assert_eq!(session_status(), SessionStatus::Connecting);

        shared.unregister(&session);
        assert_eq!(
            session_status(),
            SessionStatus::Waiting,
            "nothing is on the air any more"
        );
    }

    /// The teardown's failure note never wipes an offer the router already
    /// routed; a failure with nothing pending lands as usual.
    #[test]
    fn a_teardown_keeps_a_pending_offer_but_not_the_connected_state() {
        let sig = Signaling::default();
        sig.set_offer(minimal_sdp(), "192.168.1.5".into());
        sig.set_failed_keep_offer("peer connection failed".into());
        assert!(
            matches!(sig.inner.lock().unwrap().state, SignalState::OfferReady { .. }),
            "the pending offer outranks the failure"
        );

        let sig = Signaling::default();
        sig.set_connected();
        sig.set_failed_keep_offer("peer connection failed".into());
        assert!(
            matches!(sig.inner.lock().unwrap().state, SignalState::Failed(_)),
            "a connection with nothing pending fails as usual"
        );
    }

    /// With no session on the air, the page is served and the poll answers
    /// are what a waiting session would say — the phone page works before
    /// any Source has opened.
    #[test]
    fn a_request_with_no_session_is_answered_by_a_throwaway_idle_one() {
        let shared = Arc::new(Shared {
            router: Mutex::new(Router::default()),
            server: Mutex::new(None),
            stop: Arc::new(AtomicBool::new(false)),
        });
        let (status, _, body) = route_request("GET", "/", "", b"", &shared);
        assert_eq!(status, 200);
        assert!(String::from_utf8_lossy(&body).contains("getUserMedia"));
        let (status, _, body) = route_request("GET", "/status", "", b"", &shared);
        assert_eq!(status, 200);
        assert!(String::from_utf8_lossy(&body).contains("waiting"));
        let (status, _, _) = route_request("GET", "/answer", "", b"", &shared);
        assert_eq!(status, 409);
    }

    #[test]
    fn malformed_offer_is_rejected() {
        let sig = Signaling::default();
        let (status, _, _) = handle_request("POST", "/offer", "", b"not json", &sig);
        assert_eq!(status, 400);
    }

    #[test]
    fn answer_not_ready_yet_is_409() {
        let sig = Signaling::default();
        let (status, _, _) = handle_request("GET", "/answer", "", b"", &sig);
        assert_eq!(status, 409);
    }

    #[test]
    fn stop_marks_the_session_failed() {
        let _g = STATUS_LOCK.lock().unwrap();
        let sig = Signaling::default();
        let (status, _, _) = handle_request("POST", "/stop", "", b"", &sig);
        assert_eq!(status, 200);
        assert!(sig.is_failed());
    }

    #[test]
    fn answer_sdp_carries_the_host_candidate_in_valid_order() {
        // Regression guard for the "Invalid SDP line" bug: `create_answer`
        // marshals the added host candidate itself (a=candidate after c=,
        // inside the media section). We must not inject it manually.
        // A Safari-style offer: BUNDLE, H.264 baseline + Opus, sendonly.
        let offer_sdp = "v=0\r\no=- 4917618493350528073 2 IN IP4 127.0.0.1\r\ns=-\r\n\
            t=0 0\r\n\
            a=group:BUNDLE 0 1\r\n\
            a=msid-semantic: WMS 78E216B6-8C1E-4B8D-9B4C-8F0B1F5E7D2E\r\n\
            m=video 9 UDP/TLS/RTP/SAVPF 102 103\r\n\
            c=IN IP4 0.0.0.0\r\n\
            a=ice-ufrag:abc123\r\n\
            a=ice-pwd:def456ghi789jkl012mno345\r\n\
            a=fingerprint:sha-256 00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF\r\n\
            a=setup:actpass\r\n\
            a=mid:0\r\n\
            a=sendonly\r\n\
            a=rtcp-mux\r\n\
            a=rtpmap:102 H264/90000\r\n\
            a=fmtp:102 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f\r\n\
            a=rtpmap:103 rtx/90000\r\n\
            a=fmtp:103 apt=102\r\n\
            a=ssrc:1234567890 cname:abc123def\r\n\
            a=ssrc:1234567890 msid:78E216B6-8C1E-4B8D-9B4C-8F0B1F5E7D2E 12345678-1234-1234-1234-123456789012\r\n\
            m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
            c=IN IP4 0.0.0.0\r\n\
            a=ice-ufrag:abc123\r\n\
            a=ice-pwd:def456ghi789jkl012mno345\r\n\
            a=fingerprint:sha-256 00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF\r\n\
            a=setup:actpass\r\n\
            a=mid:1\r\n\
            a=sendonly\r\n\
            a=rtcp-mux\r\n\
            a=rtpmap:111 opus/48000/2\r\n\
            a=fmtp:111 minptime=10;useinbandfec=1\r\n\
            a=ssrc:9876543210 cname:abc123def\r\n\
            a=ssrc:9876543210 msid:78E216B6-8C1E-4B8D-9B4C-8F0B1F5E7D2E 12345678-1234-1234-1234-123456789012\r\n";
        let mut pc = build_peer("127.0.0.1", 12345).expect("build_peer");
        pc.set_remote_description(
            RTCSessionDescription::offer(offer_sdp.to_owned()).expect("offer"),
        )
        .expect("set_remote_description");
        let answer = pc.create_answer(None).expect("create_answer");
        // The answer must already contain the host candidate inside the
        // first (BUNDLE) media section, after c= and before the next m=
        // (RFC 4566 order). With BUNDLE, candidates live on the first
        // media section only — the same convention browsers use.
        let sdp = &answer.sdp;
        let video_mid = sdp.find("m=video").unwrap_or(sdp.len());
        let audio_mid = sdp.find("m=audio").unwrap_or(sdp.len());
        let video_block = &sdp[video_mid..audio_mid];
        let c_line = video_block.find("c=IN IP4").expect("video c= present");
        let cand = video_block.find("a=candidate:").expect("video candidate present");
        assert!(cand > c_line, "candidate must follow c= inside the media section");
        assert!(cand > video_block.find("a=mid:0").unwrap(), "candidate after a=mid");
        // And it must be exactly the host candidate we added.
        assert!(video_block.contains("typ host"), "host candidate marshalled");
    }

    #[test]
    fn unknown_route_is_404() {
        let sig = Signaling::default();
        let (status, _, _) = handle_request("GET", "/nope", "", b"", &sig);
        assert_eq!(status, 404);
    }

    #[test]
    fn host_ip_parses_every_host_header_shape() {
        assert_eq!(host_ip_from("192.168.1.5:48210").as_deref(), Some("192.168.1.5"));
        assert_eq!(host_ip_from("192.168.1.5").as_deref(), Some("192.168.1.5"));
        assert_eq!(host_ip_from("[::1]:48210").as_deref(), Some("::1"));
        assert_eq!(host_ip_from("reacut.local:48210"), None);
    }
}
