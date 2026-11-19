//! What happens to the microphone before the voice: gain and a noise gate.

use crate::fx::{Envelope, db};

/// Input stage: make-up gain, then a gate that closes on keyboard clicks, fans and
/// breathing between phrases.
pub struct Mic {
    sample_rate: f32,
    envelope: Envelope,
    /// Current gate gain, 0 (closed) to 1 (open), smoothed so it never clicks.
    open: f32,
    /// Samples left before a gate that fell below the threshold starts closing.
    hold: u32,
}

impl Mic {
    pub fn new(sample_rate: u32) -> Self {
        Self { sample_rate: sample_rate as f32, envelope: Envelope::default(), open: 1.0, hold: 0 }
    }

    /// `gate_db` is the threshold in dBFS; `None` leaves the gate open.
    pub fn process(&mut self, block: &mut [f32], gain_db: f32, gate_db: Option<f32>) {
        let gain = db(gain_db);
        let threshold = gate_db.map(db);
        // Fast attack so the first syllable is not cut, slow release so words do not chop.
        let (attack, release) =
            (1.0 - (-1.0 / (0.001 * self.sample_rate)).exp(), 1.0 - (-1.0 / (0.08 * self.sample_rate)).exp());
        let hold = (0.15 * self.sample_rate) as u32;
        for s in block.iter_mut() {
            let x = *s * gain;
            // A NaN or infinity from the driver would never leave the envelope or the voice's filters.
            let x = if x.is_finite() { x } else { 0.0 };
            let level = self.envelope.next(x, 0.2, 0.0015);
            let target = match threshold {
                None => 1.0,
                Some(t) if level >= t => {
                    self.hold = hold;
                    1.0
                }
                Some(_) if self.hold > 0 => {
                    self.hold -= 1;
                    1.0
                }
                Some(_) => 0.0,
            };
            self.open += (target - self.open) * if target > self.open { attack } else { release };
            *s = x * self.open;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fx::sine;

    const SR: u32 = 48_000;

    fn level(x: &[f32]) -> f32 {
        x.iter().fold(0.0f32, |m, s| m.max(s.abs()))
    }

    #[test]
    fn gain_is_in_decibels() {
        let mut m = Mic::new(SR);
        let mut block = vec![0.1f32; 100];
        m.process(&mut block, 6.0, None);
        assert!((block[99] - 0.1995).abs() < 1e-3, "{}", block[99]);
    }

    #[test]
    fn gate_mutes_quiet_noise_and_passes_speech() {
        let mut m = Mic::new(SR);
        let mut hum: Vec<f32> = (0..SR as usize).map(|i| sine(50.0, i, SR as f32) * 0.003).collect();
        m.process(&mut hum, 0.0, Some(-40.0));
        assert!(level(&hum[SR as usize / 2..]) < 1e-4, "hum leaks: {}", level(&hum[SR as usize / 2..]));

        let mut speech: Vec<f32> = (0..SR as usize / 2).map(|i| sine(200.0, i, SR as f32) * 0.3).collect();
        m.process(&mut speech, 0.0, Some(-40.0));
        assert!(level(&speech[SR as usize / 100..]) > 0.29, "speech was gated");
    }

    #[test]
    fn a_broken_sample_does_not_outlast_itself() {
        for gate in [None, Some(-40.0)] {
            let mut m = Mic::new(SR);
            let mut x: Vec<f32> = (0..SR as usize).map(|i| sine(200.0, i, SR as f32) * 0.3).collect();
            (x[1000], x[2000]) = (f32::NAN, f32::INFINITY);
            m.process(&mut x, 0.0, gate);
            assert!(x.iter().all(|s| s.is_finite()), "gate {gate:?}: the output is not finite");
            assert_eq!((x[1000], x[2000]), (0.0, 0.0));
            assert!(level(&x[SR as usize / 2..]) > 0.29, "gate {gate:?}: {}", level(&x[SR as usize / 2..]));
        }
    }

    #[test]
    fn gate_holds_between_words() {
        let mut m = Mic::new(SR);
        let mut word: Vec<f32> = (0..SR as usize / 4).map(|i| sine(200.0, i, SR as f32) * 0.3).collect();
        m.process(&mut word, 0.0, Some(-40.0));
        // A 100 ms pause is shorter than the hold, so the next word starts at full level.
        let mut pause = vec![0.0f32; SR as usize / 10];
        m.process(&mut pause, 0.0, Some(-40.0));
        let mut next = vec![0.3f32; 10];
        m.process(&mut next, 0.0, Some(-40.0));
        assert!(next[0] > 0.29, "first sample after a short pause: {}", next[0]);
    }
}
