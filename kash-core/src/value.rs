//! Runtime values.
//!
//! `Value` is the kash runtime's union type: scalars (string + the
//! primitive numeric set per `project_shell_arithmetic.md`), indexed
//! arrays, associative arrays, compound variables, namerefs, and
//! user-defined-type instances. Includes the `${(t)var}` type-introspection
//! tag and the typeclass-dispatch hooks.
