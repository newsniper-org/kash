//! Tokeniser for the kash source language.
//!
//! Turns a `&str` source buffer into a stream of `Token`s. The lexer is
//! hand-written and `no_std + alloc` friendly (see
//! `project_kash_implementation.md`). Token kinds cover POSIX shell
//! grammar plus kash extensions (mode declarations, capture lists,
//! expansion flags, glob qualifiers, etc.).
