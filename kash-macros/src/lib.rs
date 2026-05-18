//! Shared declarative macros for the kash shell project.
//!
//! This crate hosts `macro_rules!` macros used across the kash workspace.
//! Procedural macros (when needed) will live in a separate `kash-proc-macros`
//! crate so this one stays free of the `proc-macro` build dependency.

#![no_std]
#![warn(missing_docs)]

/// Split a chunk of items into a `std` branch and a non-`std` branch.
///
/// Each item in the `std` branch is annotated with
/// `#[cfg(feature = "std")]`; each item in the `else` branch is annotated
/// with `#[cfg(not(feature = "std"))]`. The `else` branch is optional.
///
/// The `feature = "std"` predicate is evaluated in the *invoking* crate's
/// context, so any crate using `ifstd!` must declare its own `std` feature.
///
/// # Invocation syntax
///
/// Rust's macro grammar doesn't allow a free-standing `else` after a
/// `name!{...}` invocation (the closing `}` ends the macro call), so the
/// invocation itself is paren-delimited and the two branches live inside:
///
/// ```text
/// ifstd!({
///     /* std items */
/// } else {
///     /* non-std items */
/// });
/// ```
///
/// # Limitations
///
/// The macro uses the `:item` matcher, so each branch must contain a
/// sequence of complete Rust items (`use`, `fn`, `struct`, `impl`, …).
/// Statement-level or expression-level code is not accepted — for those
/// cases use `#[cfg(feature = "std")]` directly.
///
/// # Examples
///
/// Two-branch form:
///
/// ```
/// # use kash_macros::ifstd;
/// ifstd!({
///     pub fn platform_name() -> &'static str { "std" }
/// } else {
///     pub fn platform_name() -> &'static str { "no_std" }
/// });
/// # assert!(!platform_name().is_empty());
/// ```
///
/// Single-branch form (no `else`) when only `std`-specific items are
/// needed:
///
/// ```
/// # use kash_macros::ifstd;
/// ifstd!({
///     pub fn pid() -> u32 { std::process::id() }
/// });
/// # // Only emitted in std builds; under `--no-default-features` the body
/// # // is dropped and `pid` simply doesn't exist.
/// ```
#[macro_export]
macro_rules! ifstd {
    (
        { $($if_std:item)* }
        $( else { $($if_not_std:item)* } )?
    ) => {
        $( #[cfg(feature = "std")] $if_std )*
        $( $( #[cfg(not(feature = "std"))] $if_not_std )* )?
    };
}

/// Same as [`ifstd!`] but keyed on the `alloc` feature instead of `std`.
///
/// Useful when an item should be available in alloc-only builds but not
/// in the bare-`no_std` (no heap) build.
///
/// # Examples
///
/// ```
/// # extern crate alloc;
/// # use kash_macros::ifalloc;
/// ifalloc!({
///     pub fn empty_owned() -> alloc::vec::Vec<u8> { alloc::vec::Vec::new() }
/// } else {
///     pub fn empty_owned() -> &'static [u8] { &[] }
/// });
/// # assert!(empty_owned().is_empty());
/// ```
#[macro_export]
macro_rules! ifalloc {
    (
        { $($if_alloc:item)* }
        $( else { $($if_not_alloc:item)* } )?
    ) => {
        $( #[cfg(feature = "alloc")] $if_alloc )*
        $( $( #[cfg(not(feature = "alloc"))] $if_not_alloc )* )?
    };
}
