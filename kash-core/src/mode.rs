//! Mode system runtime types.
//!
//! Owns the `Mode` / `BaseMode` / `Modifier` types, mode-name parsing
//! (`"default-secure"`, `"posix-strict"`, …), modifier monotonicity
//! checks, and the `.kash.mode` introspection state. Mirrors the design
//! locked in `project_shell_modes.md` / `project_shell_mode_syntax.md`.
