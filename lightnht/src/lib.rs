//! Light Nested Hash Table.
//!
//! A collision-resolution scheme inspired by Ticki's
//! ["Collision resolution with nested hash tables"][ticki] post,
//! adapted into a small in-memory map / set with an
//! `alloc`-only-friendly footprint.
//!
//! [ticki]: https://ticki.github.io/blog/collision-resolution-with-nested-hash-tables/
//!
//! # Concept
//!
//! Each *bucket* in a sub-table is one of three shapes:
//!
//! ```text
//! Bucket =
//!   | Empty
//!   | Single(K, V)
//!   | Nested(SubTable<K, V>)
//! ```
//!
//! A sub-table is a fixed-size array of buckets. On insertion, the
//! key's hash picks a slot; an empty slot becomes `Single`; an
//! occupied `Single` slot is *promoted* to a new sub-table holding
//! both the previous and the incoming entry; a `Nested` slot is
//! descended into recursively.
//!
//! Lookups follow the same descent. The recursion's *depth* is the
//! number of sub-tables the descent has gone through.
//!
//! # Locked parameters
//!
//! These are the design choices already agreed; the rest of this
//! crate is built around them.
//!
//! - **Sub-table size = 8**. Every sub-table holds exactly 8 slots
//!   (3 bits of hash per level). Picked for cache locality at the
//!   small end and a short worst-case depth at the large end —
//!   `64 / 3 ≈ 21` levels exhaust a 64-bit hash entirely.
//! - **Hash output = 64 bits**. The crate works against any
//!   `core::hash::Hasher` whose `finish()` returns a `u64`.
//! - **Hash input = `(entry, depth_coord, recon)`**. The hash is
//!   *not* a single bit-slice of one precomputed value — every
//!   descent step writes the entry, the bucket's depth coordinate
//!   (see below), and the reconstruction counter into a fresh
//!   hasher and reads back a new 64-bit value. That gives each
//!   bucket its own hash space.
//! - **`depth_coord` = absolute path**. The "depth coordinate" of a
//!   bucket is *not* its tree-level integer. It's the full path of
//!   slot indices from the root sub-table to the bucket
//!   currently being addressed (e.g. `[3, 7, 2]`). Two sub-tables
//!   at the same numeric depth but reached via different paths see
//!   disjoint hash spaces.
//! - **`recon`** is a `usize` counter, initialised to `0`. The
//!   public [`LightNht::reconstruct`] method bumps it and re-hashes
//!   every entry into a fresh root, giving every key a brand-new
//!   set of bucket coordinates. Reaching [`MAX_DEPTH`] during an
//!   insertion triggers a reconstruct automatically.
//! - **`MAX_DEPTH` = 21**. With a 64-bit hash and 3 bits per level
//!   the hash space is exhausted at depth 21. A pathological key
//!   collision past that point forces a reconstruct (which in turn
//!   randomises everything via the bumped `recon` value).
//!
//! # Generic surface
//!
//! Per the agreed minimal-bound shape:
//!
//! - `LightNht<K, V, H>` keeps `<K: Hash + Eq, V, H: Hasher>` on the
//!   inherent block.
//! - `Clone` requires `K: Clone, V: Clone` (and clones `H` from the
//!   prototype kept in the table).
//! - `Default` requires `H: Hasher + Default`; it delegates to
//!   `Self::with_hasher(H::default())`.
//!
//! `H: Clone` is a working assumption for the actual hash-computing
//! path — every descent step starts from a fresh clone of the
//! prototype hasher so the `Hasher` state is per-key. The bound is
//! lifted off the struct itself and required only on the methods
//! that need it; that way `with_hasher` can stay `const` and the
//! Clone-derive shape the user asked for stays untouched.
//!
//! `LightNhtSet<T, H>` mirrors the same shape with `T: Hash + Eq`.

#![cfg_attr(not(feature = "std"), no_std)]
#![warn(missing_docs)]

#[cfg(not(feature = "alloc"))]
compile_error!(
    "lightnht requires at least the `alloc` feature. Use \
     `--no-default-features --features alloc` for no_std builds."
);

extern crate alloc;

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::hash::{Hash, Hasher};

/// Number of slots in every sub-table. Locked at 8 (= 3 bits of
/// hash per descent level). See the crate-level docs for the
/// rationale.
pub const SUBTABLE_SIZE: usize = 8;

/// Number of low bits of a 64-bit hash consumed at each descent
/// step. `log2(SUBTABLE_SIZE)`.
pub const SLOT_BITS: u32 = 3;

/// Slot-index mask matching [`SUBTABLE_SIZE`].
pub const SLOT_MASK: u64 = (SUBTABLE_SIZE as u64) - 1;

/// Hard upper bound on the descent depth. `64 / SLOT_BITS` — past
/// this point a 64-bit hash has been fully consumed and another
/// collision triggers a [`LightNht::reconstruct`].
pub const MAX_DEPTH: usize = 21;

/// One bucket inside a sub-table.
///
/// A bucket carries either no entry, exactly one entry, or a
/// pointer to a deeper sub-table that holds the entries that
/// collided in this slot.
#[derive(Debug, Default)]
pub enum Bucket<K, V> {
    /// No entry occupies this slot.
    #[default]
    Empty,
    /// Exactly one entry sits here.
    Single(K, V),
    /// A nested sub-table holds every entry that hashed to this
    /// slot.
    Nested(Box<SubTable<K, V>>),
}

impl<K: Clone, V: Clone> Clone for Bucket<K, V> {
    fn clone(&self) -> Self {
        match self {
            Self::Empty => Self::Empty,
            Self::Single(k, v) => Self::Single(k.clone(), v.clone()),
            Self::Nested(t) => Self::Nested(t.clone()),
        }
    }
}

/// A fixed-`SUBTABLE_SIZE` array of buckets, owned by either the
/// root of a [`LightNht`] or by a `Nested` bucket up the tree.
#[derive(Debug)]
pub struct SubTable<K, V> {
    /// Slots in this sub-table.
    pub slots: [Bucket<K, V>; SUBTABLE_SIZE],
}

impl<K, V> SubTable<K, V> {
    /// All-`Empty` sub-table. Allocates the slot array on the stack
    /// then moves it into the returned value (the caller is expected
    /// to `Box` it before use).
    pub fn new() -> Self {
        Self {
            slots: core::array::from_fn(|_| Bucket::Empty),
        }
    }
}

impl<K, V> Default for SubTable<K, V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: Clone, V: Clone> Clone for SubTable<K, V> {
    fn clone(&self) -> Self {
        Self {
            slots: core::array::from_fn(|i| self.slots[i].clone()),
        }
    }
}

/// Nested hash table mapping `K` to `V`.
pub struct LightNht<K, V, H>
where
    K: Hash + Eq,
    H: Hasher,
{
    /// Root sub-table. `None` until the first insertion, so
    /// `with_hasher` can stay `const`.
    root: Option<Box<SubTable<K, V>>>,
    /// Number of live entries.
    len: usize,
    /// Reconstruction counter. Folded into every hash so that
    /// bumping it via [`Self::reconstruct`] gives every key a fresh
    /// set of slot coordinates.
    recon: usize,
    /// Prototype hasher. Real hash computation clones this each
    /// descent step so each call sees a fresh `Hasher` instance.
    hash_builder: H,
}

impl<K, V, H> LightNht<K, V, H>
where
    K: Hash + Eq,
    H: Hasher,
{
    /// Empty table whose hash computation starts from the supplied
    /// prototype hasher. `const` so the table can be embedded in a
    /// `static`.
    pub const fn with_hasher(hasher: H) -> Self {
        Self {
            root: None,
            len: 0,
            recon: 0,
            hash_builder: hasher,
        }
    }

    /// Number of live entries.
    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    /// `true` iff the table holds no entries.
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Current value of the reconstruction counter. Exposed mostly
    /// for diagnostics — every entry is re-hashed against this
    /// number, so a higher value means the table has been
    /// reconstructed more times.
    #[inline]
    #[must_use]
    pub fn recon(&self) -> usize {
        self.recon
    }

    /// Bump the reconstruction counter and re-hash every entry into
    /// a fresh root sub-table. After this returns the structural
    /// layout is fully different but the set of `(K, V)` pairs is
    /// identical.
    pub fn reconstruct(&mut self) {
        self.recon = self.recon.wrapping_add(1);
        // Drain the old root into a flat list, then re-insert each
        // entry against the new `recon` value. Drained while the
        // root pointer is `None` so the inserts always see a fresh
        // tree under the bumped counter.
        let _entries: Vec<(K, V)> = self.drain_entries();
        todo!("re-insert entries under the new recon value");
    }

    /// Drain every entry out of the tree, leaving `self.root` as
    /// `None` and `self.len` as `0`. Used by [`Self::reconstruct`].
    fn drain_entries(&mut self) -> Vec<(K, V)> {
        todo!("DFS walk that pulls Single buckets out into a Vec");
    }
}

impl<K, V, H> LightNht<K, V, H>
where
    K: Hash + Eq + Clone,
    V: Clone,
    H: Hasher + Clone,
{
    /// Insert `(key, value)`. Returns the previously-bound value if
    /// any, `None` otherwise.
    pub fn insert(&mut self, _key: K, _value: V) -> Option<V> {
        todo!("descent + Single→Nested promotion + MAX_DEPTH check")
    }
}

impl<K, V, H> LightNht<K, V, H>
where
    K: Hash + Eq,
    H: Hasher + Clone,
{
    /// Look up the value associated with `key`, if any.
    pub fn get<Q>(&self, _key: &Q) -> Option<&V>
    where
        K: core::borrow::Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        todo!("descent driven by compute_hash(key, depth_coord, recon)")
    }

    /// Remove the binding for `key`. Returns the removed value if
    /// any.
    pub fn remove<Q>(&mut self, _key: &Q) -> Option<V>
    where
        K: core::borrow::Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        todo!("descent then Single → Empty; no promotion-up for now")
    }

    /// Compute the 64-bit hash for `(entry, depth_coord, recon)`.
    /// Clones [`Self::hash_builder`] so every descent step works
    /// from a fresh `Hasher` instance — the same prototype hasher
    /// is re-used, but its state never carries over between calls.
    #[inline]
    #[allow(dead_code)] // referenced once insert / get / remove fill in
    fn compute_hash<Q>(&self, entry: &Q, depth_coord: &[u8]) -> u64
    where
        Q: Hash + ?Sized,
    {
        let mut h = self.hash_builder.clone();
        entry.hash(&mut h);
        depth_coord.hash(&mut h);
        self.recon.hash(&mut h);
        h.finish()
    }
}

impl<K, V, H> Clone for LightNht<K, V, H>
where
    K: Hash + Eq + Clone,
    V: Clone,
    H: Hasher + Clone,
{
    fn clone(&self) -> Self {
        Self {
            root: self.root.clone(),
            len: self.len,
            recon: self.recon,
            hash_builder: self.hash_builder.clone(),
        }
    }
}

impl<K, V, H> Default for LightNht<K, V, H>
where
    K: Hash + Eq,
    H: Hasher + Default,
{
    fn default() -> Self {
        Self::with_hasher(H::default())
    }
}

// ===== Set =====

/// Nested-hash-table-backed set. Layered over the same machinery
/// as [`LightNht`] with `V = ()`.
pub struct LightNhtSet<T, H>
where
    T: Hash + Eq,
    H: Hasher,
{
    inner: LightNht<T, (), H>,
}

impl<T, H> LightNhtSet<T, H>
where
    T: Hash + Eq,
    H: Hasher,
{
    /// Empty set under the supplied prototype hasher.
    pub const fn with_hasher(hasher: H) -> Self {
        Self {
            inner: LightNht::with_hasher(hasher),
        }
    }

    /// Number of members.
    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// `true` iff empty.
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Current reconstruction counter (forwards to [`LightNht::recon`]).
    #[inline]
    #[must_use]
    pub fn recon(&self) -> usize {
        self.inner.recon()
    }

    /// Forwarded [`LightNht::reconstruct`].
    pub fn reconstruct(&mut self) {
        self.inner.reconstruct();
    }
}

impl<T, H> LightNhtSet<T, H>
where
    T: Hash + Eq + Clone,
    H: Hasher + Clone,
{
    /// Insert `item`. Returns `true` if `item` was not already a
    /// member.
    pub fn insert(&mut self, item: T) -> bool {
        self.inner.insert(item, ()).is_none()
    }
}

impl<T, H> LightNhtSet<T, H>
where
    T: Hash + Eq,
    H: Hasher + Clone,
{
    /// `true` iff `item` is a member.
    pub fn contains<Q>(&self, item: &Q) -> bool
    where
        T: core::borrow::Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.inner.get(item).is_some()
    }

    /// Remove `item`. Returns `true` if it was a member.
    pub fn remove<Q>(&mut self, item: &Q) -> bool
    where
        T: core::borrow::Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.inner.remove(item).is_some()
    }
}

impl<T, H> Clone for LightNhtSet<T, H>
where
    T: Hash + Eq + Clone,
    H: Hasher + Clone,
{
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<T, H> Default for LightNhtSet<T, H>
where
    T: Hash + Eq,
    H: Hasher + Default,
{
    fn default() -> Self {
        Self::with_hasher(H::default())
    }
}
