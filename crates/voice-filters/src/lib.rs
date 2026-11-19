//! Goby's voice processing, in a crate of its own so it can keep opt-level 3 while the
//! rest of the program is built for size. Plain Rust, no dependencies; the website's wasm
//! build includes these files directly.

pub mod dsp;
pub mod formant;
pub mod fx;
pub mod mic;
pub mod recipes;
pub mod ring;
pub mod stream;
