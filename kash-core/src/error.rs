//! Unified error type for the kash engine.
//!
//! `KashError` is the crate-level error union threaded through the
//! parser, evaluator, builtins, and any embedder-facing API. The
//! categorisation follows the POSIX exit-code conventions documented
//! in the project memories (`project_shell_set_options.md`,
//! `project_shell_job_control.md`, etc.).

use alloc::string::String;
use core::fmt;

/// Crate-level result alias, with `KashError` as the error type.
pub type Result<T> = core::result::Result<T, KashError>;

/// Unified error type for the kash engine.
///
/// The set of variants is `#[non_exhaustive]` so future categories can
/// be added without a SemVer break. Each variant carries a
/// human-readable message; the `exit_code()` method maps each variant
/// to the exit status the shell process should use when the error
/// reaches the top level.
#[derive(Debug)]
#[non_exhaustive]
pub enum KashError {
    /// Syntax error at parse time. POSIX exit code `2`.
    Parse(String),
    /// Generic runtime evaluation error. POSIX exit code `1`.
    Runtime(String),
    /// Type assertion failure — `[[ x -is T ]]`, `[[ x -satisfies Tc ]]`,
    /// `assert`, mismatched compound member type, etc.
    TypeMismatch {
        /// Expected type or constraint, as a human-readable name.
        expected: String,
        /// Actual type observed, as a human-readable name.
        got: String,
    },
    /// `assert <expr>` failed with the given message.
    AssertionFailed(String),
    /// Variable / function / namespace / builtin not found by lookup.
    /// POSIX exit code `127` (command not found).
    NotFound(String),
    /// Attempt to mutate a read-only binding (`typeset -r`, captured
    /// function parameter, etc.).
    Readonly(String),
    /// `-secure` modifier monotonicity violation — caller tried to
    /// disable a `-secure` lock from inner scope, or used a feature
    /// the modifier forbids (`eval`, `(e)` re-eval, backticks, …).
    SecureViolation(String),
    /// Mode-related error: unknown base mode, illegal modifier
    /// combination, modifier-removal attempt, …
    Mode(String),
    /// Leaky background job(s) detected at shell exit while the
    /// `error-leaky-jobs` option is active. Distinct exit code `3`.
    LeakyJobs(String),
    /// I/O error from the host. Only present under the `std` feature
    /// because the variant carries a `std::io::Error`.
    #[cfg(feature = "std")]
    Io(std::io::Error),
    /// Catch-all for ad-hoc errors that don't have a dedicated
    /// variant yet.
    Other(String),
}

impl KashError {
    /// POSIX-style exit code that this error should map to when it
    /// reaches the top level.
    ///
    /// Mapping (matches the categorisation in
    /// `project_shell_set_options.md` and `project_shell_job_control.md`):
    ///
    /// - `Parse`, `Mode` → `2` (syntax / shell-misuse)
    /// - `NotFound` → `127`
    /// - `LeakyJobs` → `3`
    /// - everything else → `1`
    #[inline]
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Parse(_) | Self::Mode(_) => 2,
            Self::NotFound(_) => 127,
            Self::LeakyJobs(_) => 3,
            Self::Runtime(_)
            | Self::TypeMismatch { .. }
            | Self::AssertionFailed(_)
            | Self::Readonly(_)
            | Self::SecureViolation(_)
            | Self::Other(_) => 1,
            #[cfg(feature = "std")]
            Self::Io(_) => 1,
        }
    }
}

impl fmt::Display for KashError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse(msg) => write!(f, "parse error: {msg}"),
            Self::Runtime(msg) => write!(f, "runtime error: {msg}"),
            Self::TypeMismatch { expected, got } => {
                write!(f, "type mismatch: expected {expected}, got {got}")
            }
            Self::AssertionFailed(msg) => write!(f, "assertion failed: {msg}"),
            Self::NotFound(name) => write!(f, "not found: {name}"),
            Self::Readonly(name) => write!(f, "read-only: {name}"),
            Self::SecureViolation(msg) => write!(f, "-secure violation: {msg}"),
            Self::Mode(msg) => write!(f, "mode error: {msg}"),
            Self::LeakyJobs(msg) => write!(f, "leaky jobs: {msg}"),
            #[cfg(feature = "std")]
            Self::Io(err) => write!(f, "io error: {err}"),
            Self::Other(msg) => f.write_str(msg),
        }
    }
}

// `core::error::Error` is stable since Rust 1.81 and works in `no_std`,
// so we don't need a `std` feature gate on this impl.
impl core::error::Error for KashError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            #[cfg(feature = "std")]
            Self::Io(err) => Some(err),
            _ => None,
        }
    }
}

#[cfg(feature = "std")]
impl From<std::io::Error> for KashError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;

    #[test]
    fn exit_code_categorisation() {
        assert_eq!(KashError::Parse("x".into()).exit_code(), 2);
        assert_eq!(KashError::Mode("x".into()).exit_code(), 2);
        assert_eq!(KashError::NotFound("foo".into()).exit_code(), 127);
        assert_eq!(KashError::LeakyJobs("x".into()).exit_code(), 3);
        assert_eq!(KashError::Runtime("x".into()).exit_code(), 1);
        assert_eq!(
            KashError::TypeMismatch {
                expected: "Int".into(),
                got: "String".into(),
            }
            .exit_code(),
            1,
        );
    }

    #[test]
    fn display_messages_include_context() {
        let err = KashError::TypeMismatch {
            expected: "Int".into(),
            got: "String".into(),
        };
        let rendered = err.to_string();
        assert!(rendered.contains("Int"));
        assert!(rendered.contains("String"));
    }
}
