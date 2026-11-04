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
//! dropped (a malformed aggregate tails off silently). A fragment run whose
//! start packet was lost poisons the in-flight frame, which is discarded at
//! the marker (the decoder keeps showing the previous frame — the video
//! mailbox is latest-wins anyway).

/// Annex-B start code for one access unit.
const START_CODE: [u8; 4] = [0x00, 0x00, 0x00, 0x01];

/// What feeding one RTP payload produced.
#[derive(Debug, PartialEq, Eq)]
pub enum DepackOutcome {
    /// The frame continues, or nothing is ready yet.
    None,
    /// A complete, clean access unit in Annex-B form.
    Frame(Vec<u8>),
    /// A frame finished but was corrupt (packet loss) and was discarded —
    /// the session should request a keyframe.
    Dropped,
}

pub struct H264Depacketizer {
    /// NAL units of the in-flight access unit in Annex-B form.
    au: Vec<u8>,
    /// True while a FU-A fragment run is open (start seen, end not yet).
    fu_open: bool,
    /// The in-flight frame is corrupt (a fragment start was lost) — discard
    /// it at the marker instead of handing a hole to the decoder.
    poison: bool,
    /// RTP sequence number of the last packet fed in — a mid-frame gap
    /// means packets were lost (or a NACK retransmission landed out of
    /// order), and the half-reassembled frame would decode to garbage.
    last_seq: Option<u16>,
}

impl H264Depacketizer {
    pub fn new() -> Self {
        Self {
            au: Vec::with_capacity(64 * 1024), // one 720p frame fits without realloc
            fu_open: false,
            poison: false,
            last_seq: None,
        }
    }

    /// Drop any buffered state (session restart / track switch).
    #[allow(dead_code)] // future session-restart path
    pub(crate) fn reset(&mut self) {
        self.au.clear();
        self.fu_open = false;
        self.poison = false;
        self.last_seq = None;
    }

    /// Feed one RTP payload. Returns the completed access unit in Annex-B
    /// form (start codes between NAL units) when `marker` signals the last
    /// packet of the frame; [`DepackOutcome::Dropped`] when the frame was
    /// corrupt; [`DepackOutcome::None`] otherwise.
    pub fn push(&mut self, payload: &[u8], marker: bool, seq: u16) -> DepackOutcome {
        // RTP sequence-number continuity. The first packet of a frame can't
        // be checked (state was reset at the previous marker); every later
        // packet must follow its predecessor by exactly one. A gap poisons
        // the frame; a duplicate (NACK retransmission) is skipped — the
        // payload was already folded in.
        if !(self.au.is_empty() && !self.poison) {
            match self.last_seq.map(|l| seq.wrapping_sub(l)) {
                Some(0) => return if marker { self.finish() } else { DepackOutcome::None },
                Some(1) => {}
                Some(_) => self.poison = true, // lost packets mid-frame
                None => {}
            }
        }
        self.last_seq = Some(seq);

        let Some(&first) = payload.first() else {
            if marker {
                return self.finish();
            }
            return DepackOutcome::None; // empty payload: nothing to add
        };
        match first & 0x1F {
            // Single NAL unit (1-23): the whole payload is one NAL.
            1..=23 => {
                // A new NAL while a FU-A run is open means the run's end was
                // lost — the half NAL in the buffer would corrupt the frame.
                if self.fu_open {
                    self.poison = true;
                }
                self.fu_open = false;
                self.au.extend_from_slice(START_CODE.as_slice());
                self.au.extend_from_slice(payload);
            }
            // STAP-A (24): length-prefixed NAL units in one payload.
            24 => {
                if self.fu_open {
                    self.poison = true;
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
                    self.poison = true; // no FU header: unusable
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
                            self.poison = true; // lost the start fragment
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
            self.finish()
        } else {
            DepackOutcome::None
        }
    }

    /// Take the finished frame, or classify it when corrupt. Resets
    /// per-frame state either way.
    fn finish(&mut self) -> DepackOutcome {
        let done = std::mem::take(&mut self.au);
        let poisoned = self.poison;
        self.poison = false;
        self.fu_open = false;
        if poisoned {
            DepackOutcome::Dropped
        } else if done.is_empty() {
            DepackOutcome::None
        } else {
            DepackOutcome::Frame(done)
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

    /// Assert a clean frame equal to the expected Annex-B bytes.
    fn assert_frame(out: &DepackOutcome, expected: &[u8]) {
        match out {
            DepackOutcome::Frame(f) => assert_eq!(f, expected),
            other => panic!("expected a frame, got {other:?}"),
        }
    }

    #[test]
    fn single_nal_packets_form_one_annexb_frame() {
        let mut d = H264Depacketizer::new();
        // SPS, PPS, then an IDR — each its own RTP packet, marker on the last.
        assert_eq!(d.push(&nal(&[0x67, 0x42, 0x01]), false, 1), DepackOutcome::None);
        assert_eq!(d.push(&nal(&[0x68, 0xCE]), false, 2), DepackOutcome::None);
        let out = d.push(&nal(&[0x65, 0x88, 0x84]), true, 3);
        let expected: Vec<u8> = [
            annexb(&[0x67, 0x42, 0x01]).as_slice(),
            annexb(&[0x68, 0xCE]).as_slice(),
            annexb(&[0x65, 0x88, 0x84]).as_slice(),
        ]
        .concat();
        assert_frame(&out, &expected);
        assert_frame(&d.push(&nal(&[0x65]), true, 4), &annexb(&[0x65]));
    }

    #[test]
    fn stap_a_aggregate_splits_into_nal_units() {
        let mut d = H264Depacketizer::new();
        // STAP-A: SPS + PPS + IDR as length-prefixed sub-NALs.
        let mut payload = vec![24];
        payload.extend_from_slice(&stap_nal(&[0x67, 0x42, 0x01, 0x02]));
        payload.extend_from_slice(&stap_nal(&[0x68, 0xCE]));
        payload.extend_from_slice(&stap_nal(&[0x65, 0x11, 0x22]));
        let out = d.push(&payload, true, 1);
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
        let mut d = H264Depacketizer::new();
        // A valid NAL plus a truncated length prefix: parses the first, drops
        // the rest instead of panicking or emitting garbage.
        let payload = [
            &[24u8][..],
            &stap_nal(&[0x67, 0x42]),
            &[0x00, 0x07][..], // claims 7 bytes; only 2 remain
        ]
        .concat();
        let out = d.push(&payload, true, 1);
        assert_frame(&out, &annexb(&[0x67, 0x42]));
    }

    #[test]
    fn fu_a_fragments_reassemble_with_reconstructed_header() {
        let mut d = H264Depacketizer::new();
        // IDR NAL = [0x65, 0x88, 0x11, 0x22, 0x33, 0x44] fragmented across
        // three FU-A packets. The first byte (0x65) is carried in the FU
        // headers, so the fragment payloads hold only the remaining bytes.
        let indicator = 0x60 | 28; // F=0 NRI=3, FU-A
        let mut f0 = vec![indicator, 0x80 | 5];
        f0.extend_from_slice(&[0x88]); // start fragment
        assert_eq!(d.push(&f0, false, 1), DepackOutcome::None);
        let mut f1 = vec![indicator, 5];
        f1.extend_from_slice(&[0x11, 0x22, 0x33]); // middle fragment
        assert_eq!(d.push(&f1, false, 2), DepackOutcome::None);
        let mut f2 = vec![indicator, 0x40 | 5];
        f2.extend_from_slice(&[0x44]); // end fragment
        let out = d.push(&f2, true, 3);
        let expected: Vec<u8> = annexb(&[0x65, 0x88, 0x11, 0x22, 0x33, 0x44]); // 0x65 = 0x60|0x05
        assert_frame(&out, &expected);
    }

    #[test]
    fn lost_fu_start_poisons_the_frame() {
        let mut d = H264Depacketizer::new();
        // Middle and end fragments without a start: the AU must be dropped at
        // the marker, not handed to the decoder with a hole.
        let indicator = 0x60 | 28;
        let mut mid = vec![indicator, 5];
        mid.extend_from_slice(&[0x11, 0x22]);
        assert_eq!(d.push(&mid, false, 1), DepackOutcome::None);
        let mut end = vec![indicator, 0x40 | 5];
        end.extend_from_slice(&[0x33]);
        assert_eq!(d.push(&end, true, 2), DepackOutcome::Dropped, "corrupt frame is discarded");
        // A clean frame afterwards still works (state recovered).
        assert_frame(&d.push(&nal(&[0x65, 0xAA]), true, 3), &annexb(&[0x65, 0xAA]));
    }

    #[test]
    fn stale_fragments_before_start_are_ignored() {
        let mut d = H264Depacketizer::new();
        // FU-A packets arriving with no open run (a previous run was dropped
        // without an end): silently ignored, no garbage in the AU.
        let indicator = 0x60 | 28;
        let mut mid = vec![indicator, 5];
        mid.extend_from_slice(&[0xDE, 0xAD]);
        assert_eq!(d.push(&mid, false, 1), DepackOutcome::None);
        assert_frame(&d.push(&nal(&[0x65]), true, 2), &annexb(&[0x65]));
    }

    #[test]
    fn single_nal_interrupting_an_open_fu_run_poisons_the_frame() {
        let mut d = H264Depacketizer::new();
        // A FU-A run interrupted by a single-NAL packet: the run lost its end,
        // so the whole AU is corrupt and must be discarded at the marker —
        // never a half-reassembled NAL handed to the decoder.
        let indicator = 0x60 | 28;
        let mut start = vec![indicator, 0x80 | 5];
        start.extend_from_slice(&[0x88, 0x11]);
        assert_eq!(d.push(&start, false, 1), DepackOutcome::None);
        assert_eq!(
            d.push(&nal(&[0x65, 0x01, 0x02]), true, 2),
            DepackOutcome::Dropped,
            "a frame with an aborted FU run is dropped"
        );
        assert_frame(&d.push(&nal(&[0x65, 0xAA]), true, 3), &annexb(&[0x65, 0xAA]));
    }

    #[test]
    fn empty_payload_marker_still_finishes() {
        let mut d = H264Depacketizer::new();
        assert_eq!(d.push(&nal(&[0x65]), false, 1), DepackOutcome::None);
        assert!(matches!(
            d.push(&[], true, 2),
            DepackOutcome::Frame(_)
        ), "marker on an empty payload finishes the frame");
        assert_eq!(d.push(&[], true, 3), DepackOutcome::None, "a marker with no frame yields nothing");
    }

    #[test]
    fn reset_clears_all_state() {
        let mut d = H264Depacketizer::new();
        assert_eq!(d.push(&nal(&[0x67]), false, 1), DepackOutcome::None);
        d.reset();
        assert_frame(&d.push(&nal(&[0x65]), true, 2), &annexb(&[0x65]));
    }

    #[test]
    fn sequence_gap_poisons_the_frame() {
        let mut d = H264Depacketizer::new();
        // Packets 4 and 5 of a FU-A run never arrive: the reassembled NAL
        // would have a hole, so the frame must be dropped, not decoded.
        let indicator = 0x60 | 28;
        let mut f = vec![indicator, 0x80 | 5];
        f.extend_from_slice(&[0x88]);
        assert_eq!(d.push(&f, false, 1), DepackOutcome::None);
        let mut f = vec![indicator, 0x40 | 5];
        f.extend_from_slice(&[0x99]);
        assert_eq!(d.push(&f, true, 3), DepackOutcome::Dropped, "seq gap (2 missing) poisons");
        // A clean frame after the gap works normally.
        assert_frame(&d.push(&nal(&[0x65, 0xAA]), true, 4), &annexb(&[0x65, 0xAA]));
    }

    #[test]
    fn duplicate_packet_is_skipped() {
        let mut d = H264Depacketizer::new();
        // A NACK retransmission re-delivers packet 1 mid-run: skipped, the
        // frame still reassembles cleanly.
        let indicator = 0x60 | 28;
        let mut f = vec![indicator, 0x80 | 5];
        f.extend_from_slice(&[0x88, 0x11]);
        assert_eq!(d.push(&f, false, 1), DepackOutcome::None);
        assert_eq!(d.push(&f, false, 1), DepackOutcome::None, "duplicate ignored");
        let mut f = vec![indicator, 0x40 | 5];
        f.extend_from_slice(&[0x22]);
        let out = d.push(&f, true, 2);
        assert_frame(&out, &annexb(&[0x65, 0x88, 0x11, 0x22]));
    }

    #[test]
    fn first_packet_of_a_frame_skips_the_continuity_check() {
        let mut d = H264Depacketizer::new();
        assert_frame(&d.push(&nal(&[0x65, 0x01]), true, 5), &annexb(&[0x65, 0x01]));
        // The next frame's first packet may be any sequence number (the
        // sender's counter continues, but a fresh frame can't be judged by
        // the previous frame's tail).
        assert_eq!(d.push(&nal(&[0x67, 0x02]), false, 100), DepackOutcome::None);
        let expected: Vec<u8> = [
            annexb(&[0x67, 0x02]).as_slice(),
            annexb(&[0x65, 0x03]).as_slice(),
        ]
        .concat();
        assert_frame(&d.push(&nal(&[0x65, 0x03]), true, 101), &expected);
    }
}
