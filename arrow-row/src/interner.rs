// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

use hashbrown::hash_map::RawEntryMut;
use hashbrown::HashMap;
use std::num::NonZeroU32;
use std::ops::Index;

/// An interned value
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub struct Interned(NonZeroU32); // We use NonZeroU32 so that `Option<Interned>` is 32 bits

/// A byte array interner that generates normalized keys that are sorted with respect
/// to the interned values, e.g. `inter(a) < intern(b) => a < b`
#[derive(Debug, Default)]
pub struct OrderPreservingInterner {
    /// Provides a lookup from [`Interned`] to the normalized key
    keys: InternBuffer,
    /// Provides a lookup from [`Interned`] to the normalized value
    values: InternBuffer,
    /// Key allocation data structure
    bucket: Box<Bucket>,

    // A hash table used to perform faster re-keying, and detect duplicates
    hasher: ahash::RandomState,
    lookup: HashMap<Interned, (), ()>,
}

impl OrderPreservingInterner {
    /// Interns an iterator of values returning a list of [`Interned`] which can be
    /// used with [`Self::normalized_key`] to retrieve the normalized keys with a
    /// lifetime not tied to the mutable borrow passed to this method
    pub fn intern<I, V>(&mut self, input: I) -> Vec<Option<Interned>>
    where
        I: IntoIterator<Item = Option<V>>,
        V: AsRef<[u8]>,
    {
        let iter = input.into_iter();
        let capacity = iter.size_hint().0;
        let mut out = Vec::with_capacity(capacity);

        // (index in output, hash value, value)
        let mut to_intern: Vec<(usize, u64, V)> = Vec::with_capacity(capacity);
        let mut to_intern_len = 0;

        for (idx, item) in iter.enumerate() {
            let value: V = match item {
                Some(value) => value,
                None => {
                    out.push(None);
                    continue;
                }
            };

            let v = value.as_ref();
            let hash = self.hasher.hash_one(v);
            let entry = self
                .lookup
                .raw_entry_mut()
                .from_hash(hash, |a| &self.values[*a] == v);

            match entry {
                RawEntryMut::Occupied(o) => out.push(Some(*o.key())),
                RawEntryMut::Vacant(_) => {
                    // Push placeholder
                    out.push(None);
                    to_intern_len += v.len();
                    to_intern.push((idx, hash, value));
                }
            };
        }

        to_intern.sort_unstable_by(|(_, _, a), (_, _, b)| a.as_ref().cmp(b.as_ref()));

        self.keys.offsets.reserve(to_intern.len());
        self.keys.values.reserve(to_intern.len()); // Approximation
        self.values.offsets.reserve(to_intern.len());
        self.values.values.reserve(to_intern_len);

        for (idx, hash, value) in to_intern {
            let val = value.as_ref();

            let entry = self
                .lookup
                .raw_entry_mut()
                .from_hash(hash, |a| &self.values[*a] == val);

            match entry {
                RawEntryMut::Occupied(o) => {
                    out[idx] = Some(*o.key());
                }
                RawEntryMut::Vacant(v) => {
                    let val = value.as_ref();
                    self.bucket
                        .insert(&mut self.values, val, &mut self.keys.values);
                    self.keys.values.push(0);
                    let interned = self.keys.append();

                    let hasher = &mut self.hasher;
                    let values = &self.values;
                    v.insert_with_hasher(hash, interned, (), |key| {
                        hasher.hash_one(&values[*key])
                    });
                    out[idx] = Some(interned);
                }
            }
        }

        out
    }

    /// Returns a null-terminated byte array that can be compared against other normalized_key
    /// returned by this instance, to establish ordering of the interned values
    pub fn normalized_key(&self, key: Interned) -> &[u8] {
        &self.keys[key]
    }

    /// Converts a normalized key returned by [`Self::normalized_key`] to [`Interned`]
    /// returning `None` if it cannot be found
    pub fn lookup(&self, normalized_key: &[u8]) -> Option<Interned> {
        let len = normalized_key.len();
        if len <= 1 {
            return None;
        }

        let mut bucket = self.bucket.as_ref();
        if len > 2 {
            for v in normalized_key.iter().take(len - 2) {
                if *v == 255 {
                    bucket = bucket.next.as_ref()?;
                } else {
                    let bucket_idx = v.checked_sub(1)?;
                    bucket = bucket.slots.get(bucket_idx as usize)?.child.as_ref()?;
                }
            }
        }

        let slot_idx = normalized_key[len - 2].checked_sub(2)?;
        Some(bucket.slots.get(slot_idx as usize)?.value)
    }

    /// Returns the interned value for a given [`Interned`]
    pub fn value(&self, key: Interned) -> &[u8] {
        self.values.index(key)
    }

    /// Returns the size of this instance in bytes including self
    pub fn size(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.keys.buffer_size()
            + self.values.buffer_size()
            + self.bucket.size()
            + self.lookup.capacity() * std::mem::size_of::<Interned>()
    }
}

/// A buffer of `[u8]` indexed by `[Interned]`
#[derive(Debug)]
struct InternBuffer {
    /// Raw values
    values: Vec<u8>,
    /// The ith value is `&values[offsets[i]..offsets[i+1]]`
    offsets: Vec<usize>,
}

impl Default for InternBuffer {
    fn default() -> Self {
        Self {
            values: Default::default(),
            offsets: vec![0],
        }
    }
}

impl InternBuffer {
    /// Insert `data` returning the corresponding [`Interned`]
    fn insert(&mut self, data: &[u8]) -> Interned {
        self.values.extend_from_slice(data);
        self.append()
    }

    /// Appends the next value based on data written to `self.values`
    /// returning the corresponding [`Interned`]
    fn append(&mut self) -> Interned {
        let idx: u32 = self.offsets.len().try_into().unwrap();
        let key = Interned(NonZeroU32::new(idx).unwrap());
        self.offsets.push(self.values.len());
        key
    }

    /// Returns the byte size of the associated buffers
    fn buffer_size(&self) -> usize {
        self.values.capacity() + self.offsets.capacity() * std::mem::size_of::<usize>()
    }
}

impl Index<Interned> for InternBuffer {
    type Output = [u8];

    fn index(&self, key: Interned) -> &Self::Output {
        let index = key.0.get() as usize;
        let end = self.offsets[index];
        let start = self.offsets[index - 1];
        // SAFETY:
        // self.values is never reduced in size and values appended
        // to self.offsets are always less than self.values at the time
        unsafe { self.values.get_unchecked(start..end) }
    }
}

/// A slot corresponds to a single byte-value in the generated normalized key
///
/// It may contain a value, if not the first slot, and may contain a child [`Bucket`] representing
/// the next byte in the generated normalized key
#[derive(Debug, Clone)]
struct Slot {
    value: Interned,
    /// Child values less than `self.value` if any
    child: Option<Box<Bucket>>,
}

/// Bucket is the root of the data-structure used to allocate normalized keys
///
/// In particular it needs to generate keys that
///
/// * Contain no `0` bytes other than the null terminator
/// * Compare lexicographically in the same manner as the encoded `data`
///
/// The data structure consists of 254 slots, each of which can store a value.
/// Additionally each slot may contain a child bucket, containing values smaller
/// than the value within the slot.
///
/// Each bucket also may contain a child bucket, containing values greater than
/// all values in the current bucket
///
/// # Allocation Strategy
///
/// The contiguous slice of slots containing values is searched to find the insertion
/// point for the new value, according to the sort order.
///
/// If the insertion position exceeds 254, the number of slots, the value is inserted
/// into the child bucket of the current bucket.
///
/// If the insertion position already contains a value, the value is inserted into the
/// child bucket of that slot.
///
/// If the slot is not occupied, the value is inserted into that slot.
///
/// The final key consists of the slot indexes visited incremented by 1,
/// with the final value incremented by 2, followed by a null terminator.
///
/// Consider the case of the integers `[8, 6, 5, 7]` inserted in that order
///
/// ```ignore
/// 8: &[2, 0]
/// 6: &[1, 2, 0]
/// 5: &[1, 1, 2, 0]
/// 7: &[1, 3, 0]
/// ```
///
/// Note: this allocation strategy is optimised for interning values in sorted order
///
#[derive(Debug, Clone)]
struct Bucket {
    slots: Vec<Slot>,
    /// Bucket containing values larger than all of `slots`
    next: Option<Box<Bucket>>,
}

impl Default for Bucket {
    fn default() -> Self {
        Self {
            slots: Vec::with_capacity(254),
            next: None,
        }
    }
}

impl Bucket {
    /// Insert `data` into this bucket or one of its children, appending the
    /// normalized key to `out` as it is constructed
    ///
    /// # Panics
    ///
    /// Panics if the value already exists
    fn insert(&mut self, values_buf: &mut InternBuffer, data: &[u8], out: &mut Vec<u8>) {
        let slots_len = self.slots.len() as u8;
        // We optimise the case of inserting a value directly after those already inserted
        // as [`OrderPreservingInterner::intern`] sorts values prior to interning them
        match self.slots.last() {
            Some(slot) => {
                if &values_buf[slot.value] < data {
                    if slots_len == 254 {
                        out.push(255);
                        self.next
                            .get_or_insert_with(Default::default)
                            .insert(values_buf, data, out)
                    } else {
                        out.push(slots_len + 2);
                        let value = values_buf.insert(data);
                        self.slots.push(Slot { value, child: None });
                    }
                } else {
                    // Find insertion point
                    match self
                        .slots
                        .binary_search_by(|slot| values_buf[slot.value].cmp(data))
                    {
                        Ok(_) => unreachable!("value already exists"),
                        Err(idx) => {
                            out.push(idx as u8 + 1);
                            self.slots[idx]
                                .child
                                .get_or_insert_with(Default::default)
                                .insert(values_buf, data, out)
                        }
                    }
                }
            }
            None => {
                out.push(2);
                let value = values_buf.insert(data);
                self.slots.push(Slot { value, child: None })
            }
        }
    }

    /// Returns the size of this instance in bytes
    fn size(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.slots.capacity() * std::mem::size_of::<Slot>()
        // and account for the size of any embedded buckets in the slots
            + self.slot_child_bucket_size()
            + self.next.as_ref().map(|x| x.size()).unwrap_or_default()
    }

    /// returns the total size of any recursively allocated `Bucket`s
    /// in self.slots. This does not include the size of the child Slot itself
    fn slot_child_bucket_size(&self) -> usize {
        self.slots
            .iter()
            .map(|slot| slot.child.as_ref().map(|x| x.size()).unwrap_or_default())
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::prelude::*;

    // Clippy isn't smart enough to understand dropping mutability
    #[allow(clippy::needless_collect)]
    fn test_intern_values(values: &[u64]) {
        let mut interner = OrderPreservingInterner::default();

        // Intern a single value at a time to check ordering
        let interned: Vec<_> = values
            .iter()
            .flat_map(|v| interner.intern([Some(&v.to_be_bytes())]))
            .map(Option::unwrap)
            .collect();

        for (value, interned) in values.iter().zip(&interned) {
            assert_eq!(interner.value(*interned), &value.to_be_bytes());
        }

        let normalized_keys: Vec<_> = interned
            .iter()
            .map(|x| interner.normalized_key(*x))
            .collect();

        for (interned, normalized) in interned.iter().zip(&normalized_keys) {
            assert_eq!(*interned, interner.lookup(normalized).unwrap());
        }

        for (i, a) in normalized_keys.iter().enumerate() {
            for (j, b) in normalized_keys.iter().enumerate() {
                let interned_cmp = a.cmp(b);
                let values_cmp = values[i].cmp(&values[j]);
                assert_eq!(
                    interned_cmp, values_cmp,
                    "({:?} vs {:?}) vs ({} vs {})",
                    a, b, values[i], values[j]
                )
            }
        }
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn test_interner() {
        test_intern_values(&[8, 6, 5, 7]);

        let mut values: Vec<_> = (0_u64..2000).collect();
        test_intern_values(&values);

        let mut rng = thread_rng();
        values.shuffle(&mut rng);
        test_intern_values(&values);
    }

    #[test]
    fn test_intern_duplicates() {
        // Unsorted with duplicates
        let values = [0_u8, 1, 8, 4, 1, 0];
        let mut interner = OrderPreservingInterner::default();

        let interned = interner.intern(values.iter().map(std::slice::from_ref).map(Some));
        let interned: Vec<_> = interned.into_iter().map(Option::unwrap).collect();

        assert_eq!(interned[0], interned[5]);
        assert_eq!(interned[1], interned[4]);
        assert!(
            interner.normalized_key(interned[0]) < interner.normalized_key(interned[1])
        );
        assert!(
            interner.normalized_key(interned[1]) < interner.normalized_key(interned[2])
        );
        assert!(
            interner.normalized_key(interned[1]) < interner.normalized_key(interned[3])
        );
        assert!(
            interner.normalized_key(interned[3]) < interner.normalized_key(interned[2])
        );
    }

    #[test]
    fn test_intern_sizes() {
        let mut interner = OrderPreservingInterner::default();

        // Intern a 1K values each 8 bytes large
        let num_items = 1000;
        let mut values: Vec<usize> = (0..num_items).collect();
        values.reverse();

        // intern these values 1 at a time (otherwise the interner
        // will sort them first);
        for v in values {
            interner.intern([Some(v.to_be_bytes())]);
        }

        let reported = interner.size();

        // Figure out the expected size (this is a second
        // implementation of size()) as a double check
        let min_expected = BucketWalker::new()
            .visit_bucket(interner.bucket.as_ref())
            .memory_estimate()
            // hash table  size
            + interner.lookup.capacity() *  std::mem::size_of::<Interned>()
            // key/value storage
            + interner.keys.buffer_size()
            + interner.values.buffer_size();

        assert!(
            reported > min_expected,
            "reported size {reported} not larger than min expected size: {min_expected}"
        )
    }

    // Walks over the buckets / slots counting counting them all
    struct BucketWalker {
        num_buckets: usize,
        num_slots: usize,
    }

    impl BucketWalker {
        fn new() -> Self {
            Self {
                num_buckets: 0,
                num_slots: 0,
            }
        }

        // recursively visit the bucket and any slots/buckets contained
        fn visit_bucket(mut self, bucket: &Bucket) -> Self {
            self.num_buckets += 1;
            let acc = bucket
                .slots
                .iter()
                .fold(self, |acc, slot| acc.visit_slot(slot));

            if let Some(next) = bucket.next.as_ref() {
                acc.visit_bucket(next.as_ref())
            } else {
                acc
            }
        }

        // recursively visit slot and any slots/buckets
        fn visit_slot(mut self, slot: &Slot) -> Self {
            self.num_slots += 1;
            if let Some(child) = slot.child.as_ref() {
                self.visit_bucket(child.as_ref())
            } else {
                self
            }
        }

        // estimate how much memory is used just for Buckets / Slots
        // (an underestimate of the total memory used for the
        // interner as it doesn't contain any actual values)
        fn memory_estimate(self) -> usize {
            self.num_buckets * std::mem::size_of::<Bucket>()
                + self.num_slots * std::mem::size_of::<Slot>()
        }
    }
}
