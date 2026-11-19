//! Small effect building blocks the voices are made of. All of them allocate once in
//! `new` and then run per sample in the audio callback without locks or allocation.

use std::f32::consts::TAU;

/// `f32::clamp` without its `min <= max` check: that panic prints both floats, and that
/// alone links core's float formatter into the exe. Same result for every input.
#[inline]
pub fn clamp(x: f32, lo: f32, hi: f32) -> f32 {
    let x = if x < lo { lo } else { x };
    if x > hi { hi } else { x }
}

/// Sine LFO.
#[derive(Clone, Copy, Default)]
pub struct Lfo {
    phase: f32,
}

impl Lfo {
    #[inline]
    pub fn next(&mut self, hz: f32, sample_rate: f32) -> f32 {
        self.phase = (self.phase + hz / sample_rate).fract();
        (self.phase * TAU).sin()
    }
}

/// xorshift32 white noise in -1..1; deterministic, cheap, good enough for audio.
#[derive(Clone, Copy)]
pub struct Noise(u32);

impl Default for Noise {
    fn default() -> Self {
        Noise(0x9e37_79b9)
    }
}

impl Noise {
    /// Not an `Iterator`: it never ends, and `Option` would only get in the way.
    #[allow(clippy::should_implement_trait)]
    #[inline]
    pub fn next(&mut self) -> f32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.0 = x;
        (x as f32 / u32::MAX as f32) * 2.0 - 1.0
    }
}

/// Slowly wandering random value in -1..1 (smoothed noise), for "drunk" and "mutation" wobble.
#[derive(Clone, Copy, Default)]
pub struct Wander {
    noise: Noise,
    value: f32,
    target: f32,
    countdown: u32,
}

impl Wander {
    #[inline]
    pub fn next(&mut self, hz: f32, sample_rate: f32) -> f32 {
        if self.countdown == 0 {
            self.target = self.noise.next();
            self.countdown = (sample_rate / hz.max(0.01)) as u32;
        }
        self.countdown -= 1;
        self.value += (self.target - self.value) * (hz * 2.0 / sample_rate).min(1.0);
        self.value
    }
}

/// RBJ biquad, transposed direct form II.
#[derive(Clone, Copy, Default)]
pub struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    z1: f32,
    z2: f32,
}

pub enum Shape {
    Low,
    High,
    Band,
}

impl Biquad {
    pub fn new(shape: Shape, sample_rate: f32, freq: f32, q: f32) -> Self {
        let w = TAU * (freq / sample_rate).min(0.49);
        let (sin, cos) = w.sin_cos();
        let alpha = sin / (2.0 * q);
        let a0 = 1.0 + alpha;
        let (b0, b1, b2) = match shape {
            Shape::Low => ((1.0 - cos) / 2.0, 1.0 - cos, (1.0 - cos) / 2.0),
            Shape::High => ((1.0 + cos) / 2.0, -(1.0 + cos), (1.0 + cos) / 2.0),
            Shape::Band => (alpha, 0.0, -alpha),
        };
        Self { b0: b0 / a0, b1: b1 / a0, b2: b2 / a0, a1: -2.0 * cos / a0, a2: (1.0 - alpha) / a0, z1: 0.0, z2: 0.0 }
    }

    /// A bell: boosts or cuts a band around `freq` by `gain_db`, everything else passes.
    pub fn peak(sample_rate: f32, freq: f32, q: f32, gain_db: f32) -> Self {
        let w = TAU * (freq / sample_rate).min(0.49);
        let (sin, cos) = w.sin_cos();
        let alpha = sin / (2.0 * q);
        let a = 10f32.powf(gain_db / 40.0);
        let a0 = 1.0 + alpha / a;
        Self {
            b0: (1.0 + alpha * a) / a0,
            b1: -2.0 * cos / a0,
            b2: (1.0 - alpha * a) / a0,
            a1: -2.0 * cos / a0,
            a2: (1.0 - alpha / a) / a0,
            z1: 0.0,
            z2: 0.0,
        }
    }

    /// Raises or lowers everything below `freq`: bass weight or rumble cut.
    pub fn low_shelf(sample_rate: f32, freq: f32, gain_db: f32) -> Self {
        Self::shelf(sample_rate, freq, gain_db, false)
    }

    /// Raises or lowers everything above `freq`: air, or taming a harsh top end.
    pub fn high_shelf(sample_rate: f32, freq: f32, gain_db: f32) -> Self {
        Self::shelf(sample_rate, freq, gain_db, true)
    }

    /// RBJ shelf with slope 1, so the transition is as steep as it gets without overshoot.
    fn shelf(sample_rate: f32, freq: f32, gain_db: f32, high: bool) -> Self {
        let w = TAU * (freq / sample_rate).min(0.49);
        let (sin, cos) = w.sin_cos();
        let a = 10f32.powf(gain_db / 40.0);
        let alpha = sin * std::f32::consts::FRAC_1_SQRT_2;
        let beta = 2.0 * a.sqrt() * alpha;
        let (ap, am) = (a + 1.0, a - 1.0);
        let (b0, b1, b2, a0, a1, a2) = if high {
            (
                a * (ap + am * cos + beta),
                -2.0 * a * (am + ap * cos),
                a * (ap + am * cos - beta),
                ap - am * cos + beta,
                2.0 * (am - ap * cos),
                ap - am * cos - beta,
            )
        } else {
            (
                a * (ap - am * cos + beta),
                2.0 * a * (am - ap * cos),
                a * (ap - am * cos - beta),
                ap + am * cos + beta,
                -2.0 * (am + ap * cos),
                ap + am * cos - beta,
            )
        };
        Self { b0: b0 / a0, b1: b1 / a0, b2: b2 / a0, a1: a1 / a0, a2: a2 / a0, z1: 0.0, z2: 0.0 }
    }

    #[inline]
    pub fn process(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.z1;
        self.z1 = self.b1 * x - self.a1 * y + self.z2;
        self.z2 = self.b2 * x - self.a2 * y;
        y
    }

    pub fn clear(&mut self) {
        (self.z1, self.z2) = (0.0, 0.0);
    }
}

/// Power-of-two delay line with fractional (linear) reads.
pub struct Delay {
    buf: Vec<f32>,
    mask: usize,
    write: usize,
}

impl Delay {
    pub fn new(max_samples: usize) -> Self {
        let len = (max_samples + 2).next_power_of_two();
        Self { buf: vec![0.0; len], mask: len - 1, write: 0 }
    }

    #[inline]
    pub fn push(&mut self, x: f32) {
        self.write = (self.write + 1) & self.mask;
        self.buf[self.write] = x;
    }

    /// The sample `delay` samples ago (0 = the one just pushed).
    #[inline]
    pub fn read(&self, delay: f32) -> f32 {
        let d = clamp(delay, 0.0, self.mask as f32 - 1.0);
        let i = d as usize;
        let t = d - i as f32;
        let a = self.buf[(self.write.wrapping_sub(i)) & self.mask];
        let b = self.buf[(self.write.wrapping_sub(i + 1)) & self.mask];
        a + (b - a) * t
    }

    pub fn clear(&mut self) {
        self.buf.fill(0.0);
    }
}

/// Modulated short delays: chorus (no feedback) and flanger (feedback, shorter delay).
pub struct Modulated {
    line: Delay,
    lfo: [Lfo; 2],
    sample_rate: f32,
    last: f32,
}

impl Modulated {
    pub fn new(sample_rate: f32) -> Self {
        let mut lfo = [Lfo::default(); 2];
        lfo[1].phase = 0.33;
        Self { line: Delay::new((sample_rate * 0.04) as usize), lfo, sample_rate, last: 0.0 }
    }

    /// Two voices 12-25 ms behind, wobbling slowly: a fuller, doubled voice.
    #[inline]
    pub fn chorus(&mut self, x: f32) -> f32 {
        self.line.push(x);
        let ms = self.sample_rate / 1000.0;
        let a = self.line.read((15.0 + 4.0 * self.lfo[0].next(0.7, self.sample_rate)) * ms);
        let b = self.line.read((21.0 + 4.0 * self.lfo[1].next(0.9, self.sample_rate)) * ms);
        (a + b) * 0.5
    }

    /// 1-7 ms sweep with feedback: the jet-plane swoosh.
    #[inline]
    pub fn flanger(&mut self, x: f32, rate_hz: f32) -> f32 {
        self.line.push(x + self.last * 0.6);
        let ms = self.sample_rate / 1000.0;
        self.last = self.line.read((4.0 + 3.0 * self.lfo[0].next(rate_hz, self.sample_rate)) * ms);
        self.last
    }

    pub fn clear(&mut self) {
        self.line.clear();
        self.last = 0.0;
    }
}

/// Feedback echo with a darkening low-pass in the loop, like tape or a canyon wall.
pub struct Echo {
    line: Delay,
    tone: Biquad,
    sample_rate: f32,
}

impl Echo {
    pub fn new(sample_rate: f32) -> Self {
        Self {
            line: Delay::new(sample_rate as usize),
            tone: Biquad::new(Shape::Low, sample_rate, 3500.0, 0.7),
            sample_rate,
        }
    }

    /// Returns only the echoes; the caller mixes them with the dry signal.
    #[inline]
    pub fn process(&mut self, x: f32, ms: f32, feedback: f32) -> f32 {
        // Read before this sample is pushed, so "0 samples ago" is already one sample late.
        let wet = self.line.read(ms * self.sample_rate / 1000.0 - 1.0);
        self.line.push(x + self.tone.process(wet) * feedback);
        wet
    }

    pub fn clear(&mut self) {
        self.line.clear();
        self.tone.clear();
    }
}

/// Schroeder/Freeverb-style mono reverb: parallel damped combs into series all-passes.
pub struct Reverb {
    combs: [(Delay, usize, f32); 4],
    allpasses: [(Delay, usize); 2],
}

impl Reverb {
    pub fn new(sample_rate: f32) -> Self {
        let scale = sample_rate / 44_100.0;
        let comb = |n: f32| (Delay::new((n * scale * 1.6) as usize), (n * scale) as usize, 0.0);
        let ap = |n: f32| (Delay::new((n * scale) as usize + 2), (n * scale) as usize);
        Self { combs: [comb(1116.0), comb(1188.0), comb(1277.0), comb(1356.0)], allpasses: [ap(556.0), ap(441.0)] }
    }

    /// `size` 0..1 stretches the room and its decay; `damp` 0..1 darkens the tail.
    /// Returns only the reverberation.
    #[inline]
    pub fn process(&mut self, x: f32, size: f32, damp: f32) -> f32 {
        // Capped so even the biggest hall dies out within about a second: in voice chat a
        // long tail smears the next sentence.
        let feedback = 0.70 + 0.22 * size;
        let stretch = 0.6 + 0.6 * size;
        let mut out = 0.0;
        for (line, len, low) in &mut self.combs {
            let y = line.read(*len as f32 * stretch);
            *low = y * (1.0 - damp) + *low * damp;
            line.push(x * 0.25 + *low * feedback);
            out += y;
        }
        for (line, len) in &mut self.allpasses {
            let delayed = line.read(*len as f32);
            line.push(out + delayed * 0.5);
            out = delayed - out * 0.5;
        }
        out * 0.5
    }

    pub fn clear(&mut self) {
        for (line, _, low) in &mut self.combs {
            line.clear();
            *low = 0.0;
        }
        for (line, _) in &mut self.allpasses {
            line.clear();
        }
    }
}

/// Lo-fi: holds samples to fake a lower sample rate and quantises to fewer bits.
#[derive(Clone, Copy, Default)]
pub struct Crusher {
    held: f32,
    phase: f32,
}

impl Crusher {
    #[inline]
    pub fn process(&mut self, x: f32, rate_hz: f32, bits: u32, sample_rate: f32) -> f32 {
        self.phase += rate_hz / sample_rate;
        if self.phase >= 1.0 {
            self.phase -= 1.0;
            let steps = (1u32 << bits.clamp(1, 16)) as f32 / 2.0;
            self.held = (x * steps).round() / steps;
        }
        self.held
    }

    pub fn clear(&mut self) {
        self.held = 0.0;
    }
}

/// Envelope follower: fast attack, slower release, in linear amplitude.
#[derive(Clone, Copy, Default)]
pub struct Envelope {
    level: f32,
}

impl Envelope {
    #[inline]
    pub fn next(&mut self, x: f32, attack: f32, release: f32) -> f32 {
        let a = x.abs();
        let k = if a > self.level { attack } else { release };
        self.level += (a - self.level) * k;
        self.level
    }

    pub fn clear(&mut self) {
        self.level = 0.0;
    }
}

/// Evens out a voice: loud words are pulled down toward the threshold, so whispers and
/// shouts sit closer together. Feed-forward peak detector, 3 ms attack, 120 ms release.
#[derive(Clone, Copy)]
pub struct Compressor {
    env: Envelope,
    attack: f32,
    release: f32,
}

impl Compressor {
    pub fn new(sample_rate: f32) -> Self {
        let coef = |ms: f32| 1.0 - (-1.0 / (ms * 0.001 * sample_rate)).exp();
        Self { env: Envelope::default(), attack: coef(3.0), release: coef(120.0) }
    }

    /// `threshold` is linear amplitude; above it, every `ratio` dB in becomes 1 dB out.
    #[inline]
    pub fn process(&mut self, x: f32, threshold: f32, ratio: f32) -> f32 {
        let env = self.env.next(x, self.attack, self.release);
        if env <= threshold {
            return x;
        }
        x * (env / threshold).powf(1.0 / ratio - 1.0)
    }

    pub fn clear(&mut self) {
        self.env.clear();
    }
}

/// Channel vocoder: the voice's spectral envelope, measured in 16 bands, shapes a buzzy
/// synth (one note or a chord). The classic "talking synth" robot.
pub struct Vocoder {
    analysis: [Biquad; 16],
    synthesis: [Biquad; 16],
    envelopes: [Envelope; 16],
    phases: [f32; 3],
    sample_rate: f32,
}

impl Vocoder {
    pub fn new(sample_rate: f32) -> Self {
        let band = |i: usize| 120.0 * (6000.0f32 / 120.0).powf(i as f32 / 15.0);
        let make = || std::array::from_fn(|i| Biquad::new(Shape::Band, sample_rate, band(i), 6.0));
        Self {
            analysis: make(),
            synthesis: make(),
            envelopes: [Envelope::default(); 16],
            phases: [0.0; 3],
            sample_rate,
        }
    }

    /// `note_hz` is the carrier root; with `chord` a major triad is played on top.
    #[inline]
    pub fn process(&mut self, x: f32, note_hz: f32, chord: bool) -> f32 {
        let ratios = [1.0, 1.26, 1.5];
        let voices = if chord { 3 } else { 1 };
        let mut carrier = 0.0;
        for (phase, ratio) in self.phases.iter_mut().zip(ratios).take(voices) {
            *phase = (*phase + note_hz * ratio / self.sample_rate).fract();
            carrier += *phase * 2.0 - 1.0; // sawtooth: rich in harmonics for every band
        }
        carrier /= voices as f32;
        let mut out = 0.0;
        for i in 0..16 {
            let level = self.envelopes[i].next(self.analysis[i].process(x), 0.05, 0.002);
            out += self.synthesis[i].process(carrier) * level;
        }
        out * 6.0
    }

    pub fn clear(&mut self) {
        self.analysis.iter_mut().chain(&mut self.synthesis).for_each(Biquad::clear);
        self.envelopes.iter_mut().for_each(Envelope::clear);
    }
}

/// A bad connection: now and then the voice drops to silence, or stutters, repeating the
/// last few tens of milliseconds it had. Every edge is a 3 ms crossfade, so no clicks.
pub struct Dropout {
    buf: Vec<f32>,
    write: usize,
    noise: Noise,
    sample_rate: f32,
    fade: usize,
    /// Samples until the next event; counts down while idle.
    gap: u32,
    /// Samples left in the running event; 0 while idle.
    left: u32,
    /// 0 is the plain voice, 1 is the event; moves over `fade` samples at each edge.
    mix: f32,
    /// Stutter loop: ring index of its first sample, its length (0 for a dropout), and position.
    loop_start: usize,
    loop_len: usize,
    phase: usize,
}

impl Dropout {
    pub fn new(sample_rate: f32) -> Self {
        Self {
            buf: vec![0.0; (sample_rate * 0.08) as usize],
            write: 0,
            noise: Noise::default(),
            sample_rate,
            fade: ((sample_rate * 0.003) as usize).max(1),
            gap: (sample_rate * 1.6) as u32,
            left: 0,
            mix: 0.0,
            loop_start: 0,
            loop_len: 0,
            phase: 0,
        }
    }

    /// `amount` 0..1 sets how often events happen; 0 passes the voice untouched.
    #[inline]
    pub fn process(&mut self, x: f32, amount: f32) -> f32 {
        if amount <= 0.0 {
            self.left = 0;
            self.mix = 0.0;
            self.loop_len = 0;
            self.push(x);
            return x;
        }
        if self.left > 0 {
            self.left -= 1;
            if self.left == 0 {
                self.gap = self.next_gap(amount);
            }
        } else if self.gap > 0 {
            self.gap -= 1;
        } else {
            self.begin();
        }
        let step = 1.0 / self.fade as f32;
        self.mix = if self.left > 0 { (self.mix + step).min(1.0) } else { (self.mix - step).max(0.0) };
        if self.mix == 0.0 {
            self.loop_len = 0;
            self.push(x);
            return x;
        }
        let wet = if self.loop_len == 0 { 0.0 } else { self.loop_sample() };
        let y = x + (wet - x) * self.mix;
        // The ring is frozen during a stutter: the loop is read from it.
        if self.loop_len == 0 {
            self.push(y);
        }
        y
    }

    fn begin(&mut self) {
        let sr = self.sample_rate;
        self.left = ((0.04 + 0.16 * self.unit()) * sr) as u32;
        let stutter = self.unit() < 0.5;
        self.loop_len = if stutter { ((0.03 + 0.03 * self.unit()) * sr) as usize } else { 0 };
        self.loop_start = (self.write + self.buf.len() - self.loop_len) % self.buf.len();
        self.phase = 0;
    }

    fn next_gap(&mut self, amount: f32) -> u32 {
        (1.6 * self.sample_rate / amount * (0.5 + self.unit())) as u32
    }

    /// The next sample of the stutter loop. The seam fades the loop's tail into the samples
    /// just before it, so each repeat joins the next without a step.
    #[inline]
    fn loop_sample(&mut self) -> f32 {
        let k = self.phase;
        let a = self.at(self.loop_start + k);
        let y = if k + self.fade >= self.loop_len {
            let t = (k + self.fade + 1 - self.loop_len) as f32 / (self.fade + 1) as f32;
            let b = self.at(self.loop_start + k + self.buf.len() - self.loop_len);
            a + (b - a) * t
        } else {
            a
        };
        self.phase += 1;
        if self.phase == self.loop_len {
            self.phase = 0;
        }
        y
    }

    #[inline]
    fn at(&self, i: usize) -> f32 {
        let n = self.buf.len();
        self.buf[if i >= n { i - n } else { i }]
    }

    #[inline]
    fn push(&mut self, x: f32) {
        self.buf[self.write] = x;
        self.write += 1;
        if self.write == self.buf.len() {
            self.write = 0;
        }
    }

    /// Uniform in 0..1.
    fn unit(&mut self) -> f32 {
        (self.noise.next() + 1.0) * 0.5
    }

    pub fn clear(&mut self) {
        self.buf.fill(0.0);
        self.write = 0;
        self.gap = (self.sample_rate * 1.6) as u32;
        self.left = 0;
        self.mix = 0.0;
        self.loop_len = 0;
        self.phase = 0;
    }
}

/// Radio keying: a short beep when the speaker keys up and a "kshh" tail when they stop,
/// like a push-to-talk radio. Talking is judged on the untouched microphone sample.
pub struct Roger {
    env: Envelope,
    talking: bool,
    quiet: u32,
    hold: u32,
    tone: Lfo,
    beep: u32,
    beep_len: u32,
    beep_fade: f32,
    burst: u32,
    burst_len: u32,
    burst_gain: f32,
    cut_step: f32,
    cutting: bool,
    noise: Noise,
    hiss: Biquad,
    sample_rate: f32,
}

impl Roger {
    pub fn new(sample_rate: f32) -> Self {
        let samples = |ms: f32| (ms * 0.001 * sample_rate) as u32;
        Self {
            env: Envelope::default(),
            talking: false,
            quiet: 0,
            hold: samples(350.0),
            tone: Lfo::default(),
            beep: samples(60.0),
            beep_len: samples(60.0),
            beep_fade: samples(5.0).max(1) as f32,
            burst: samples(140.0),
            burst_len: samples(140.0),
            burst_gain: 1.0,
            cut_step: 1.0 / samples(3.0).max(1) as f32,
            cutting: false,
            noise: Noise::default(),
            hiss: Biquad::new(Shape::Band, sample_rate, 2500.0, 0.8),
            sample_rate,
        }
    }

    /// `dry` is the untouched microphone sample, used only to tell when someone is talking;
    /// `x` is the voice going out. Returns `x` with the keying sounds mixed in at `level`.
    #[inline]
    pub fn process(&mut self, dry: f32, x: f32, level: f32) -> f32 {
        let env = self.env.next(dry, 0.01, 0.0005);
        if !self.talking {
            if env > 0.02 {
                self.talking = true;
                self.quiet = 0;
                self.start_beep();
            }
        } else if env < 0.008 {
            self.quiet += 1;
            if self.quiet >= self.hold {
                self.talking = false;
                self.start_burst();
            }
        } else {
            self.quiet = 0;
        }
        let sound = self.beep_sample() + self.burst_sample();
        if level <= 0.0 { x } else { x + sound * level }
    }

    fn start_beep(&mut self) {
        if self.burst < self.burst_len {
            self.cutting = true;
        }
        self.beep = 0;
        self.tone = Lfo::default();
    }

    fn start_burst(&mut self) {
        self.burst = 0;
        self.burst_gain = 1.0;
        self.cutting = false;
        self.hiss.clear();
    }

    /// 60 ms sine at 1250 Hz, faded in and out over 5 ms.
    #[inline]
    fn beep_sample(&mut self) -> f32 {
        if self.beep >= self.beep_len {
            return 0.0;
        }
        let edge = self.beep.min(self.beep_len - 1 - self.beep) as f32 / self.beep_fade;
        self.beep += 1;
        self.tone.next(1250.0, self.sample_rate) * edge.min(1.0) * 0.25
    }

    /// 140 ms of noise around 2500 Hz, dying out linearly. A new beep fades it out in 3 ms.
    #[inline]
    fn burst_sample(&mut self) -> f32 {
        if self.burst >= self.burst_len {
            return 0.0;
        }
        let decay = 1.0 - self.burst as f32 / self.burst_len as f32;
        self.burst += 1;
        let y = self.hiss.process(self.noise.next()) * decay * self.burst_gain * 0.3;
        if self.cutting {
            self.burst_gain -= self.cut_step;
            if self.burst_gain <= 0.0 {
                self.burst = self.burst_len;
            }
        }
        y
    }

    pub fn clear(&mut self) {
        self.env.clear();
        self.talking = false;
        self.quiet = 0;
        self.beep = self.beep_len;
        self.burst = self.burst_len;
        self.cutting = false;
        self.hiss.clear();
    }
}

/// Soft saturation; `drive` 1 is gentle, 10 is fuzz.
#[inline]
pub fn saturate(x: f32, drive: f32) -> f32 {
    (x * drive).tanh() / drive.tanh().max(1e-3)
}

/// Converts semitones (plus cents) to a playback ratio.
#[inline]
pub fn ratio(semitones: f32) -> f32 {
    2f32.powf(semitones / 12.0)
}

#[inline]
pub fn db(gain_db: f32) -> f32 {
    10f32.powf(gain_db / 20.0)
}

/// A unit sine of `hz` at sample `i`, for tests.
#[cfg(test)]
pub fn sine(hz: f32, i: usize, sample_rate: f32) -> f32 {
    (i as f32 * hz * TAU / sample_rate).sin()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48_000.0;

    fn peak(x: impl Iterator<Item = f32>) -> f32 {
        x.fold(0.0, |m, s| m.max(s.abs()))
    }

    #[test]
    fn lfo_and_noise_stay_in_range() {
        let mut l = Lfo::default();
        let mut n = Noise::default();
        let mut w = Wander::default();
        for _ in 0..100_000 {
            assert!(l.next(5.0, SR).abs() <= 1.0);
            assert!(n.next().abs() <= 1.0);
            assert!(w.next(3.0, SR).abs() <= 1.0);
        }
    }

    #[test]
    fn delay_reads_back_what_it_was_given() {
        let mut d = Delay::new(100);
        for i in 0..50 {
            d.push(i as f32);
        }
        assert_eq!(d.read(0.0), 49.0);
        assert_eq!(d.read(10.0), 39.0);
        assert!((d.read(10.5) - 38.5).abs() < 1e-6);
    }

    #[test]
    fn echo_repeats_after_its_delay() {
        let mut e = Echo::new(SR);
        let out: Vec<f32> =
            (0..SR as usize / 2).map(|i| e.process(if i == 0 { 1.0 } else { 0.0 }, 100.0, 0.5)).collect();
        let first = out.iter().position(|s| s.abs() > 0.5).unwrap();
        assert_eq!(first, (SR * 0.1) as usize);
    }

    #[test]
    fn reverb_rings_on_and_decays() {
        let mut r = Reverb::new(SR);
        let out: Vec<f32> = (0..SR as usize * 4).map(|i| r.process(if i < 64 { 0.5 } else { 0.0 }, 0.6, 0.3)).collect();
        let early = peak(out[(SR * 0.2) as usize..(SR * 0.4) as usize].iter().copied());
        let late = peak(out[(SR * 3.5) as usize..].iter().copied());
        assert!(early > 0.005, "no tail: {early}");
        assert!(late < early * 0.1, "does not decay: {late} vs {early}");
    }

    #[test]
    fn crusher_quantises_to_its_bit_depth() {
        let mut c = Crusher::default();
        for i in 0..1000 {
            let y = c.process(sine(100.0, i, SR), 8000.0, 3, SR);
            assert!(((y * 4.0).round() - y * 4.0).abs() < 1e-5, "{y} is not on a 3-bit step");
        }
    }

    #[test]
    fn vocoder_speaks_only_when_the_voice_does() {
        let mut v = Vocoder::new(SR);
        let silent = peak((0..SR as usize / 4).map(|_| v.process(0.0, 110.0, false)));
        let voiced = peak((0..SR as usize / 4).map(|i| v.process(sine(300.0, i, SR) * 0.5, 110.0, true)));
        assert!(silent < 1e-6);
        assert!(voiced > 0.01 && voiced.is_finite(), "{voiced}");
    }

    #[test]
    fn modulated_delays_stay_bounded() {
        let mut m = Modulated::new(SR);
        for i in 0..SR as usize {
            let x = sine(220.0, i, SR) * 0.5;
            assert!(m.chorus(x).abs() <= 0.5 + 1e-4);
        }
        m.clear();
        let p = peak((0..SR as usize).map(|i| m.flanger(sine(220.0, i, SR) * 0.5, 0.3)));
        assert!(p < 1.5 && p.is_finite(), "{p}");
    }

    #[test]
    fn filters_pass_and_cut_where_expected() {
        let level = |shape: fn() -> Biquad, hz: f32| {
            let mut f = shape();
            peak((0..SR as usize / 2).map(|i| f.process(sine(hz, i, SR))).skip(SR as usize / 10))
        };
        let lp = || Biquad::new(Shape::Low, SR, 1000.0, 0.707);
        let hp = || Biquad::new(Shape::High, SR, 1000.0, 0.707);
        assert!(level(lp, 100.0) > 0.9 && level(lp, 8000.0) < 0.05);
        assert!(level(hp, 8000.0) > 0.9 && level(hp, 100.0) < 0.05);
    }

    #[test]
    fn eq_shapes_boost_and_cut_their_band() {
        let gain = |make: fn() -> Biquad, hz: f32| {
            let mut f = make();
            peak((0..SR as usize / 2).map(|i| f.process(sine(hz, i, SR))).skip(SR as usize / 4))
        };
        let bell = || Biquad::peak(SR, 1500.0, 1.0, 6.0);
        assert!((1.9..2.1).contains(&gain(bell, 1500.0)));
        assert!((0.95..1.05).contains(&gain(bell, 150.0)));
        assert!((0.95..1.05).contains(&gain(bell, 12_000.0)));
        let bass = || Biquad::low_shelf(SR, 200.0, 6.0);
        assert!((1.9..2.1).contains(&gain(bass, 50.0)));
        assert!((0.95..1.05).contains(&gain(bass, 4000.0)));
        let air = || Biquad::high_shelf(SR, 4000.0, -6.0);
        assert!((0.45..0.55).contains(&gain(air, 12_000.0)));
        assert!((0.95..1.05).contains(&gain(air, 200.0)));
    }

    #[test]
    fn zero_gain_eq_is_transparent() {
        let gain = |make: fn() -> Biquad, hz: f32| {
            let mut f = make();
            peak((0..SR as usize / 2).map(|i| f.process(sine(hz, i, SR))).skip(SR as usize / 4))
        };
        let bell = || Biquad::peak(SR, 1500.0, 1.0, 0.0);
        let bass = || Biquad::low_shelf(SR, 200.0, 0.0);
        let air = || Biquad::high_shelf(SR, 4000.0, 0.0);
        for hz in [50.0, 1500.0, 12_000.0] {
            assert!((0.99..1.01).contains(&gain(bell, hz)), "bell at {hz}");
            assert!((0.99..1.01).contains(&gain(bass, hz)), "low shelf at {hz}");
            assert!((0.99..1.01).contains(&gain(air, hz)), "high shelf at {hz}");
        }
    }

    #[test]
    fn compressor_leaves_quiet_audio_and_tames_loud_audio() {
        let mut c = Compressor::new(SR);
        for i in 0..SR as usize / 2 {
            let x = sine(200.0, i, SR) * 0.05;
            assert!((c.process(x, 0.1, 4.0) - x).abs() < 1e-6);
        }
        let mut c = Compressor::new(SR);
        let loud = peak((0..SR as usize).map(|i| c.process(sine(200.0, i, SR) * 0.8, 0.1, 4.0)).skip(SR as usize / 2));
        assert!((0.15..0.19).contains(&loud), "{loud}");
    }

    #[test]
    fn compressor_at_ratio_one_is_transparent() {
        let mut c = Compressor::new(SR);
        for i in 0..SR as usize / 2 {
            let x = sine(200.0, i, SR) * 0.8;
            assert!((c.process(x, 0.1, 1.0) - x).abs() < 1e-6);
        }
    }

    #[test]
    fn compressor_stays_finite_on_silence_and_square_waves() {
        let mut c = Compressor::new(SR);
        for _ in 0..SR as usize / 10 {
            assert_eq!(c.process(0.0, 0.1, 4.0), 0.0);
        }
        for i in 0..SR as usize {
            let x = if (i / 50) % 2 == 0 { 1.0 } else { -1.0 };
            let y = c.process(x, 0.1, 4.0);
            assert!(y.is_finite() && y.abs() <= 1.0, "{y}");
        }
    }

    #[test]
    fn dropout_at_zero_amount_passes_the_voice() {
        let mut d = Dropout::new(SR);
        for i in 0..SR as usize {
            let x = sine(200.0, i, SR) * 0.5;
            assert_eq!(d.process(x, 0.0), x);
        }
    }

    #[test]
    fn dropout_cuts_and_stutters_without_clicks() {
        let mut d = Dropout::new(SR);
        let (mut quiet, mut biggest_step, mut prev) = (0usize, 0.0f32, 0.0f32);
        let total = SR as usize * 20;
        for i in 0..total {
            let x = sine(200.0, i, SR) * 0.5;
            let y = d.process(x, 1.0);
            assert!(y.is_finite() && y.abs() <= 0.5 + 1e-3, "{y} at {i}");
            if x.abs() > 0.1 && y.abs() < 0.01 {
                quiet += 1;
            }
            biggest_step = biggest_step.max((y - prev).abs());
            prev = y;
        }
        let fraction = quiet as f32 / total as f32;
        assert!((0.01..0.25).contains(&fraction), "dropout fraction {fraction}");
        assert!(biggest_step < 0.08, "click of {biggest_step}");
    }

    #[test]
    fn dropout_cleared_then_silence_is_exact_silence() {
        let mut d = Dropout::new(SR);
        for i in 0..SR as usize {
            d.process(sine(200.0, i, SR) * 0.5, 1.0);
        }
        d.clear();
        for _ in 0..SR as usize * 3 {
            assert_eq!(d.process(0.0, 1.0), 0.0);
        }
    }

    #[test]
    fn roger_stays_silent_without_speech() {
        let mut r = Roger::new(SR);
        for _ in 0..SR as usize {
            assert_eq!(r.process(0.0, 0.0, 1.0), 0.0);
        }
    }

    #[test]
    fn roger_at_zero_level_returns_the_voice() {
        let mut r = Roger::new(SR);
        for i in 0..SR as usize / 2 {
            let dry = sine(1000.0, i, SR) * 0.3;
            let x = sine(300.0, i, SR) * 0.5;
            assert_eq!(r.process(dry, x, 0.0), x);
        }
    }

    #[test]
    fn roger_beeps_on_keyup_and_hisses_on_keydown() {
        let mut r = Roger::new(SR);
        let tone = SR as usize;
        let out: Vec<f32> = (0..tone * 2)
            .map(|i| {
                let dry = if i < tone { sine(1000.0, i, SR) * 0.3 } else { 0.0 };
                r.process(dry, 0.0, 1.0)
            })
            .collect();
        let ms = |t: usize| (t as f32 * SR / 1000.0) as usize;
        assert!(out.iter().all(|s| s.is_finite() && s.abs() < 0.5));

        let beep = peak(out[..ms(80)].iter().copied());
        assert!(beep > 0.1, "no beep: {beep}");

        let burst = peak(out[tone + ms(480)..tone + ms(640)].iter().copied());
        assert!(burst > 0.01, "no burst: {burst}");
        assert!(out[tone + ms(350)..tone + ms(480)].iter().all(|&s| s == 0.0), "burst too early");
        assert!(out[tone + ms(660)..].iter().all(|&s| s == 0.0), "burst too late");
    }

    #[test]
    fn roger_beep_is_1250_hz() {
        let mut r = Roger::new(SR);
        let out: Vec<f32> = (0..SR as usize / 4)
            .map(|i| {
                let dry = if i < SR as usize / 10 { sine(1000.0, i, SR) * 0.3 } else { 0.0 };
                r.process(dry, 0.0, 1.0)
            })
            .collect();
        let start = out.iter().position(|s| s.abs() > 0.0).unwrap();
        let beep = &out[start..start + (SR * 0.06) as usize];
        let crossings = beep.windows(2).filter(|w| (w[0] >= 0.0) != (w[1] >= 0.0)).count();
        let hz = crossings as f32 / 2.0 / 0.06;
        assert!((hz - 1250.0).abs() <= 60.0, "beep at {hz} Hz");
    }
}
