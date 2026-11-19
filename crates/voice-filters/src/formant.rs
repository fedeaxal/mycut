//! Pitch and timbre moved apart.
//!
//! [`PitchShifter`](crate::dsp::PitchShifter) plays the voice faster or slower, so the
//! resonances of the throat and mouth (the formants) move with the pitch: up is a chipmunk,
//! down is a giant, and neither is another person. A person with a higher voice has
//! slightly smaller resonances, not ones scaled by the same amount.
//!
//! Here the voice is taken apart into grains, each about two cycles long and centred on the
//! loudest part of a cycle, where the vocal folds close and the mouth rings. Laying the
//! grains closer together or further apart changes the pitch and leaves the ringing, the
//! timbre, alone. Reading each grain faster or slower moves the timbre and leaves the pitch
//! alone. (Pitch-synchronous overlap-add.)

use crate::dsp::dot;

/// The tracker listens at a quarter of the sample rate: a voice's pitch lives under
/// 500 Hz, and a search over a quarter of the samples and a quarter of the lags costs a
/// sixteenth.
const DECIMATE: usize = 4;

/// Finds how long one cycle of the voice is, two hundred times a second.
///
/// The measure is YIN's: how different the last 20 ms are from themselves one lag
/// earlier, divided by the average of that difference over all shorter lags. The first lag
/// where it dips low is the period; picking the first one, not the deepest, is what keeps
/// it from answering with two cycles instead of one.
pub struct PitchTracker {
    /// The decimated signal, kept in one straight run so every window is contiguous.
    low: Vec<f32>,
    fill: usize,
    sum: f32,
    count: usize,
    since: usize,
    hop: usize,
    window: usize,
    min_lag: usize,
    max_lag: usize,
    energy: Vec<f64>,
    diff: Vec<f32>,
    period: f32,
    voiced: bool,
    /// The last estimate was an octave away from the one before and was not believed yet.
    doubt: bool,
}

impl PitchTracker {
    pub fn new(sample_rate: u32) -> Self {
        let rate = sample_rate as usize / DECIMATE;
        // 60 to 420 Hz: a low man to a child, with room for a raised voice.
        let (min_lag, max_lag, window) = (rate / 420, rate / 60, rate / 50);
        let span = window + max_lag;
        Self {
            low: vec![0.0; 2 * span],
            fill: 0,
            sum: 0.0,
            count: 0,
            since: 0,
            hop: rate / 200,
            window,
            min_lag,
            max_lag,
            energy: vec![0.0; span + 1],
            diff: vec![0.0; max_lag + 2],
            period: sample_rate as f32 / 125.0,
            voiced: false,
            doubt: false,
        }
    }

    /// Length of one cycle in samples; the last one heard while nobody is speaking.
    pub fn period(&self) -> f32 {
        self.period
    }

    /// Whether the sound right now has a pitch at all: a vowel has, an "s" or a breath has not.
    pub fn voiced(&self) -> bool {
        self.voiced
    }

    /// Takes a sample in without looking for the pitch.
    #[inline]
    pub fn feed(&mut self, x: f32) -> bool {
        self.sum += x;
        self.count += 1;
        if self.count < DECIMATE {
            return false;
        }
        if self.fill == self.low.len() {
            let span = self.window + self.max_lag;
            self.low.copy_within(self.fill - span.., 0);
            self.fill = span;
        }
        self.low[self.fill] = self.sum / DECIMATE as f32;
        self.fill += 1;
        (self.sum, self.count) = (0.0, 0);
        true
    }

    #[inline]
    pub fn push(&mut self, x: f32) {
        if self.feed(x) {
            self.since += 1;
            if self.since >= self.hop && self.fill >= self.window + self.max_lag {
                self.since = 0;
                self.analyse();
            }
        }
    }

    fn analyse(&mut self) {
        let (n, longest) = (self.window, self.max_lag);
        let s = &self.low[self.fill - (n + longest)..self.fill];
        let mut sum = 0.0f64;
        self.energy[0] = 0.0;
        for (k, v) in s.iter().enumerate() {
            sum += (v * v) as f64;
            self.energy[k + 1] = sum;
        }
        let energy = &self.energy;
        let own = energy[n + longest] - energy[longest];
        // Under about -66 dB there is nothing to measure, only the room.
        if own < n as f64 * 2.5e-7 {
            self.voiced = false;
            return;
        }
        let newest = &s[longest..];
        let mut running = 0.0f32;
        self.diff[0] = 1.0;
        for (lag, slot) in self.diff.iter_mut().enumerate().take(longest + 1).skip(1) {
            let from = longest - lag;
            let earlier = energy[from + n] - energy[from];
            let d = ((own + earlier) as f32 - 2.0 * dot(newest, &s[from..from + n])).max(0.0);
            running += d;
            *slot = d * lag as f32 / running.max(1e-12);
        }
        self.diff[longest + 1] = 1.0;

        let diff = &self.diff;
        let mut lag = (self.min_lag..=longest).find(|&l| diff[l] < 0.15);
        if let Some(l) = lag.as_mut() {
            while *l < longest && diff[*l + 1] < diff[*l] {
                *l += 1;
            }
        }
        let lag = lag.unwrap_or_else(|| {
            (self.min_lag..=longest).fold(self.min_lag, |best, l| if diff[l] < diff[best] { l } else { best })
        });
        self.voiced = diff[lag] < 0.4;
        if !self.voiced {
            return;
        }
        // The dip is rarely on a whole sample: fit a parabola through it and its neighbours.
        let (a, b, c) = (diff[lag - 1], diff[lag], diff[lag + 1]);
        let bend = a - 2.0 * b + c;
        let nudge = if bend > 1e-9 { (0.5 * (a - c) / bend).clamp(-0.5, 0.5) } else { 0.0 };
        let period = (lag as f32 + nudge) * DECIMATE as f32;
        // One frame an octave off is a slip of the search far more often than a real leap
        // of the voice; a second one in a row is believed.
        let leap = period / self.period;
        if !(0.55..=1.8).contains(&leap) && !self.doubt {
            self.doubt = true;
        } else {
            self.period = period;
            self.doubt = false;
        }
    }
}

/// One windowed cycle of the voice on its way to the output.
#[derive(Clone, Copy, Default)]
struct Grain {
    /// How far behind the write head its middle is read from.
    lag: f32,
    /// Samples since its middle; negative while it fades in.
    t: f32,
    /// Half its length in samples; zero for a free slot.
    half: f32,
    /// How much the read position falls behind per sample: `1 - formant`.
    step: f32,
    gain: f32,
}

const GRAINS: usize = 8;

/// `f32::clamp` without its assertion: that one prints both bounds when they are the wrong
/// way round, which pulls the whole float formatter (24 KB) into the program. These bounds
/// never are.
#[inline]
fn clamp(x: f32, low: f32, high: f32) -> f32 {
    if x < low {
        low
    } else if x > high {
        high
    } else {
        x
    }
}

/// Changes the pitch and the timbre of a voice separately.
pub struct FormantShifter {
    buf: Vec<f32>,
    mask: usize,
    write: usize,
    tracker: PitchTracker,
    pitch: f32,
    formant: f32,
    grains: [Grain; GRAINS],
    /// Samples until the next grain starts.
    next: f32,
    /// How far behind the write head the middle of the newest grain's source is. Negative
    /// while that middle has not arrived yet.
    anchor: f32,
    /// Whether the newest grain sits on a cycle of a voiced sound.
    anchored: bool,
    /// Grains in a row whose place was far from the loud part of the cycle.
    astray: u8,
    seed: u32,
    line: Vec<f32>,
    held: Vec<f32>,
    energy: Vec<f64>,
    min_half: f32,
    max_half: f32,
    min_match: usize,
    max_match: usize,
    max_reach: usize,
}

impl FormantShifter {
    pub fn new(sample_rate: u32) -> Self {
        let sr = sample_rate as usize;
        let len = (sr * 17 / 100).next_power_of_two();
        let (max_match, max_reach) = (sr / 94, sr / 1000);
        Self {
            buf: vec![0.0; len],
            mask: len - 1,
            write: 0,
            tracker: PitchTracker::new(sample_rate),
            pitch: 1.0,
            formant: 1.0,
            grains: [Grain::default(); GRAINS],
            next: 0.0,
            anchor: 0.0,
            anchored: false,
            astray: 0,
            seed: 0x2545_f491,
            line: vec![0.0; max_match + 2 * max_reach],
            held: vec![0.0; max_match],
            energy: vec![0.0; sr / 30],
            min_half: sr as f32 / 3000.0,
            max_half: sr as f32 / 50.0,
            min_match: sr / 500,
            max_match,
            max_reach,
        }
    }

    /// Both are ratios: 2.0 is an octave up, and a `formant` of 1.0 keeps the speaker's own timbre.
    pub fn set(&mut self, pitch: f32, formant: f32) {
        self.pitch = pitch.clamp(0.25, 4.0);
        self.formant = formant.clamp(0.5, 2.0);
    }

    /// Length of one cycle of the voice in samples, as last measured.
    pub fn period(&self) -> f32 {
        self.tracker.period()
    }

    pub fn voiced(&self) -> bool {
        self.tracker.voiced()
    }

    /// How far behind the microphone the output is on average, in samples.
    pub fn delay(&self) -> f32 {
        let period = self.tracker.period();
        (1.0 - self.formant).abs() * period / self.formant + 3.0 + period / 2.0
    }

    /// Records without playing, for a voice that does not use it, so it never comes back
    /// holding speech from whenever it last ran.
    #[inline]
    pub fn listen(&mut self, x: f32) {
        self.buf[self.write] = x;
        self.write = (self.write + 1) & self.mask;
        self.tracker.feed(x);
        self.anchor += 1.0;
        self.anchored = false;
    }

    #[inline]
    fn behind(&self, delay: usize) -> f32 {
        self.buf[self.write.wrapping_sub(delay) & self.mask]
    }

    #[inline]
    fn tap(&self, delay: f32) -> f32 {
        let pos = self.write as f32 - delay;
        let i = pos.floor();
        let t = pos - i;
        let i = i as isize;
        let at = |k: isize| self.buf[(i + k) as usize & self.mask];
        let (y0, y1, y2, y3) = (at(-1), at(0), at(1), at(2));
        let c1 = 0.5 * (y2 - y0);
        let c2 = y0 - 2.5 * y1 + 2.0 * y2 - 0.5 * y3;
        let c3 = 0.5 * (y3 - y0) + 1.5 * (y1 - y2);
        ((c3 * t + c2) * t + c1) * t + y1
    }

    #[inline]
    pub fn process(&mut self, x: f32) -> f32 {
        self.buf[self.write] = x;
        self.tracker.push(x);
        self.next -= 1.0;
        if self.next <= 0.0 {
            self.spawn();
        }
        let mut y = 0.0;
        for k in 0..GRAINS {
            let g = self.grains[k];
            if g.half == 0.0 {
                continue;
            }
            let u = (g.t / g.half).abs();
            if u < 1.0 {
                // Two of these one half-length apart add up to exactly one.
                let window = 1.0 - u * u * (3.0 - 2.0 * u);
                y += window * g.gain * self.tap(g.lag + g.t * g.step);
            }
            self.grains[k].t = g.t + 1.0;
            if g.t + 1.0 >= g.half {
                self.grains[k].half = 0.0;
            }
        }
        self.anchor += 1.0;
        self.write = (self.write + 1) & self.mask;
        y
    }

    /// Starts the next grain: where in the recent past it reads from decides everything
    /// about how the result sounds.
    fn spawn(&mut self) {
        let period = self.tracker.period();
        // How far apart grains start sets the pitch; how long each one is, read at the
        // formant's speed, is always two cycles of the source.
        let apart = clamp(period / self.pitch, self.min_half, self.max_half);
        let half = clamp(period / self.formant, self.min_half, (4.0 * apart).min(self.max_half));
        let step = 1.0 - self.formant;
        // A grain read faster than time passes catches up with the microphone by its end,
        // and one read slower starts ahead of where it will be: either way it needs this
        // much of a head start to never ask for a sample that has not arrived.
        let floor = step.abs() * half + 3.0;
        let ahead = half + self.next;

        // A whole number of cycles on from the grain before, so the two line up.
        let turns = ((self.anchor + ahead - floor) / period).floor();
        let mut lag = self.anchor + ahead - turns * period;
        if self.tracker.voiced() {
            let loud = self.loudest(period) + ahead;
            let wanted = loud - ((loud - floor) / period).floor() * period;
            if self.anchored {
                lag -= self.line_up(lag - ahead, period, lag - floor);
            }
            let mut off = wanted - lag;
            if off > period / 2.0 {
                off -= period;
            } else if off < -period / 2.0 {
                off += period;
            }
            self.astray = if off.abs() > period / 3.0 { self.astray + 1 } else { 0 };
            if !self.anchored || self.astray >= 3 {
                // A voice just started, or the grains have slid off the loud part of the
                // cycle: one join that does not line up is the price of landing on it.
                lag = wanted;
                self.astray = 0;
            } else {
                // Otherwise creep towards it, a sample a grain: too little to hear.
                lag = (lag + off.clamp(-1.0, 1.0)).max(floor);
            }
            self.anchored = true;
        } else {
            // Noise has no cycle. Repeating the same stretch of it at the grain rate would
            // give it one, a buzz inside every "s", so each grain reads from a random spot.
            lag = floor + self.random() * period;
            self.anchored = false;
        }

        let slot = (0..GRAINS).find(|&k| self.grains[k].half == 0.0).unwrap_or_else(|| {
            let done = |g: &Grain| g.t / g.half;
            (1..GRAINS).fold(0, |best, k| if done(&self.grains[k]) > done(&self.grains[best]) { k } else { best })
        });
        // Grains closer together than they are long pile up; this keeps the level.
        self.grains[slot] = Grain { lag, t: -ahead, half, step, gain: (apart / half).min(1.0) };
        self.anchor = lag - ahead;
        self.next += apart;
    }

    /// Where in the last cycle the voice is loudest, in samples behind the write head: the
    /// middle of the most energetic third of it.
    fn loudest(&mut self, period: f32) -> f32 {
        let cycle = (period as usize).min(self.energy.len() * 3 / 4 - 1);
        let width = (cycle / 3).max(8);
        let mut sum = 0.0f64;
        self.energy[0] = 0.0;
        for k in 0..cycle + width {
            let v = self.behind(k);
            sum += (v * v) as f64;
            self.energy[k + 1] = sum;
        }
        let energy = &self.energy;
        let power = |from: usize| energy[from + width] - energy[from];
        let from = (1..cycle).fold(0, |best, k| if power(k) > power(best) { k } else { best });
        (from + width / 2) as f32
    }

    /// How many samples later the new grain should read so its waveform lies on top of the
    /// previous grain's, at most `room`. `fresh` is where its middle would be read from,
    /// in samples behind the write head; both middles may still be in the future, so the
    /// stretches compared are the ones the same distance before each.
    fn line_up(&mut self, fresh: f32, period: f32, room: f32) -> f32 {
        let n = (period as usize).clamp(self.min_match, self.max_match);
        let reach = ((period / 8.0) as usize).clamp(2, self.max_reach);
        let later = reach.min(room.max(0.0) as usize);
        let back = (-fresh.min(self.anchor)).max(0.0) as usize + reach + 2;
        let old = self.anchor.round() as isize + back as isize;
        let new = fresh.round() as isize + back as isize;
        if old < 0 || new < reach as isize {
            return 0.0;
        }
        let (old, new) = (old as usize, new as usize);

        let mut own = 0.0f32;
        for j in 0..n {
            let v = self.behind(old + n - 1 - j);
            self.held[j] = v;
            own += v * v;
        }
        let span = n + reach + later;
        let mut sum = 0.0f64;
        self.energy[0] = 0.0;
        for i in 0..span {
            let v = self.behind(new + reach + n - 1 - i);
            self.line[i] = v;
            sum += (v * v) as f64;
            self.energy[i + 1] = sum;
        }
        if own < 1e-9 {
            return 0.0;
        }
        let (held, line, energy) = (&self.held[..n], &self.line[..span], &self.energy);
        let mut best = (reach, f32::MIN);
        for at in 0..=reach + later {
            let other = (energy[at + n] - energy[at]) as f32 + 1e-9;
            let score = dot(held, &line[at..at + n]) / (own * other).sqrt();
            if score > best.1 {
                best = (at, score);
            }
        }
        if best.1 > 0.5 { best.0 as f32 - reach as f32 } else { 0.0 }
    }

    fn random(&mut self) -> f32 {
        self.seed = self.seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (self.seed >> 8) as f32 / (1 << 24) as f32
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::PitchShifter;
    use crate::fx::{Biquad, Noise, Shape};
    use std::f32::consts::TAU;

    const SR: u32 = 48_000;

    /// A pulse for every closing of the vocal folds, rung through two resonances: the
    /// simplest thing that has both a pitch and a timbre to tell apart.
    fn vowel(pitch_hz: f32, secs: f32) -> Vec<f32> {
        let sr = SR as f32;
        let mut first = Biquad::new(Shape::Band, sr, 700.0, 6.0);
        let mut second = Biquad::new(Shape::Band, sr, 1200.0, 8.0);
        let mut phase = 0.0f32;
        (0..(sr * secs) as usize)
            .map(|_| {
                phase += pitch_hz / sr;
                let pulse = if phase >= 1.0 {
                    phase -= 1.0;
                    1.0
                } else {
                    0.0
                };
                (first.process(pulse) + 0.7 * second.process(pulse)) * 4.0
            })
            .collect()
    }

    fn shifted(input: &[f32], pitch: f32, formant: f32) -> Vec<f32> {
        let mut s = FormantShifter::new(SR);
        s.set(pitch, formant);
        input.iter().map(|&x| s.process(x)).collect()
    }

    /// The settled second half, where the tracker has long found the voice.
    fn tail(x: &[f32]) -> &[f32] {
        &x[x.len() / 2..]
    }

    /// Pitch by autocorrelation: the shortest lag that matches nearly as well as the best.
    fn pitch_of(x: &[f32]) -> f32 {
        let n = 6000;
        let x = &x[..n + 900];
        let score = |lag: usize| -> f32 { (0..n).map(|i| x[i] * x[i + lag]).sum() };
        let scores: Vec<f32> = (100..900).map(score).collect();
        let best = scores.iter().cloned().fold(f32::MIN, f32::max);
        let lag = 100 + scores.iter().position(|&s| s > 0.9 * best).unwrap();
        let lag = (lag..lag + 30).fold(lag, |b, l| if scores[l - 100] > scores[b - 100] { l } else { b });
        SR as f32 / lag as f32
    }

    /// Where the weight of the spectrum between 300 and 3000 Hz lies. Measured over
    /// frames of 21 ms, too short to tell one harmonic from the next, so it follows the
    /// formants and not the pitch.
    fn brightness(x: &[f32]) -> f32 {
        const FRAME: usize = 1024;
        let (mut weighted, mut total) = (0.0f64, 0.0f64);
        for step in 0..=108 {
            let hz = 300.0 + 25.0 * step as f32;
            let mut power = 0.0f64;
            for frame in x[..16_384].chunks_exact(FRAME) {
                let (mut re, mut im) = (0.0f32, 0.0f32);
                for (i, v) in frame.iter().enumerate() {
                    let hann = 0.5 - 0.5 * (i as f32 * TAU / FRAME as f32).cos();
                    let (sin, cos) = (i as f32 * hz * TAU / SR as f32).sin_cos();
                    re += v * hann * cos;
                    im += v * hann * sin;
                }
                power += (re * re + im * im) as f64;
            }
            weighted += power * hz as f64;
            total += power;
        }
        (weighted / total) as f32
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt()
    }

    #[test]
    fn the_tracker_finds_the_pitch_of_a_voice() {
        for hz in [85.0, 120.0, 180.0, 240.0, 330.0] {
            let mut tracker = PitchTracker::new(SR);
            for x in vowel(hz, 0.5) {
                tracker.push(x);
            }
            let found = SR as f32 / tracker.period();
            assert!(tracker.voiced(), "{hz} Hz was not heard as a voice");
            assert!((found / hz - 1.0).abs() < 0.02, "{hz} Hz was taken for {found}");
        }
    }

    #[test]
    fn the_tracker_hears_no_pitch_in_noise_or_silence() {
        let mut noise = Noise::default();
        let mut tracker = PitchTracker::new(SR);
        let mut voiced = 0;
        for _ in 0..SR {
            tracker.push(noise.next() * 0.3);
            voiced += tracker.voiced() as u32;
        }
        assert!(voiced < SR / 20, "noise had a pitch for {voiced} samples");
        let mut tracker = PitchTracker::new(SR);
        for _ in 0..SR {
            tracker.push(0.0);
        }
        assert!(!tracker.voiced());
    }

    #[test]
    fn pitch_goes_up_and_the_timbre_stays() {
        let input = vowel(120.0, 2.0);
        let out = shifted(&input, 1.5, 1.0);
        let pitch = pitch_of(tail(&out));
        assert!((pitch / 180.0 - 1.0).abs() < 0.03, "pitch {pitch}");
        let moved = brightness(tail(&out)) / brightness(tail(&input));
        assert!((0.93..1.07).contains(&moved), "timbre moved by {moved}");
    }

    #[test]
    fn pitch_goes_down_and_the_timbre_stays() {
        let input = vowel(150.0, 2.0);
        let out = shifted(&input, 0.75, 1.0);
        let pitch = pitch_of(tail(&out));
        assert!((pitch / 112.5 - 1.0).abs() < 0.03, "pitch {pitch}");
        let moved = brightness(tail(&out)) / brightness(tail(&input));
        assert!((0.93..1.07).contains(&moved), "timbre moved by {moved}");
    }

    #[test]
    fn timbre_moves_and_the_pitch_stays() {
        let input = vowel(120.0, 2.0);
        for formant in [0.85, 1.2] {
            let out = shifted(&input, 1.0, formant);
            let pitch = pitch_of(tail(&out));
            assert!((pitch / 120.0 - 1.0).abs() < 0.02, "pitch {pitch} at formant {formant}");
            let moved = brightness(tail(&out)) / brightness(tail(&input));
            assert!((moved / formant - 1.0).abs() < 0.07, "timbre moved by {moved}, not {formant}");
        }
    }

    #[test]
    fn the_plain_shifter_drags_the_timbre_along() {
        let input = vowel(120.0, 2.0);
        let mut plain = PitchShifter::new(SR);
        plain.set_ratio(1.5);
        let out: Vec<f32> = input.iter().map(|&x| plain.process(x)).collect();
        let moved = brightness(tail(&out)) / brightness(tail(&input));
        assert!(moved > 1.3, "timbre moved by only {moved}");
    }

    #[test]
    fn the_level_holds() {
        let input = vowel(130.0, 2.0);
        for (pitch, formant) in [(1.0, 1.0), (1.5, 1.0), (1.5, 1.15), (0.75, 0.87), (2.0, 1.25)] {
            let out = shifted(&input, pitch, formant);
            let gain = rms(tail(&out)) / rms(tail(&input));
            assert!((0.6..1.6).contains(&gain), "level changed {gain} times at {pitch} / {formant}");
        }
    }

    #[test]
    fn silence_stays_silent_and_noise_stays_bounded() {
        let out = shifted(&vec![0.0; SR as usize], 1.5, 1.2);
        assert!(out.iter().all(|&y| y == 0.0));
        let mut noise = Noise::default();
        let input: Vec<f32> = (0..SR * 2).map(|_| noise.next() * 0.5).collect();
        for (pitch, formant) in [(0.25, 0.5), (4.0, 2.0), (4.0, 0.5), (0.25, 2.0), (1.3, 1.1)] {
            let out = shifted(&input, pitch, formant);
            assert!(out.iter().all(|y| y.is_finite() && y.abs() < 2.0), "blew up at {pitch} / {formant}");
            assert!(rms(tail(&out)) > 0.05, "noise went missing at {pitch} / {formant}");
        }
    }

    #[test]
    fn the_voice_comes_out_within_15_ms() {
        let input = vowel(110.0, 1.0);
        let mut s = FormantShifter::new(SR);
        s.set(1.4, 1.15);
        let mut worst = 0.0f32;
        for (i, &x) in input.iter().enumerate() {
            s.process(x);
            if i > SR as usize / 2 {
                worst = worst.max(s.grains.iter().filter(|g| g.half > 0.0).map(|g| g.lag).fold(0.0, f32::max));
            }
        }
        assert!(worst < 0.015 * SR as f32, "a grain read {worst} samples back");
        assert!(s.delay() < 0.012 * SR as f32);
    }

    /// `cargo test --release -p goby-dsp formant_cost -- --ignored --nocapture`
    #[test]
    #[ignore = "benchmark"]
    fn formant_cost() {
        let input = vowel(120.0, 10.0);
        for (pitch, formant) in [(1.5, 1.15), (0.75, 0.87), (2.0, 1.25)] {
            let t = std::time::Instant::now();
            let out = shifted(&input, pitch, formant);
            let share = t.elapsed().as_secs_f64() / 10.0 * 100.0;
            println!("{pitch} / {formant}: {share:.3} % of one core, rms {:.3}", rms(&out));
        }
    }
}
