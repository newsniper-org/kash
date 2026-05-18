//! Re-export of the engine's storage abstraction.
//!
//! The traits and the default backend live in the [`kash-gadt`] crate
//! so they can be reused by sibling crates (line editor, transpiler,
//! first-party utilities) without pulling in the rest of `kash-core`.
//! This module is a thin re-export so historical paths inside
//! `kash-core` (`crate::collections::MapBackend`, etc.) keep working.
//!
//! See [the `kash-gadt` crate-level docs][kash-gadt] for the design
//! rationale and a survey of plausible alternative backends
//! (hashbrown, indexmap, litemap, phf, cphf).
//!
//! [`kash-gadt`]: kash_gadt
//! [kash-gadt]: kash_gadt

pub use kash_gadt::{BTreeBackend, MapBackend, MapStorage, SetStorage};
