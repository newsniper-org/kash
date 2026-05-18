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

}

impl<K, V, H> LightNht<K, V, H>
where
    K: Hash + Eq,
    H: Hasher + Clone,
{
    /// Insert `(key, value)`. Returns the previously-bound value if
    /// any.
    ///
    /// If a descent reaches `MAX_DEPTH` (the 64-bit hash is fully
    /// consumed without finding an empty slot) the table is
    /// [`reconstruct`](Self::reconstruct)ed under a bumped `recon`
    /// counter and the insertion retries. The retry budget is
    /// bounded so a pathological hasher can't loop forever.
    pub fn insert(&mut self, key: K, value: V) -> Option<V> {
        if self.root.is_none() {
            self.root = Some(Box::<SubTable<K, V>>::default());
        }
        let mut key = key;
        let mut value = value;
        let mut depth_coord = Vec::new();
        for _ in 0..RECONSTRUCT_RETRY_BUDGET {
            depth_coord.clear();
            let root = self.root.as_mut().expect("root allocated above");
            match Self::try_insert_into(
                &self.hash_builder,
                self.recon,
                root,
                key,
                value,
                &mut depth_coord,
                0,
                &mut self.len,
            ) {
                Ok(prev) => return prev,
                Err((k, v)) => {
                    self.reconstruct();
                    if self.root.is_none() {
                        self.root = Some(Box::<SubTable<K, V>>::default());
                    }
                    key = k;
                    value = v;
                }
            }
        }
        panic!(
            "lightnht: insert hit MAX_DEPTH even after {RECONSTRUCT_RETRY_BUDGET} \
             reconstructs — hasher quality is almost certainly the issue"
        );
    }

    /// Recursive insertion helper. Returns `Err((key, value))` if
    /// the descent ran past [`MAX_DEPTH`] so the caller can
    /// reconstruct and retry.
    #[allow(clippy::too_many_arguments)]
    fn try_insert_into(
        hash_builder: &H,
        recon: usize,
        sub_table: &mut SubTable<K, V>,
        key: K,
        value: V,
        depth_coord: &mut Vec<u8>,
        depth: usize,
        len_counter: &mut usize,
    ) -> Result<Option<V>, (K, V)> {
        if depth >= MAX_DEPTH {
            return Err((key, value));
        }
        let hash = compute_hash_with(hash_builder, recon, &key, depth_coord);
        let slot_idx = (hash & SLOT_MASK) as usize;
        match &mut sub_table.slots[slot_idx] {
            Bucket::Empty => {
                sub_table.slots[slot_idx] = Bucket::Single(key, value);
                *len_counter += 1;
                Ok(None)
            }
            Bucket::Single(existing_k, _) if *existing_k == key => {
                // Overwrite. Replace the whole bucket so we get the
                // owned old value back out cleanly.
                let old = core::mem::replace(
                    &mut sub_table.slots[slot_idx],
                    Bucket::Single(key, value),
                );
                match old {
                    Bucket::Single(_, old_v) => Ok(Some(old_v)),
                    _ => unreachable!("matched Single above"),
                }
            }
            Bucket::Single(_, _) => {
                // Promote: pop the existing single, install a fresh
                // sub-table in its place, then re-insert both entries
                // into the new sub-table one level deeper.
                let popped = core::mem::replace(
                    &mut sub_table.slots[slot_idx],
                    Bucket::Nested(Box::<SubTable<K, V>>::default()),
                );
                let (ek, ev) = match popped {
                    Bucket::Single(k, v) => (k, v),
                    _ => unreachable!("matched Single above"),
                };
                let nested = match &mut sub_table.slots[slot_idx] {
                    Bucket::Nested(n) => n,
                    _ => unreachable!("just installed Nested"),
                };
                depth_coord.push(slot_idx as u8);
                // Existing entry first; this one was already in the
                // table so the counter shouldn't move.
                let r1 = Self::try_insert_into(
                    hash_builder,
                    recon,
                    nested,
                    ek,
                    ev,
                    depth_coord,
                    depth + 1,
                    len_counter,
                );
                if let Err(bounced) = r1 {
                    depth_coord.pop();
                    return Err(bounced);
                }
                // Counter shouldn't have moved (we just re-located an
                // existing entry); rewind it if it did.
                *len_counter -= 1;
                let r2 = Self::try_insert_into(
                    hash_builder,
                    recon,
                    nested,
                    key,
                    value,
                    depth_coord,
                    depth + 1,
                    len_counter,
                );
                depth_coord.pop();
                r2
            }
            Bucket::Nested(nested) => {
                depth_coord.push(slot_idx as u8);
                let r = Self::try_insert_into(
                    hash_builder,
                    recon,
                    nested,
                    key,
                    value,
                    depth_coord,
                    depth + 1,
                    len_counter,
                );
                depth_coord.pop();
                r
            }
        }
    }

    /// Look up the value associated with `key`.
    pub fn get<Q>(&self, key: &Q) -> Option<&V>
    where
        K: core::borrow::Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let root = self.root.as_ref()?;
        let mut depth_coord = Vec::new();
        Self::get_in(&self.hash_builder, self.recon, root, key, &mut depth_coord)
    }

    fn get_in<'t, Q>(
        hash_builder: &H,
        recon: usize,
        sub_table: &'t SubTable<K, V>,
        key: &Q,
        depth_coord: &mut Vec<u8>,
    ) -> Option<&'t V>
    where
        K: core::borrow::Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let hash = compute_hash_with(hash_builder, recon, key, depth_coord);
        let slot_idx = (hash & SLOT_MASK) as usize;
        match &sub_table.slots[slot_idx] {
            Bucket::Empty => None,
            Bucket::Single(k, v) if k.borrow() == key => Some(v),
            Bucket::Single(_, _) => None,
            Bucket::Nested(nested) => {
                depth_coord.push(slot_idx as u8);
                let r = Self::get_in(hash_builder, recon, nested, key, depth_coord);
                depth_coord.pop();
                r
            }
        }
    }

    /// Look up `key` and return a mutable reference to its value.
    /// Same descent as [`Self::get`].
    pub fn get_mut<Q>(&mut self, key: &Q) -> Option<&mut V>
    where
        K: core::borrow::Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let root = self.root.as_mut()?;
        let mut depth_coord = Vec::new();
        Self::get_mut_in(&self.hash_builder, self.recon, root, key, &mut depth_coord)
    }

    fn get_mut_in<'t, Q>(
        hash_builder: &H,
        recon: usize,
        sub_table: &'t mut SubTable<K, V>,
        key: &Q,
        depth_coord: &mut Vec<u8>,
    ) -> Option<&'t mut V>
    where
        K: core::borrow::Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let hash = compute_hash_with(hash_builder, recon, key, depth_coord);
        let slot_idx = (hash & SLOT_MASK) as usize;
        match &mut sub_table.slots[slot_idx] {
            Bucket::Empty => None,
            Bucket::Single(k, v) if (k as &K).borrow() == key => Some(v),
            Bucket::Single(_, _) => None,
            Bucket::Nested(nested) => {
                depth_coord.push(slot_idx as u8);
                let r = Self::get_mut_in(hash_builder, recon, nested, key, depth_coord);
                depth_coord.pop();
                r
            }
        }
    }

    /// Drop every entry. The root pointer is freed.
    #[inline]
    pub fn clear(&mut self) {
        self.root = None;
        self.len = 0;
    }

    /// Remove the binding for `key`. Returns the removed value if
    /// any.
    ///
    /// Removal turns the matching `Single` slot back into `Empty`.
    /// Sub-tables that drop to zero or one live entries are *not*
    /// collapsed back up the tree in this minimal cut; the doc on
    /// the crate notes that a follow-up commit can add the
    /// promote-up optimisation if it ever matters in practice.
    pub fn remove<Q>(&mut self, key: &Q) -> Option<V>
    where
        K: core::borrow::Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let root = self.root.as_mut()?;
        let mut depth_coord = Vec::new();
        let result = Self::remove_in(
            &self.hash_builder,
            self.recon,
            root,
            key,
            &mut depth_coord,
        );
        if result.is_some() {
            self.len -= 1;
        }
        result
    }

    fn remove_in<Q>(
        hash_builder: &H,
        recon: usize,
        sub_table: &mut SubTable<K, V>,
        key: &Q,
        depth_coord: &mut Vec<u8>,
    ) -> Option<V>
    where
        K: core::borrow::Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let hash = compute_hash_with(hash_builder, recon, key, depth_coord);
        let slot_idx = (hash & SLOT_MASK) as usize;
        match &mut sub_table.slots[slot_idx] {
            Bucket::Empty => None,
            Bucket::Single(k, _) if k.borrow() == key => {
                let old = core::mem::replace(
                    &mut sub_table.slots[slot_idx],
                    Bucket::Empty,
                );
                match old {
                    Bucket::Single(_, v) => Some(v),
                    _ => unreachable!("matched Single above"),
                }
            }
            Bucket::Single(_, _) => None,
            Bucket::Nested(nested) => {
                depth_coord.push(slot_idx as u8);
                let r = Self::remove_in(hash_builder, recon, nested, key, depth_coord);
                depth_coord.pop();
                r
            }
        }
    }

    /// Bump the reconstruction counter and re-hash every entry into
    /// a fresh root. The set of `(K, V)` pairs is preserved; the
    /// structural layout is entirely different.
    pub fn reconstruct(&mut self) {
        self.recon = self.recon.wrapping_add(1);
        let entries = self.drain_entries();
        // Fresh root, then re-insert every drained entry under the
        // bumped counter. Inserts go through `try_insert_into`
        // directly because `insert` itself would re-enter
        // `reconstruct` on a MAX_DEPTH error, which is exactly the
        // recursion we're trying to avoid.
        self.root = Some(Box::<SubTable<K, V>>::default());
        for (k, v) in entries {
            let mut depth_coord = Vec::new();
            let root = self.root.as_mut().expect("just installed");
            let result = Self::try_insert_into(
                &self.hash_builder,
                self.recon,
                root,
                k,
                v,
                &mut depth_coord,
                0,
                &mut self.len,
            );
            match result {
                Ok(prev) => debug_assert!(prev.is_none(), "duplicate key in reconstruct"),
                Err(_) => panic!(
                    "lightnht: MAX_DEPTH still exceeded after reconstructing — \
                     hasher quality is almost certainly the issue"
                ),
            }
        }
    }

    /// DFS-drain every entry from the current root, then clear the
    /// root pointer. Used by [`Self::reconstruct`].
    fn drain_entries(&mut self) -> Vec<(K, V)> {
        let mut out = Vec::with_capacity(self.len);
        if let Some(root) = self.root.take() {
            drain_subtable(*root, &mut out);
        }
        self.len = 0;
        out
    }

    /// Compute the 64-bit hash for `(entry, depth_coord, recon)`
    /// against the table's current prototype hasher and reconstruct
    /// counter. Useful for tests, diagnostics, and external descent
    /// drivers — internally [`Self::insert`] / [`Self::get`] /
    /// [`Self::remove`] go through the same routine.
    ///
    /// Each call clones the prototype hasher so its state never
    /// carries between calls.
    #[inline]
    pub fn compute_hash<Q>(&self, entry: &Q, depth_coord: &[u8]) -> u64
    where
        Q: Hash + ?Sized,
    {
        compute_hash_with(&self.hash_builder, self.recon, entry, depth_coord)
    }
}

impl<K, V, H> LightNht<K, V, H>
where
    K: Hash + Eq,
    H: Hasher,
{
    /// Iterate `(&K, &V)` pairs in DFS order over the sub-table tree.
    /// The order is implementation-defined and changes after each
    /// [`Self::reconstruct`]; do not rely on it.
    #[inline]
    pub fn iter(&self) -> Iter<'_, K, V> {
        let mut stack = Vec::new();
        if let Some(root) = self.root.as_ref() {
            stack.push((root.as_ref(), 0usize));
        }
        Iter { stack }
    }
}

/// Borrowing iterator over every live `(&K, &V)` in a [`LightNht`].
/// Walks the sub-table tree depth-first, yielding `Single` buckets
/// in DFS order. State is a stack of `(sub_table, next_slot_index)`
/// pairs.
pub struct Iter<'a, K, V> {
    stack: Vec<(&'a SubTable<K, V>, usize)>,
}

impl<'a, K, V> Iterator for Iter<'a, K, V> {
    type Item = (&'a K, &'a V);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            // Borrow-checker dance: peel one frame off the stack
            // pointer at a time so the mutable update to the slot
            // index doesn't alias the immutable read of the bucket.
            let frame = self.stack.last_mut()?;
            let (sub, idx) = (frame.0, frame.1);
            if idx >= SUBTABLE_SIZE {
                self.stack.pop();
                continue;
            }
            frame.1 = idx + 1;
            match &sub.slots[idx] {
                Bucket::Empty => continue,
                Bucket::Single(k, v) => return Some((k, v)),
                Bucket::Nested(nested) => {
                    self.stack.push((nested.as_ref(), 0));
                }
            }
        }
    }
}

/// Compute the 64-bit hash for `(entry, depth_coord, recon)` against
/// `hash_builder`. Clones the prototype hasher so the input fed in
/// here can't leak into a later call.
#[inline]
fn compute_hash_with<H, Q>(hash_builder: &H, recon: usize, entry: &Q, depth_coord: &[u8]) -> u64
where
    H: Hasher + Clone,
    Q: Hash + ?Sized,
{
    let mut h = hash_builder.clone();
    entry.hash(&mut h);
    depth_coord.hash(&mut h);
    recon.hash(&mut h);
    h.finish()
}

/// Owned DFS over a [`SubTable`]: every `Single` bucket spills its
/// `(K, V)` into `out`, every `Nested` recurses, every `Empty` is a
/// no-op. The sub-table is consumed.
fn drain_subtable<K, V>(sub: SubTable<K, V>, out: &mut Vec<(K, V)>) {
    for bucket in sub.slots {
        match bucket {
            Bucket::Empty => {}
            Bucket::Single(k, v) => out.push((k, v)),
            Bucket::Nested(inner) => drain_subtable(*inner, out),
        }
    }
}

/// Upper bound on the number of consecutive reconstruct-retries
/// during a single `insert`. Reaching this means the hasher is
/// producing the same colliding chain even after fresh `recon`
/// values — almost certainly a broken hasher, not a real hash
/// space exhaustion.
const RECONSTRUCT_RETRY_BUDGET: usize = 4;

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

    /// Iterate `&T` members in DFS order. The order is
    /// implementation-defined and changes after [`Self::reconstruct`].
    #[inline]
    pub fn iter(&self) -> SetIter<'_, T> {
        SetIter {
            inner: self.inner.iter(),
        }
    }
}

/// Borrowing iterator over every live `&T` in a [`LightNhtSet`].
pub struct SetIter<'a, T> {
    inner: Iter<'a, T, ()>,
}

impl<'a, T> Iterator for SetIter<'a, T> {
    type Item = &'a T;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(|(k, _)| k)
    }
}

impl<T, H> LightNhtSet<T, H>
where
    T: Hash + Eq,
    H: Hasher + Clone,
{
    /// Insert `item`. Returns `true` if `item` was not already a
    /// member.
    pub fn insert(&mut self, item: T) -> bool {
        self.inner.insert(item, ()).is_none()
    }

    /// Forwarded [`LightNht::reconstruct`]. Lives on the
    /// `H: Hasher + Clone` block because the reconstruct path
    /// re-hashes every entry via the prototype hasher.
    pub fn reconstruct(&mut self) {
        self.inner.reconstruct();
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

// ===== kash-gadt MapStorage / SetStorage impls =====
//
// These pull `lightnht` into the `MapBackend`-family picture so a
// future `LightNhtBackend` zero-sized marker can slot in next to
// `BTreeBackend`. `K` / `T` need `Ord` here because the trait
// signature (intentionally) keeps the `Ord` bound for the BTree
// path; in practice every shell-key type (`String`, `&str`, …)
// already satisfies both `Ord` and `Hash + Eq`, so the doubled
// bound just narrows callers to keys that work in either backend.

impl<K, V, H> kash_gadt::MapStorage<K, V> for LightNht<K, V, H>
where
    K: Hash + Eq + Ord + Clone,
    V: Clone,
    H: Hasher + Clone + Default,
{
    type Iter<'a>
        = Iter<'a, K, V>
    where
        Self: 'a,
        K: 'a,
        V: 'a;

    #[inline]
    fn get<Q>(&self, key: &Q) -> Option<&V>
    where
        K: core::borrow::Borrow<Q>,
        Q: Ord + Hash + Eq + ?Sized,
    {
        LightNht::<K, V, H>::get(self, key)
    }

    #[inline]
    fn get_mut<Q>(&mut self, key: &Q) -> Option<&mut V>
    where
        K: core::borrow::Borrow<Q>,
        Q: Ord + Hash + Eq + ?Sized,
    {
        LightNht::<K, V, H>::get_mut(self, key)
    }

    #[inline]
    fn contains_key<Q>(&self, key: &Q) -> bool
    where
        K: core::borrow::Borrow<Q>,
        Q: Ord + Hash + Eq + ?Sized,
    {
        LightNht::<K, V, H>::get(self, key).is_some()
    }

    #[inline]
    fn insert(&mut self, key: K, value: V) -> Option<V> {
        LightNht::<K, V, H>::insert(self, key, value)
    }

    #[inline]
    fn remove<Q>(&mut self, key: &Q) -> Option<V>
    where
        K: core::borrow::Borrow<Q>,
        Q: Ord + Hash + Eq + ?Sized,
    {
        LightNht::<K, V, H>::remove(self, key)
    }

    #[inline]
    fn clear(&mut self) {
        LightNht::<K, V, H>::clear(self);
    }

    #[inline]
    fn len(&self) -> usize {
        LightNht::<K, V, H>::len(self)
    }

    #[inline]
    fn is_empty(&self) -> bool {
        LightNht::<K, V, H>::is_empty(self)
    }

    #[inline]
    fn iter(&self) -> Self::Iter<'_> {
        LightNht::<K, V, H>::iter(self)
    }
}

impl<T, H> kash_gadt::SetStorage<T> for LightNhtSet<T, H>
where
    T: Hash + Eq + Ord + Clone,
    H: Hasher + Clone + Default,
{
    type Iter<'a>
        = SetIter<'a, T>
    where
        Self: 'a,
        T: 'a;

    #[inline]
    fn contains<Q>(&self, value: &Q) -> bool
    where
        T: core::borrow::Borrow<Q>,
        Q: Ord + Hash + Eq + ?Sized,
    {
        LightNhtSet::<T, H>::contains(self, value)
    }

    #[inline]
    fn insert(&mut self, value: T) -> bool {
        LightNhtSet::<T, H>::insert(self, value)
    }

    #[inline]
    fn remove<Q>(&mut self, value: &Q) -> bool
    where
        T: core::borrow::Borrow<Q>,
        Q: Ord + Hash + Eq + ?Sized,
    {
        LightNhtSet::<T, H>::remove(self, value)
    }

    #[inline]
    fn clear(&mut self) {
        // Forwards to the inner map's clear.
        self.inner.clear();
    }

    #[inline]
    fn len(&self) -> usize {
        LightNhtSet::<T, H>::len(self)
    }

    #[inline]
    fn is_empty(&self) -> bool {
        LightNhtSet::<T, H>::is_empty(self)
    }

    #[inline]
    fn iter(&self) -> Self::Iter<'_> {
        LightNhtSet::<T, H>::iter(self)
    }

    fn is_subset(&self, other: &Self) -> bool {
        // O(|self|) walk; each `contains` is a fresh descent through
        // `other`'s tree.
        for item in self.iter() {
            if !LightNhtSet::<T, H>::contains(other, item) {
                return false;
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::{String, ToString};
    use core::hash::Hasher;

    /// Tiny FxHash-style prototype hasher, just so the tests can
    /// run without depending on `std::collections::DefaultHasher`.
    /// **Not** a recommendation — `lightnht` exposes the hasher
    /// choice to its caller, and the project's "pick a real hasher"
    /// discussion is the next step. This is only here so the tests
    /// in this file have *some* `H: Hasher + Clone + Default` to
    /// drive the algorithm.
    #[derive(Clone, Default)]
    struct TestHasher {
        state: u64,
    }

    impl Hasher for TestHasher {
        fn finish(&self) -> u64 {
            self.state
        }
        fn write(&mut self, bytes: &[u8]) {
            // Fx-style rotate-and-mix. Adequate for tests; not for
            // real workloads.
            for &b in bytes {
                self.state = self
                    .state
                    .rotate_left(5)
                    .wrapping_add(b as u64)
                    .wrapping_mul(0x9E37_79B9_7F4A_7C15);
            }
        }
    }

    type Map = LightNht<String, i32, TestHasher>;
    type Set = LightNhtSet<String, TestHasher>;

    fn k(s: &str) -> String {
        s.to_string()
    }

    #[test]
    fn empty_map_has_zero_len() {
        let m: Map = Map::default();
        assert_eq!(m.len(), 0);
        assert!(m.is_empty());
        assert!(m.get("anything").is_none());
    }

    #[test]
    fn insert_single_entry_reads_back() {
        let mut m: Map = Map::default();
        assert_eq!(m.insert(k("foo"), 1), None);
        assert_eq!(m.len(), 1);
        assert_eq!(m.get("foo"), Some(&1));
        assert!(m.get("bar").is_none());
    }

    #[test]
    fn insert_overwrites_existing() {
        let mut m: Map = Map::default();
        assert_eq!(m.insert(k("foo"), 1), None);
        assert_eq!(m.insert(k("foo"), 2), Some(1));
        assert_eq!(m.len(), 1);
        assert_eq!(m.get("foo"), Some(&2));
    }

    #[test]
    fn many_inserts_all_readable() {
        let mut m: Map = Map::default();
        for i in 0..100 {
            let key = alloc::format!("k{i}");
            assert_eq!(m.insert(key, i as i32), None);
        }
        assert_eq!(m.len(), 100);
        for i in 0..100 {
            let key = alloc::format!("k{i}");
            assert_eq!(m.get(&key), Some(&(i as i32)));
        }
        assert!(m.get("k100").is_none());
    }

    #[test]
    fn remove_returns_value_and_drops_len() {
        let mut m: Map = Map::default();
        m.insert(k("a"), 10);
        m.insert(k("b"), 20);
        m.insert(k("c"), 30);
        assert_eq!(m.remove("b"), Some(20));
        assert_eq!(m.len(), 2);
        assert!(m.get("b").is_none());
        assert_eq!(m.get("a"), Some(&10));
        assert_eq!(m.get("c"), Some(&30));
    }

    #[test]
    fn remove_missing_returns_none() {
        let mut m: Map = Map::default();
        m.insert(k("a"), 1);
        assert_eq!(m.remove("missing"), None);
        assert_eq!(m.len(), 1);
    }

    #[test]
    fn reconstruct_preserves_contents() {
        let mut m: Map = Map::default();
        for i in 0..30 {
            m.insert(alloc::format!("k{i}"), i as i32);
        }
        let recon_before = m.recon();
        m.reconstruct();
        assert_eq!(m.recon(), recon_before + 1);
        assert_eq!(m.len(), 30);
        for i in 0..30 {
            assert_eq!(m.get(&alloc::format!("k{i}")), Some(&(i as i32)));
        }
    }

    #[test]
    fn reinsert_after_remove_works() {
        let mut m: Map = Map::default();
        m.insert(k("a"), 1);
        m.remove("a");
        assert_eq!(m.insert(k("a"), 99), None);
        assert_eq!(m.get("a"), Some(&99));
    }

    #[test]
    fn set_insert_then_contains() {
        let mut s: Set = Set::default();
        assert!(s.insert(k("alpha")));
        assert!(s.insert(k("beta")));
        assert!(!s.insert(k("alpha")));
        assert_eq!(s.len(), 2);
        assert!(s.contains("alpha"));
        assert!(s.contains("beta"));
        assert!(!s.contains("gamma"));
    }

    #[test]
    fn set_remove_drops_member() {
        let mut s: Set = Set::default();
        s.insert(k("x"));
        assert!(s.remove("x"));
        assert!(!s.contains("x"));
        assert_eq!(s.len(), 0);
    }

    #[test]
    fn clone_keeps_independent_state() {
        let mut a: Map = Map::default();
        a.insert(k("foo"), 1);
        let mut b = a.clone();
        b.insert(k("foo"), 2);
        b.insert(k("bar"), 3);
        assert_eq!(a.get("foo"), Some(&1));
        assert!(a.get("bar").is_none());
        assert_eq!(b.get("foo"), Some(&2));
        assert_eq!(b.get("bar"), Some(&3));
    }

    #[test]
    fn iter_yields_every_entry() {
        let mut m: Map = Map::default();
        let mut expected: alloc::vec::Vec<(String, i32)> = alloc::vec::Vec::new();
        for i in 0..50 {
            let key = alloc::format!("iter-{i:03}");
            m.insert(key.clone(), i);
            expected.push((key, i));
        }
        let mut seen: alloc::vec::Vec<(String, i32)> = m
            .iter()
            .map(|(k, v)| (k.clone(), *v))
            .collect();
        seen.sort();
        expected.sort();
        assert_eq!(seen, expected);
    }

    #[test]
    fn iter_on_empty_map_yields_nothing() {
        let m: Map = Map::default();
        assert_eq!(m.iter().count(), 0);
    }

    #[test]
    fn get_mut_updates_in_place() {
        let mut m: Map = Map::default();
        m.insert(k("counter"), 0);
        if let Some(v) = m.get_mut("counter") {
            *v = 42;
        }
        assert_eq!(m.get("counter"), Some(&42));
    }

    #[test]
    fn clear_drops_everything() {
        let mut m: Map = Map::default();
        for i in 0..10 {
            m.insert(alloc::format!("k{i}"), i);
        }
        m.clear();
        assert_eq!(m.len(), 0);
        assert!(m.iter().next().is_none());
        // Re-insert after clear should work.
        m.insert(k("after"), 1);
        assert_eq!(m.get("after"), Some(&1));
    }

    #[test]
    fn set_iter_yields_every_member() {
        let mut s: Set = Set::default();
        for i in 0..30 {
            s.insert(alloc::format!("m{i}"));
        }
        let mut got: alloc::vec::Vec<String> = s.iter().cloned().collect();
        got.sort();
        let mut expected: alloc::vec::Vec<String> = (0..30)
            .map(|i| alloc::format!("m{i}"))
            .collect();
        expected.sort();
        assert_eq!(got, expected);
    }

    // ===== kash-gadt MapStorage / SetStorage trait impls =====

    #[test]
    fn map_storage_trait_basic_ops() {
        use kash_gadt::MapStorage;
        let mut m: Map = Map::default();
        assert_eq!(MapStorage::len(&m), 0);
        assert!(MapStorage::is_empty(&m));
        MapStorage::insert(&mut m, k("foo"), 1);
        MapStorage::insert(&mut m, k("bar"), 2);
        assert_eq!(MapStorage::len(&m), 2);
        assert_eq!(MapStorage::get(&m, "foo"), Some(&1));
        assert!(MapStorage::contains_key(&m, "bar"));
        assert_eq!(MapStorage::remove(&mut m, "foo"), Some(1));
        assert_eq!(MapStorage::len(&m), 1);
    }

    #[test]
    fn map_storage_iter_via_trait() {
        use kash_gadt::MapStorage;
        let mut m: Map = Map::default();
        m.insert(k("a"), 1);
        m.insert(k("b"), 2);
        let count = MapStorage::iter(&m).count();
        assert_eq!(count, 2);
    }

    #[test]
    fn set_storage_trait_basic_ops() {
        use kash_gadt::SetStorage;
        let mut s: Set = Set::default();
        assert!(SetStorage::insert(&mut s, k("alpha")));
        assert!(!SetStorage::insert(&mut s, k("alpha"))); // duplicate
        assert!(SetStorage::insert(&mut s, k("beta")));
        assert_eq!(SetStorage::len(&s), 2);
        assert!(SetStorage::contains(&s, "alpha"));
        assert!(SetStorage::remove(&mut s, "alpha"));
        assert!(!SetStorage::contains(&s, "alpha"));
    }

    #[test]
    fn set_storage_is_subset() {
        use kash_gadt::SetStorage;
        let mut a: Set = Set::default();
        let mut b: Set = Set::default();
        a.insert(k("x"));
        a.insert(k("y"));
        b.insert(k("x"));
        b.insert(k("y"));
        b.insert(k("z"));
        assert!(SetStorage::is_subset(&a, &b));
        assert!(!SetStorage::is_subset(&b, &a));
    }

    #[test]
    fn map_storage_swappable_with_btree() {
        // A generic function written against `MapStorage` accepts
        // both `BTreeMap` and `LightNht`. This compiles iff the
        // trait surface really is interchangeable.
        use kash_gadt::MapStorage;
        fn round_trip<M>(map: &mut M)
        where
            M: MapStorage<String, i32>,
        {
            map.insert("ten".to_string(), 10);
            map.insert("twenty".to_string(), 20);
            assert_eq!(map.get("ten"), Some(&10));
            assert_eq!(map.remove("ten"), Some(10));
            assert_eq!(map.len(), 1);
        }
        let mut light: Map = Map::default();
        let mut btree: alloc::collections::BTreeMap<String, i32> =
            alloc::collections::BTreeMap::new();
        round_trip(&mut light);
        round_trip(&mut btree);
    }

    #[test]
    fn promotion_drives_nesting() {
        // Stress test: insert enough entries that several
        // sub-tables get promoted. Then verify all are reachable.
        let mut m: Map = Map::default();
        for i in 0..1000 {
            m.insert(alloc::format!("entry-{i:04}"), i as i32);
        }
        assert_eq!(m.len(), 1000);
        for i in 0..1000 {
            assert_eq!(
                m.get(&alloc::format!("entry-{i:04}")),
                Some(&(i as i32)),
            );
        }
    }
}
