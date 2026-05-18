//! Unified error type for the kash engine.
//!
//! Defines `KashError` and the crate-level `Result<T>` alias used by the
//! parser, evaluator, builtins, and embedder-facing API. Implementation
//! lands in a follow-up commit (see project memory:
//! `project_shell_set_options.md` for the exit-code categorisation).
