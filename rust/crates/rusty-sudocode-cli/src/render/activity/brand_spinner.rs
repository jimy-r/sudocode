//! Four-cell, one-row approximation of the Sudoprivacy hexagonal mark.
//! Keep the outline and its negative space stable at this resolution; rotate
//! the inner facets instead of resampling the whole logo into a solid blob.
//! Frames are precomputed. No image/font work runs on render ticks.

const FRAMES: [&str; 4] = ["⠰⣋⣩⠆", "⠰⣙⣍⠆", "⠰⣩⣋⠆", "⠰⣍⣙⠆"];

// Three existing 80 ms ticks per pose retain the original 960 ms cycle.
const TICKS_PER_FRAME: usize = 3;

#[inline]
pub(super) fn frame(index: usize) -> &'static str {
    FRAMES[(index / TICKS_PER_FRAME) % FRAMES.len()]
}
