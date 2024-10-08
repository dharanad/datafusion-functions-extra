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

//! [`ArrowBytesViewMap`] and [`ArrowBytesViewSet`] for storing maps/sets of values from
//! `StringViewArray`/`BinaryViewArray`.
//! Much of the code is from `binary_map.rs`, but with simpler implementation because we directly use the
//! [`GenericByteViewBuilder`].
use ahash::RandomState;
use arrow::array::cast::AsArray;
use arrow::array::{Array, ArrayBuilder, ArrayRef, GenericByteViewBuilder};
use arrow::datatypes::{BinaryViewType, ByteViewType, DataType, StringViewType};
use datafusion::arrow;
use datafusion::common::hash_utils::create_hashes;
use datafusion::common::utils::proxy::{RawTableAllocExt, VecAllocExt};
use datafusion::physical_expr::binary_map::OutputType;
use std::fmt::Debug;
use std::sync::Arc;

/// Optimized map for storing Arrow "byte view" types (`StringView`, `BinaryView`)
/// values that can produce the set of keys on
/// output as `GenericBinaryViewArray` without copies.
///
/// Equivalent to `HashSet<String, V>` but with better performance for arrow
/// data.
///
/// # Generic Arguments
///
/// * `V`: payload type
///
/// # Description
///
/// This is a specialized HashMap with the following properties:
///
/// 1. Optimized for storing and emitting Arrow byte types  (e.g.
///    `StringViewArray` / `BinaryViewArray`) very efficiently by minimizing copying of
///    the string values themselves, both when inserting and when emitting the
///    final array.
///
/// 2. Retains the insertion order of entries in the final array. The values are
///    in the same order as they were inserted.
///
/// Note this structure can be used as a `HashSet` by specifying the value type
/// as `()`, as is done by [`ArrowBytesViewSet`].
///
/// This map is used by the special `COUNT DISTINCT` aggregate function to
/// store the distinct values, and by the `GROUP BY` operator to store
/// group values when they are a single string array.

// TODO: Remove after DataFusion next release once insert_or_update and get_payloads are added to the collection.
// Copied from datafusion/physical-expr-common/binary_view_map.rs.
pub struct ArrowBytesViewMap<V>
where
    V: Debug + PartialEq + Eq + Clone + Copy + Default,
{
    /// Should the output be StringView or BinaryView?
    output_type: OutputType,
    /// Underlying hash set for each distinct value
    map: hashbrown::raw::RawTable<Entry<V>>,
    /// Total size of the map in bytes
    map_size: usize,

    /// Builder for output array
    builder: GenericByteViewBuilder<BinaryViewType>,
    /// random state used to generate hashes
    random_state: RandomState,
    /// buffer that stores hash values (reused across batches to save allocations)
    hashes_buffer: Vec<u64>,
    /// `(payload, null_index)` for the 'null' value, if any
    /// NOTE null_index is the logical index in the final array, not the index
    /// in the buffer
    null: Option<(V, usize)>,
}

/// The size, in number of entries, of the initial hash table
const INITIAL_MAP_CAPACITY: usize = 512;

impl<V> ArrowBytesViewMap<V>
where
    V: Debug + PartialEq + Eq + Clone + Copy + Default,
{
    pub fn new(output_type: OutputType) -> Self {
        Self {
            output_type,
            map: hashbrown::raw::RawTable::with_capacity(INITIAL_MAP_CAPACITY),
            map_size: 0,
            builder: GenericByteViewBuilder::new(),
            random_state: RandomState::new(),
            hashes_buffer: vec![],
            null: None,
        }
    }

    /// Return the contents of this map and replace it with a new empty map with
    /// the same output type
    pub fn take(&mut self) -> Self {
        let mut new_self = Self::new(self.output_type);
        std::mem::swap(self, &mut new_self);
        new_self
    }

    /// Inserts each value from `values` into the map, invoking `payload_fn` for
    /// each value if *not* already present, deferring the allocation of the
    /// payload until it is needed.
    ///
    /// Note that this is different than a normal map that would replace the
    /// existing entry
    ///
    /// # Arguments:
    ///
    /// `values`: array whose values are inserted
    ///
    /// `make_payload_fn`:  invoked for each value that is not already present
    /// to create the payload, in order of the values in `values`
    ///
    /// `observe_payload_fn`: invoked once, for each value in `values`, that was
    /// already present in the map, with corresponding payload value.
    ///
    /// # Returns
    ///
    /// The payload value for the entry, either the existing value or
    /// the newly inserted value
    ///
    /// # Safety:
    ///
    /// Note that `make_payload_fn` and `observe_payload_fn` are only invoked
    /// with valid values from `values`, not for the `NULL` value.
    pub fn insert_if_new<MP, OP>(&mut self, values: &ArrayRef, make_payload_fn: MP, observe_payload_fn: OP)
    where
        MP: FnMut(Option<&[u8]>) -> V,
        OP: FnMut(V),
    {
        // Sanity check array type
        match self.output_type {
            OutputType::BinaryView => {
                assert!(matches!(values.data_type(), DataType::BinaryView));
                self.insert_if_new_inner::<MP, OP, BinaryViewType>(values, make_payload_fn, observe_payload_fn)
            }
            OutputType::Utf8View => {
                assert!(matches!(values.data_type(), DataType::Utf8View));
                self.insert_if_new_inner::<MP, OP, StringViewType>(values, make_payload_fn, observe_payload_fn)
            }
            _ => unreachable!("Utf8/Binary should use `ArrowBytesSet`"),
        };
    }

    /// Generic version of [`Self::insert_if_new`] that handles `ByteViewType`
    /// (both StringView and BinaryView)
    ///
    /// Note this is the only function that is generic on [`ByteViewType`], which
    /// avoids having to template the entire structure,  making the code
    /// simpler and understand and reducing code bloat due to duplication.
    ///
    /// See comments on `insert_if_new` for more details
    fn insert_if_new_inner<MP, OP, B>(&mut self, values: &ArrayRef, mut make_payload_fn: MP, mut observe_payload_fn: OP)
    where
        MP: FnMut(Option<&[u8]>) -> V,
        OP: FnMut(V),
        B: ByteViewType,
    {
        // step 1: compute hashes
        let batch_hashes = &mut self.hashes_buffer;
        batch_hashes.clear();
        batch_hashes.resize(values.len(), 0);
        create_hashes(&[values.clone()], &self.random_state, batch_hashes)
            // hash is supported for all types and create_hashes only
            // returns errors for unsupported types
            .unwrap();

        // step 2: insert each value into the set, if not already present
        let values = values.as_byte_view::<B>();

        // Ensure lengths are equivalent
        assert_eq!(values.len(), batch_hashes.len());

        for (value, &hash) in values.iter().zip(batch_hashes.iter()) {
            // handle null value
            let Some(value) = value else {
                let payload = if let Some(&(payload, _offset)) = self.null.as_ref() {
                    payload
                } else {
                    let payload = make_payload_fn(None);
                    let null_index = self.builder.len();
                    self.builder.append_null();
                    self.null = Some((payload, null_index));
                    payload
                };
                observe_payload_fn(payload);
                continue;
            };

            // get the value as bytes
            let value: &[u8] = value.as_ref();

            let entry = self.map.get_mut(hash, |header| {
                let v = self.builder.get_value(header.view_idx);

                if v.len() != value.len() {
                    return false;
                }

                v == value
            });

            let payload = if let Some(entry) = entry {
                entry.payload
            } else {
                // no existing value, make a new one.
                let payload = make_payload_fn(Some(value));

                let inner_view_idx = self.builder.len();
                let new_header = Entry {
                    view_idx: inner_view_idx,
                    hash,
                    payload,
                };

                self.builder.append_value(value);

                self.map.insert_accounted(new_header, |h| h.hash, &mut self.map_size);
                payload
            };
            observe_payload_fn(payload);
        }
    }

    /// Inserts each value from `values` into the map, invoking `make_payload_fn` for
    /// each value if not already present, or `update_payload_fn` if the value already exists.
    ///
    /// This function handles both the insert and update cases.
    ///
    /// # Arguments:
    ///
    /// `values`: The array whose values are inserted or updated in the map.
    ///
    /// `make_payload_fn`: Invoked for each value that is not already present
    /// to create the payload, in the order of the values in `values`.
    ///
    /// `update_payload_fn`: Invoked for each value that is already present,
    /// allowing the payload to be updated in-place.
    ///
    /// # Safety:
    ///
    /// Note that `make_payload_fn` and `update_payload_fn` are only invoked
    /// with valid values from `values`, not for the `NULL` value.
    pub fn insert_or_update<MP, UP>(&mut self, values: &ArrayRef, make_payload_fn: MP, update_payload_fn: UP)
    where
        MP: FnMut(Option<&[u8]>) -> V,
        UP: FnMut(&mut V),
    {
        // Check the output type and dispatch to the appropriate internal function
        match self.output_type {
            OutputType::BinaryView => {
                assert!(matches!(values.data_type(), DataType::BinaryView));
                self.insert_or_update_inner::<MP, UP, BinaryViewType>(values, make_payload_fn, update_payload_fn)
            }
            OutputType::Utf8View => {
                assert!(matches!(values.data_type(), DataType::Utf8View));
                self.insert_or_update_inner::<MP, UP, StringViewType>(values, make_payload_fn, update_payload_fn)
            }
            _ => unreachable!("Utf8/Binary should use `ArrowBytesMap`"),
        };
    }

    /// Generic version of [`Self::insert_or_update`] that handles `ByteViewType`
    /// (both StringView and BinaryView).
    ///
    /// This is the only function that is generic on [`ByteViewType`], which avoids having
    /// to template the entire structure, simplifying the code and reducing code bloat due
    /// to duplication.
    ///
    /// See comments on `insert_or_update` for more details.
    fn insert_or_update_inner<MP, UP, B>(
        &mut self,
        values: &ArrayRef,
        mut make_payload_fn: MP,
        mut update_payload_fn: UP,
    ) where
        MP: FnMut(Option<&[u8]>) -> V,
        UP: FnMut(&mut V),
        B: ByteViewType,
    {
        // step 1: compute hashes
        let batch_hashes = &mut self.hashes_buffer;
        batch_hashes.clear();
        batch_hashes.resize(values.len(), 0);
        create_hashes(&[values.clone()], &self.random_state, batch_hashes)
            // hash is supported for all types and create_hashes only
            // returns errors for unsupported types
            .unwrap();

        // step 2: insert each value into the set, if not already present
        let values = values.as_byte_view::<B>();

        // Ensure lengths are equivalent
        assert_eq!(values.len(), batch_hashes.len());

        for (value, &hash) in values.iter().zip(batch_hashes.iter()) {
            // Handle null value
            let Some(value) = value else {
                if let Some((ref mut payload, _)) = self.null {
                    update_payload_fn(payload);
                } else {
                    let payload = make_payload_fn(None);
                    let null_index = self.builder.len();
                    self.builder.append_null();
                    self.null = Some((payload, null_index));
                }
                continue;
            };

            let value: &[u8] = value.as_ref();

            let entry = self.map.get_mut(hash, |header| {
                let v = self.builder.get_value(header.view_idx);

                if v.len() != value.len() {
                    return false;
                }

                v == value
            });

            if let Some(entry) = entry {
                update_payload_fn(&mut entry.payload);
            } else {
                // no existing value, make a new one.
                let payload = make_payload_fn(Some(value));

                let inner_view_idx = self.builder.len();
                let new_header = Entry {
                    view_idx: inner_view_idx,
                    hash,
                    payload,
                };

                self.builder.append_value(value);

                self.map.insert_accounted(new_header, |h| h.hash, &mut self.map_size);
            };
        }
    }

    /// Generic version of [`Self::get_payloads`] that handles `ByteViewType`
    /// (both StringView and BinaryView).
    ///
    /// This function computes the hashes for each value and retrieves the payloads
    /// stored in the map, leveraging small value optimizations when possible.
    ///
    /// # Arguments:
    ///
    /// `values`: The array whose payloads are being retrieved.
    ///
    /// # Returns
    ///
    /// A vector of payloads for each value, or `None` if the value is not found.
    ///
    /// # Safety:
    ///
    /// This function ensures that small values are handled using inline optimization
    /// and larger values are safely retrieved from the builder.
    fn get_payloads_inner<B>(self, values: &ArrayRef) -> Vec<Option<V>>
    where
        B: ByteViewType,
    {
        // Step 1: Compute hashes
        let mut batch_hashes = vec![0u64; values.len()];
        create_hashes(&[values.clone()], &self.random_state, &mut batch_hashes).unwrap(); // Compute the hashes for the values

        // Step 2: Get payloads for each value
        let values = values.as_byte_view::<B>();
        assert_eq!(values.len(), batch_hashes.len()); // Ensure hash count matches value count

        let mut payloads = Vec::with_capacity(values.len());

        for (value, &hash) in values.iter().zip(batch_hashes.iter()) {
            // Handle null value
            let Some(value) = value else {
                if let Some(&(payload, _)) = self.null.as_ref() {
                    payloads.push(Some(payload));
                } else {
                    payloads.push(None);
                }
                continue;
            };

            let value: &[u8] = value.as_ref();

            let entry = self.map.get(hash, |header| {
                let v = self.builder.get_value(header.view_idx);
                v.len() == value.len() && v == value
            });

            let payload = entry.map(|e| e.payload);
            payloads.push(payload);
        }

        payloads
    }

    /// Retrieves the payloads for each value from `values`, either by using
    /// small value optimizations or larger value handling.
    ///
    /// This function will compute hashes for each value and attempt to retrieve
    /// the corresponding payload from the map. If the value is not found, it will return `None`.
    ///
    /// # Arguments:
    ///
    /// `values`: The array whose payloads need to be retrieved.
    ///
    /// # Returns
    ///
    /// A vector of payloads for each value, or `None` if the value is not found.
    pub fn get_payloads(self, values: &ArrayRef) -> Vec<Option<V>> {
        match self.output_type {
            OutputType::BinaryView => {
                assert!(matches!(values.data_type(), DataType::BinaryView));
                self.get_payloads_inner::<BinaryViewType>(values)
            }
            OutputType::Utf8View => {
                assert!(matches!(values.data_type(), DataType::Utf8View));
                self.get_payloads_inner::<StringViewType>(values)
            }
            _ => unreachable!("Utf8/Binary should use `ArrowBytesMap`"),
        }
    }

    /// Converts this set into a `StringViewArray`, or `BinaryViewArray`,
    /// containing each distinct value
    /// that was inserted. This is done without copying the values.
    ///
    /// The values are guaranteed to be returned in the same order in which
    /// they were first seen.
    pub fn into_state(self) -> ArrayRef {
        let mut builder = self.builder;
        match self.output_type {
            OutputType::BinaryView => {
                let array = builder.finish();

                Arc::new(array)
            }
            OutputType::Utf8View => {
                // SAFETY:
                // we asserted the input arrays were all the correct type and
                // thus since all the values that went in were valid (e.g. utf8)
                // so are all the values that come out
                let array = builder.finish();
                let array = unsafe { array.to_string_view_unchecked() };
                Arc::new(array)
            }
            _ => {
                unreachable!("Utf8/Binary should use `ArrowBytesMap`")
            }
        }
    }

    /// Total number of entries (including null, if present)
    pub fn len(&self) -> usize {
        self.non_null_len() + self.null.map(|_| 1).unwrap_or(0)
    }

    /// Is the set empty?
    pub fn is_empty(&self) -> bool {
        self.map.is_empty() && self.null.is_none()
    }

    /// Number of non null entries
    pub fn non_null_len(&self) -> usize {
        self.map.len()
    }

    /// Return the total size, in bytes, of memory used to store the data in
    /// this set, not including `self`
    pub fn size(&self) -> usize {
        self.map_size + self.builder.allocated_size() + self.hashes_buffer.allocated_size()
    }
}

impl<V> Debug for ArrowBytesViewMap<V>
where
    V: Debug + PartialEq + Eq + Clone + Copy + Default,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ArrowBytesMap")
            .field("map", &"<map>")
            .field("map_size", &self.map_size)
            .field("view_builder", &self.builder)
            .field("random_state", &self.random_state)
            .field("hashes_buffer", &self.hashes_buffer)
            .finish()
    }
}

/// Entry in the hash table -- see [`ArrowBytesViewMap`] for more details
#[derive(Debug, PartialEq, Eq, Hash, Clone, Copy)]
struct Entry<V>
where
    V: Debug + PartialEq + Eq + Clone + Copy + Default,
{
    /// The idx into the views array
    view_idx: usize,

    hash: u64,

    /// value stored by the entry
    payload: V,
}

#[cfg(test)]
mod tests {
    use arrow::array::{GenericByteViewArray, StringViewArray};
    use hashbrown::HashMap;

    use super::*;

    #[test]
    fn test_insert_or_update_count_u8() {
        let values = GenericByteViewArray::from(vec![
            Some("a"),
            Some("✨🔥✨🔥✨🔥✨🔥✨🔥✨🔥✨🔥✨🔥"),
            Some("🔥"),
            Some("✨✨✨"),
            Some("foobarbaz"),
            Some("🔥"),
            Some("✨🔥✨🔥✨🔥✨🔥✨🔥✨🔥✨🔥✨🔥"),
        ]);

        let mut map: ArrowBytesViewMap<u8> = ArrowBytesViewMap::new(OutputType::Utf8View);
        let arr: ArrayRef = Arc::new(values);

        map.insert_or_update(
            &arr,
            |_| 1u8,
            |count| {
                *count += 1;
            },
        );

        let expected_counts = [
            ("a", 1),
            ("✨🔥✨🔥✨🔥✨🔥✨🔥✨🔥✨🔥✨🔥", 2),
            ("🔥", 2),
            ("✨✨✨", 1),
            ("foobarbaz", 1),
        ];

        for value in expected_counts.iter() {
            let string_array = GenericByteViewArray::from(vec![Some(value.0)]);
            let arr: ArrayRef = Arc::new(string_array);

            let mut result_payload: Option<u8> = None;

            map.insert_or_update(
                &arr,
                |_| {
                    panic!("Unexpected new entry during verification");
                },
                |count| {
                    result_payload = Some(*count);
                },
            );

            assert_eq!(result_payload.unwrap(), value.1);
        }
    }

    #[test]
    fn test_insert_if_new_after_insert_or_update() {
        let initial_values = GenericByteViewArray::from(vec![Some("A"), Some("B"), Some("B"), Some("C"), Some("C")]);

        let mut map: ArrowBytesViewMap<u8> = ArrowBytesViewMap::new(OutputType::Utf8View);
        let arr: ArrayRef = Arc::new(initial_values);

        map.insert_or_update(
            &arr,
            |_| 1u8,
            |count| {
                *count += 1;
            },
        );

        let additional_values = GenericByteViewArray::from(vec![Some("A"), Some("D"), Some("E")]);
        let arr_additional: ArrayRef = Arc::new(additional_values);

        map.insert_if_new(&arr_additional, |_| 5u8, |_| {});

        let expected_payloads = [Some(1u8), Some(2u8), Some(2u8), Some(5u8), Some(5u8)];

        let combined_arr = GenericByteViewArray::from(vec![Some("A"), Some("B"), Some("C"), Some("D"), Some("E")]);

        let arr_combined: ArrayRef = Arc::new(combined_arr);
        let payloads = map.get_payloads(&arr_combined);

        assert_eq!(payloads, expected_payloads);
    }

    #[test]
    fn test_get_payloads_u8() {
        let values = GenericByteViewArray::from(vec![
            Some("A"),
            Some("bcdefghijklmnop"),
            Some("X"),
            Some("Y"),
            None,
            Some("qrstuvqxyzhjwya"),
            Some("✨🔥"),
            Some("🔥"),
            Some("🔥🔥🔥🔥🔥🔥"),
            Some("A"), // Duplicate to test the count increment
            Some("Y"), // Another duplicate to test the count increment
        ]);

        let mut map: ArrowBytesViewMap<u8> = ArrowBytesViewMap::new(OutputType::Utf8View);
        let arr: ArrayRef = Arc::new(values);

        map.insert_or_update(
            &arr,
            |_| 1u8,
            |count| {
                *count += 1;
            },
        );

        let expected_payloads = [
            Some(2u8),
            Some(1u8),
            Some(1u8),
            Some(2u8),
            Some(1u8),
            Some(1u8),
            Some(1u8),
            Some(1u8),
            Some(1u8),
            Some(2u8),
            Some(2u8),
        ];

        let payloads = map.get_payloads(&arr);

        assert_eq!(payloads.len(), expected_payloads.len());

        for (i, payload) in payloads.iter().enumerate() {
            assert_eq!(*payload, expected_payloads[i]);
        }
    }

    #[derive(Debug, PartialEq, Eq, Default, Clone, Copy)]
    struct TestPayload {
        // store the string value to check against input
        index: usize, // store the index of the string (each new string gets the next sequential input)
    }

    /// Wraps an [`ArrowBytesViewMap`], validating its invariants
    struct TestMap {
        map: ArrowBytesViewMap<TestPayload>,
        // stores distinct strings seen, in order
        strings: Vec<Option<String>>,
        // map strings to index in strings
        indexes: HashMap<Option<String>, usize>,
    }

    impl TestMap {
        /// creates a map with TestPayloads for the given strings and then
        /// validates the payloads
        fn new() -> Self {
            Self {
                map: ArrowBytesViewMap::new(OutputType::Utf8View),
                strings: vec![],
                indexes: HashMap::new(),
            }
        }

        /// Inserts strings into the map
        fn insert(&mut self, strings: &[Option<&str>]) {
            let string_array = StringViewArray::from(strings.to_vec());
            let arr: ArrayRef = Arc::new(string_array);

            let mut next_index = self.indexes.len();
            let mut actual_new_strings = vec![];
            let mut actual_seen_indexes = vec![];
            // update self with new values, keeping track of newly added values
            for str in strings {
                let str = str.map(|s| s.to_string());
                let index = self.indexes.get(&str).cloned().unwrap_or_else(|| {
                    actual_new_strings.push(str.clone());
                    let index = self.strings.len();
                    self.strings.push(str.clone());
                    self.indexes.insert(str, index);
                    index
                });
                actual_seen_indexes.push(index);
            }

            // insert the values into the map, recording what we did
            let mut seen_new_strings = vec![];
            let mut seen_indexes = vec![];
            self.map.insert_if_new(
                &arr,
                |s| {
                    let value = s.map(|s| String::from_utf8(s.to_vec()).expect("Non utf8 string"));
                    let index = next_index;
                    next_index += 1;
                    seen_new_strings.push(value);
                    TestPayload { index }
                },
                |payload| {
                    seen_indexes.push(payload.index);
                },
            );

            assert_eq!(actual_seen_indexes, seen_indexes);
            assert_eq!(actual_new_strings, seen_new_strings);
        }

        /// Call `self.map.into_array()` validating that the strings are in the same
        /// order as they were inserted
        fn into_array(self) -> ArrayRef {
            let Self {
                map,
                strings,
                indexes: _,
            } = self;

            let arr = map.into_state();
            let expected: ArrayRef = Arc::new(StringViewArray::from(strings));
            assert_eq!(&arr, &expected);
            arr
        }
    }

    #[test]
    fn test_map() {
        let input = vec![
            // Note mix of short/long strings
            Some("A"),
            Some("bcdefghijklmnop1234567"),
            Some("X"),
            Some("Y"),
            None,
            Some("qrstuvqxyzhjwya"),
            Some("✨🔥"),
            Some("🔥"),
            Some("🔥🔥🔥🔥🔥🔥"),
        ];

        let mut test_map = TestMap::new();
        test_map.insert(&input);
        test_map.insert(&input); // put it in twice
        let expected_output: ArrayRef = Arc::new(StringViewArray::from(input));
        assert_eq!(&test_map.into_array(), &expected_output);
    }
}
