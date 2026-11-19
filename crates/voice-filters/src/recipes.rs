//! The voice catalog as plain data, so this crate is all another program needs to change
//! voices: 74 recipes as constants, a stable id for each, and the notes for running them
//! live on an audio stream you already have — WebRTC, a decoder, a file — on any
//! platform. Nothing here knows about Windows.
//!
//! The values are mirrored from the app's `src/voices.rs`, where they were tuned by ear
//! and regression-tested (click-free switching, bounded output, silence after speech).
//! That copy stays the source of truth: when a recipe is retuned there, mirror it here.
//! Ids never change — they are the settings key.
//!
//! # The pipeline
//!
//! One [`Voice`](crate::dsp::Voice) per stream, mono `f32` in and out, any block size.
//! Build it once; after that, `process` allocates nothing, locks nothing, calls nothing:
//!
//! ```
//! use goby_dsp::dsp::Voice;
//! use goby_dsp::recipes;
//!
//! let mut voice = Voice::new(48_000);      // build at the rate you actually run
//! voice.set(&recipes::MINION, 0.0, 1.0);   // recipe, extra semitones, output level
//!
//! // Whatever the decoder hands you, say one 10 ms Opus frame (480 samples at 48 kHz):
//! let mut block = vec![0.0f32; 480];
//! voice.process(&mut block);               // in place, soft-limited to -1..1
//! ```
//!
//! # Receiving WebRTC audio
//!
//! ```text
//! Opus RTP → decode (48 kHz f32) → [fold to mono] → [Mic: gain + gate, optional]
//!          → Voice::process → re-encode / play out
//! ```
//!
//! WebRTC's Opus decoder outputs 48 kHz — the rate the catalog was tuned at — so nothing
//! needs resampling. It delivers 10 ms frames; feed each through `process` as it arrives.
//! Blocks of any size are fine, odd tails included. Decoded audio comes as planar `f32`
//! (use the mono plane directly) or interleaved bytes; for the latter,
//! [`to_mono`](crate::stream::to_mono) folds any interleaved packet (I16, I32 or F32,
//! any channel count) into the mono block the voice wants.
//!
//! # Sample rate
//!
//! [`Voice::new`](crate::dsp::Voice::new) takes whatever rate you run at and every stage
//! scales with it, but the recipe numbers were tuned and tested at 48 kHz. Keep 48 kHz
//! when you can; other rates work yet shift how the tuned voices sound.
//!
//! # Channels
//!
//! [`Voice`](crate::dsp::Voice) is strictly mono. For stereo material run one instance
//! per channel; for a call, fold to mono first — one voice, half the CPU.
//!
//! # Threads
//!
//! `process` is safe to call straight inside a real-time audio callback. If the WebRTC
//! callback must stay cheap, hand `f32` samples to a worker thread through
//! [`ring`](crate::ring::ring) — a lock-free SPSC ring, `push_all` on the decoder side
//! and `pop` on the processing side — and after a stall skip the backlog instead of
//! playing it late. A voice is plain data that moves freely; building one allocates, so
//! build it on the thread that runs it. To play the result out to a local device with
//! its own clock, [`Reader`](crate::stream::Reader) is the app's adaptive resampler for
//! the drift.
//!
//! # Latency
//!
//! [`Voice::delay`](crate::dsp::Voice::delay) reports the algorithmic delay in samples:
//! about 7 ms while the pitch shifter runs, under 12 ms once the formant shifter joins,
//! zero without either. Echo and reverb are tails, not pipeline delay. Subtract the
//! delay when timestamping, to keep lips in sync.
//!
//! # Switching voices
//!
//! [`Voice::set`](crate::dsp::Voice::set) is instant and lock-free; the new recipe takes
//! over on the next `process`, through a 10 ms level dip (5 ms each way at 48 kHz), so
//! switching mid-word never clicks. `tone` bends pitch on top of the recipe — the
//! harmonies with it — and a tone that turns the shifter on or off dips the same way;
//! the rest bend at once. `volume` is the output level. `tune` in the recipe snaps pitch
//! to a scale instead of shifting it (1 any semitone, 2 a major scale, 3 its five-note
//! core).
//!
//! # Levels
//!
//! Feed about ±0.5 in; the last stage is a `tanh` soft limit, so output never leaves
//! -1..1 however loud the input. Recipes that come out quieter carry `gain` up to 2.4 to
//! make up for it — loudness is part of the recipe, so leave `volume` at 1.0 unless you
//! want a trim of your own on top.
//!
//! # Before the voice
//!
//! [`Mic`](crate::mic::Mic) adds input gain and a noise gate for a far-field or noisy
//! microphone; audio that already arrived clean over WebRTC usually skips it.
//!
//! # One-time thread setup
//!
//! Call [`flush_denormals`](crate::dsp::flush_denormals) once per processing thread
//! (x86 only, a no-op elsewhere) so denormal values in the reverb tails cannot stall
//! the DSP.
//!
//! # Cost
//!
//! The app ships this whole pipeline under 1 % of one CPU core with the microphone live
//! (its README figure); recipes are data, so switching costs nothing beyond the fade.
//! Each voice instance owns its delay lines and reverb, on the order of a hundred KB at
//! 48 kHz.
//!
//! # Recipe fields at a glance
//!
//! [`Recipe`](crate::dsp::Recipe) documents every field; the units, for quick reference:
//!
//! | Field | Unit | Off |
//! |---|---|---|
//! | `pitch` | semitones, the shifter clamps to ±24 | 0 |
//! | `formant` | throat ratio, 1.15 smaller, 0.85 bigger; 0 = timbre follows pitch | 0 |
//! | `tune` | 1 nearest semitone, 2 major scale, 3 its five-note core | 0 |
//! | `harmony` | two extra copies, semitones each; 0 = unused | `[0, 0]` |
//! | `harmony_mix` | 0..1 | 0 |
//! | `vibrato_hz` / `vibrato_cents` | Hz / cents | 0 |
//! | `wobble_hz` / `wobble_cents` | Hz / cents, slow random drift | 0 |
//! | `ring_hz` / `ring_mix` | Hz / 0..1 | 0 |
//! | `whisper` | 0..1, noise replaces the voiced buzz | 0 |
//! | `vocoder_hz` / `vocoder_chord` | Hz carrier / bool | 0 |
//! | `comb_ms` / `comb_feedback` | ms / 0..1 | 0 |
//! | `chorus` | 0..1 | 0 |
//! | `flanger_hz` / `flanger_mix` | Hz / 0..1 | 0 |
//! | `tremolo_hz` / `tremolo_depth` | Hz / 0..1 | 0 |
//! | `highpass_hz` / `lowpass_hz` | Hz, 0 = off | 0 |
//! | `eq_hz` / `eq_db` | Hz / dB, one peaking band | 0 |
//! | `bass_db` | dB shelf under 180 Hz | 0 |
//! | `squash` | compression ratio above -20 dB; 0 = off | 0 |
//! | `drive` / `hard_clip` | saturation amount / bool square-off | 0 |
//! | `crush_hz` / `crush_bits` | sample-and-hold Hz / bits; 0 = off | 0 |
//! | `hiss` | radio static while talking, 0..1 | 0 |
//! | `dropout` | gaps and stutters, 0..1 | 0 |
//! | `roger` | keyed-radio beep and static burst, 0..1 | 0 |
//! | `echo_ms` / `echo_feedback` / `echo_mix` | ms / 0..1 / 0..1 | 0 |
//! | `reverb_size` / `reverb_damp` / `reverb_mix` | 0..1 room size / damping / wet | 0 |
//! | `gain` | linear make-up gain | 1.0 |
//!
//! # License
//!
//! GPL-3.0-or-later, like the rest of the crate.

use crate::dsp::Recipe;

/// Which part of the catalog a voice sits in; the app localizes these names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Category {
    Basics,
    People,
    Creatures,
    Robots,
    Places,
    Devices,
    Fun,
}

impl Category {
    pub const ALL: [Category; 7] = [
        Category::Basics,
        Category::People,
        Category::Creatures,
        Category::Robots,
        Category::Places,
        Category::Devices,
        Category::Fun,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Category::Basics => "Basics",
            Category::People => "People",
            Category::Creatures => "Creatures",
            Category::Robots => "Robots",
            Category::Places => "Places",
            Category::Devices => "Devices",
            Category::Fun => "Fun",
        }
    }
}

/// One catalog entry: the settings id, the category, and the recipe itself.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VoiceDef {
    /// Stable key for settings files; never rename.
    pub id: &'static str,
    pub category: Category,
    pub recipe: Recipe,
}

// Basics: the first six keep the ids the app started with.

/// No change: the dry signal, as a recipe to hand to `Voice::set`.
pub const OFF: Recipe = Recipe::DRY;

/// The Minion look-alike: +8 semitones with the formants riding along (chipmunk colour).
pub const MINION: Recipe = Recipe { pitch: 8.0, ..Recipe::DRY };

/// Full helium: +12 semitones, formants riding along.
pub const HELIUM: Recipe = Recipe { pitch: 12.0, ..Recipe::DRY };

/// A giant: -6 semitones in a small room.
pub const GIANT: Recipe = Recipe { pitch: -6.0, reverb_size: 0.3, reverb_damp: 0.5, reverb_mix: 0.12, ..Recipe::DRY };

/// The classic robot: 55 Hz ring modulation at full wet over a 6 ms comb.
pub const ROBOT: Recipe =
    Recipe { ring_hz: 55.0, ring_mix: 1.0, comb_ms: 6.0, comb_feedback: 0.55, gain: 1.4, ..Recipe::DRY };

/// AM radio: the 450-2800 Hz band, driven.
pub const RADIO: Recipe = Recipe { highpass_hz: 450.0, lowpass_hz: 2800.0, drive: 4.0, gain: 1.2, ..Recipe::DRY };

// People

/// A kid: +5 semitones with a touch of chorus.
pub const KID: Recipe = Recipe { pitch: 5.0, chorus: 0.15, ..Recipe::DRY };

/// A baby: +9 semitones with a fast 6 Hz waver.
pub const BABY: Recipe = Recipe { pitch: 9.0, tremolo_hz: 6.0, tremolo_depth: 0.15, ..Recipe::DRY };

/// A woman: +5 semitones and the throat shrunk further (1.15), so it reads as pitch,
/// not chipmunk; the 120 Hz highpass clears rumble.
pub const WOMAN: Recipe = Recipe { pitch: 5.0, formant: 1.15, highpass_hz: 120.0, gain: 1.2, ..Recipe::DRY };

/// A man: -5 semitones with the throat enlarged (0.87).
pub const MAN: Recipe = Recipe { pitch: -5.0, formant: 0.87, gain: 1.2, ..Recipe::DRY };

/// A bass voice: -7 semitones, softened above 6 kHz, in a light room.
pub const BASS: Recipe =
    Recipe { pitch: -7.0, lowpass_hz: 6000.0, reverb_size: 0.2, reverb_damp: 0.6, reverb_mix: 0.1, ..Recipe::DRY };

/// Grandpa: -2 semitones, a shaky 5 Hz vibrato, dulled above 4 kHz.
pub const GRANDPA: Recipe =
    Recipe { pitch: -2.0, vibrato_hz: 5.0, vibrato_cents: 18.0, lowpass_hz: 4000.0, ..Recipe::DRY };

/// Grandma: +2 semitones, a thinner 5.5 Hz vibrato, lows cut below 200 Hz.
pub const GRANDMA: Recipe =
    Recipe { pitch: 2.0, vibrato_hz: 5.5, vibrato_cents: 28.0, highpass_hz: 200.0, ..Recipe::DRY };

/// A teen: +2 semitones with a slow 2 Hz, 45-cent wander.
pub const TEEN: Recipe = Recipe { pitch: 2.0, wobble_hz: 2.0, wobble_cents: 45.0, ..Recipe::DRY };

/// A pirate: -4 semitones, a bit of drive, a small room.
pub const PIRATE: Recipe =
    Recipe { pitch: -4.0, drive: 2.0, reverb_size: 0.25, reverb_damp: 0.5, reverb_mix: 0.12, ..Recipe::DRY };

/// Nervous: a 7 Hz vibrato and a 7 Hz tremolo; pitch untouched.
pub const NERVOUS: Recipe =
    Recipe { vibrato_hz: 7.0, vibrato_cents: 35.0, tremolo_hz: 7.0, tremolo_depth: 0.2, ..Recipe::DRY };

/// Drunk: a lazy 1.2 Hz, 70-cent sway with a short 140 ms slap echo.
pub const DRUNK: Recipe = Recipe {
    pitch: -1.0,
    wobble_hz: 1.2,
    wobble_cents: 70.0,
    echo_ms: 140.0,
    echo_feedback: 0.2,
    echo_mix: 0.2,
    ..Recipe::DRY
};

/// A whisper: the voiced buzz replaced by noise, the vowel shapes kept, band-limited
/// from 300 Hz and made up louder.
pub const WHISPER: Recipe = Recipe { whisper: 1.0, highpass_hz: 300.0, gain: 1.6, ..Recipe::DRY };

/// Sleepy: -2 semitones, dull above 3.5 kHz, breathing at 0.5 Hz.
pub const SLEEPY: Recipe =
    Recipe { pitch: -2.0, lowpass_hz: 3500.0, tremolo_hz: 0.5, tremolo_depth: 0.25, ..Recipe::DRY };

/// The movie announcer: -2 semitones, driven, a 260 ms echo in a big room.
pub const ANNOUNCER: Recipe = Recipe {
    pitch: -2.0,
    drive: 1.5,
    echo_ms: 260.0,
    echo_feedback: 0.25,
    echo_mix: 0.2,
    reverb_size: 0.7,
    reverb_damp: 0.4,
    reverb_mix: 0.2,
    ..Recipe::DRY
};

/// A strongman: -3 semitones with the throat enlarged (0.82) and a little drive.
pub const STRONGMAN: Recipe = Recipe { pitch: -3.0, formant: 0.82, drive: 1.3, ..Recipe::DRY };

/// The film-trailer voice: -3 semitones, +5 dB of bass, crushed by a 6:1 compressor,
/// +2.2 make-up gain.
pub const TRAILER: Recipe = Recipe {
    pitch: -3.0,
    formant: 0.88,
    bass_db: 5.0,
    squash: 6.0,
    reverb_size: 0.35,
    reverb_damp: 0.5,
    reverb_mix: 0.12,
    gain: 2.2,
    ..Recipe::DRY
};

/// A villain: -2 semitones over a sub-octave layer, in a big dark room.
pub const VILLAIN: Recipe = Recipe {
    pitch: -2.0,
    formant: 0.9,
    harmony: [-12.0, 0.0],
    harmony_mix: 0.25,
    reverb_size: 0.7,
    reverb_damp: 0.7,
    reverb_mix: 0.2,
    ..Recipe::DRY
};

/// The sports commentator: a 2.5 kHz presence peak, compressed, driven, faint static,
/// a 120 ms echo.
pub const COMMENTATOR: Recipe = Recipe {
    eq_hz: 2500.0,
    eq_db: 6.0,
    squash: 4.0,
    drive: 2.0,
    hiss: 0.03,
    echo_ms: 120.0,
    echo_feedback: 0.2,
    echo_mix: 0.2,
    gain: 1.25,
    ..Recipe::DRY
};

// Creatures

/// A gremlin: +6 semitones with a 90 Hz buzz and some drive.
pub const GREMLIN: Recipe = Recipe { pitch: 6.0, ring_hz: 90.0, ring_mix: 0.15, drive: 1.5, ..Recipe::DRY };

/// A rodent: +10 semitones doubled by chorus.
pub const RODENT: Recipe = Recipe { pitch: 10.0, chorus: 0.3, ..Recipe::DRY };

/// An ogre: -6 semitones, driven, dull above 5 kHz.
pub const OGRE: Recipe = Recipe { pitch: -6.0, drive: 2.5, lowpass_hz: 5000.0, ..Recipe::DRY };

/// A troll: -7 semitones with a 30 Hz rumble under it, in a small room.
pub const TROLL: Recipe = Recipe {
    pitch: -7.0,
    ring_hz: 30.0,
    ring_mix: 0.3,
    reverb_size: 0.3,
    reverb_damp: 0.5,
    reverb_mix: 0.15,
    ..Recipe::DRY
};

/// A demon: -8 semitones over a sub-octave layer, driven hard, in the big dark room.
pub const DEMON: Recipe = Recipe {
    pitch: -8.0,
    harmony: [-12.0, 0.0],
    harmony_mix: 0.45,
    drive: 2.5,
    reverb_size: 0.8,
    reverb_damp: 0.4,
    reverb_mix: 0.25,
    ..Recipe::DRY
};

/// A zombie: -3 semitones with a sick 3 Hz vibrato, crushed to 8 bits, dull above 2.5 kHz.
pub const ZOMBIE: Recipe = Recipe {
    pitch: -3.0,
    vibrato_hz: 3.0,
    vibrato_cents: 25.0,
    crush_hz: 12000.0,
    crush_bits: 8,
    lowpass_hz: 2500.0,
    gain: 1.3,
    ..Recipe::DRY
};

/// A ghost: breathy, a very slow flanger, in a huge reverberant space.
pub const GHOST: Recipe = Recipe {
    pitch: 1.0,
    whisper: 0.35,
    flanger_hz: 0.2,
    flanger_mix: 0.5,
    reverb_size: 0.75,
    reverb_damp: 0.35,
    reverb_mix: 0.4,
    ..Recipe::DRY
};

/// Possessed: a -5/+4 semitone harmony stack with a 60 Hz shimmer, in a big room; pitch
/// untouched.
pub const POSSESSED: Recipe = Recipe {
    harmony: [-5.0, 4.0],
    harmony_mix: 0.5,
    ring_hz: 60.0,
    ring_mix: 0.15,
    reverb_size: 0.8,
    reverb_damp: 0.5,
    reverb_mix: 0.3,
    ..Recipe::DRY
};

/// A vampire: -3 semitones with chorus, in a huge room left bright.
pub const VAMPIRE: Recipe =
    Recipe { pitch: -3.0, chorus: 0.3, reverb_size: 0.8, reverb_damp: 0.3, reverb_mix: 0.2, ..Recipe::DRY };

/// A dragon: -10 semitones over a -5 semitone layer, driven, the room at its biggest.
pub const DRAGON: Recipe = Recipe {
    pitch: -10.0,
    harmony: [-5.0, 0.0],
    harmony_mix: 0.4,
    drive: 3.0,
    reverb_size: 0.9,
    reverb_damp: 0.5,
    reverb_mix: 0.25,
    ..Recipe::DRY
};

/// A swarm of insects: 110 Hz ring modulation with a 20 Hz tremolo; pitch untouched.
pub const SWARM: Recipe = Recipe { ring_hz: 110.0, ring_mix: 0.4, tremolo_hz: 20.0, tremolo_depth: 0.4, ..Recipe::DRY };

/// An alien: +3 semitones, 250 Hz ring modulation and a 1.5 Hz flanger.
pub const ALIEN: Recipe =
    Recipe { pitch: 3.0, ring_hz: 250.0, ring_mix: 0.3, flanger_hz: 1.5, flanger_mix: 0.4, ..Recipe::DRY };

// Robots & sci-fi

/// An android: the voice played through a 110 Hz vocoder carrier.
pub const ANDROID: Recipe = Recipe { vocoder_hz: 110.0, gain: 1.4, ..Recipe::DRY };

/// A dictator: 30 Hz ring modulation at full wet, band-limited hard, clipped square.
pub const DICTATOR: Recipe = Recipe {
    ring_hz: 30.0,
    ring_mix: 1.0,
    highpass_hz: 400.0,
    lowpass_hz: 3400.0,
    drive: 3.0,
    hard_clip: true,
    gain: 1.4,
    ..Recipe::DRY
};

/// A cyborg: -4 semitones over a 3 ms metallic comb, in a small room.
pub const CYBORG: Recipe = Recipe {
    pitch: -4.0,
    comb_ms: 3.0,
    comb_feedback: 0.6,
    reverb_size: 0.3,
    reverb_damp: 0.4,
    reverb_mix: 0.12,
    ..Recipe::DRY
};

/// A mutant: a 2 Hz wander of a full 300 cents, plus chorus; pitch untouched.
pub const MUTANT: Recipe = Recipe { wobble_hz: 2.0, wobble_cents: 300.0, chorus: 0.3, ..Recipe::DRY };

/// 8-bit game: crushed to 5 bits at 8 kHz with a faint 440 Hz tone. (Id "8bit": a Rust
/// constant cannot start with a digit.)
pub const EIGHT_BIT: Recipe =
    Recipe { crush_hz: 8000.0, crush_bits: 5, ring_hz: 440.0, ring_mix: 0.1, gain: 1.2, ..Recipe::DRY };

/// A synth choir: vocoded onto a 220 Hz chord, in a hall.
pub const SYNTH_CHOIR: Recipe = Recipe {
    vocoder_hz: 220.0,
    vocoder_chord: true,
    reverb_size: 0.6,
    reverb_damp: 0.4,
    reverb_mix: 0.2,
    gain: 1.4,
    ..Recipe::DRY
};

/// A hologram: an 8 Hz shimmer, a slow flanger and a 90 ms echo; pitch untouched.
pub const HOLOGRAM: Recipe = Recipe {
    ring_hz: 8.0,
    ring_mix: 0.25,
    flanger_hz: 0.6,
    flanger_mix: 0.5,
    echo_ms: 90.0,
    echo_feedback: 0.3,
    echo_mix: 0.25,
    ..Recipe::DRY
};

/// A spaceship interior: band-limited, faint static, driven, a 1.5 ms comb.
pub const SPACESHIP: Recipe = Recipe {
    highpass_hz: 500.0,
    lowpass_hz: 2500.0,
    hiss: 0.02,
    drive: 2.0,
    comb_ms: 1.5,
    comb_feedback: 0.3,
    ..Recipe::DRY
};

/// A robot breaking down: the robot recipe plus a 6 Hz, 250-cent stutter and dropouts.
pub const BROKEN_ROBOT: Recipe = Recipe {
    ring_hz: 55.0,
    ring_mix: 1.0,
    comb_ms: 6.0,
    comb_feedback: 0.55,
    wobble_hz: 6.0,
    wobble_cents: 250.0,
    dropout: 1.0,
    gain: 1.4,
    ..Recipe::DRY
};

/// A helmet radio: an 0.8 ms comb, a 300 Hz presence bump, compressed, dulled, +2.4 gain.
pub const HELMET: Recipe = Recipe {
    comb_ms: 0.8,
    comb_feedback: 0.5,
    eq_hz: 300.0,
    eq_db: 4.0,
    squash: 4.0,
    lowpass_hz: 5000.0,
    gain: 2.4,
    ..Recipe::DRY
};

// Places

/// A cave: the longest reverberant room, half wet.
pub const CAVE: Recipe = Recipe { reverb_size: 0.95, reverb_damp: 0.45, reverb_mix: 0.5, ..Recipe::DRY };

/// A cathedral: the biggest room, left bright.
pub const CATHEDRAL: Recipe = Recipe { reverb_size: 1.0, reverb_damp: 0.15, reverb_mix: 0.45, ..Recipe::DRY };

/// A bathroom: short and bright.
pub const BATHROOM: Recipe = Recipe { reverb_size: 0.3, reverb_damp: 0.05, reverb_mix: 0.4, ..Recipe::DRY };

/// A stadium: a 300 ms echo in a big room.
pub const STADIUM: Recipe = Recipe {
    echo_ms: 300.0,
    echo_feedback: 0.25,
    echo_mix: 0.35,
    reverb_size: 0.8,
    reverb_damp: 0.3,
    reverb_mix: 0.25,
    ..Recipe::DRY
};

/// Underwater: everything above 900 Hz gone, swimming in chorus with a 3 Hz sway.
pub const UNDERWATER: Recipe =
    Recipe { lowpass_hz: 900.0, chorus: 0.5, vibrato_hz: 3.0, vibrato_cents: 25.0, gain: 1.5, ..Recipe::DRY };

/// A canyon: a 380 ms echo, half wet.
pub const CANYON: Recipe = Recipe { echo_ms: 380.0, echo_feedback: 0.28, echo_mix: 0.5, ..Recipe::DRY };

/// A tin can: a resonant 1.2 ms comb, lows cut below 300 Hz.
pub const TIN_CAN: Recipe = Recipe { comb_ms: 1.2, comb_feedback: 0.7, highpass_hz: 300.0, ..Recipe::DRY };

// Devices

/// A phone: the 300-3400 Hz telephone band.
pub const PHONE: Recipe = Recipe { highpass_hz: 300.0, lowpass_hz: 3400.0, drive: 1.5, gain: 1.3, ..Recipe::DRY };

/// A megaphone: 500-4000 Hz, driven hard and clipped, with a little space around it.
pub const MEGAPHONE: Recipe = Recipe {
    highpass_hz: 500.0,
    lowpass_hz: 4000.0,
    drive: 5.0,
    hard_clip: true,
    reverb_size: 0.4,
    reverb_damp: 0.4,
    reverb_mix: 0.15,
    ..Recipe::DRY
};

/// A walkie-talkie: 400-3000 Hz, crushed to 8 bits, static, driven.
pub const WALKIE: Recipe = Recipe {
    highpass_hz: 400.0,
    lowpass_hz: 3000.0,
    crush_hz: 8000.0,
    crush_bits: 8,
    hiss: 0.04,
    drive: 2.0,
    ..Recipe::DRY
};

/// An old radio: 300-3500 Hz, driven, static, a slow 0.5 Hz waver.
pub const OLD_RADIO: Recipe = Recipe {
    highpass_hz: 300.0,
    lowpass_hz: 3500.0,
    drive: 2.5,
    hiss: 0.02,
    tremolo_hz: 0.5,
    tremolo_depth: 0.2,
    ..Recipe::DRY
};

/// Drive-through: 600-3000 Hz, driven, static.
pub const DRIVE_THRU: Recipe = Recipe { highpass_hz: 600.0, lowpass_hz: 3000.0, drive: 4.0, hiss: 0.03, ..Recipe::DRY };

/// A worn cassette: a slow 0.8 Hz wow, dull above 7 kHz, a hiss floor.
pub const CASSETTE: Recipe =
    Recipe { wobble_hz: 0.8, wobble_cents: 20.0, lowpass_hz: 7000.0, hiss: 0.01, drive: 1.3, ..Recipe::DRY };

/// Squad radio: 400-3000 Hz, compressed, driven, static, and a roger beep on keying.
pub const SQUAD_RADIO: Recipe = Recipe {
    highpass_hz: 400.0,
    lowpass_hz: 3000.0,
    squash: 4.0,
    drive: 2.0,
    hiss: 0.03,
    roger: 1.0,
    gain: 1.4,
    ..Recipe::DRY
};

/// A cheap headset: 250-3200 Hz, overdriven into hard clipping, crushed, pulled 4.4 dB
/// down.
pub const CHEAP_HEADSET: Recipe = Recipe {
    highpass_hz: 250.0,
    lowpass_hz: 3200.0,
    drive: 6.0,
    hard_clip: true,
    crush_hz: 9000.0,
    crush_bits: 7,
    gain: 0.45,
    ..Recipe::DRY
};

/// A bad signal: the phone band with static and gaps and stutters.
pub const BAD_SIGNAL: Recipe =
    Recipe { highpass_hz: 300.0, lowpass_hz: 3400.0, drive: 1.5, hiss: 0.02, dropout: 0.8, gain: 1.3, ..Recipe::DRY };

// Fun

/// A chipmunk choir: +12 semitones with +16 and +19 semitone layers.
pub const CHIPMUNK_CHOIR: Recipe = Recipe { pitch: 12.0, harmony: [16.0, 19.0], harmony_mix: 0.5, ..Recipe::DRY };

/// Opera: a 5.5 Hz, 45-cent vibrato in a big hall; pitch untouched.
pub const OPERA: Recipe =
    Recipe { vibrato_hz: 5.5, vibrato_cents: 45.0, reverb_size: 0.9, reverb_damp: 0.3, reverb_mix: 0.3, ..Recipe::DRY };

/// A choir: +4 and +7 semitone layers, chorus, a big hall; pitch untouched.
pub const CHOIR: Recipe = Recipe {
    harmony: [4.0, 7.0],
    harmony_mix: 0.5,
    chorus: 0.3,
    reverb_size: 0.9,
    reverb_damp: 0.3,
    reverb_mix: 0.3,
    ..Recipe::DRY
};

/// Karaoke: a 180 ms echo in a medium room.
pub const KARAOKE: Recipe = Recipe {
    echo_ms: 180.0,
    echo_feedback: 0.3,
    echo_mix: 0.3,
    reverb_size: 0.5,
    reverb_damp: 0.2,
    reverb_mix: 0.2,
    ..Recipe::DRY
};

/// Glitch: an 8 Hz pitch stutter, crushed to 10 bits, a 15 Hz buzz.
pub const GLITCH: Recipe = Recipe {
    wobble_hz: 8.0,
    wobble_cents: 60.0,
    crush_hz: 11000.0,
    crush_bits: 10,
    ring_hz: 15.0,
    ring_mix: 0.2,
    ..Recipe::DRY
};

/// A duet: the voice plus its sub-octave, half and half.
pub const DUET: Recipe = Recipe { harmony: [-12.0, 0.0], harmony_mix: 0.5, ..Recipe::DRY };

/// A flying jet: a slow 0.25 Hz flanger, nearly full wet.
pub const JET: Recipe = Recipe { flanger_hz: 0.25, flanger_mix: 0.9, ..Recipe::DRY };

/// A helium robot: +9 semitones, 70 Hz ring modulation, a 4 ms comb.
pub const HELIUM_ROBOT: Recipe =
    Recipe { pitch: 9.0, ring_hz: 70.0, ring_mix: 0.7, comb_ms: 4.0, comb_feedback: 0.4, ..Recipe::DRY };

/// Slow motion: -10 semitones with the throat enlarged (0.8), in a huge dark room.
pub const SLOW_MOTION: Recipe =
    Recipe { pitch: -10.0, formant: 0.8, reverb_size: 0.9, reverb_damp: 0.7, reverb_mix: 0.3, ..Recipe::DRY };

/// Autotune: the pitch snapped to the notes of a major pentatonic scale.
pub const AUTOTUNE: Recipe = Recipe { tune: 3, ..Recipe::DRY };

/// A chord choir: pitch snapped to the major scale, +4/+7 layers, a hall.
pub const CHORD_CHOIR: Recipe = Recipe {
    tune: 2,
    harmony: [4.0, 7.0],
    harmony_mix: 0.6,
    reverb_size: 0.7,
    reverb_damp: 0.3,
    reverb_mix: 0.25,
    ..Recipe::DRY
};

/// A talkbox: pitch snapped to any semitone, vocoded onto a 110 Hz carrier.
pub const TALKBOX: Recipe = Recipe { tune: 1, vocoder_hz: 110.0, ..Recipe::DRY };

/// The catalog, in the app's order; the first six keep the ids the app started with.
pub static ALL: &[VoiceDef] = &[
    VoiceDef { id: "off", category: Category::Basics, recipe: OFF },
    VoiceDef { id: "minion", category: Category::Basics, recipe: MINION },
    VoiceDef { id: "helium", category: Category::Basics, recipe: HELIUM },
    VoiceDef { id: "giant", category: Category::Basics, recipe: GIANT },
    VoiceDef { id: "robot", category: Category::Basics, recipe: ROBOT },
    VoiceDef { id: "radio", category: Category::Basics, recipe: RADIO },
    VoiceDef { id: "kid", category: Category::People, recipe: KID },
    VoiceDef { id: "baby", category: Category::People, recipe: BABY },
    VoiceDef { id: "woman", category: Category::People, recipe: WOMAN },
    VoiceDef { id: "man", category: Category::People, recipe: MAN },
    VoiceDef { id: "bass", category: Category::People, recipe: BASS },
    VoiceDef { id: "grandpa", category: Category::People, recipe: GRANDPA },
    VoiceDef { id: "grandma", category: Category::People, recipe: GRANDMA },
    VoiceDef { id: "teen", category: Category::People, recipe: TEEN },
    VoiceDef { id: "pirate", category: Category::People, recipe: PIRATE },
    VoiceDef { id: "nervous", category: Category::People, recipe: NERVOUS },
    VoiceDef { id: "drunk", category: Category::People, recipe: DRUNK },
    VoiceDef { id: "whisper", category: Category::People, recipe: WHISPER },
    VoiceDef { id: "sleepy", category: Category::People, recipe: SLEEPY },
    VoiceDef { id: "announcer", category: Category::People, recipe: ANNOUNCER },
    VoiceDef { id: "strongman", category: Category::People, recipe: STRONGMAN },
    VoiceDef { id: "trailer", category: Category::People, recipe: TRAILER },
    VoiceDef { id: "villain", category: Category::People, recipe: VILLAIN },
    VoiceDef { id: "commentator", category: Category::People, recipe: COMMENTATOR },
    VoiceDef { id: "gremlin", category: Category::Creatures, recipe: GREMLIN },
    VoiceDef { id: "rodent", category: Category::Creatures, recipe: RODENT },
    VoiceDef { id: "ogre", category: Category::Creatures, recipe: OGRE },
    VoiceDef { id: "troll", category: Category::Creatures, recipe: TROLL },
    VoiceDef { id: "demon", category: Category::Creatures, recipe: DEMON },
    VoiceDef { id: "zombie", category: Category::Creatures, recipe: ZOMBIE },
    VoiceDef { id: "ghost", category: Category::Creatures, recipe: GHOST },
    VoiceDef { id: "possessed", category: Category::Creatures, recipe: POSSESSED },
    VoiceDef { id: "vampire", category: Category::Creatures, recipe: VAMPIRE },
    VoiceDef { id: "dragon", category: Category::Creatures, recipe: DRAGON },
    VoiceDef { id: "swarm", category: Category::Creatures, recipe: SWARM },
    VoiceDef { id: "alien", category: Category::Creatures, recipe: ALIEN },
    VoiceDef { id: "android", category: Category::Robots, recipe: ANDROID },
    VoiceDef { id: "dictator", category: Category::Robots, recipe: DICTATOR },
    VoiceDef { id: "cyborg", category: Category::Robots, recipe: CYBORG },
    VoiceDef { id: "mutant", category: Category::Robots, recipe: MUTANT },
    VoiceDef { id: "8bit", category: Category::Robots, recipe: EIGHT_BIT },
    VoiceDef { id: "synth_choir", category: Category::Robots, recipe: SYNTH_CHOIR },
    VoiceDef { id: "hologram", category: Category::Robots, recipe: HOLOGRAM },
    VoiceDef { id: "spaceship", category: Category::Robots, recipe: SPACESHIP },
    VoiceDef { id: "broken_robot", category: Category::Robots, recipe: BROKEN_ROBOT },
    VoiceDef { id: "helmet", category: Category::Robots, recipe: HELMET },
    VoiceDef { id: "cave", category: Category::Places, recipe: CAVE },
    VoiceDef { id: "cathedral", category: Category::Places, recipe: CATHEDRAL },
    VoiceDef { id: "bathroom", category: Category::Places, recipe: BATHROOM },
    VoiceDef { id: "stadium", category: Category::Places, recipe: STADIUM },
    VoiceDef { id: "underwater", category: Category::Places, recipe: UNDERWATER },
    VoiceDef { id: "canyon", category: Category::Places, recipe: CANYON },
    VoiceDef { id: "tin_can", category: Category::Places, recipe: TIN_CAN },
    VoiceDef { id: "phone", category: Category::Devices, recipe: PHONE },
    VoiceDef { id: "megaphone", category: Category::Devices, recipe: MEGAPHONE },
    VoiceDef { id: "walkie", category: Category::Devices, recipe: WALKIE },
    VoiceDef { id: "old_radio", category: Category::Devices, recipe: OLD_RADIO },
    VoiceDef { id: "drive_thru", category: Category::Devices, recipe: DRIVE_THRU },
    VoiceDef { id: "cassette", category: Category::Devices, recipe: CASSETTE },
    VoiceDef { id: "squad_radio", category: Category::Devices, recipe: SQUAD_RADIO },
    VoiceDef { id: "cheap_headset", category: Category::Devices, recipe: CHEAP_HEADSET },
    VoiceDef { id: "bad_signal", category: Category::Devices, recipe: BAD_SIGNAL },
    VoiceDef { id: "chipmunk_choir", category: Category::Fun, recipe: CHIPMUNK_CHOIR },
    VoiceDef { id: "opera", category: Category::Fun, recipe: OPERA },
    VoiceDef { id: "choir", category: Category::Fun, recipe: CHOIR },
    VoiceDef { id: "karaoke", category: Category::Fun, recipe: KARAOKE },
    VoiceDef { id: "glitch", category: Category::Fun, recipe: GLITCH },
    VoiceDef { id: "duet", category: Category::Fun, recipe: DUET },
    VoiceDef { id: "jet", category: Category::Fun, recipe: JET },
    VoiceDef { id: "helium_robot", category: Category::Fun, recipe: HELIUM_ROBOT },
    VoiceDef { id: "slow_motion", category: Category::Fun, recipe: SLOW_MOTION },
    VoiceDef { id: "autotune", category: Category::Fun, recipe: AUTOTUNE },
    VoiceDef { id: "chord_choir", category: Category::Fun, recipe: CHORD_CHOIR },
    VoiceDef { id: "talkbox", category: Category::Fun, recipe: TALKBOX },
];

/// The voice with the given settings id.
pub fn index_of(id: &str) -> Option<usize> {
    ALL.iter().position(|v| v.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_mirrors_the_app() {
        assert_eq!(ALL.len(), 74, "src/voices.rs has 74; mirror them all");
        let first: Vec<_> = ALL[..6].iter().map(|v| v.id).collect();
        assert_eq!(first, ["off", "minion", "helium", "giant", "robot", "radio"]);
        for (i, v) in ALL.iter().enumerate() {
            assert!(ALL[i + 1..].iter().all(|b| b.id != v.id), "duplicate id {}", v.id);
        }
        assert!(ALL.iter().all(|v| v.id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')));
    }

    #[test]
    fn only_off_is_dry() {
        for v in ALL {
            if v.id == "off" {
                assert_eq!(v.recipe, Recipe::DRY);
            } else {
                assert!(v.recipe != Recipe::DRY, "{}", v.id);
            }
        }
    }

    #[test]
    fn spot_checks() {
        let demon = &ALL[index_of("demon").unwrap()];
        assert_eq!(demon.recipe.pitch, -8.0);
        assert_eq!(demon.recipe.harmony, [-12.0, 0.0]);
        assert_eq!(demon.recipe.harmony_mix, 0.45);
        assert_eq!(demon.recipe.drive, 2.5);
        assert_eq!(demon.recipe.reverb_size, 0.8);
        assert_eq!(demon.recipe.reverb_damp, 0.4);
        assert_eq!(demon.recipe.reverb_mix, 0.25);
        assert_eq!(ALL[index_of("autotune").unwrap()].recipe.tune, 3);
        assert_eq!(ALL[index_of("chord_choir").unwrap()].recipe.tune, 2);
        assert_eq!(ALL[index_of("talkbox").unwrap()].recipe.tune, 1);
        assert_eq!(ALL[index_of("cheap_headset").unwrap()].recipe.gain, 0.45);
        assert_eq!(ALL[index_of("helmet").unwrap()].recipe.gain, 2.4);
    }
}
