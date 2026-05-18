//! Map / set storage abstraction.
//!
//! Every map / set inside the engine goes through one of two
//! generic-driven layers:
//!
//! 1. The trait pair [`MapStorage<K, V>`] and [`SetStorage<T>`]
//!    abstracts the operations call sites actually use (`get`,
//!    `insert`, `remove`, `contains{_key}`, `iter`, `len`, `clear`,
//!    plus `is_subset` for the modifier-set monotonicity check).
//!    Any container that exposes those methods can be slotted in.
//! 2. The [`MapBackend`] trait is a "family" of containers — its
//!    `Map<K, V>` and `Set<T>` GATs let a single type parameter
//!    select both at once. Downstream types (`Mode<B>`, `Frame<B>`,
//!    `Scope<B>`, `Evaluator<B>`) take one `B: MapBackend` parameter
//!    and use `B::Map<…>` / `B::Set<…>` wherever they hold owned
//!    storage.
//!
//! The default backend, [`BTreeBackend`], wires both slots to
//! `alloc::collections::BTreeMap` / `BTreeSet`. Picked because:
//!
//! - they live in `alloc` (no `std` hasher dependency, so the
//!   default `alloc`-only build can use them),
//! - they iterate in `Ord` order, which keeps `trap` / `alias` /
//!   function-table listings deterministic without a separate sort
//!   pass,
//! - at the small `n` a shell deals with (a few hundred bindings at
//!   most) the `O(log n)` lookups are indistinguishable from the
//!   `O(1)` of a hash table, and the cache layout is friendlier.
//!
//! Implementing a new backend is one `impl MapBackend for …` block
//! plus matching `impl MapStorage` / `impl SetStorage` blocks for the
//! container types it picks. Everything downstream picks up the swap
//! through type-parameter substitution.

use alloc::collections::{btree_map, btree_set, BTreeMap, BTreeSet};

/// Map-of-`K`-to-`V` storage abstraction. The set of methods is the
/// intersection of what every call site in the engine uses.
///
/// `Clone` is a supertrait so subshell-style environment snapshots
/// (and the `Evaluator: Clone` line they imply) can snapshot maps
/// without an extra trait constraint at every call site. Concrete
/// implementations are expected to have `Clone` whenever their key
/// and value types do — BTreeMap satisfies that out of the box.
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
        Q: Ord + ?Sized;

    /// Mutable lookup, same `Borrow<Q>` shape as `get`.
    fn get_mut<Q>(&mut self, key: &Q) -> Option<&mut V>
    where
        K: core::borrow::Borrow<Q>,
        Q: Ord + ?Sized;

    /// `true` iff `key` is bound.
    fn contains_key<Q>(&self, key: &Q) -> bool
    where
        K: core::borrow::Borrow<Q>,
        Q: Ord + ?Sized;

    /// Insert / overwrite. Returns the old value if there was one.
    fn insert(&mut self, key: K, value: V) -> Option<V>;

    /// Remove the binding for `key`. Returns the old value if any.
    fn remove<Q>(&mut self, key: &Q) -> Option<V>
    where
        K: core::borrow::Borrow<Q>,
        Q: Ord + ?Sized;

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
        Q: Ord + ?Sized;

    /// Insert `value`. Returns `true` if `value` wasn't already a
    /// member.
    fn insert(&mut self, value: T) -> bool;

    /// Remove `value`. Returns `true` if it was a member.
    fn remove<Q>(&mut self, value: &Q) -> bool
    where
        T: core::borrow::Borrow<Q>,
        Q: Ord + ?Sized;

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
    type Map<K, V>: MapStorage<K, V>
    where
        K: Ord + Clone,
        V: Clone;

    /// Concrete set type used by this backend.
    type Set<T>: SetStorage<T>
    where
        T: Ord + Clone;
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
        K: Ord + Clone,
        V: Clone;
    type Set<T>
        = BTreeSet<T>
    where
        T: Ord + Clone;
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
        Q: Ord + ?Sized,
    {
        BTreeMap::get(self, key)
    }

    #[inline]
    fn get_mut<Q>(&mut self, key: &Q) -> Option<&mut V>
    where
        K: core::borrow::Borrow<Q>,
        Q: Ord + ?Sized,
    {
        BTreeMap::get_mut(self, key)
    }

    #[inline]
    fn contains_key<Q>(&self, key: &Q) -> bool
    where
        K: core::borrow::Borrow<Q>,
        Q: Ord + ?Sized,
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
        Q: Ord + ?Sized,
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
        Q: Ord + ?Sized,
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
        Q: Ord + ?Sized,
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
