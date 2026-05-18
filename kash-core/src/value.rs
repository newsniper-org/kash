//! Runtime value representation.
//!
//! `Value` is the kash runtime's union type: scalars (string + the
//! primitive numeric set per `project_shell_arithmetic.md`), indexed
//! arrays, associative arrays, compound variables, namerefs, and
//! user-defined-type instances. Includes the `${(t)var}` type-introspection
//! tag and the typeclass-dispatch hooks.
//!
//! Scope of this commit: scalar, indexed array (`-a`), associative
//! array (`-A`). Compound, nameref (`-n`), and user-defined types
//! (`-T`) still exist only as enum-variant placeholders / future work.

use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// Runtime value held by a variable binding.
///
/// The variant set is `#[non_exhaustive]` so we can add compound /
/// nameref / typed-numeric shapes without a SemVer break.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum Value {
    /// Unset (the variable name has no binding) or the explicit empty
    /// scalar — the two are indistinguishable here; lookups return
    /// `None` for unset, `Some(Empty)` for explicitly empty.
    #[default]
    Empty,
    /// A scalar string.
    Scalar(String),
    /// Indexed array — a flat list of element strings.
    Array(Vec<String>),
    /// Associative array — string key → string value.
    AssocArray(BTreeMap<String, String>),
}

impl Value {
    /// Construct a scalar from anything `String`-convertible.
    #[inline]
    #[must_use]
    pub fn scalar<S: Into<String>>(s: S) -> Self {
        Self::Scalar(s.into())
    }

    /// Render the value to a single string for `"$var"`-style scalar
    /// contexts. Arrays render their element at index `0` (POSIX);
    /// associative arrays render their first inserted value (the
    /// only key without a numeric index that ksh93 / bash agree on).
    #[inline]
    #[must_use]
    pub fn to_scalar_string(&self) -> String {
        match self {
            Self::Empty => String::new(),
            Self::Scalar(s) => s.clone(),
            Self::Array(a) => a.first().cloned().unwrap_or_default(),
            Self::AssocArray(m) => m.values().next().cloned().unwrap_or_default(),
        }
    }

    /// `true` if the value is considered empty / unset for the
    /// purposes of `${var:-}` and similar defaulting forms.
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        match self {
            Self::Empty => true,
            Self::Scalar(s) => s.is_empty(),
            Self::Array(a) => a.is_empty(),
            Self::AssocArray(m) => m.is_empty(),
        }
    }
}

impl From<String> for Value {
    #[inline]
    fn from(s: String) -> Self {
        Self::Scalar(s)
    }
}

impl From<&str> for Value {
    #[inline]
    fn from(s: &str) -> Self {
        Self::Scalar(s.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalar_roundtrip() {
        let v = Value::scalar("hi");
        assert_eq!(v.to_scalar_string(), "hi");
        assert!(!v.is_empty());
    }

    #[test]
    fn empty_default() {
        let v = Value::default();
        assert_eq!(v.to_scalar_string(), "");
        assert!(v.is_empty());
    }

    #[test]
    fn array_scalar_context_picks_first() {
        let v = Value::Array(alloc::vec!["a".into(), "b".into()]);
        assert_eq!(v.to_scalar_string(), "a");
    }

    #[test]
    fn assoc_array_scalar_context_picks_first_inserted() {
        let mut m = BTreeMap::new();
        m.insert("a".into(), "alpha".into());
        m.insert("b".into(), "beta".into());
        let v = Value::AssocArray(m);
        // BTreeMap iterates by key order, so "a" wins.
        assert_eq!(v.to_scalar_string(), "alpha");
    }
}
