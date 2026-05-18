//! GAT-based map / set storage abstraction for the kash shell engine.
//!
//! Every owned map or set inside `kash-core` goes through one of two
//! layers defined here:
//!
//! 1. The trait pair [`MapStorage<K, V>`] and [`SetStorage<T>`]
//!    abstracts the operations call sites actually use (`get`,
//!    `get_mut`, `contains_key` / `contains`, `insert`, `remove`,
//!    `clear`, `iter`, `len`, `is_empty`, plus `is_subset` for the
//!    mode-modifier monotonicity check). Method signatures use
//!    `Borrow<Q>`-shaped key parameters so a `K = String` map can be
//!    queried with `&str` without an intermediate `String::from`,
//!    matching the native `BTreeMap` / `HashMap` ergonomics.
//! 2. The [`MapBackend`] trait is a "family" of containers — its
//!    `Map<K, V>` and `Set<T>` GATs let a single type parameter
//!    select both at once. Downstream `kash-core` types
//!    (`Mode<B>`, `Frame<B>`, `Scope<B>`, `Evaluator<B>`) take one
//!    `B: MapBackend` parameter and use `B::Map<…>` / `B::Set<…>`
//!    wherever they hold owned storage.
//!
//! `Clone` is a supertrait on both `MapStorage` and `SetStorage`
//! because the subshell-style environment snapshots in the evaluator
//! clone the whole table. That keeps the per-call-site trait bound
//! list short; concrete backends are expected to be `Clone` whenever
//! their key / value / element types are.
//!
//! # Default backend: [`BTreeBackend`]
//!
//! Wires both slots to `alloc::collections::BTreeMap` / `BTreeSet`.
//! Picked because:
//!
//! - they live in `alloc` (no `std`-only hasher dependency), so the
//!   default `alloc`-only build can use them,
//! - they iterate in `Ord` order, which keeps `trap` / `alias` /
//!   function-table listings deterministic without a separate sort
//!   pass,
//! - at the small `n` a shell deals with (a few hundred bindings at
//!   most) the `O(log n)` lookups are indistinguishable from the
//!   `O(1)` of a hash table, and the cache layout is friendlier.
//!
//! # Survey of alternative backends
//!
//! Trade-offs against the BTree default. None of these are *wired
//! up* — implementing one is a `impl MapBackend for FooBackend`
//! plus matching `impl MapStorage` / `impl SetStorage` blocks, plus
//! a Cargo feature flag if the backend needs an external dep.
//!
//! - **`hashbrown`** — the implementation crate that backs the
//!   standard library's `HashMap` (Swiss-table design). `no_std +
//!   alloc` friendly out of the box; no external hasher required
//!   (uses `foldhash`). Best raw lookup speed; lookups are
//!   amortised `O(1)`. Iteration order is unspecified, so call sites
//!   that emit listings (`trap`, `alias`) would need to sort first.
//!   Good fit for the variable-scope hot path; less good for the
//!   tables we read back to the user.
//!
//! - **`indexmap`** — `IndexMap<K, V>` is a `HashMap` + `Vec<K>`
//!   that preserves insertion order. `no_std + alloc` friendly,
//!   built on top of `hashbrown`. Same `O(1)` lookup as a plain
//!   hash table, plus deterministic *definition*-order iteration —
//!   that's actually closer to what `bash` / `ksh93` do for `alias`
//!   listings than BTreeMap's alphabetical order. Strong candidate
//!   for `alias` / `functions` / `traps`.
//!
//! - **`litemap`** — ICU4X's small-map helper. Internally a sorted
//!   `Vec<(K, V)>` so it costs one allocation total, lookup is
//!   binary search, and iteration is sorted. Memory-optimal for
//!   "the engine only ever has a few dozen entries" cases. Could
//!   make sense for `Mode::modifiers` (a set of at most ~5
//!   elements) or per-frame variable scope in shell scripts that
//!   barely set any locals; less attractive once an entry count
//!   pushes past a hundred or two because every insertion in the
//!   middle is `O(n)`.
//!
//! - **`phf`** — perfect-hash-function map / set built at compile
//!   time by a procedural macro. Static — *no* `insert` / `remove`.
//!   Lookups are a few CPU cycles. Cannot live inside the
//!   `MapBackend` trait (which expects a `Default` constructor and
//!   mutation), but **is a great fit for the static lookup tables
//!   the engine carries**: `reserved_word()`, `is_builtin_name()`,
//!   `normalize_signal()`, the POSIX character-class name table in
//!   the glob matcher, the redirect-operator dispatch. Those are
//!   currently handled by `matches!` chains, which the compiler
//!   already turns into jump tables for ≤ 16 entries; `phf` becomes
//!   worth it once any of those grows past that.
//!
//! - **`cphf`** — fully `const`-evaluable PHF. Same use case as
//!   `phf` (read-only static tables) but skips the proc-macro build
//!   step. Useful where build-time hygiene matters; the runtime
//!   trade-off vs `phf` is negligible.
//!
//! # Backend-shape summary
//!
//! | Crate / form | mutable | no_std | listing order | lookup |
//! |---|:-:|:-:|---|---|
//! | `BTreeMap` (current default) | ✅ | ✅ | sorted | O(log n), cache-friendly |
//! | `hashbrown` | ✅ | ✅ | undefined | O(1), fastest |
//! | `indexmap` | ✅ | ✅ | insertion order | O(1) |
//! | `litemap` | ✅ | ✅ | sorted | O(log n), one alloc total |
//! | `phf` / `cphf` | ✗ | ✅ | declaration order | O(1), const tables |
//!
//! # Plausible target picture
//!
//! - `Mode::modifiers` — `litemap` or stay on BTreeSet (≤ 5
//!   entries; sort order is fine).
//! - `Scope::Frame::bindings` — `hashbrown` (lookup-heavy hot path,
//!   listing not needed at this layer).
//! - `Evaluator::aliases` / `functions` / `traps` — `indexmap`
//!   (listing in definition order matches bash / ksh93).
//! - Reserved-word / builtin / signal-name tables (currently
//!   `matches!` arms in `kash-core`) — `phf` or `cphf` once they
//!   grow past the threshold where the compiler still produces
//!   linear-search jump tables.

#![cfg_attr(not(feature = "std"), no_std)]
#![warn(missing_docs)]

#[cfg(not(feature = "alloc"))]
compile_error!(
    "kash-gadt requires at least the `alloc` feature. Use \
     `--no-default-features --features alloc` for no_std builds."
);

extern crate alloc;

use alloc::collections::{btree_map, btree_set, BTreeMap, BTreeSet};

/// Map-of-`K`-to-`V` storage abstraction. The set of methods is the
/// intersection of what every call site in the kash engine uses.
///
/// `Clone` is a supertrait so subshell-style environment snapshots
/// can clone the whole table without per-call-site trait bounds.
pub trait MapStorage<K, V>: Default + Clone {
    /// Lifetime-bound iterator type for the `iter` method. A GAT so
    /// implementations can return their own concrete iterator
    /// without an extra boxed-allocation layer.
    type Iter<'a>: Iterator<Item = (&'a K, &'a V)>
    where
        Self: 'a,
        K: 'a,
        V: 'a;

    /// Lookup. Returns the binding for `key`, if any. The borrow-
    /// shaped signature mirrors `BTreeMap` / `HashMap`'s native one
    /// so callers can pass `&str` for a `K = String` map without an
    /// extra `String::from` round-trip.
    fn get<Q>(&self, key: &Q) -> Option<&V>
    where
        K: core::borrow::Borrow<Q>,
        Q: Ord + core::hash::Hash + Eq + ?Sized;

    /// Mutable lookup, same `Borrow<Q>` shape as `get`.
    fn get_mut<Q>(&mut self, key: &Q) -> Option<&mut V>
    where
        K: core::borrow::Borrow<Q>,
        Q: Ord + core::hash::Hash + Eq + ?Sized;

    /// `true` iff `key` is bound.
    fn contains_key<Q>(&self, key: &Q) -> bool
    where
        K: core::borrow::Borrow<Q>,
        Q: Ord + core::hash::Hash + Eq + ?Sized;

    /// Insert / overwrite. Returns the old value if there was one.
    fn insert(&mut self, key: K, value: V) -> Option<V>;

    /// Remove the binding for `key`. Returns the old value if any.
    fn remove<Q>(&mut self, key: &Q) -> Option<V>
    where
        K: core::borrow::Borrow<Q>,
        Q: Ord + core::hash::Hash + Eq + ?Sized;

    /// Drop every binding.
    fn clear(&mut self);

    /// Number of bindings.
    fn len(&self) -> usize;

    /// `true` iff there are no bindings.
    fn is_empty(&self) -> bool;

    /// Iterate `(key, value)` pairs in whatever order the backend
    /// gives (sorted for `BTreeMap`; insertion-order for `IndexMap`;
    /// arbitrary for hash backends).
    fn iter(&self) -> Self::Iter<'_>;
}

/// Set-of-`T` storage abstraction. Mirrors [`MapStorage`] but for
/// sets, plus `is_subset` (needed by the mode-modifier monotonicity
/// guard). `Clone` supertrait for the same reason.
pub trait SetStorage<T>: Default + Clone {
    /// Lifetime-bound iterator type, for the same reasons as
    /// [`MapStorage::Iter`].
    type Iter<'a>: Iterator<Item = &'a T>
    where
        Self: 'a,
        T: 'a;

    /// `true` iff `value` is a member.
    fn contains<Q>(&self, value: &Q) -> bool
    where
        T: core::borrow::Borrow<Q>,
        Q: Ord + core::hash::Hash + Eq + ?Sized;

    /// Insert `value`. Returns `true` if `value` wasn't already a
    /// member.
    fn insert(&mut self, value: T) -> bool;

    /// Remove `value`. Returns `true` if it was a member.
    fn remove<Q>(&mut self, value: &Q) -> bool
    where
        T: core::borrow::Borrow<Q>,
        Q: Ord + core::hash::Hash + Eq + ?Sized;

    /// Drop every member.
    fn clear(&mut self);

    /// Number of members.
    fn len(&self) -> usize;

    /// `true` iff the set is empty.
    fn is_empty(&self) -> bool;

    /// Iterate members.
    fn iter(&self) -> Self::Iter<'_>;

    /// `true` iff every member of `self` is also a member of `other`.
    /// Used by the mode-modifier monotonicity check.
    fn is_subset(&self, other: &Self) -> bool;
}

/// A "family" of map / set storage types. The associated GATs pick
/// both containers at once given a single type parameter.
///
/// `Clone` bounds on the key / value / element types flow through to
/// the storage `MapStorage` / `SetStorage` supertrait so subshell
/// snapshots (which clone the whole evaluator state) compile without
/// per-call-site trait bounds.
pub trait MapBackend {
    /// Concrete map type used by this backend for owned bindings.
    ///
    /// The `Hash + Eq` bounds are present on top of `Ord + Clone`
    /// so a hash-table backend (`LightnhtBackend`, future
    /// `HashBrownBackend`, …) can implement [`MapStorage`] without
    /// the engine having to spell two separate trait families. Keys
    /// commonly used through the engine (`String`, `&str`, integer
    /// types) all satisfy the union without effort.
    type Map<K, V>: MapStorage<K, V>
    where
        K: Ord + Clone + core::hash::Hash + Eq,
        V: Clone;

    /// Concrete set type used by this backend.
    type Set<T>: SetStorage<T>
    where
        T: Ord + Clone + core::hash::Hash + Eq;
}

/// Default backend: `alloc::collections::BTreeMap` /
/// `alloc::collections::BTreeSet`. Zero-sized marker type — picking
/// the backend only changes which storage types are slotted in, not
/// the runtime value flow.
#[derive(Clone, Copy, Debug, Default)]
pub struct BTreeBackend;

impl MapBackend for BTreeBackend {
    type Map<K, V>
        = BTreeMap<K, V>
    where
        K: Ord + Clone + core::hash::Hash + Eq,
        V: Clone;
    type Set<T>
        = BTreeSet<T>
    where
        T: Ord + Clone + core::hash::Hash + Eq;
}

// ===== BTreeMap / BTreeSet trait impls =====

impl<K: Ord + Clone, V: Clone> MapStorage<K, V> for BTreeMap<K, V> {
    type Iter<'a>
        = btree_map::Iter<'a, K, V>
    where
        K: 'a,
        V: 'a;

    #[inline]
    fn get<Q>(&self, key: &Q) -> Option<&V>
    where
        K: core::borrow::Borrow<Q>,
        Q: Ord + core::hash::Hash + Eq + ?Sized,
    {
        BTreeMap::get(self, key)
    }

    #[inline]
    fn get_mut<Q>(&mut self, key: &Q) -> Option<&mut V>
    where
        K: core::borrow::Borrow<Q>,
        Q: Ord + core::hash::Hash + Eq + ?Sized,
    {
        BTreeMap::get_mut(self, key)
    }

    #[inline]
    fn contains_key<Q>(&self, key: &Q) -> bool
    where
        K: core::borrow::Borrow<Q>,
        Q: Ord + core::hash::Hash + Eq + ?Sized,
    {
        BTreeMap::contains_key(self, key)
    }

    #[inline]
    fn insert(&mut self, key: K, value: V) -> Option<V> {
        BTreeMap::insert(self, key, value)
    }

    #[inline]
    fn remove<Q>(&mut self, key: &Q) -> Option<V>
    where
        K: core::borrow::Borrow<Q>,
        Q: Ord + core::hash::Hash + Eq + ?Sized,
    {
        BTreeMap::remove(self, key)
    }

    #[inline]
    fn clear(&mut self) {
        BTreeMap::clear(self);
    }

    #[inline]
    fn len(&self) -> usize {
        BTreeMap::len(self)
    }

    #[inline]
    fn is_empty(&self) -> bool {
        BTreeMap::is_empty(self)
    }

    #[inline]
    fn iter(&self) -> Self::Iter<'_> {
        BTreeMap::iter(self)
    }
}

impl<T: Ord + Clone> SetStorage<T> for BTreeSet<T> {
    type Iter<'a>
        = btree_set::Iter<'a, T>
    where
        T: 'a;

    #[inline]
    fn contains<Q>(&self, value: &Q) -> bool
    where
        T: core::borrow::Borrow<Q>,
        Q: Ord + core::hash::Hash + Eq + ?Sized,
    {
        BTreeSet::contains(self, value)
    }

    #[inline]
    fn insert(&mut self, value: T) -> bool {
        BTreeSet::insert(self, value)
    }

    #[inline]
    fn remove<Q>(&mut self, value: &Q) -> bool
    where
        T: core::borrow::Borrow<Q>,
        Q: Ord + core::hash::Hash + Eq + ?Sized,
    {
        BTreeSet::remove(self, value)
    }

    #[inline]
    fn clear(&mut self) {
        BTreeSet::clear(self);
    }

    #[inline]
    fn len(&self) -> usize {
        BTreeSet::len(self)
    }

    #[inline]
    fn is_empty(&self) -> bool {
        BTreeSet::is_empty(self)
    }

    #[inline]
    fn iter(&self) -> Self::Iter<'_> {
        BTreeSet::iter(self)
    }

    #[inline]
    fn is_subset(&self, other: &Self) -> bool {
        BTreeSet::is_subset(self, other)
    }
}
