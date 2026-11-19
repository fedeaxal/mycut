//! Real-time voice effects. Everything here runs inside the audio callback:
//! no allocation after construction, no locks, no syscalls.

use std::f32::consts::PI;

use crate::formant::FormantShifter;
use crate::fx::{
    Biquad, Compressor, Crusher, Delay, Dropout, Echo, Envelope, Lfo, Modulated, Noise, Reverb, Roger, Shape, Vocoder,
    Wander, ratio, saturate,
};

/// Delay-line pitch shifter with correlation-aligned jumps (WSOLA in a ring).
///
/// A read tap drifts through the delay line at `1 - ratio` samples per sample.
/// When it runs out of room it jumps by `lag` samples, and `lag` is picked so the
/// waveform at the new spot lines up with the old one; the short crossfade between
/// them then adds instead of cancelling. Formants move with pitch, which is exactly
/// the "chipmunk" colour the Minion and Helium voices want.
pub struct PitchShifter {
    buf: Vec<f32>,
    mask: usize,
    write: usize,
    ratio: f32,
    delay: f32,
    /// Delay of the incoming tap, progress of the crossfade towards it, and its length.
    fade: Option<(f32, usize, usize)>,
    min_delay: f32,
    min_lag: usize,
    max_lag: usize,
    fade_len: usize,
    corr_len: usize,
    /// Lag of the previous jump: a good guess at the current pitch period.
    last_lag: usize,
    /// Samples spent waiting for a good enough match before a downward jump.
    wait: u32,
    /// Scratch for `best_lag`: the searched stretch of `buf` unwrapped, and its running energy.
    line: Vec<f32>,
    energy: Vec<f64>,
    /// Raised-cosine crossfade gain for each step of a fade.
    fade_gain: Vec<f32>,
}

impl PitchShifter {
    pub fn new(sample_rate: u32) -> Self {
        let ms = |m: f32| (sample_rate as f32 * m / 1000.0) as usize;
        // The lag range only has to hold one full period of a low voice (80 Hz = 12.5 ms);
        // every extra millisecond here is a millisecond of latency.
        let (min_lag, max_lag, fade_len, corr_len) = (ms(6.0), ms(13.5), ms(5.0), ms(5.0));
        let min_delay = 4.0;
        // Deepest the tap can go: largest lag plus drift during a fade, twice, at two octaves.
        let deepest = min_delay as usize + max_lag + 6 * fade_len;
        let len = (deepest + corr_len + max_lag + 8).next_power_of_two();
        Self {
            buf: vec![0.0; len],
            mask: len - 1,
            write: 0,
            ratio: 1.0,
            delay: min_delay + max_lag as f32 / 2.0,
            fade: None,
            min_delay,
            min_lag,
            max_lag,
            fade_len,
            corr_len,
            last_lag: max_lag,
            wait: 0,
            line: vec![0.0; corr_len + max_lag],
            energy: vec![0.0; corr_len + max_lag + 1],
            fade_gain: (0..fade_len).map(|t| 0.5 - 0.5 * (PI * t as f32 / fade_len as f32).cos()).collect(),
        }
    }

    pub fn set_ratio(&mut self, ratio: f32) {
        self.ratio = ratio.clamp(0.25, 4.0);
    }

    /// Records without playing, for a shifter the current voice does not use. Otherwise it
    /// would still hold speech from whenever it last ran, and play a few milliseconds of
    /// that when a voice brings it back.
    #[inline]
    fn listen(&mut self, x: f32) {
        self.buf[self.write] = x;
        self.write = (self.write + 1) & self.mask;
    }

    #[inline]
    fn at(&self, i: isize) -> f32 {
        // Negative indices wrap correctly because the length is a power of two.
        self.buf[i as usize & self.mask]
    }

    /// Lag (at most `longest`) that best lines up the audio behind `from` with the audio
    /// behind `from + sign*lag`, and its normalised correlation in -1..=1.
    fn best_lag(&mut self, from: f32, sign: isize, longest: usize) -> (usize, f32) {
        let longest = longest.clamp(self.min_lag, self.max_lag);
        let n = self.corr_len;
        let base = self.write as isize - from as isize;
        // Unwrap every window the search can touch into one straight run, so the dot
        // products below read contiguous memory and vectorise. Window ends at `x + n`
        // (the audio behind `from`) and at `y(lag) + n`.
        let start = if sign > 0 { base - (longest + n) as isize + 1 } else { base - n as isize + 1 };
        let span = n + longest;
        let mut sum = 0.0f64;
        self.energy[0] = 0.0;
        for k in 0..span {
            let v = self.buf[(start + k as isize) as usize & self.mask];
            self.line[k] = v;
            sum += (v * v) as f64;
            self.energy[k + 1] = sum;
        }
        let x = if sign > 0 { longest } else { 0 };
        let y = |lag: usize| if sign > 0 { longest - lag } else { lag };
        let (line, energy) = (&self.line[..span], &self.energy[..=span]);
        let xx = (energy[x + n] - energy[x]) as f32 + 1e-9;
        let score = |lag: usize| {
            let y = y(lag);
            let yy = (energy[y + n] - energy[y]) as f32 + 1e-9;
            dot(&line[x..x + n], &line[y..y + n]) / (xx * yy).sqrt()
        };
        // Coarse pass every 2 samples, then refine around the winner.
        let mut best = (self.min_lag, f32::MIN);
        for lag in (self.min_lag..=longest).step_by(2) {
            let s = score(lag);
            if s > best.1 {
                best = (lag, s);
            }
        }
        for lag in [best.0.saturating_sub(1), best.0 + 1] {
            if (self.min_lag..=longest).contains(&lag) {
                let s = score(lag);
                if s > best.1 {
                    best = (lag, s);
                }
            }
        }
        best
    }

    #[inline]
    fn tap(&self, delay: f32) -> f32 {
        // Cubic Hermite read `delay` samples behind the write head.
        let pos = self.write as f32 - delay;
        let i = pos.floor();
        let t = pos - i;
        let i = i as isize;
        let (y0, y1, y2, y3) = (self.at(i - 1), self.at(i), self.at(i + 1), self.at(i + 2));
        let c1 = 0.5 * (y2 - y0);
        let c2 = y0 - 2.5 * y1 + 2.0 * y2 - 0.5 * y3;
        let c3 = 0.5 * (y3 - y0) + 1.5 * (y1 - y2);
        ((c3 * t + c2) * t + c1) * t + y1
    }

    #[inline]
    pub fn process(&mut self, x: f32) -> f32 {
        self.buf[self.write] = x;
        let drift = 1.0 - self.ratio;
        let mut y = self.tap(self.delay);
        if let Some((next, t, len)) = self.fade {
            // A shortened fade steps through the same curve faster.
            y += (self.tap(next) - y) * self.fade_gain[t * self.fade_len / len];
            self.fade = if t + 1 >= len {
                self.delay = next;
                None
            } else {
                Some((next + drift, t + 1, len))
            };
        }
        self.delay += drift;

        if self.fade.is_none() {
            // Far up the tap races towards the write head: over a full-length fade it
            // covered more than a whole period, the next jump was due before this one
            // ended, and the tap ran off into stale audio (Minion at +12 st, about +20
            // in all). A shorter fade keeps each one inside part of a period.
            let len = if drift < 0.0 {
                self.fade_len.min((0.8 * self.min_lag as f32 / -drift) as usize).max(32)
            } else {
                self.fade_len
            };
            let room = len as f32 * drift.abs() + 1.0;
            if drift > 0.0 {
                // Shifting down the tap falls behind. Jump forward as soon as one period
                // (the last lag) fits, if the waveform lines up well; only when the full lag
                // range fits is the best match taken regardless. This keeps shifting down
                // about as cheap in latency as shifting up.
                let reach = self.delay - self.min_delay - room;
                let forced = reach >= self.max_lag as f32;
                if reach >= self.last_lag as f32 {
                    self.wait += 1;
                    if forced || self.wait % 32 == 1 {
                        let (lag, score) = self.best_lag(self.delay, -1, reach as usize);
                        if forced || score > 0.6 {
                            self.last_lag = lag;
                            self.wait = 0;
                            self.fade = Some((self.delay - lag as f32, 0, len));
                        }
                    }
                }
            } else if drift < 0.0 && self.delay < self.min_delay + room {
                let (lag, _) = self.best_lag(self.delay, 1, self.max_lag);
                self.last_lag = lag;
                self.fade = Some((self.delay + lag as f32, 0, len));
            }
        }
        // Kept masked: an ever-growing index would lose precision as f32 after a few minutes.
        self.write = (self.write + 1) & self.mask;
        y
    }
}

/// Makes the calling thread's floating point treat denormal numbers as zero, both as
/// inputs and as results. Every decaying tail ends in them (a reverb ringing out, the gate
/// closing), and the CPU takes a slow path for each one: with the gate closed the
/// heaviest voices cost twice as much as while you talk. Nothing audible lives down there.
pub fn flush_denormals() {
    #[cfg(target_arch = "x86_64")]
    #[allow(deprecated)]
    unsafe {
        use std::arch::x86_64::{_mm_getcsr, _mm_setcsr};
        // Bit 15 flushes results to zero, bit 6 reads denormal inputs as zero.
        _mm_setcsr(_mm_getcsr() | 0x8040);
    }
}

/// Dot product in eight independent lanes, which the compiler turns into SIMD; a single
/// running sum would have to be added up in order, one sample at a time.
#[inline(never)]
pub(crate) fn dot(a: &[f32], b: &[f32]) -> f32 {
    let mut acc = [0.0f32; 8];
    let (a8, a_rest) = a.as_chunks::<8>();
    let (b8, b_rest) = b.as_chunks::<8>();
    for (x, y) in a8.iter().zip(b8) {
        for k in 0..8 {
            acc[k] += x[k] * y[k];
        }
    }
    let tail: f32 = a_rest.iter().zip(b_rest).map(|(x, y)| x * y).sum();
    acc.iter().sum::<f32>() + tail
}

/// Everything a voice is made of. Zero switches a stage off, so a recipe only names what it uses.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Recipe {
    /// Pitch shift in semitones; formants move with it (the chipmunk / giant colour)
    /// unless `formant` says otherwise.
    pub pitch: f32,
    /// Timbre as a ratio, apart from the pitch: 1.15 is a smaller throat, 0.85 a bigger
    /// one, and 1.0 keeps the speaker's own while `pitch` moves. Zero leaves the voice to
    /// the plain shifter, where timbre follows pitch.
    pub formant: f32,
    /// Pulls the pitch to the nearest note: 1 any semitone, 2 a major scale, 3 its five-note core.
    pub tune: u32,
    /// Up to two extra pitched copies of the voice, in semitones (0 = unused).
    pub harmony: [f32; 2],
    pub harmony_mix: f32,
    pub vibrato_hz: f32,
    pub vibrato_cents: f32,
    /// Slow random pitch drift: drunk, nervous, mutating.
    pub wobble_hz: f32,
    pub wobble_cents: f32,
    pub ring_hz: f32,
    pub ring_mix: f32,
    /// Replaces the voiced buzz with noise while keeping the vowel shapes.
    pub whisper: f32,
    /// Carrier root of the vocoder in Hz (0 = no vocoder).
    pub vocoder_hz: f32,
    pub vocoder_chord: bool,
    /// Short feedback comb: metallic, tin-can, robotic.
    pub comb_ms: f32,
    pub comb_feedback: f32,
    pub chorus: f32,
    pub flanger_hz: f32,
    pub flanger_mix: f32,
    pub tremolo_hz: f32,
    pub tremolo_depth: f32,
    pub highpass_hz: f32,
    pub lowpass_hz: f32,
    /// One peaking band: its centre in Hz and its boost or cut in dB.
    pub eq_hz: f32,
    pub eq_db: f32,
    /// Boost or cut under 180 Hz, in dB.
    pub bass_db: f32,
    /// Compression ratio above -20 dB (0 = none): 6 is the wall of voice of a film trailer.
    pub squash: f32,
    /// Saturation drive (0 = clean); `hard_clip` squares it off instead.
    pub drive: f32,
    pub hard_clip: bool,
    /// Sample-and-hold rate in Hz and bit depth (0 = clean).
    pub crush_hz: f32,
    pub crush_bits: u32,
    /// Radio static that only plays while you talk.
    pub hiss: f32,
    /// A bad connection: gaps and stutters, 0..=1.
    pub dropout: f32,
    /// Radio keying: a beep as a phrase starts and a burst of static as it ends.
    pub roger: f32,
    pub echo_ms: f32,
    pub echo_feedback: f32,
    pub echo_mix: f32,
    pub reverb_size: f32,
    pub reverb_damp: f32,
    pub reverb_mix: f32,
    /// Make-up gain for recipes that lose or add a lot of level.
    pub gain: f32,
}

impl Recipe {
    pub const DRY: Recipe = Recipe {
        pitch: 0.0,
        formant: 0.0,
        tune: 0,
        harmony: [0.0; 2],
        harmony_mix: 0.0,
        vibrato_hz: 0.0,
        vibrato_cents: 0.0,
        wobble_hz: 0.0,
        wobble_cents: 0.0,
        ring_hz: 0.0,
        ring_mix: 0.0,
        whisper: 0.0,
        vocoder_hz: 0.0,
        vocoder_chord: false,
        comb_ms: 0.0,
        comb_feedback: 0.0,
        chorus: 0.0,
        flanger_hz: 0.0,
        flanger_mix: 0.0,
        tremolo_hz: 0.0,
        tremolo_depth: 0.0,
        highpass_hz: 0.0,
        lowpass_hz: 0.0,
        eq_hz: 0.0,
        eq_db: 0.0,
        bass_db: 0.0,
        squash: 0.0,
        drive: 0.0,
        hard_clip: false,
        crush_hz: 0.0,
        crush_bits: 0,
        hiss: 0.0,
        dropout: 0.0,
        roger: 0.0,
        echo_ms: 0.0,
        echo_feedback: 0.0,
        echo_mix: 0.0,
        reverb_size: 0.0,
        reverb_damp: 0.0,
        reverb_mix: 0.0,
        gain: 1.0,
    };

    /// The same recipe with every field brought within what [`Voice`] runs —
    /// the clamp every slider and stored value passes through, so a value
    /// from somewhere the catalog never wrote can never hand a stage a
    /// number it does not take. The two booleans need nothing.
    pub fn sanitised(self) -> Self {
        fn within(value: f32, lo: f32, hi: f32, otherwise: f32) -> f32 {
            if value.is_finite() {
                value.clamp(lo, hi)
            } else {
                otherwise
            }
        }
        Self {
            pitch: within(self.pitch, -24.0, 24.0, 0.0),
            formant: within(self.formant, 0.0, 2.0, 0.0),
            tune: self.tune.min(3),
            harmony: [
                within(self.harmony[0], -24.0, 24.0, 0.0),
                within(self.harmony[1], -24.0, 24.0, 0.0),
            ],
            harmony_mix: within(self.harmony_mix, 0.0, 1.0, 0.0),
            vibrato_hz: within(self.vibrato_hz, 0.0, 12.0, 0.0),
            vibrato_cents: within(self.vibrato_cents, 0.0, 600.0, 0.0),
            wobble_hz: within(self.wobble_hz, 0.0, 12.0, 0.0),
            wobble_cents: within(self.wobble_cents, 0.0, 600.0, 0.0),
            ring_hz: within(self.ring_hz, 0.0, 2000.0, 0.0),
            ring_mix: within(self.ring_mix, 0.0, 1.0, 0.0),
            whisper: within(self.whisper, 0.0, 1.0, 0.0),
            vocoder_hz: within(self.vocoder_hz, 0.0, 1000.0, 0.0),
            comb_ms: within(self.comb_ms, 0.0, 50.0, 0.0),
            comb_feedback: within(self.comb_feedback, 0.0, 0.95, 0.0),
            chorus: within(self.chorus, 0.0, 1.0, 0.0),
            flanger_hz: within(self.flanger_hz, 0.0, 10.0, 0.0),
            flanger_mix: within(self.flanger_mix, 0.0, 1.0, 0.0),
            tremolo_hz: within(self.tremolo_hz, 0.0, 20.0, 0.0),
            tremolo_depth: within(self.tremolo_depth, 0.0, 1.0, 0.0),
            highpass_hz: within(self.highpass_hz, 0.0, 4000.0, 0.0),
            lowpass_hz: within(self.lowpass_hz, 0.0, 20000.0, 0.0),
            eq_hz: within(self.eq_hz, 0.0, 12000.0, 0.0),
            eq_db: within(self.eq_db, -24.0, 24.0, 0.0),
            bass_db: within(self.bass_db, -24.0, 24.0, 0.0),
            squash: within(self.squash, 0.0, 32.0, 0.0),
            drive: within(self.drive, 0.0, 10.0, 0.0),
            crush_hz: within(self.crush_hz, 0.0, 48000.0, 0.0),
            crush_bits: self.crush_bits.min(16),
            hiss: within(self.hiss, 0.0, 1.0, 0.0),
            dropout: within(self.dropout, 0.0, 1.0, 0.0),
            roger: within(self.roger, 0.0, 1.0, 0.0),
            echo_ms: within(self.echo_ms, 0.0, 1000.0, 0.0),
            echo_feedback: within(self.echo_feedback, 0.0, 0.95, 0.0),
            echo_mix: within(self.echo_mix, 0.0, 1.0, 0.0),
            reverb_size: within(self.reverb_size, 0.0, 1.0, 0.0),
            reverb_damp: within(self.reverb_damp, 0.0, 1.0, 0.0),
            reverb_mix: within(self.reverb_mix, 0.0, 1.0, 0.0),
            gain: within(self.gain, 0.05, 4.0, 1.0),
            ..self
        }
    }
}

/// The note of `scale` nearest to `note`, both as MIDI numbers with fractions in between.
fn snap(note: f32, scale: u32) -> f32 {
    // One bit a semitone, C first: C D E F G A B, and C D E G A.
    let allowed: u16 = match scale {
        2 => 0b1010_1011_0101,
        3 => 0b0010_1001_0101,
        _ => 0xfff,
    };
    let near = note.round();
    let mut best = f32::MAX;
    for step in [0.0, -1.0, 1.0, -2.0, 2.0] {
        let n = near + step;
        if allowed >> (n as i32).rem_euclid(12) & 1 == 1 && (n - note).abs() < (best - note).abs() {
            best = n;
        }
    }
    best
}

/// Length of each half of the dip between two voices: 5 ms at 48 kHz.
const FADE: u32 = 240;

/// Runs one recipe. Every stage is allocated up front, so switching voices in the audio
/// thread never allocates; stages a recipe does not use cost nothing.
pub struct Voice {
    sample_rate: f32,
    recipe: Recipe,
    /// The recipe asked for; it takes over once the current one has faded out.
    next: Recipe,
    /// Output level in samples of fade, 0..=FADE.
    level: u32,
    tone: f32,
    /// The tone asked for; one that takes the pitch shifter in or out of use waits for a fade too.
    next_tone: f32,
    volume: f32,
    shifters: [PitchShifter; 3],
    former: FormantShifter,
    /// How far `tune` last moved the pitch to reach a note; held while nobody speaks.
    bend: f32,
    vibrato: Lfo,
    wobble: Wander,
    ring: Lfo,
    tremolo: Lfo,
    noise: Noise,
    vocoder: Vocoder,
    comb: Delay,
    chorus: Modulated,
    flanger: Modulated,
    highpass: Biquad,
    lowpass: Biquad,
    eq: Biquad,
    bass: Biquad,
    squash: Compressor,
    crusher: Crusher,
    envelope: Envelope,
    dropout: Dropout,
    roger: Roger,
    echo: Echo,
    reverb: Reverb,
}

impl Voice {
    pub fn new(sample_rate: u32) -> Self {
        let sr = sample_rate as f32;
        Self {
            sample_rate: sr,
            recipe: Recipe::DRY,
            next: Recipe::DRY,
            level: FADE,
            tone: 0.0,
            next_tone: 0.0,
            volume: 1.0,
            shifters: std::array::from_fn(|_| PitchShifter::new(sample_rate)),
            former: FormantShifter::new(sample_rate),
            bend: 1.0,
            vibrato: Lfo::default(),
            wobble: Wander::default(),
            ring: Lfo::default(),
            tremolo: Lfo::default(),
            noise: Noise::default(),
            vocoder: Vocoder::new(sr),
            comb: Delay::new((sr * 0.03) as usize),
            chorus: Modulated::new(sr),
            flanger: Modulated::new(sr),
            highpass: Biquad::default(),
            lowpass: Biquad::default(),
            eq: Biquad::default(),
            bass: Biquad::default(),
            squash: Compressor::new(sr),
            crusher: Crusher::default(),
            envelope: Envelope::default(),
            dropout: Dropout::new(sr),
            roger: Roger::new(sr),
            echo: Echo::new(sr),
            reverb: Reverb::new(sr),
        }
    }

    /// `tone` moves any voice up or down in semitones; `volume` is the output level.
    pub fn set(&mut self, recipe: &Recipe, tone: f32, volume: f32) {
        self.next = *recipe;
        self.next_tone = tone;
        self.volume = volume;
    }

    /// Swaps in the next recipe and tone; called at the bottom of a fade, where nothing is heard.
    fn switch(&mut self) {
        let new = self.next;
        self.recipe = new;
        self.tone = self.next_tone;
        let sr = self.sample_rate;
        if new.highpass_hz > 0.0 {
            self.highpass = Biquad::new(Shape::High, sr, new.highpass_hz, 0.707);
        }
        if new.lowpass_hz > 0.0 {
            self.lowpass = Biquad::new(Shape::Low, sr, new.lowpass_hz, 0.707);
        }
        if new.eq_db != 0.0 {
            self.eq = Biquad::peak(sr, new.eq_hz, 1.0, new.eq_db);
        }
        if new.bass_db != 0.0 {
            self.bass = Biquad::low_shelf(sr, 180.0, new.bass_db);
        }
        self.bend = 1.0;
        // Tails of the previous voice would ring on into the new one.
        self.vocoder.clear();
        self.comb.clear();
        self.chorus.clear();
        self.flanger.clear();
        self.squash.clear();
        self.crusher.clear();
        self.envelope.clear();
        self.dropout.clear();
        self.roger.clear();
        self.echo.clear();
        self.reverb.clear();
    }

    fn shifting(&self) -> bool {
        self.shifts(self.tone)
    }

    /// Whether the current recipe would go through the pitch shifter at `tone`.
    fn shifts(&self, tone: f32) -> bool {
        let r = &self.recipe;
        (r.pitch + tone).abs() > 0.01 || r.vibrato_cents > 0.0 || r.wobble_cents > 0.0 || self.formed()
    }

    /// Whether the current recipe moves pitch and timbre apart, which the plain shifter cannot.
    fn formed(&self) -> bool {
        self.recipe.formant > 0.0 || self.recipe.tune > 0
    }

    /// How much further a voice shifted by `shift` has to move to land on a note of `scale`.
    fn bend(&mut self, shift: f32, scale: u32) -> f32 {
        if self.former.voiced() {
            let hz = self.sample_rate / self.former.period() * shift;
            let note = 12.0 * (hz / 440.0).log2() + 69.0;
            self.bend = ratio(snap(note, scale) - note);
        }
        self.bend
    }

    /// How far behind the input the output currently is, in samples.
    pub fn delay(&self) -> f32 {
        match (self.shifting(), self.formed()) {
            (false, _) => 0.0,
            (true, false) => self.shifters[0].delay,
            (true, true) => self.former.delay(),
        }
    }

    /// Changing voice dips the output for FADE samples and comes back up over as many, so
    /// the jump between two voices, or into a cleared echo, is never heard as a click. A tone
    /// that takes the pitch shifter in or out of use is such a jump too: the shifter plays a
    /// few milliseconds behind the microphone. Any other tone only bends the pitch, at once.
    pub fn process(&mut self, block: &mut [f32]) {
        let mut start = 0;
        while start < block.len() {
            let changing = self.next != self.recipe || self.shifts(self.next_tone) != self.shifting();
            if changing && self.level == 0 {
                self.switch();
                continue;
            }
            if !changing {
                self.tone = self.next_tone;
            }
            // Up to the bottom of the dip, so the switch lands on a sample boundary.
            let end = if changing { block.len().min(start + self.level as usize) } else { block.len() };
            let part = &mut block[start..end];
            self.run(part);
            if changing || self.level < FADE {
                for s in part {
                    self.level = if changing { self.level - 1 } else { (self.level + 1).min(FADE) };
                    *s *= self.level as f32 / FADE as f32;
                }
            }
            start = end;
        }
    }

    fn run(&mut self, block: &mut [f32]) {
        let r = self.recipe;
        let sr = self.sample_rate;
        if r == Recipe::DRY && self.tone == 0.0 {
            for s in block.iter_mut() {
                self.shifters.iter_mut().for_each(|p| p.listen(*s));
                self.former.listen(*s);
                *s *= self.volume;
            }
            return;
        }
        let shifting = self.shifting();
        let modulated = r.vibrato_cents > 0.0 || r.wobble_cents > 0.0;
        let formed = self.formed();
        let timbre = if r.formant > 0.0 { r.formant } else { 1.0 };
        let bend = if r.tune > 0 { self.bend(ratio(r.pitch + self.tone), r.tune) } else { 1.0 };
        let own = ratio(r.pitch + self.tone) * bend;
        self.shifters[0].set_ratio(own);
        self.former.set(own, timbre);
        for (k, semis) in r.harmony.iter().enumerate() {
            self.shifters[k + 1].set_ratio(ratio(semis + self.tone) * bend);
        }
        let harmonies = r.harmony.iter().filter(|s| **s != 0.0).count();

        for s in block.iter_mut() {
            let dry = *s;
            let mut x = dry;
            if shifting {
                if modulated {
                    let cents = self.vibrato.next(r.vibrato_hz, sr) * r.vibrato_cents
                        + self.wobble.next(r.wobble_hz, sr) * r.wobble_cents;
                    let bent = ratio(r.pitch + self.tone + cents / 100.0) * bend;
                    self.shifters[0].set_ratio(bent);
                    self.former.set(bent, timbre);
                }
                if formed {
                    self.shifters[0].listen(dry);
                    x = self.former.process(dry);
                } else {
                    self.former.listen(dry);
                    x = self.shifters[0].process(dry);
                }
            } else {
                self.shifters[0].listen(dry);
                self.former.listen(dry);
            }
            let mut h = 0.0;
            for k in 0..2 {
                if r.harmony[k] != 0.0 {
                    h += self.shifters[k + 1].process(dry);
                } else {
                    self.shifters[k + 1].listen(dry);
                }
            }
            if harmonies > 0 {
                x = x * (1.0 - r.harmony_mix) + h / harmonies as f32 * r.harmony_mix;
            }
            if r.ring_mix > 0.0 {
                x = x * (1.0 - r.ring_mix) + x * self.ring.next(r.ring_hz, sr) * r.ring_mix * 1.4;
            }
            if r.whisper > 0.0 {
                x = x * (1.0 - r.whisper) + x * self.noise.next() * 1.7 * r.whisper;
            }
            if r.vocoder_hz > 0.0 {
                // A tuned voice plays the synth itself: its note is the one being sung.
                let note = if r.tune > 0 { sr / self.former.period() * own } else { r.vocoder_hz };
                x = self.vocoder.process(x, note, r.vocoder_chord);
            }
            if r.comb_ms > 0.0 {
                let y = x + self.comb.read(r.comb_ms * sr / 1000.0 - 1.0) * r.comb_feedback;
                self.comb.push(y);
                x = y * (1.0 - r.comb_feedback * 0.5);
            }
            if r.chorus > 0.0 {
                x = x * (1.0 - r.chorus * 0.5) + self.chorus.chorus(x) * r.chorus;
            }
            if r.flanger_mix > 0.0 {
                x = x * (1.0 - r.flanger_mix * 0.5) + self.flanger.flanger(x, r.flanger_hz) * r.flanger_mix * 0.6;
            }
            if r.tremolo_depth > 0.0 {
                x *= 1.0 - r.tremolo_depth * (0.5 + 0.5 * self.tremolo.next(r.tremolo_hz, sr));
            }
            if r.highpass_hz > 0.0 {
                x = self.highpass.process(x);
            }
            if r.lowpass_hz > 0.0 {
                x = self.lowpass.process(x);
            }
            if r.eq_db != 0.0 {
                x = self.eq.process(x);
            }
            if r.bass_db != 0.0 {
                x = self.bass.process(x);
            }
            if r.squash > 0.0 {
                x = self.squash.process(x, 0.1, r.squash);
            }
            if r.drive > 0.0 {
                x = if r.hard_clip { (x * r.drive).clamp(-0.6, 0.6) } else { saturate(x, r.drive) * 0.8 };
            }
            if r.crush_bits > 0 {
                x = self.crusher.process(x, r.crush_hz, r.crush_bits, sr);
            }
            if r.hiss > 0.0 {
                let talking = self.envelope.next(dry, 0.01, 0.0005);
                x += self.noise.next() * r.hiss * (talking * 4.0).min(1.0);
            }
            if r.dropout > 0.0 {
                x = self.dropout.process(x, r.dropout);
            }
            if r.roger > 0.0 {
                x = self.roger.process(dry, x, r.roger);
            }
            if r.echo_mix > 0.0 {
                x += self.echo.process(x, r.echo_ms, r.echo_feedback) * r.echo_mix;
            }
            if r.reverb_mix > 0.0 {
                x = x * (1.0 - r.reverb_mix * 0.3)
                    + self.reverb.process(x, r.reverb_size, r.reverb_damp) * r.reverb_mix * 2.0;
            }
            // Soft limit so a loud scream never clips hard in voice chat.
            *s = (x * r.gain * self.volume).tanh();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every field a caller could hand over — from a slider, a stored value,
    /// a hand-edited file — comes back where the voice runs, and a field the
    /// recipe never set stays off.
    #[test]
    fn a_recipe_from_somewhere_else_comes_back_where_the_voice_runs() {
        let wild = Recipe {
            pitch: 99.0,
            formant: f32::NAN,
            vibrato_cents: -30.0,
            drive: 500.0,
            echo_ms: 99_999.0,
            gain: f32::INFINITY,
            crush_bits: 99,
            ..Recipe::DRY
        }
        .sanitised();
        assert_eq!(wild.pitch, 24.0);
        assert_eq!(wild.formant, 0.0, "not a number: where the field does least");
        assert_eq!(wild.vibrato_cents, 0.0);
        assert_eq!(wild.drive, 10.0);
        assert_eq!(wild.echo_ms, 1000.0);
        assert_eq!(wild.gain, 1.0);
        assert_eq!(wild.crush_bits, 16);
        // Everything the wild value never touched stays where DRY has it.
        let untouched = Recipe { pitch: 0.0, drive: 0.0, echo_ms: 0.0, crush_bits: 0, ..wild };
        assert_eq!(untouched, Recipe::DRY, "nothing else moved");
    }

    #[test]
    fn tone_moves_even_a_dry_voice() {
        let mut v = Voice::new(SR);
        v.set(&Recipe::DRY, 12.0, 1.0);
        let mut x = sine(220.0, 1.0);
        v.process(&mut x);
        // The soft limiter squashes the sine a little, so look at the dominant frequency only.
        let f = dominant_freq(&x[SR as usize / 4..]);
        assert!((f - 440.0).abs() <= 4.0, "got {f}");
    }

    /// A buzzy vowel with a gliding pitch and syllables, closer to speech than a sine.
    fn speech(secs: f32) -> Vec<f32> {
        let mut phase = 0.0f32;
        (0..(SR as f32 * secs) as usize)
            .map(|i| {
                let t = i as f32 / SR as f32;
                phase += (120.0 + 30.0 * (t * 1.3).sin()) / SR as f32;
                let syllables = (0.5 + 0.5 * (t * 2.0 * PI * 3.0).sin()).powi(2);
                (1..12).map(|h| (phase * h as f32 * 2.0 * PI).sin() / h as f32).sum::<f32>() * 0.15 * syllables
            })
            .collect()
    }

    /// A stage the other voice does not use stands still meanwhile; whatever it still held
    /// of the last phrase used to play when its voice came back, into a quiet microphone.
    #[test]
    fn a_voice_picked_again_in_silence_is_silent() {
        const D: Recipe = Recipe::DRY;
        let stages = [
            ("vocoder", Recipe { vocoder_hz: 110.0, gain: 1.4, ..D }),
            ("chord", Recipe { vocoder_hz: 220.0, vocoder_chord: true, ..D }),
            ("hiss", Recipe { hiss: 0.04, ..D }),
            ("crusher", Recipe { crush_hz: 8000.0, crush_bits: 8, ..D }),
            ("echo", Recipe { echo_ms: 380.0, echo_feedback: 0.28, echo_mix: 0.5, ..D }),
            ("comb", Recipe { comb_ms: 6.0, comb_feedback: 0.55, ..D }),
            ("chorus", Recipe { chorus: 0.5, ..D }),
            ("flanger", Recipe { flanger_hz: 0.25, flanger_mix: 0.9, ..D }),
            ("filters", Recipe { highpass_hz: 450.0, lowpass_hz: 2800.0, drive: 4.0, ..D }),
            ("reverb", Recipe { reverb_size: 0.95, reverb_damp: 0.45, reverb_mix: 0.5, ..D }),
            ("pitch", Recipe { pitch: -6.0, harmony: [-12.0, 7.0], harmony_mix: 0.5, ..D }),
        ];
        // The other voice is picked in the middle of a word, and then the speaker stops.
        let (mut word, cut) = (speech(1.0), SR as usize * 3 / 4);
        word.resize(SR as usize * 6, 0.0);
        for (name, recipe) in stages {
            let mut v = Voice::new(SR);
            let mut x = word.clone();
            v.set(&recipe, 0.0, 1.0);
            v.process(&mut x[..cut]);
            v.set(&D, 0.0, 1.0);
            v.process(&mut x[cut..]);
            v.set(&recipe, 0.0, 1.0);
            let mut back = vec![0.0; SR as usize];
            v.process(&mut back);
            let peak = back.iter().fold(0.0f32, |m, s| m.max(s.abs()));
            assert!(peak < 1e-6, "{name} plays {peak} of what it held");
        }
    }

    /// The tone slider and the on/off switch can keep the recipe and still take the pitch
    /// shifter in or out of use, between the microphone and a tap 7 ms behind it. Each used
    /// to step by two to nine times what either side does alone.
    #[test]
    fn a_tone_that_starts_or_stops_the_shifter_does_not_click() {
        const D: Recipe = Recipe::DRY;
        let radio = Recipe { highpass_hz: 450.0, lowpass_hz: 2800.0, drive: 4.0, gain: 1.2, ..D };
        let cave = Recipe { reverb_size: 0.95, reverb_damp: 0.45, reverb_mix: 0.5, ..D };
        let minion = Recipe { pitch: 8.0, ..D };
        let input = speech(2.0);
        let run = |pick: &dyn Fn(usize) -> (Recipe, f32)| {
            let mut v = Voice::new(SR);
            let mut out = input.clone();
            for (b, chunk) in out.chunks_mut(144).enumerate() {
                let (recipe, tone) = pick(b);
                v.set(&recipe, tone, 1.0);
                v.process(chunk);
            }
            out.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0, f32::max)
        };
        // The last two are the effect turned off, which also puts the tone back to 0.
        let pairs = [
            ((radio, 0.0), (radio, 1.0)),
            ((cave, 0.0), (cave, -1.0)),
            ((minion, 0.0), (minion, -8.0)),
            ((cave, 3.0), (D, 0.0)),
            ((D, 3.0), (D, 0.0)),
        ];
        for (i, (a, b)) in pairs.into_iter().enumerate() {
            let steady = run(&|_| a).max(run(&|_| b));
            let toggling = run(&|k| if k / 12 % 2 == 0 { a } else { b });
            assert!(toggling <= steady * 1.25, "pair {i} steps {toggling}, either side alone {steady}");
        }
    }

    #[test]
    fn dry_voice_without_tone_is_bit_exact() {
        let mut v = Voice::new(SR);
        v.set(&Recipe::DRY, 0.0, 1.0);
        let input = sine(330.0, 0.1);
        let mut x = input.clone();
        v.process(&mut x);
        assert_eq!(x, input);
    }

    const SR: u32 = 48_000;

    fn sine(freq: f32, secs: f32) -> Vec<f32> {
        (0..(SR as f32 * secs) as usize).map(|i| (2.0 * PI * freq * i as f32 / SR as f32).sin() * 0.5).collect()
    }

    /// Dominant frequency via a coarse DFT scan; good to ~1 Hz, plenty for a ratio check.
    fn dominant_freq(x: &[f32]) -> f32 {
        let (mut best, mut best_f) = (0.0, 0.0);
        let mut f = 50.0;
        while f < 2000.0 {
            let w = 2.0 * PI * f / SR as f32;
            let (mut re, mut im) = (0.0f32, 0.0f32);
            for (i, s) in x.iter().enumerate() {
                let (sn, cs) = (w * i as f32).sin_cos();
                re += s * cs;
                im += s * sn;
            }
            let m = re * re + im * im;
            if m > best {
                best = m;
                best_f = f;
            }
            f += 1.0;
        }
        best_f
    }

    fn shifted(freq: f32, semis: f32) -> f32 {
        let mut p = PitchShifter::new(SR);
        p.set_ratio(2f32.powf(semis / 12.0));
        let out: Vec<f32> = sine(freq, 1.0).into_iter().map(|s| p.process(s)).collect();
        dominant_freq(&out[SR as usize / 4..])
    }

    #[test]
    fn shifts_up_an_octave() {
        let f = shifted(220.0, 12.0);
        assert!((f - 440.0).abs() <= 4.0, "got {f}");
    }

    #[test]
    fn shifts_minion_interval() {
        let f = shifted(200.0, 8.0);
        assert!((f - 200.0 * 2f32.powf(8.0 / 12.0)).abs() <= 4.0, "got {f}");
    }

    #[test]
    fn shifts_down() {
        let f = shifted(300.0, -6.0);
        assert!((f - 300.0 * 2f32.powf(-0.5)).abs() <= 4.0, "got {f}");
    }

    #[test]
    fn best_lag_matches_the_plain_search() {
        // The straightforward version: score every lag sample by sample through the ring.
        fn plain(p: &PitchShifter, from: f32, sign: isize, longest: usize) -> (usize, f32) {
            let longest = longest.clamp(p.min_lag, p.max_lag);
            let base = p.write as isize - from as isize;
            let score = |lag: usize| {
                let other = base - sign * lag as isize;
                let (mut xy, mut xx, mut yy) = (0.0f32, 1e-9f32, 1e-9f32);
                for j in 0..p.corr_len as isize {
                    let (a, b) = (p.at(base - j), p.at(other - j));
                    xy += a * b;
                    xx += a * a;
                    yy += b * b;
                }
                xy / (xx * yy).sqrt()
            };
            let mut best = (p.min_lag, f32::MIN);
            for lag in (p.min_lag..=longest).step_by(2) {
                let s = score(lag);
                if s > best.1 {
                    best = (lag, s);
                }
            }
            for lag in [best.0.saturating_sub(1), best.0 + 1] {
                if (p.min_lag..=longest).contains(&lag) && score(lag) > best.1 {
                    best = (lag, score(lag));
                }
            }
            best
        }
        let mut p = PitchShifter::new(SR);
        let (low, high) = (speech(0.5), sine(611.0, 0.5));
        // Run past the end of the ring so the windows wrap around it.
        for (a, b) in low.iter().zip(&high) {
            p.listen(a + 0.2 * b);
        }
        for (from, sign, longest) in [(10.0, 1, 1000), (700.0, -1, 650), (300.0, -1, 400), (0.0, 1, 500)] {
            let (lag, score) = p.best_lag(from, sign, longest);
            let (want, best) = plain(&p, from, sign, longest);
            assert!(lag == want && (score - best).abs() < 1e-4, "{lag} {score} vs {want} {best}");
        }
    }

    /// Minion with the tone slider at the top is +20 st. With full-length fades the tap
    /// outran the write head there and nine tenths of the output was off the harmonics.
    #[test]
    fn very_high_shifts_stay_on_the_voice() {
        let f0 = 130.0f32;
        let input: Vec<f32> = (0..SR as usize * 2)
            .map(|i| {
                (1..10).map(|h| (i as f32 * f0 * h as f32 * 2.0 * PI / SR as f32).sin() / h as f32).sum::<f32>() * 0.2
            })
            .collect();
        for semis in [19.0f32, 20.0, 24.0] {
            let r = 2f32.powf(semis / 12.0);
            let mut p = PitchShifter::new(SR);
            p.set_ratio(r);
            let mut out = Vec::new();
            for s in &input {
                out.push(p.process(*s));
                assert!(p.delay >= p.min_delay - 1.0, "{semis:+} st: tap at {} samples", p.delay);
            }
            let x = &out[SR as usize..];
            let total: f32 = x.iter().map(|v| v * v).sum();
            let mut harmonics = 0.0f32;
            let mut k = 1.0;
            while f0 * r * k < 20_000.0 {
                let w = 2.0 * PI * f0 * r * k / SR as f32;
                let (mut re, mut im) = (0.0f32, 0.0f32);
                for (i, s) in x.iter().enumerate() {
                    let (sn, cs) = (w * i as f32).sin_cos();
                    re += s * cs;
                    im += s * sn;
                }
                harmonics += 2.0 * (re * re + im * im) / x.len() as f32;
                k += 1.0;
            }
            let off = 1.0 - harmonics / total;
            assert!(off < 0.5, "{semis:+} st: {:.0} % off the harmonics", off * 100.0);
        }
    }

    #[test]
    fn unity_ratio_is_a_short_delay() {
        let mut p = PitchShifter::new(SR);
        let input = sine(440.0, 0.5);
        let out: Vec<f32> = input.iter().map(|s| p.process(*s)).collect();
        let peak = out[SR as usize / 10..].iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!((peak - 0.5).abs() < 0.02, "peak {peak}");
    }
}

#[cfg(test)]
mod latency {
    use super::*;

    /// The shifter's own delay is the part of the voice latency we control; keep it small
    /// in both directions. Prints the numbers with `--nocapture`.
    #[test]
    fn pitch_shift_delay_stays_under_10_ms() {
        let sr = 48_000u32;
        // A rough voice: 120 Hz buzz with harmonics.
        let input: Vec<f32> = (0..sr as usize * 2)
            .map(|i| {
                (1..8)
                    .map(|h| (i as f32 * 120.0 * h as f32 * std::f32::consts::TAU / sr as f32).sin() / h as f32)
                    .sum::<f32>()
                    * 0.2
            })
            .collect();
        for semis in [-12.0f32, -6.0, -3.0, 3.0, 8.0, 12.0] {
            let mut p = PitchShifter::new(sr);
            p.set_ratio(2f32.powf(semis / 12.0));
            let mut sum = 0.0f64;
            for (i, s) in input.iter().enumerate() {
                p.process(*s);
                if i > sr as usize / 2 {
                    sum += p.delay as f64;
                }
            }
            let avg = sum / (input.len() - sr as usize / 2 - 1) as f64 / sr as f64 * 1000.0;
            println!("{semis:+} st: average delay {avg:.2} ms");
            assert!(avg < 10.0, "{semis:+} st: {avg:.2} ms");
        }
    }
}
