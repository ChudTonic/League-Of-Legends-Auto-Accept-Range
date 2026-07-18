//! Skins feature logic (chroma/forms special cases, historic mode, random
//! selection). S1 ships only the `special` forms/HOL table; the sibling
//! modules (`chroma`, `historic`, `random`) land in S4.

#![allow(dead_code)] // consumed by S4+

pub mod chroma;
pub mod historic;
pub mod loadscreen_label;
pub mod random;
pub mod special;
