//! The per-sample plumbing around the voices: microphone packets into mono float, the
//! scope taps, the dry mix, and the reader that drains a ring at its output's own clock.
//! The WASAPI side, which only moves whole buffers, stays in the app.

use std::sync::atomic::{AtomicU32, Ordering::Relaxed};

use crate::ring::Consumer;

/// Ring fill the outputs steer towards. Larger is safer, smaller is snappier.
const TARGET_MS: f64 = 12.0;

/// Sample format of a stream's buffer; shared mode is always float, exclusive mode is not.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Sample {
    F32,
    I32,
    I16,
}

/// Turns one captured packet into mono float, whatever the stream delivers.
///
/// # Safety
/// `data` holds `frames * channels` samples in the `sample` format.
pub unsafe fn to_mono(data: *const u8, frames: usize, channels: usize, sample: Sample, out: &mut [f32]) {
    let n = frames * channels;
    let read = |i: usize| unsafe {
        match sample {
            Sample::F32 => *(data as *const f32).add(i),
            Sample::I32 => *(data as *const i32).add(i) as f32 / 2_147_483_648.0,
            Sample::I16 => *(data as *const i16).add(i) as f32 / 32_768.0,
        }
    };
    debug_assert!(out.len() >= frames && n >= frames);
    for (f, o) in out[..frames].iter_mut().enumerate() {
        *o = (0..channels).map(|c| read(f * channels + c)).sum::<f32>() / channels as f32;
    }
}

/// Writes `block` into a scope's taps from `start` on, wrapping around at the end.
pub fn tap(scope: &[AtomicU32], start: usize, block: &[f32]) {
    let mut j = start % scope.len();
    for s in block {
        scope[j].store(s.to_bits(), Relaxed);
        j += 1;
        if j == scope.len() {
            j = 0;
        }
    }
}

/// Mixes `share` of the untouched `dry` signal back into the voiced `block`.
pub fn blend(block: &mut [f32], dry: &[f32], share: f32) {
    for (s, d) in block.iter_mut().zip(dry) {
        *s = *s * (1.0 - share) + d * share;
    }
}

/// Pulls processed audio from the ring at the output's own clock.
pub struct Reader {
    ring: Consumer,
    rate: u32,
    frac: f64,
    s0: f32,
    s1: f32,
    target: f64,
    fill: f64,
    /// Fill at which playback (re)starts: the target plus half a mic packet, so the ring
    /// starts out at the top of its sawtooth rather than the bottom.
    start: f64,
    priming: bool,
    underruns: u32,
}

impl Reader {
    pub fn new(ring: Consumer, rate: u32) -> Self {
        let target = rate as f64 * TARGET_MS / 1000.0;
        Self {
            ring,
            rate,
            frac: 0.0,
            s0: 0.0,
            s1: 0.0,
            target,
            fill: target,
            start: target,
            priming: true,
            underruns: 0,
        }
    }

    /// Sizes the cushion from the mic's packet and this output's period, both in frames;
    /// a packet of 0 keeps the default. The ring has to bridge one mic packet plus 2 ms of
    /// scheduling jitter. When the output takes much smaller bites than the mic delivers
    /// (10 ms shared-mode packets into the cable's 2.67 ms periods), the fill sweeps evenly
    /// over a packet, so half a packet plus one period is enough to never run dry.
    pub fn set_target(&mut self, packet: u32, period: u32) {
        if packet == 0 {
            return;
        }
        let slack = self.rate / 500;
        let frames = (packet + slack).min(packet / 2 + period + slack) as f64;
        if frames != self.target {
            self.target = frames;
            self.fill = frames;
        }
        self.start = frames + (packet / 2) as f64;
    }

    /// Fills an output buffer of `channels`-sample frames, the same sample in each channel.
    pub fn render(&mut self, buf: &mut [f32], channels: usize) {
        for frame in buf.chunks_exact_mut(channels) {
            frame.fill(self.next());
        }
    }

    /// Samples waiting in the ring.
    pub fn queued(&self) -> usize {
        self.ring.len()
    }

    /// Times the ring ran dry so far.
    pub fn underruns(&self) -> u32 {
        self.underruns
    }

    fn next(&mut self) -> f32 {
        let avail = self.ring.len() as f64;
        if self.priming {
            if avail < self.start {
                return 0.0;
            }
            self.priming = false;
        }
        // After a long stall, throw away the backlog instead of playing it late.
        if avail > self.target * 4.0 {
            self.ring.skip((avail - self.target) as usize);
        }
        self.fill += (avail - self.fill) * 0.0005;
        // Proportional nudge, capped at ±0.5 %: inaudible, but enough for any real clock drift.
        let nudge = ((self.fill - self.target) / self.target * 0.02).clamp(-0.005, 0.005);
        self.frac += 1.0 + nudge;
        while self.frac >= 1.0 {
            self.frac -= 1.0;
            self.s0 = self.s1;
            match self.ring.pop() {
                Some(s) => self.s1 = s,
                None => {
                    self.priming = true;
                    self.underruns += 1;
                    return 0.0;
                }
            }
        }
        self.s0 + (self.s1 - self.s0) * self.frac as f32
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ring::ring;

    const RATE: u32 = 48_000;

    #[test]
    fn taps_wrap_around_the_scope() {
        let scope: Vec<AtomicU32> = (0..4).map(|_| AtomicU32::new(0)).collect();
        tap(&scope, 3, &[1.0, 2.0, 3.0]);
        let got: Vec<f32> = scope.iter().map(|s| f32::from_bits(s.load(Relaxed))).collect();
        assert_eq!(got, [2.0, 3.0, 0.0, 1.0]);
    }

    #[test]
    fn exclusive_formats_turn_into_mono_float() {
        let mut out = [0.0f32; 2];
        let i16_stereo: [i16; 4] = [16384, -16384, i16::MAX, i16::MAX];
        unsafe { to_mono(i16_stereo.as_ptr() as *const u8, 2, 2, Sample::I16, &mut out) };
        assert_eq!(out[0], 0.0);
        assert!((out[1] - 1.0).abs() < 1e-4);

        let i32_mono: [i32; 2] = [i32::MIN, 1 << 30];
        unsafe { to_mono(i32_mono.as_ptr() as *const u8, 2, 1, Sample::I32, &mut out) };
        assert_eq!(out, [-1.0, 0.5]);

        let f32_stereo: [f32; 4] = [0.25, 0.75, -1.0, 0.0];
        unsafe { to_mono(f32_stereo.as_ptr() as *const u8, 2, 2, Sample::F32, &mut out) };
        assert_eq!(out, [0.5, -0.5]);
    }

    #[test]
    fn a_smaller_mic_packet_shrinks_the_cushion() {
        // Exclusive mode: 3 ms packets, so the reader aims for 5 ms instead of 12.
        let (mut prod, cons) = ring(RATE as usize);
        let mut r = Reader::new(cons, RATE);
        let target = RATE * 5 / 1000;
        r.set_target(RATE * 3 / 1000, 128);
        assert_eq!(r.target, target as f64);
        let mut fed = 0.0f64;
        for _ in 0..20_000 {
            fed += RATE as f64 / 1000.0;
            while fed >= 1.0 {
                prod.push(0.1);
                fed -= 1.0;
            }
            for _ in 0..RATE / 1000 {
                r.next();
            }
        }
        let fill = r.ring.len() as u32;
        assert!(fill.abs_diff(target) < target / 2, "fill {fill}, target {target}");
        assert_eq!(r.underruns, 0);
    }

    #[test]
    fn shared_mode_packets_reach_the_cable_without_gaps() {
        // 10 ms mic packets, each up to 1 ms late, into the cable's 2.67 ms periods: the
        // cushion stays under one packet and the cable never runs dry, from the first packet on.
        let (packet, period) = (RATE / 100, 128);
        for drift in [-0.002, 0.0, 0.002] {
            let (mut prod, cons) = ring(RATE as usize);
            let mut r = Reader::new(cons, RATE);
            r.set_target(packet, period);
            assert!(r.target < packet as f64, "target {} frames", r.target);
            let (mut seed, mut sent, mut due) = (1u32, 0u32, 0.0f64);
            for t in 0..RATE * 60 {
                if t as f64 >= due {
                    (0..packet).for_each(|_| _ = prod.push(0.1));
                    sent += 1;
                    seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    let late = (seed >> 16) as f64 / 65_536.0 * RATE as f64 / 1000.0;
                    due = (sent * packet) as f64 / (1.0 + drift) + late;
                }
                if t % period == 0 {
                    (0..period).for_each(|_| _ = r.next());
                }
            }
            assert_eq!(r.underruns, 0, "drift {drift}");
        }
    }

    #[test]
    fn fill_stays_near_target_under_clock_drift() {
        let target = (RATE as f64 * TARGET_MS / 1000.0) as usize;
        for drift in [-0.002, 0.0, 0.002] {
            // Feed RATE * (1 + drift) samples per second, read RATE per second, for 60 s.
            let (mut prod, cons) = ring(RATE as usize);
            let mut r = Reader::new(cons, RATE);
            let mut fed = 0.0f64;
            for _ in 0..60_000 {
                fed += RATE as f64 * (1.0 + drift) / 1000.0;
                while fed >= 1.0 {
                    prod.push(0.1);
                    fed -= 1.0;
                }
                for _ in 0..RATE / 1000 {
                    r.next();
                }
            }
            let fill = r.ring.len();
            assert!(fill.abs_diff(target) < target / 2, "drift {drift}: fill {fill}, target {target}");
        }
    }
}
