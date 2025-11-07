//! Provides structures and methods for handling aggregated note metadata.
//!
//! This module contains the API for reading and building note metadata blobs.
//! Metadata typically includes aggregated data like reaction counts, reply counts,
//! and reposts, which are derived from other events.
//!
//! ## Reading Metadata
//!
//! The primary way to read metadata is to get a [`NoteMetadata`] object
//! from [`Ndb::get_note_metadata`] and then iterate over it.
//!
//! ```no_run
//! # use nostrdb::{Ndb, Transaction, Error, NoteMetadataEntryVariant};
//! # let ndb: Ndb = todo!();
//! # let txn: Transaction = todo!();
//! # let note_id: [u8; 32] = [0; 32];
//! // Get the metadata for a note
//! let metadata = ndb.get_note_metadata(&txn, &note_id).unwrap();
//!
//! // Iterate over the metadata entries
//! for entry in metadata {
//!     match entry {
//!         NoteMetadataEntryVariant::Counts(counts) => {
//!             println!("Total Reactions: {}", counts.reactions());
//!             println!("Thread Replies: {}", counts.thread_replies());
//!         }
//!         NoteMetadataEntryVariant::Reaction(reaction) => {
//!             let mut buf = [0i8; 128];
//!             println!(
//!                 "Reaction: {} (Count: {})",
//!                 reaction.as_str(&mut buf),
//!                 reaction.count()
//!             );
//!         }
//!         NoteMetadataEntryVariant::Unknown(_) => {
//!             // Handle unknown entry types
//!         }
//!     }
//! }
//! ```
//!
//! ## Building Metadata
//!
//! To create a new metadata blob, you can use the [`NoteMetadataBuilder`].
//!
//! ```no_run
//! # use nostrdb::{NoteMetadataBuilder, NoteMetadataEntryBuf, Counts};
//! // Create a "counts" entry
//! let counts_data = Counts {
//!     total_reactions: 10,
//!     thread_replies: 5,
//!     quotes: 2,
//!     direct_replies: 3,
//!     reposts: 1,
//! };
//! let mut counts_entry = NoteMetadataEntryBuf::counts(&counts_data);
//!
//! // Build the metadata blob
//! let mut builder = NoteMetadataBuilder::new();
//! builder.add_entry(counts_entry.borrow());
//! let metadata_buf = builder.build();
//!
//! // The resulting `metadata_buf.buf` (a Vec<u8>) can now be stored.

use crate::bindings;

/// A borrowed reference to a note's aggregated metadata.
///
/// This structure provides read-only access to metadata entries, such as
/// reaction counts, reply counts, etc. It is obtained via
/// [`Ndb::get_note_metadata`].
///
/// The primary way to use this is by iterating over it, which yields
/// [`NoteMetadataEntryVariant`] items.
pub struct NoteMetadata<'a> {
    /// Borrowed, exclusive mutable reference
    ptr: &'a bindings::ndb_note_meta,
}

/// A borrowed reference to a single metadata entry.
///
/// This is a generic wrapper. It's typically consumed by calling
/// [`.variant()`](Self::variant) to get a specific type, like [`CountsEntry`] or
/// [`ReactionEntry`].
pub struct NoteMetadataEntry<'a> {
    entry: &'a bindings::ndb_note_meta_entry,
}

/// A metadata entry representing aggregated counts for a note.
pub struct CountsEntry<'a> {
    entry: NoteMetadataEntry<'a>,
}

/// A metadata entry representing a specific reaction and its count
/// (e.g., "❤️" - 5 times).
pub struct ReactionEntry<'a> {
    entry: NoteMetadataEntry<'a>,
}

impl<'a> ReactionEntry<'a> {
    pub(crate) fn new(entry: NoteMetadataEntry<'a>) -> Self {
        Self { entry }
    }

    pub fn as_ptr(&self) -> *mut bindings::ndb_note_meta_entry {
        self.entry.as_ptr()
    }

    /// The number of times this specific reaction was seen.
    pub fn count(&self) -> u32 {
        unsafe { *bindings::ndb_note_meta_reaction_count(self.as_ptr()) }
    }

    /// Gets the string content of the reaction (e.g., "❤️" or "+").
    ///
    /// Note: This function requires a temporary buffer to write the emoji into.
    pub fn as_str(&'a self, buf: &'a mut [i8; 128]) -> &'a str {
        unsafe {
            let rstr = bindings::ndb_note_meta_reaction_str(self.as_ptr());
            // weird android compilation issue
            #[cfg(target_os = "android")]
            let ptr = {
                bindings::ndb_reaction_to_str(rstr, buf.as_mut_ptr() as *mut u8)
            };
            #[cfg(not(target_os = "android"))]
            let ptr = {
                bindings::ndb_reaction_to_str(rstr, buf.as_mut_ptr())
            };
            let byte_slice: &[u8] = std::slice::from_raw_parts(ptr as *mut u8, libc::strlen(ptr));
            std::str::from_utf8_unchecked(byte_slice)
        }
    }
}

impl<'a> CountsEntry<'a> {
    pub(crate) fn new(entry: NoteMetadataEntry<'a>) -> Self {
        Self { entry }
    }

    pub fn as_ptr(&self) -> *mut bindings::ndb_note_meta_entry {
        self.entry.as_ptr()
    }

    /// Total number of replies in the thread (recursive).
    pub fn thread_replies(&self) -> u32 {
        unsafe { *bindings::ndb_note_meta_counts_thread_replies(self.as_ptr()) }
    }

    /// Number of direct replies to the note.
    pub fn direct_replies(&self) -> u16 {
        unsafe { *bindings::ndb_note_meta_counts_direct_replies(self.as_ptr()) }
    }

    /// Number of quotes (reposts with content).
    pub fn quotes(&self) -> u16 {
        unsafe { *bindings::ndb_note_meta_counts_quotes(self.as_ptr()) }
    }

    /// Number of simple reposts (kind 6/16).
    pub fn reposts(&self) -> u16 {
        unsafe { *bindings::ndb_note_meta_counts_reposts(self.as_ptr()) }
    }

    /// Total number of reactions (e.g., kind 7) of all types.
    pub fn reactions(&self) -> u32 {
        unsafe { *bindings::ndb_note_meta_counts_total_reactions(self.as_ptr()) }
    }
}

/// An enumeration of the different types of note metadata entries.
///
/// This is the item yielded when iterating over [`NoteMetadata`].
pub enum NoteMetadataEntryVariant<'a> {
    /// Aggregated counts (replies, reposts, reactions).
    Counts(CountsEntry<'a>),

    /// A specific reaction (e.g., "❤️") and its count.
    Reaction(ReactionEntry<'a>),

    /// An entry of an unknown or unsupported type.
    Unknown(NoteMetadataEntry<'a>),
}

impl<'a> NoteMetadataEntryVariant<'a> {
    pub fn new(entry: NoteMetadataEntry<'a>) -> Self {
        if entry.type_id() == bindings::ndb_metadata_type_NDB_NOTE_META_COUNTS as u16 {
            NoteMetadataEntryVariant::Counts(CountsEntry::new(entry))
        } else if entry.type_id() == bindings::ndb_metadata_type_NDB_NOTE_META_REACTION as u16 {
            NoteMetadataEntryVariant::Reaction(ReactionEntry::new(entry))
        } else {
            NoteMetadataEntryVariant::Unknown(entry)
        }
    }
}

/// An owned buffer representing a single metadata entry.
///
/// This is used with the [`NoteMetadataBuilder`] to construct a complete
/// metadata blob.
pub struct NoteMetadataEntryBuf {
    entry: bindings::ndb_note_meta_entry,
}

/// A plain data struct used to create a "Counts" metadata entry.
///
/// See [`NoteMetadataEntryBuf::counts`].
pub struct Counts {
    pub total_reactions: u32,
    pub thread_replies: u32,
    pub quotes: u16,
    pub direct_replies: u16,
    pub reposts: u16,
}

impl<'a> NoteMetadataEntry<'a> {
    pub fn new(entry: &'a bindings::ndb_note_meta_entry) -> Self {
        Self { entry }
    }

    pub fn entry(&self) -> &bindings::ndb_note_meta_entry {
        self.entry
    }

    pub fn as_ptr(&self) -> *mut bindings::ndb_note_meta_entry {
        self.entry() as *const bindings::ndb_note_meta_entry as *mut bindings::ndb_note_meta_entry
    }

    pub fn type_id(&self) -> u16 {
        unsafe { *bindings::ndb_note_meta_entry_type(self.as_ptr()) }
    }

    pub fn variant(self) -> NoteMetadataEntryVariant<'a> {
        NoteMetadataEntryVariant::new(self)
    }
}

/// An iterator over metadata entries in a [`NoteMetadata`] object.
pub struct NoteMetadataEntryIter<'a> {
    metadata: NoteMetadata<'a>,
    index: u16,
}

impl<'a> NoteMetadataEntryIter<'a> {
    pub fn new(metadata: NoteMetadata<'a>) -> Self {
        Self { index: 0, metadata }
    }

    pub fn done(&mut self) -> bool {
        self.index >= self.metadata.count()
    }
}

impl<'a> Iterator for NoteMetadataEntryIter<'a> {
    type Item = NoteMetadataEntryVariant<'a>;

    fn next(&mut self) -> Option<NoteMetadataEntryVariant<'a>> {
        if self.done() {
            return None;
        }

        let ind = self.index;
        self.index += 1;

        self.metadata.entry_at(ind).map(|e| e.variant())
    }
}

impl<'a> IntoIterator for NoteMetadata<'a> {
    type Item = NoteMetadataEntryVariant<'a>;
    type IntoIter = NoteMetadataEntryIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        NoteMetadataEntryIter::new(self)
    }
}

impl bindings::ndb_note_meta_builder {
    pub fn as_mut_ptr(&mut self) -> *mut bindings::ndb_note_meta_builder {
        self as *mut bindings::ndb_note_meta_builder
    }
}

impl NoteMetadataEntryBuf {
    pub fn counts(counts: &Counts) -> Self {
        let mut me = Self {
            entry: bindings::ndb_note_meta_entry {
                type_: 0,
                aux: bindings::ndb_note_meta_entry__bindgen_ty_2 { value: 0 },
                aux2: bindings::ndb_note_meta_entry__bindgen_ty_1 { reposts: 0 },
                payload: bindings::ndb_note_meta_entry__bindgen_ty_3 { value: 0 },
            },
        };

        unsafe {
            bindings::ndb_note_meta_counts_set(
                me.as_ptr(),
                counts.total_reactions,
                counts.quotes,
                counts.direct_replies,
                counts.thread_replies,
                counts.reposts,
            );
        };

        me
    }

    pub fn as_ptr(&mut self) -> *mut bindings::ndb_note_meta_entry {
        self.borrow().as_ptr()
    }

    pub fn borrow<'a>(&'a mut self) -> NoteMetadataEntry<'a> {
        NoteMetadataEntry {
            entry: &mut self.entry,
        }
    }
}

impl bindings::ndb_note_meta {
    pub fn as_mut_ptr(&mut self) -> *mut bindings::ndb_note_meta {
        self as *mut bindings::ndb_note_meta
    }
}

/// An owned, heap-allocated buffer containing a complete note metadata blob.
///
/// This is the output of the [`NoteMetadataBuilder`]. The internal `buf` can be
/// used to write the metadata to the database.
pub struct NoteMetadataBuf {
    pub buf: Vec<u8>,
}

/// A builder for constructing a new [`NoteMetadataBuf`].
///
/// This is used to create the raw metadata blob that can be stored in the database.
/// See the [module-level documentation](self) for a build example.
pub struct NoteMetadataBuilder {
    buf: Vec<u8>,
    builder: bindings::ndb_note_meta_builder,
}

impl Default for NoteMetadataBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl NoteMetadataBuilder {
    /// Creates a new builder with a default initial capacity.
    pub fn new() -> Self {
        Self::with_capacity(128)
    }

    /// Finalizes the build and returns an owned [`NoteMetadataBuf`].
    pub fn build(mut self) -> NoteMetadataBuf {
        let size = unsafe {
            let mut meta: *mut bindings::ndb_note_meta = std::ptr::null_mut();
            bindings::ndb_note_meta_build(self.builder.as_mut_ptr(), &mut meta);
            assert!(!meta.is_null());
            bindings::ndb_note_meta_total_size(meta)
        };
        if size < self.buf.capacity() {
            self.buf.truncate(size);
        }
        unsafe {
            self.buf.set_len(size);
        }
        NoteMetadataBuf { buf: self.buf }
    }

    /// Adds a metadata entry to the builder.
    ///
    /// This may reallocate the internal buffer if more space is needed.
    pub fn add_entry(&mut self, entry: NoteMetadataEntry<'_>) {
        let remaining = self.buf.capacity() - self.buf.len();
        if remaining < 16 {
            self.buf.reserve(16);
            unsafe {
                bindings::ndb_note_meta_builder_resized(
                    self.builder.as_mut_ptr(),
                    self.buf.as_mut_ptr(),
                    self.buf.capacity(),
                );
            }
        }
        unsafe {
            let entry_ptr = bindings::ndb_note_meta_add_entry(self.builder.as_mut_ptr());
            if entry_ptr.is_null() {
                panic!("out of memory?");
            }
            self.buf.set_len(self.buf.len() + 16);
            libc::memcpy(
                entry_ptr as *mut std::ffi::c_void,
                entry.as_ptr() as *const std::ffi::c_void,
                16,
            );
        }
    }

    /// Creates a new builder with a specific capacity (in number of entries).
    pub fn with_capacity(capacity: usize) -> Self {
        let size = 16 * capacity;
        let mut me = Self {
            buf: Vec::with_capacity(size),
            builder: bindings::ndb_note_meta_builder {
                cursor: bindings::cursor {
                    start: std::ptr::null_mut(),
                    p: std::ptr::null_mut(),
                    end: std::ptr::null_mut(),
                },
            },
        };

        unsafe {
            bindings::ndb_note_meta_builder_init(
                me.builder.as_mut_ptr(),
                me.buf.as_mut_ptr(),
                size,
            );
        };

        me
    }
}

impl<'a> NoteMetadata<'a> {
    pub fn new(ptr: &'a bindings::ndb_note_meta) -> NoteMetadata<'a> {
        Self { ptr }
    }

    #[inline]
    pub fn as_ptr(&self) -> *mut bindings::ndb_note_meta {
        self.ptr as *const bindings::ndb_note_meta as *mut bindings::ndb_note_meta
    }

    pub fn count(&self) -> u16 {
        unsafe { bindings::ndb_note_meta_entries_count(self.as_ptr()) }
    }

    pub fn entry_at(&self, index: u16) -> Option<NoteMetadataEntry<'a>> {
        if index > self.count() - 1 {
            return None;
        }

        let ptr = unsafe {
            bindings::ndb_note_meta_entry_at(self.as_ptr(), index as std::os::raw::c_int)
        };

        Some(NoteMetadataEntry::new(unsafe { &mut *ptr }))
    }

    pub fn flags(&mut self) -> &mut u64 {
        unsafe {
            let p = bindings::ndb_note_meta_flags(self.as_ptr());
            &mut *p
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::test_util;
    use crate::{Filter, Ndb, NoteKey, Transaction};
    use futures::StreamExt;

    #[tokio::test]
    async fn test_metadata() {
        let db = "target/testdbs/test_metadata";
        test_util::cleanup_db(&db);

        {
            let mut ndb = Ndb::new(db, &Config::new()).expect("ndb");
            let filter = Filter::new().kinds(vec![7]).build();
            let filters = vec![filter];

            let sub_id = ndb.subscribe(&filters).expect("sub_id");
            let mut sub = sub_id.stream(&ndb).notes_per_await(1);
            let id: [u8; 32] = [
                0xd4, 0x4a, 0xd9, 0x6c, 0xb8, 0x92, 0x40, 0x92, 0xa7, 0x6b, 0xc2, 0xaf, 0xdd, 0xeb,
                0x12, 0xeb, 0x85, 0x23, 0x3c, 0x0d, 0x03, 0xa7, 0xd9, 0xad, 0xc4, 0x2c, 0x2a, 0x85,
                0xa7, 0x9a, 0x43, 0x05,
            ];

            let _ = ndb.process_event(r#"["EVENT","a",{"content":"👀","created_at":1761514455,"id":"66af95a6bdfec756344f48241562b684082ff9c76ea940c11c4fd85e91e1219c","kind":7,"pubkey":"d5805ae449e108e907091c67cdf49a9835b3cac3dd11489ad215c0ddf7c658fc","sig":"69f4a3fe7c1cc6aa9c9cc4a2e90e4b71c3b9afaad262e68b92336e0493ff1a748b5dcc20ab6e86d4551dc5ea680ddfa1c08d47f9e4845927e143e8ef2183479b","tags":[["e","d44ad96cb8924092a76bc2afddeb12eb85233c0d03a7d9adc42c2a85a79a4305","wss://relay.primal.net/","04c915daefee38317fa734444acee390a8269fe5810b2241e5e6dd343dfbecc9"],["p","04c915daefee38317fa734444acee390a8269fe5810b2241e5e6dd343dfbecc9","wss://relay.primal.net/"],["k","1"]]}]"#);

            let _ = ndb.process_event(r#"["EVENT","b",{"content":"+","created_at":1761514412,"id":"7124bca1479edeb1476d94ed6620ee1210194590b08cf1df385d053679d73fe7","kind":7,"pubkey":"af92154b4fd002924031386f71333b0afd9741a076f5c738bc2603a5b59d671f","sig":"311e7b92ae479262c8ad91ee745eca9c78d469459577d7fb598bff1e6c580f289b3c1d82cd769d0891da9248250d6877357ddaf293f33f496af9e6c8894bc485","tags":[["p","04c915daefee38317fa734444acee390a8269fe5810b2241e5e6dd343dfbecc9","wss://premium.primal.net/","ODELL"],["k","1"],["e","d44ad96cb8924092a76bc2afddeb12eb85233c0d03a7d9adc42c2a85a79a4305","wss://premium.primal.net/"],["client","Coracle","31990:97c70a44366a6535c145b333f973ea86dfdc2d7a99da618c40c64705ad98e322:1685968093690"]]}]"#);

            let res = sub.next().await.expect("await ok");
            assert_eq!(res, vec![NoteKey::new(1)]);

            sub.next().await.expect("await ok");
            //assert_eq!(res, vec![NoteKey::new(2)]);

            // ensure that unsubscribing kills the stream
            assert!(ndb.unsubscribe(sub_id).is_ok());
            assert!(sub.next().await.is_none());

            let txn = Transaction::new(&ndb).unwrap();
            let meta = ndb.get_note_metadata(&txn, &id).expect("what");
            let mut count = 0;
            let mut buf: [i8; 128] = [0; 128];

            for entry in meta {
                match entry {
                    NoteMetadataEntryVariant::Counts(counts) => {
                        assert!(counts.reactions() == 2)
                    }

                    NoteMetadataEntryVariant::Reaction(reaction) => {
                        let s = reaction.as_str(&mut buf);
                        assert!(s == "👀" || s == "+");
                        assert!(reaction.count() == 1);
                    }

                    NoteMetadataEntryVariant::Unknown(_) => {
                        assert!(false);
                    }
                }
                count += 1;
            }

            // 1 count entry, 2 reaction entries
            assert!(count == 3);
        }

        test_util::cleanup_db(&db);
    }
}
