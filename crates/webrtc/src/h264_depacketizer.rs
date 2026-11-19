//! RFC 6184 H.264 RTP depacketizer — re-assembles RTP payloads into Annex-B
//! byte streams, one decodable access unit per output.
//!
//! rtc 0.20 (the sans-I/O WebRTC core) hands the application raw RTP packets
//! — the ecosystem no longer ships a depacketizer, so this is ours. It is
//! deliberately minimal and allocation-light: Single NAL units and STAP-A
//! aggregates append straight into the in-flight frame buffer (one copy per
//! packet), FU-A fragments append into the same buffer without a reassembly
//! staging pass. The frame is handed out when the RTP marker bit arrives.
//!
//! What we accept: NAL types 1-23 (single), 24 (STAP-A), 28 (FU-A) — exactly
//! what browsers' H.264 senders produce. MTAP / FU-B and unknown types are
//! dropped (a malformed aggregate tails off silently).
//!
//! ## Loss handling: a hold, not a poison
//!
//! A sequence-number gap no longer discards the in-flight frame. It starts a
//! *hold*: the frame stays open and every following packet is buffered until
//! either the missing packet arrives (NACK retransmission — the common case,
//! see the NACK config in `phone::build_peer`) and the frame completes
//! intact, or [`REORDER_WAIT`] passes and the incomplete frame is dropped.
//! This is what keeps a recoverable burst from costing a keyframe round
//! trip (a visible stutter while the picture waits for the IDR). Rewound
//! sequence numbers — retransmissions landing after their successors — are
//! ignored outright: their bytes cannot be spliced into an already-passed
//! position, and folding them in (or poisoning on sight, as earlier
//! revisions did) only destroyed frames the network had actually repaired.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Annex-B start code for one access unit.
const START_CODE: [u8; 4] = [0x00, 0x00, 0x00, 0x01];

/// How long a gapped frame waits for its missing packets before it is
/// dropped. The NACK cycle (`phone::build_peer`) re-requests a loss every
/// 50ms and gives up after 3 requests, so a recoverable packet lands within
/// ~150ms of the gap; 250ms covers that plus RTT and jitter. A longer wait
/// delays the (rare) unrecoverable drop — and with it the keyframe
/// recovery — past the point of usefulness.
const REORDER_WAIT: Duration = Duration::from_millis(250);

/// What feeding one RTP payload produced.
#[derive(Debug, PartialEq, Eq)]
pub enum DepackOutcome {
    /// The frame continues, or nothing is ready yet.
    None,
    /// A complete, clean access unit in Annex-B form.
    Frame(Vec<u8>),
    /// A frame finished but was corrupt and was discarded — the session
    /// should request a keyframe.
    Dropped(DepackDropReason),
}

/// Why a finished frame was discarded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DepackDropReason {
    /// Sequence numbers jumped and the missing packets never arrived within
    /// [`REORDER_WAIT`] — real loss, unrecovered.
    SeqGap,
    /// A single-NAL / STAP-A packet arrived while a FU-A run was open — the
    /// run's end fragment was lost (or reordered behind).
    FuInterrupted,
    /// A FU-A end/middle fragment arrived with no start fragment — the
    /// fragment that opened the NAL was lost.
    MissingFuStart,
    /// Payload was malformed (e.g. a FU-A packet too short to carry a header).
    Malformed,
}

/// A frame on hold: waiting for the sequence number `missing_lo` (the hole)
/// until `deadline`. Packets after the hole are buffered in arrival order.
struct Hold {
    missing_lo: u16,
    deadline: Instant,
}

/// One packet buffered while a hold is open. Payloads live in the shared
/// `pending_data` arena (`off..off+len`) — one allocation grows with the
/// hold instead of one per packet.
struct PendingEntry {
    seq: u16,
    marker: bool,
    off: u32,
    len: u32,
}

pub struct H264Depacketizer {
    /// NAL units of the in-flight access unit in Annex-B form.
    au: Vec<u8>,
    /// True while a FU-A fragment run is open (start seen, end not yet).
    fu_open: bool,
    /// The in-flight frame is corrupt — discard it at the marker instead of
    /// handing a hole to the decoder. Carries the *first* reason the frame
    /// went bad; later poisoning sites must not overwrite it.
    poison: Option<DepackDropReason>,
    /// RTP sequence number of the last packet fed in — the continuity anchor
    /// for gaps (hold), duplicates (skip) and rewinds (ignore).
    last_seq: Option<u16>,
    /// The open gap hold, if any (see the module docs).
    hold: Option<Hold>,
    /// Packets buffered while a hold is open, in arrival order.
    pending: Vec<PendingEntry>,
    /// Payload bytes of `pending`, contiguous.
    pending_data: Vec<u8>,
    /// Completed frames not yet surfaced (a resolve can replay several
    /// markers at once; `push` hands out one per call).
    ready: VecDeque<DepackOutcome>,
}

impl H264Depacketizer {
    pub fn new() -> Self {
        Self {
            au: Vec::with_capacity(64 * 1024), // one 720p frame fits without realloc
            fu_open: false,
            poison: None,
            last_seq: None,
            hold: None,
            pending: Vec::new(),
            pending_data: Vec::new(),
            ready: VecDeque::new(),
        }
    }

    /// Drop any buffered state (session restart / track switch).
    #[allow(dead_code)] // future session-restart path
    pub(crate) fn reset(&mut self) {
        self.au.clear();
        self.fu_open = false;
        self.poison = None;
        self.last_seq = None;
        self.hold = None;
        self.pending.clear();
        self.pending_data.clear();
        self.ready.clear();
    }

    /// Feed one RTP payload. Returns the earliest completed-frame outcome
    /// not yet surfaced: the packet's own outcome on the clean path,
    /// `None` while a hold buffers, or the first of several outcomes when a
    /// resolve replays a run of buffered markers at once (the rest wait in
    /// [`Self::take_outcome`]).
    pub fn push(&mut self, payload: &[u8], marker: bool, seq: u16, now: Instant) -> DepackOutcome {
        if let Some(hold) = &self.hold
            && now >= hold.deadline
        {
            self.expire_hold(now);
        }
        if self.hold.is_some() {
            self.push_holding(payload, marker, seq, now);
        } else {
            self.feed_inner(payload, marker, seq, now);
        }
        self.take_outcome().unwrap_or(DepackOutcome::None)
    }

    /// Take the next completed-frame outcome queued by an earlier resolve
    /// (a buffered run can contain several markers; one `push` surfaces
    /// one). The session drains this after every push.
    pub fn take_outcome(&mut self) -> Option<DepackOutcome> {
        self.ready.pop_front()
    }

    /// The classification + NAL-dispatch core, shared by the live path and
    /// the resolve/expire replays.
    fn feed_inner(&mut self, payload: &[u8], marker: bool, seq: u16, now: Instant) {
        // RTP sequence-number continuity. A forward jump opens a hold for
        // the hole (the frame may still be saved by the retransmission); a
        // rewind is a retransmission of already-passed data — its bytes
        // cannot be spliced, so it is ignored; a duplicate is skipped.
        if let Some(last) = self.last_seq {
            match seq.wrapping_sub(last) {
                0 => {
                    if marker {
                        self.finish_into_ready();
                    }
                    return;
                }
                1 => {}
                d if d < 0x8000 => {
                    self.hold = Some(Hold { missing_lo: last.wrapping_add(1), deadline: now + REORDER_WAIT });
                    self.buffer_packet(payload, marker, seq);
                    return;
                }
                _ => return, // rewind: retransmission of given-up data
            }
        }
        self.last_seq = Some(seq);

        let Some(&first) = payload.first() else {
            if marker {
                self.finish_into_ready();
            }
            return; // empty payload: nothing to add
        };
        match first & 0x1F {
            // Single NAL unit (1-23): the whole payload is one NAL.
            1..=23 => {
                // A new NAL while a FU-A run is open means the run's end was
                // lost — the half NAL in the buffer would corrupt the frame.
                if self.fu_open {
                    self.poison.get_or_insert(DepackDropReason::FuInterrupted);
                }
                self.fu_open = false;
                self.au.extend_from_slice(START_CODE.as_slice());
                self.au.extend_from_slice(payload);
            }
            // STAP-A (24): length-prefixed NAL units in one payload.
            24 => {
                if self.fu_open {
                    self.poison.get_or_insert(DepackDropReason::FuInterrupted);
                }
                self.fu_open = false;
                let mut rest = &payload[1..];
                while rest.len() >= 2 {
                    let n = usize::from(u16::from_be_bytes([rest[0], rest[1]]));
                    if n == 0 || rest.len() < 2 + n {
                        break; // malformed tail: ignore what doesn't parse
                    }
                    self.au.extend_from_slice(START_CODE.as_slice());
                    self.au.extend_from_slice(&rest[2..2 + n]);
                    rest = &rest[2 + n..];
                }
            }
            // FU-A (28): one NAL fragmented across packets.
            28 => {
                if payload.len() < 2 {
                    self.poison.get_or_insert(DepackDropReason::Malformed); // no FU header: unusable
                } else {
                    let s = payload[1] & 0x80 != 0;
                    let e = payload[1] & 0x40 != 0;
                    if s {
                        // Reassemble the NAL header: type bits from the FU
                        // header, everything else (F + NRI) from the indicator.
                        self.fu_open = true;
                        self.au.extend_from_slice(START_CODE.as_slice());
                        self.au.push((first & 0xE0) | (payload[1] & 0x1F));
                        self.au.extend_from_slice(&payload[2..]);
                    } else if e {
                        if self.fu_open {
                            self.au.extend_from_slice(&payload[2..]);
                        } else {
                            self.poison.get_or_insert(DepackDropReason::MissingFuStart);
                        }
                        self.fu_open = false;
                    } else if self.fu_open {
                        self.au.extend_from_slice(&payload[2..]);
                    }
                    // Fragments after the end / before a start: stale run,
                    // ignore.
                }
            }
            // MTAP / FU-B / reserved: not produced by browser senders — drop.
            _ => {}
        }

        if marker {
            self.finish_into_ready();
        }
    }

    /// A packet arrived while a hold is open: resolve (the hole itself),
    /// ignore (rewind / duplicate), or buffer (anything newer).
    fn push_holding(&mut self, payload: &[u8], marker: bool, seq: u16, now: Instant) {
        let missing_lo = self.hold.as_ref().expect("holding").missing_lo;
        if seq == missing_lo {
            // The hole is filled: splice the packet in and replay the
            // buffered run through the normal path.
            self.hold = None;
            self.feed_inner(payload, marker, seq, now);
            self.replay_pending(now);
            return;
        }
        if seq.wrapping_sub(missing_lo) >= 0x8000 {
            return; // rewound past the hole: retransmission of given-up data
        }
        if self.pending.iter().any(|e| e.seq == seq) {
            return; // already buffered (duplicate retransmission)
        }
        self.buffer_packet(payload, marker, seq);
    }

    /// The hold expired: the missing packet never came. Drop the held frame
    /// (the reference chain is broken — the session requests a keyframe),
    /// then replay the buffered run as a fresh start.
    fn expire_hold(&mut self, now: Instant) {
        self.hold = None;
        let reason = self.poison.take().unwrap_or(DepackDropReason::SeqGap);
        let had_frame = !self.au.is_empty() || reason != DepackDropReason::SeqGap;
        self.au.clear();
        self.fu_open = false;
        // The buffered run resumes at its own sequence numbers; the stale
        // anchor would misread the first replayed packet as another gap.
        self.last_seq = None;
        if had_frame {
            self.ready.push_back(DepackOutcome::Dropped(reason));
        }
        self.replay_pending(now);
    }

    /// Feed the packets buffered during a hold through the normal path, in
    /// order. A second hole inside the run starts a new hold; everything
    /// after it goes back to the buffer.
    fn replay_pending(&mut self, now: Instant) {
        let entries = std::mem::take(&mut self.pending);
        let mut arena = std::mem::take(&mut self.pending_data);
        for entry in &entries {
            // Payload bytes always come from the taken arena; anything the
            // replay buffers (a second hold) lands in the live structures.
            let payload = &arena[entry.off as usize..entry.off as usize + entry.len as usize];
            if self.hold.is_some() {
                // The replay opened a new hold before reaching this entry:
                // re-buffer the rest into the live hold buffer.
                self.buffer_packet(payload, entry.marker, entry.seq);
            } else {
                self.feed_inner(payload, entry.marker, entry.seq, now);
            }
        }
        if self.pending_data.is_empty() {
            // Nothing new was buffered during the replay: recycle the old
            // arena's capacity. (If something was, its bytes live in the
            // fresh `pending_data` and the old arena is simply dropped.)
            arena.clear();
            self.pending_data = arena;
        }
    }

    /// Stash a packet in the hold buffer (payload into the shared arena).
    fn buffer_packet(&mut self, payload: &[u8], marker: bool, seq: u16) {
        let off = self.pending_data.len() as u32;
        self.pending_data.extend_from_slice(payload);
        self.pending.push(PendingEntry { seq, marker, off, len: payload.len() as u32 });
    }

    /// Take the finished frame, or classify it when corrupt. Resets
    /// per-frame state either way; the outcome joins the ready queue.
    fn finish_into_ready(&mut self) {
        let done = std::mem::take(&mut self.au);
        let reason = self.poison.take();
        self.fu_open = false;
        match reason {
            Some(reason) => self.ready.push_back(DepackOutcome::Dropped(reason)),
            None if !done.is_empty() => self.ready.push_back(DepackOutcome::Frame(done)),
            None => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A single-NAL RTP payload: the first byte *is* the NAL header (type +
    /// NRI), the rest the NAL body.
    fn nal(n: &[u8]) -> Vec<u8> {
        n.to_vec()
    }

    /// The Annex-B form of one NAL: start code + the NAL (header included).
    fn annexb(n: &[u8]) -> Vec<u8> {
        START_CODE.iter().chain(n.iter()).copied().collect()
    }

    /// One STAP-A sub-NAL entry: 2-byte big-endian length + NAL.
    fn stap_nal(n: &[u8]) -> Vec<u8> {
        let mut entry = Vec::with_capacity(2 + n.len());
        entry.extend_from_slice(&(n.len() as u16).to_be_bytes());
        entry.extend_from_slice(n);
        entry
    }

    /// A FU-A fragment payload: indicator + FU header + body.
    fn fu(indicator: u8, fu_header: u8, body: &[u8]) -> Vec<u8> {
        let mut p = vec![indicator, fu_header];
        p.extend_from_slice(body);
        p
    }

    /// Assert a clean frame equal to the expected Annex-B bytes.
    fn assert_frame(out: &DepackOutcome, expected: &[u8]) {
        match out {
            DepackOutcome::Frame(f) => assert_eq!(f, expected),
            other => panic!("expected a frame, got {other:?}"),
        }
    }

    #[test]
    fn single_nal_packets_form_one_annexb_frame() {
        let t = Instant::now();
        let mut d = H264Depacketizer::new();
        // SPS, PPS, then an IDR — each its own RTP packet, marker on the last.
        assert_eq!(d.push(&nal(&[0x67, 0x42, 0x01]), false, 1, t), DepackOutcome::None);
        assert_eq!(d.push(&nal(&[0x68, 0xCE]), false, 2, t), DepackOutcome::None);
        let out = d.push(&nal(&[0x65, 0x88, 0x84]), true, 3, t);
        let expected: Vec<u8> = [
            annexb(&[0x67, 0x42, 0x01]).as_slice(),
            annexb(&[0x68, 0xCE]).as_slice(),
            annexb(&[0x65, 0x88, 0x84]).as_slice(),
        ]
        .concat();
        assert_frame(&out, &expected);
        assert_frame(&d.push(&nal(&[0x65]), true, 4, t), &annexb(&[0x65]));
    }

    #[test]
    fn stap_a_aggregate_splits_into_nal_units() {
        let t = Instant::now();
        let mut d = H264Depacketizer::new();
        // STAP-A: SPS + PPS + IDR as length-prefixed sub-NALs.
        let mut payload = vec![24];
        payload.extend_from_slice(&stap_nal(&[0x67, 0x42, 0x01, 0x02]));
        payload.extend_from_slice(&stap_nal(&[0x68, 0xCE]));
        payload.extend_from_slice(&stap_nal(&[0x65, 0x11, 0x22]));
        let out = d.push(&payload, true, 1, t);
        let expected: Vec<u8> = [
            annexb(&[0x67, 0x42, 0x01, 0x02]).as_slice(),
            annexb(&[0x68, 0xCE]).as_slice(),
            annexb(&[0x65, 0x11, 0x22]).as_slice(),
        ]
        .concat();
        assert_frame(&out, &expected);
    }

    #[test]
    fn stap_a_malformed_tail_is_ignored() {
        let t = Instant::now();
        let mut d = H264Depacketizer::new();
        // A valid NAL plus a truncated length prefix: parses the first, drops
        // the rest instead of panicking or emitting garbage.
        let payload = [
            &[24u8][..],
            &stap_nal(&[0x67, 0x42]),
            &[0x00, 0x07][..], // claims 7 bytes; only 2 remain
        ]
        .concat();
        let out = d.push(&payload, true, 1, t);
        assert_frame(&out, &annexb(&[0x67, 0x42]));
    }

    #[test]
    fn fu_a_fragments_reassemble_with_reconstructed_header() {
        let t = Instant::now();
        let mut d = H264Depacketizer::new();
        // IDR NAL = [0x65, 0x88, 0x11, 0x22, 0x33, 0x44] fragmented across
        // three FU-A packets. The first byte (0x65) is carried in the FU
        // headers, so the fragment payloads hold only the remaining bytes.
        let indicator = 0x60 | 28; // F=0 NRI=3, FU-A
        assert_eq!(
            d.push(&fu(indicator, 0x80 | 5, &[0x88]), false, 1, t),
            DepackOutcome::None
        );
        assert_eq!(d.push(&fu(indicator, 5, &[0x11, 0x22, 0x33]), false, 2, t), DepackOutcome::None);
        let out = d.push(&fu(indicator, 0x40 | 5, &[0x44]), true, 3, t);
        let expected: Vec<u8> = annexb(&[0x65, 0x88, 0x11, 0x22, 0x33, 0x44]); // 0x65 = 0x60|0x05
        assert_frame(&out, &expected);
    }

    #[test]
    fn lost_fu_start_poisons_the_frame() {
        let t = Instant::now();
        let mut d = H264Depacketizer::new();
        // Middle and end fragments without a start: the AU must be dropped at
        // the marker, not handed to the decoder with a hole.
        let indicator = 0x60 | 28;
        assert_eq!(d.push(&fu(indicator, 5, &[0x11, 0x22]), false, 1, t), DepackOutcome::None);
        let out = d.push(&fu(indicator, 0x40 | 5, &[0x33]), true, 2, t);
        assert_eq!(
            out,
            DepackOutcome::Dropped(DepackDropReason::MissingFuStart),
            "corrupt frame is discarded"
        );
        // A clean frame afterwards still works (state recovered).
        assert_frame(&d.push(&nal(&[0x65, 0xAA]), true, 3, t), &annexb(&[0x65, 0xAA]));
    }

    #[test]
    fn stale_fragments_before_start_are_ignored() {
        let t = Instant::now();
        let mut d = H264Depacketizer::new();
        // FU-A packets arriving with no open run (a previous run was dropped
        // without an end): silently ignored, no garbage in the AU.
        let indicator = 0x60 | 28;
        assert_eq!(d.push(&fu(indicator, 5, &[0xDE, 0xAD]), false, 1, t), DepackOutcome::None);
        assert_frame(&d.push(&nal(&[0x65]), true, 2, t), &annexb(&[0x65]));
    }

    #[test]
    fn single_nal_interrupting_an_open_fu_run_poisons_the_frame() {
        let t = Instant::now();
        let mut d = H264Depacketizer::new();
        // A FU-A run interrupted by a single-NAL packet: the run lost its end,
        // so the whole AU is corrupt and must be discarded at the marker —
        // never a half-reassembled NAL handed to the decoder.
        let indicator = 0x60 | 28;
        assert_eq!(
            d.push(&fu(indicator, 0x80 | 5, &[0x88, 0x11]), false, 1, t),
            DepackOutcome::None
        );
        assert_eq!(
            d.push(&nal(&[0x65, 0x01, 0x02]), true, 2, t),
            DepackOutcome::Dropped(DepackDropReason::FuInterrupted),
            "a frame with an aborted FU run is dropped"
        );
        assert_frame(&d.push(&nal(&[0x65, 0xAA]), true, 3, t), &annexb(&[0x65, 0xAA]));
    }

    #[test]
    fn empty_payload_marker_still_finishes() {
        let t = Instant::now();
        let mut d = H264Depacketizer::new();
        assert_eq!(d.push(&nal(&[0x65]), false, 1, t), DepackOutcome::None);
        assert!(matches!(
            d.push(&[], true, 2, t),
            DepackOutcome::Frame(_)
        ), "marker on an empty payload finishes the frame");
        assert_eq!(d.push(&[], true, 3, t), DepackOutcome::None, "a marker with no frame yields nothing");
    }

    #[test]
    fn reset_clears_all_state() {
        let t = Instant::now();
        let mut d = H264Depacketizer::new();
        assert_eq!(d.push(&nal(&[0x67]), false, 1, t), DepackOutcome::None);
        d.reset();
        assert_frame(&d.push(&nal(&[0x65]), true, 2, t), &annexb(&[0x65]));
    }

    /// A gap holds the frame instead of dropping it; when the missing packet
    /// never arrives, the hold expires and the incomplete frame is dropped —
    /// while the buffered tail still completes its own (clean) frames.
    #[test]
    fn a_gap_holds_then_drops_on_expiry() {
        let t = Instant::now();
        let mut d = H264Depacketizer::new();
        assert_eq!(d.push(&nal(&[0x65, 0x01]), false, 1, t), DepackOutcome::None);
        // Packet 2 is lost; packet 3 (the frame's marker) reveals the gap and
        // is buffered.
        assert_eq!(d.push(&nal(&[0x65, 0x02]), true, 3, t), DepackOutcome::None, "gap holds");
        // Nothing resolves within REORDER_WAIT: the held frame is dropped,
        // the buffered marker completes its frame (it was never corrupt).
        let out = d.push(&nal(&[0x65, 0x03]), false, 4, t + REORDER_WAIT * 2);
        assert_eq!(out, DepackOutcome::Dropped(DepackDropReason::SeqGap), "unrecovered gap drops");
        assert_frame(
            &d.take_outcome().expect("buffered marker replays"),
            &annexb(&[0x65, 0x02]),
        );
        // State is clean: the next frame goes through normally (the packet
        // that triggered expiry, seq 4, is mid-frame and joins seq 5).
        let expected: Vec<u8> = [annexb(&[0x65, 0x03]).as_slice(), annexb(&[0x65, 0x04]).as_slice()].concat();
        assert_frame(&d.push(&nal(&[0x65, 0x04]), true, 5, t + REORDER_WAIT * 2), &expected);
    }

    /// THE recovery path: a packet lost mid-frame arrives within the hold
    /// window and the frame completes as if nothing was lost. (Packet 3
    /// carries the marker — on the wire the marker marks the frame's last
    /// packet, and it was buffered while the hole was open.)
    #[test]
    fn a_gap_resolves_when_the_missing_packet_arrives() {
        let t = Instant::now();
        let mut d = H264Depacketizer::new();
        assert_eq!(d.push(&nal(&[0x65, 0x01]), false, 1, t), DepackOutcome::None);
        assert_eq!(d.push(&nal(&[0x65, 0x03]), true, 3, t), DepackOutcome::None, "gap holds");
        // The NACK retransmission of packet 2 lands: splice + replay.
        let out = d.push(&nal(&[0x65, 0x02]), false, 2, t + Duration::from_millis(40));
        let expected: Vec<u8> = [
            annexb(&[0x65, 0x01]).as_slice(),
            annexb(&[0x65, 0x02]).as_slice(),
            annexb(&[0x65, 0x03]).as_slice(),
        ]
        .concat();
        assert_frame(&out, &expected);
    }

    /// FU-A fragments across the hole: the retransmitted middle fragment
    /// completes the run and the reassembled NAL is byte-identical.
    #[test]
    fn a_gap_inside_a_fu_run_resolves_with_the_retransmission() {
        let t = Instant::now();
        let mut d = H264Depacketizer::new();
        let indicator = 0x60 | 28;
        assert_eq!(
            d.push(&fu(indicator, 0x80 | 5, &[0x88]), false, 1, t),
            DepackOutcome::None
        );
        // Fragment 2 lost; fragments 3 and 4 (end, marker) buffer.
        assert_eq!(d.push(&fu(indicator, 5, &[0x22]), false, 3, t), DepackOutcome::None);
        assert_eq!(d.push(&fu(indicator, 0x40 | 5, &[0x44]), true, 4, t), DepackOutcome::None);
        // Retransmission of fragment 2 resolves the hold.
        let out = d.push(&fu(indicator, 5, &[0x11]), false, 2, t + Duration::from_millis(60));
        assert_frame(&out, &annexb(&[0x65, 0x88, 0x11, 0x22, 0x44]));
    }

    /// A retransmission landing after its successors is ignored: the frame
    /// keeps the bytes it already had and completes cleanly (the earlier
    /// behaviour poisoned it — one recovered loss cost one frame anyway).
    #[test]
    fn a_late_retransmission_is_ignored_and_the_frame_stays_clean() {
        let t = Instant::now();
        let mut d = H264Depacketizer::new();
        assert_eq!(d.push(&nal(&[0x65, 0x01]), false, 1, t), DepackOutcome::None);
        assert_eq!(d.push(&nal(&[0x65, 0x02]), false, 2, t), DepackOutcome::None);
        // Packet 1 arrives again (RTX loop / duplicate retransmission).
        assert_eq!(d.push(&nal(&[0x65, 0x01]), false, 1, t), DepackOutcome::None, "rewind ignored");
        let out = d.push(&nal(&[0x65, 0x03]), true, 3, t);
        let expected: Vec<u8> = [
            annexb(&[0x65, 0x01]).as_slice(),
            annexb(&[0x65, 0x02]).as_slice(),
            annexb(&[0x65, 0x03]).as_slice(),
        ]
        .concat();
        assert_frame(&out, &expected);
    }

    /// A duplicate retransmission landing while a hold is open changes
    /// nothing; the real retransmission still resolves the frame.
    #[test]
    fn duplicates_during_a_hold_are_ignored() {
        let t = Instant::now();
        let mut d = H264Depacketizer::new();
        assert_eq!(d.push(&nal(&[0x65, 0x01]), false, 1, t), DepackOutcome::None);
        assert_eq!(d.push(&nal(&[0x65, 0x03]), false, 3, t), DepackOutcome::None, "gap holds");
        // Packet 3 retransmitted twice more before packet 2 arrives.
        assert_eq!(d.push(&nal(&[0x65, 0x03]), false, 3, t), DepackOutcome::None, "dup buffered ignored");
        assert_eq!(d.push(&nal(&[0x65, 0x03]), false, 3, t), DepackOutcome::None, "dup buffered ignored");
        assert_eq!(d.push(&nal(&[0x65, 0x02]), false, 2, t + Duration::from_millis(40)), DepackOutcome::None);
        let out = d.push(&nal(&[0x65, 0x04]), true, 4, t + Duration::from_millis(60));
        let expected: Vec<u8> = [
            annexb(&[0x65, 0x01]).as_slice(),
            annexb(&[0x65, 0x02]).as_slice(),
            annexb(&[0x65, 0x03]).as_slice(),
            annexb(&[0x65, 0x04]).as_slice(),
        ]
        .concat();
        assert_frame(&out, &expected);
    }

    /// A retransmission of data older than the hole (given up long ago)
    /// neither resolves nor disturbs the hold.
    #[test]
    fn a_rewind_past_the_hole_is_ignored_during_a_hold() {
        let t = Instant::now();
        let mut d = H264Depacketizer::new();
        assert!(matches!(
            d.push(&nal(&[0x65, 0x01]), true, 1, t),
            DepackOutcome::Frame(_)
        ));
        assert_eq!(d.push(&nal(&[0x65, 0x02]), false, 2, t), DepackOutcome::None);
        assert_eq!(d.push(&nal(&[0x65, 0x04]), false, 4, t), DepackOutcome::None, "gap holds (3)");
        // Retransmission of packet 1 — older than the hole — is ignored.
        assert_eq!(d.push(&nal(&[0x65, 0x01]), false, 1, t), DepackOutcome::None, "old rewind ignored");
        assert_eq!(d.push(&nal(&[0x65, 0x03]), false, 3, t + Duration::from_millis(40)), DepackOutcome::None);
        let out = d.push(&nal(&[0x65, 0x05]), true, 5, t + Duration::from_millis(60));
        let expected: Vec<u8> = [
            annexb(&[0x65, 0x02]).as_slice(),
            annexb(&[0x65, 0x03]).as_slice(),
            annexb(&[0x65, 0x04]).as_slice(),
            annexb(&[0x65, 0x05]).as_slice(),
        ]
        .concat();
        assert_frame(&out, &expected);
    }

    /// Two holes in one hold: resolving the first replays what it can and
    /// re-holds at the second; the last retransmission completes the run.
    #[test]
    fn two_holes_cascade_into_a_second_hold() {
        let t = Instant::now();
        let mut d = H264Depacketizer::new();
        assert_eq!(d.push(&nal(&[0x65, 0x01]), false, 1, t), DepackOutcome::None);
        // Packets 2 and 4 lost; 3 and 5 (marker) buffer.
        assert_eq!(d.push(&nal(&[0x65, 0x03]), false, 3, t), DepackOutcome::None);
        assert_eq!(d.push(&nal(&[0x65, 0x05]), true, 5, t), DepackOutcome::None);
        // Retransmission of 2 resolves the first hole; the replay hits the
        // second hole at 4→5 and holds again (no marker reached yet).
        assert_eq!(
            d.push(&nal(&[0x65, 0x02]), false, 2, t + Duration::from_millis(40)),
            DepackOutcome::None,
            "second hole re-holds"
        );
        // Retransmission of 4 completes everything.
        let out = d.push(&nal(&[0x65, 0x04]), false, 4, t + Duration::from_millis(90));
        let expected: Vec<u8> = [
            annexb(&[0x65, 0x01]).as_slice(),
            annexb(&[0x65, 0x02]).as_slice(),
            annexb(&[0x65, 0x03]).as_slice(),
            annexb(&[0x65, 0x04]).as_slice(),
            annexb(&[0x65, 0x05]).as_slice(),
        ]
        .concat();
        assert_frame(&out, &expected);
    }

    /// A whole frame lost (its first packet skipped past): the next frame's
    /// packets hold until the deadline, then the run replays clean — the
    /// lost frame simply never existed.
    #[test]
    fn a_whole_lost_frame_holds_then_replays_the_next_one() {
        let t = Instant::now();
        let mut d = H264Depacketizer::new();
        assert_frame(&d.push(&nal(&[0x65, 0x01]), true, 1, t), &annexb(&[0x65, 0x01]));
        // Frame at seqs 2-3 lost entirely; frame 4-5 (marker) buffers.
        assert_eq!(d.push(&nal(&[0x65, 0x04]), false, 4, t), DepackOutcome::None, "jump holds");
        assert_eq!(d.push(&nal(&[0x65, 0x05]), true, 5, t), DepackOutcome::None);
        // Deadline passes: nothing held to drop, the buffered frame replays.
        let out = d.push(&nal(&[0x65, 0x06]), true, 6, t + REORDER_WAIT * 2);
        let expected: Vec<u8> = [annexb(&[0x65, 0x04]).as_slice(), annexb(&[0x65, 0x05]).as_slice()].concat();
        assert_frame(&out, &expected);
        assert_frame(
            &d.take_outcome().expect("second buffered frame"),
            &annexb(&[0x65, 0x06]),
        );
    }

    #[test]
    fn duplicate_packet_is_skipped() {
        let t = Instant::now();
        let mut d = H264Depacketizer::new();
        // A NACK retransmission re-delivers packet 1 mid-run: skipped, the
        // frame still reassembles cleanly.
        let indicator = 0x60 | 28;
        assert_eq!(
            d.push(&fu(indicator, 0x80 | 5, &[0x88, 0x11]), false, 1, t),
            DepackOutcome::None
        );
        assert_eq!(d.push(&fu(indicator, 0x80 | 5, &[0x88, 0x11]), false, 1, t), DepackOutcome::None, "duplicate ignored");
        let out = d.push(&fu(indicator, 0x40 | 5, &[0x22]), true, 2, t);
        assert_frame(&out, &annexb(&[0x65, 0x88, 0x11, 0x22]));
    }

    /// The next frame's first packet may be any sequence number (the
    /// sender's counter continues, but the previous frame's tail may have
    /// been lost): the jump holds briefly, then replays the run clean.
    #[test]
    fn first_packet_of_a_frame_after_a_jump_replays_clean() {
        let t = Instant::now();
        let mut d = H264Depacketizer::new();
        assert_frame(&d.push(&nal(&[0x65, 0x01]), true, 5, t), &annexb(&[0x65, 0x01]));
        // The next frame starts at 100 (synthetic jump) and its marker at 101.
        assert_eq!(d.push(&nal(&[0x67, 0x02]), false, 100, t), DepackOutcome::None, "jump holds");
        assert_eq!(d.push(&nal(&[0x65, 0x03]), true, 101, t), DepackOutcome::None);
        let out = d.push(&nal(&[0x65, 0x04]), true, 102, t + REORDER_WAIT * 2);
        let expected: Vec<u8> = [
            annexb(&[0x67, 0x02]).as_slice(),
            annexb(&[0x65, 0x03]).as_slice(),
        ]
        .concat();
        assert_frame(&out, &expected);
        assert_frame(&d.take_outcome().expect("queued frame"), &annexb(&[0x65, 0x04]));
    }
}
