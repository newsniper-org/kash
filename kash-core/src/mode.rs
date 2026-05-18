//! Mode system runtime types.
//!
//! `Mode` is the `<base>[-<modifier>]*` value that controls which kash
//! features are available and which corner-case semantics fire. The
//! lexical-scope rules (a function's mode is captured at definition
//! time), the modifier-monotonicity guard, and the `.kash.mode`
//! introspection state all build on this module. The design itself is
//! frozen in `project_shell_modes.md` /
//! `project_shell_mode_syntax.md`.

use alloc::collections::BTreeSet;
use alloc::string::{String, ToString};
use core::fmt;

use crate::error::KashError;

/// One of the five base mode buckets.
///
/// The variants are listed in increasing "extension permissiveness"
/// roughly so `Ord` matches that intuition, but downstream code should
/// not rely on the numeric ordering — match on the variant.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum BaseMode {
    /// `posix-strict` — POSIX features only. Extensions parse-rejected.
    PosixStrict,
    /// `posix-aware` — extensions allowed, corner-case semantics follow POSIX.
    PosixAware,
    /// `ksh93u-strict` — ksh93u+m feature set only, kash extensions off.
    Ksh93uStrict,
    /// `ksh93u-aware` — full feature set, ksh93 corner-case semantics.
    Ksh93uAware,
    /// `default` — full feature set, new-shell footgun-eliminating defaults.
    Default,
}

impl BaseMode {
    /// Canonical lowercase name (`"default"`, `"posix-strict"`, …).
    #[inline]
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PosixStrict => "posix-strict",
            Self::PosixAware => "posix-aware",
            Self::Ksh93uStrict => "ksh93u-strict",
            Self::Ksh93uAware => "ksh93u-aware",
            Self::Default => "default",
        }
    }

    /// Parse the canonical lowercase name. Returns `None` for anything else.
    #[must_use]
    pub fn parse_token(s: &str) -> Option<Self> {
        match s {
            "posix-strict" => Some(Self::PosixStrict),
            "posix-aware" => Some(Self::PosixAware),
            "ksh93u-strict" => Some(Self::Ksh93uStrict),
            "ksh93u-aware" => Some(Self::Ksh93uAware),
            "default" => Some(Self::Default),
            _ => None,
        }
    }
}

impl fmt::Display for BaseMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Mode modifier suffix. Currently only `-secure`, but the variant set
/// is `#[non_exhaustive]` so future modifiers (`-noglob`,
/// `-noeval`, `-no-network`, …) can be added without a SemVer break.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum Modifier {
    /// `-secure` — footgun-elimination profile. Forces `errexit`,
    /// `pipefail`, `nounset`, `noclobber`, `error-leaky-jobs`, and the
    /// `warn-*` family on; bans `eval`, backticks, `(e)` re-eval, and
    /// the `null glob → fail` lock.
    Secure,
}

impl Modifier {
    /// Canonical name without the leading `-` (e.g. `"secure"`).
    #[inline]
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Secure => "secure",
        }
    }

    /// Parse the canonical name. `s` should not include the leading `-`.
    #[must_use]
    pub fn parse_token(s: &str) -> Option<Self> {
        match s {
            "secure" => Some(Self::Secure),
            _ => None,
        }
    }
}

impl fmt::Display for Modifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Fully-resolved mode value: a base bucket plus a set of modifiers.
///
/// `Mode`s compare by both `base` and `modifiers`. The modifier set is
/// a `BTreeSet` so iteration / `Display` are deterministic and the
/// monotonicity subset check is straightforward.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mode {
    /// Base mode bucket.
    pub base: BaseMode,
    /// Active modifiers, ordered by `Modifier`'s `Ord` impl.
    pub modifiers: BTreeSet<Modifier>,
}

impl Mode {
    /// Mode with the given base and no modifiers.
    #[inline]
    #[must_use]
    pub const fn new(base: BaseMode) -> Self {
        Self {
            base,
            modifiers: BTreeSet::new(),
        }
    }

    /// `default` — the implicit mode at script top when no `mode`
    /// declaration, shebang flag, or symlink invocation overrides it.
    #[inline]
    #[must_use]
    pub const fn default_mode() -> Self {
        Self::new(BaseMode::Default)
    }

    /// True if `self` has every modifier of `outer`. Used by the
    /// modifier-monotonicity guard: an inner `mode` declaration can
    /// only *add* modifiers, never drop them.
    #[inline]
    #[must_use]
    pub fn modifiers_satisfy(&self, outer: &Self) -> bool {
        outer.modifiers.is_subset(&self.modifiers)
    }

    /// Parse a mode name like `"default"`, `"default-secure"`,
    /// `"posix-strict"`, `"ksh93u-aware-secure"`, …
    ///
    /// Returns `KashError::Mode(...)` for unknown bases, unknown
    /// modifiers, duplicate modifiers, and "modifier with no base"
    /// inputs.
    pub fn parse(s: &str) -> Result<Self, KashError> {
        // Try each known base name as a longest-match prefix.
        const BASES: &[(&str, BaseMode)] = &[
            // Listed longest-first so e.g. `ksh93u-strict` is tested
            // before any hypothetical future `ksh93u` base would be.
            ("ksh93u-strict", BaseMode::Ksh93uStrict),
            ("ksh93u-aware", BaseMode::Ksh93uAware),
            ("posix-strict", BaseMode::PosixStrict),
            ("posix-aware", BaseMode::PosixAware),
            ("default", BaseMode::Default),
        ];

        for (name, base) in BASES {
            if s == *name {
                return Ok(Self::new(*base));
            }
            if let Some(rest) = s.strip_prefix(name).and_then(|r| r.strip_prefix('-')) {
                if rest.is_empty() {
                    return Err(KashError::Mode(alloc::format!(
                        "mode name `{s}` ends with a dangling `-`"
                    )));
                }
                let mut modifiers = BTreeSet::new();
                for segment in rest.split('-') {
                    if segment.is_empty() {
                        return Err(KashError::Mode(alloc::format!(
                            "mode name `{s}` has an empty modifier segment"
                        )));
                    }
                    let m = Modifier::parse_token(segment).ok_or_else(|| {
                        KashError::Mode(alloc::format!("unknown modifier `-{segment}` in mode `{s}`"))
                    })?;
                    if !modifiers.insert(m) {
                        return Err(KashError::Mode(alloc::format!(
                            "modifier `-{segment}` appears more than once in mode `{s}`"
                        )));
                    }
                }
                return Ok(Self {
                    base: *base,
                    modifiers,
                });
            }
        }

        // Special-case the "no base" form (`"-secure"`, `""`) for a
        // friendlier error message.
        if s.starts_with('-') || s.is_empty() {
            return Err(KashError::Mode(alloc::format!(
                "mode `{s}` has no base (one of: default, posix-strict, posix-aware, \
                 ksh93u-strict, ksh93u-aware)"
            )));
        }
        Err(KashError::Mode(alloc::format!("unknown mode `{s}`")))
    }
}

impl Default for Mode {
    fn default() -> Self {
        Self::default_mode()
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.base.as_str())?;
        for m in &self.modifiers {
            f.write_str("-")?;
            f.write_str(m.as_str())?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::format;

    fn mode(base: BaseMode, mods: &[Modifier]) -> Mode {
        let mut m = Mode::new(base);
        for &x in mods {
            m.modifiers.insert(x);
        }
        m
    }

    #[test]
    fn parse_bare_base() {
        assert_eq!(Mode::parse("default").unwrap(), Mode::new(BaseMode::Default));
        assert_eq!(
            Mode::parse("posix-strict").unwrap(),
            Mode::new(BaseMode::PosixStrict),
        );
        assert_eq!(
            Mode::parse("ksh93u-aware").unwrap(),
            Mode::new(BaseMode::Ksh93uAware),
        );
    }

    #[test]
    fn parse_with_modifier() {
        assert_eq!(
            Mode::parse("default-secure").unwrap(),
            mode(BaseMode::Default, &[Modifier::Secure]),
        );
        assert_eq!(
            Mode::parse("posix-strict-secure").unwrap(),
            mode(BaseMode::PosixStrict, &[Modifier::Secure]),
        );
        assert_eq!(
            Mode::parse("ksh93u-aware-secure").unwrap(),
            mode(BaseMode::Ksh93uAware, &[Modifier::Secure]),
        );
    }

    #[test]
    fn parse_rejects_unknown_base() {
        let err = Mode::parse("bogus").unwrap_err();
        let rendered = format!("{err}");
        assert!(rendered.contains("bogus"), "got: {rendered}");
    }

    #[test]
    fn parse_rejects_unknown_modifier() {
        let err = Mode::parse("default-zzz").unwrap_err();
        let rendered = format!("{err}");
        assert!(rendered.contains("zzz"), "got: {rendered}");
    }

    #[test]
    fn parse_rejects_duplicate_modifier() {
        let err = Mode::parse("default-secure-secure").unwrap_err();
        let rendered = format!("{err}");
        assert!(rendered.contains("more than once"), "got: {rendered}");
    }

    #[test]
    fn parse_rejects_modifier_without_base() {
        Mode::parse("-secure").unwrap_err();
        Mode::parse("").unwrap_err();
    }

    #[test]
    fn display_round_trips() {
        for src in ["default", "posix-strict", "default-secure", "ksh93u-aware-secure"] {
            let parsed = Mode::parse(src).unwrap();
            assert_eq!(format!("{parsed}"), src);
        }
    }

    #[test]
    fn monotonicity_subset() {
        let outer = Mode::new(BaseMode::Default);
        let inner_secure = mode(BaseMode::Default, &[Modifier::Secure]);
        // Inner with more modifiers satisfies outer.
        assert!(inner_secure.modifiers_satisfy(&outer));
        // Outer with more modifiers is NOT satisfied by inner with fewer.
        assert!(!outer.modifiers_satisfy(&inner_secure));
        // Same modifier set: satisfies trivially.
        assert!(outer.modifiers_satisfy(&outer));
        // Base changes don't affect monotonicity — only modifiers do.
        let inner_other_base_same_mods =
            mode(BaseMode::PosixAware, &[Modifier::Secure]);
        assert!(inner_other_base_same_mods.modifiers_satisfy(&inner_secure));
    }
}
