//! Abstract syntax tree node definitions.
//!
//! Models commands, pipelines, redirections, quotes, expansion flags,
//! mode declarations, function definitions (POSIX + capture-list form),
//! typeclass / instance / namespace declarations, and the various
//! literal forms (strings, here-docs, numeric primitives, complex
//! literals). Designed to be allocation-friendly (Vec/Box) but
//! `no_std + alloc` compatible.
