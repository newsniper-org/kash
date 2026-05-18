//! Core engine of the kash shell.
//!
//! This crate is the entry point for embedders that want the kash language
//! and runtime without the line-editor or any of the first-party utility
//! commands. It owns the parser, the evaluator, the mode system, the
//! built-in command set, the variable / namespace / typeclass machinery,
//! and the error type.
//!
//! Feature flags:
//!
//! - `alloc` *(default)* — heap-allocated containers without the rest of
//!   `std`. Lets the language core (parser, AST, evaluator on in-memory
//!   values, mode system, typeclass dispatch) run in `no_std` targets
//!   that have a heap allocator.
//! - `std` — pulls in the full standard library. Required for anything
//!   that touches the filesystem, environment, threads, signals, or
//!   terminal I/O. Implies `alloc`.
//!
//! `--no-default-features` (neither feature) is rejected by a
//! `compile_error!` — the engine fundamentally needs a heap.

#![cfg_attr(not(feature = "std"), no_std)]
#![warn(missing_docs)]
// Several engine modules are placeholders in this commit. Silence the
// resulting dead-code/unused-imports lints crate-wide until each module
// gains real content; the lints come back as content lands.
#![allow(dead_code, unused_imports)]

#[cfg(not(feature = "alloc"))]
compile_error!(
    "kash-core requires at least the `alloc` feature. Use \
     `--no-default-features --features alloc` for no_std builds."
);

extern crate alloc;

use kash_macros::ifstd;

pub mod ast;
pub mod error;
pub mod eval;
pub mod lexer;
pub mod mode;
pub mod parser;
pub mod scope;
pub mod value;

/// Semantic version of this crate, as in `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

// `process_id` only makes sense in `std` builds (no_std targets have no
// concept of an OS process ID). Use the single-branch form of `ifstd!` —
// the function simply doesn't exist under `--no-default-features --features alloc`.
ifstd!({
    /// Return the running process ID. Only available with the `std` feature.
    #[must_use]
    pub fn process_id() -> u32 {
        std::process::id()
    }
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_matches_cargo() {
        assert_eq!(VERSION, env!("CARGO_PKG_VERSION"));
    }

    #[cfg(feature = "std")]
    #[test]
    fn process_id_is_nonzero() {
        assert!(process_id() > 0);
    }
}
