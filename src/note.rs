use crate::tags::Tags;
use crate::transaction::Transaction;
use crate::{bindings, Error};
use ::std::os::raw::c_uchar;
use std::hash::Hash;

#[derive(Debug, Clone, Copy, Eq, Ord, PartialEq, PartialOrd, Hash)]
pub struct NoteKey(u64);

impl NoteKey {
    pub fn as_u64(&self) -> u64 {
        self.0
    }

    pub fn new(key: u64) -> Self {
        NoteKey(key)
    }
}

pub struct NoteBuildOptions<'a> {
    /// Generate the created_at based on the current time, otherwise the id field will remain untouched
    pub set_created_at: bool,

    /// Sign with the secret key, otherwise sig field will remain untouched
    pub sign_key: Option<&'a [u8; 32]>,
}

impl<'a> Default for NoteBuildOptions<'a> {
    fn default() -> Self {
        NoteBuildOptions {
            set_created_at: true,
            sign_key: None,
        }
    }
}

impl<'a> NoteBuildOptions<'a> {
    pub fn created_at(mut self, set_created_at: bool) -> Self {
        self.set_created_at = set_created_at;
        self
    }

    pub fn sign(mut self, seckey: &'a [u8; 32]) -> NoteBuildOptions<'a> {
        self.sign_key = Some(seckey);
        self
    }
}

#[derive(Debug, Clone)]
pub enum Note<'a> {
    /// A note in-memory outside of nostrdb. This note is a pointer to a note in
    /// memory and will be free'd when [Drop]ped. Method such as [Note::from_json]
    /// will create owned notes in memory.
    ///
    /// [Drop]: std::ops::Drop
    Owned {
        ptr: *mut bindings::ndb_note,
        size: usize,
    },

    /// A note inside of nostrdb. Tied to the lifetime of a
    /// [Transaction] to ensure no reading of data outside
    /// of a transaction.
    Transactional {
        ptr: *mut bindings::ndb_note,
        size: usize,
        key: NoteKey,
        transaction: &'a Transaction,
    },
}

impl<'a> Note<'a> {
    /// Constructs an owned `Note`. This note is a pointer to a note in
    /// memory and will be free'd when [Drop]ped. You normally wouldn't
    /// use this method directly, public consumer would use from_json instead.
    ///
    /// [Drop]: std::ops::Drop
    #[allow(dead_code)]
    pub(crate) fn new_owned(ptr: *mut bindings::ndb_note, size: usize) -> Note<'static> {
        Note::Owned { ptr, size }
    }

    /// Constructs a `Note` in a transactional context.
    /// Use [Note::new_transactional] to create a new transactional note.
    /// You normally wouldn't use this method directly, it is used by
    /// functions that get notes from the database like
    /// [ndb_get_note_by_id]
    pub(crate) fn new_transactional(
        ptr: *mut bindings::ndb_note,
        size: usize,
        key: NoteKey,
        transaction: &'a Transaction,
    ) -> Note<'a> {
        Note::Transactional {
            ptr,
            size,
            key,
            transaction,
        }
    }

    pub fn txn(&'a self) -> Option<&'a Transaction> {
        match self {
            Note::Transactional { transaction, .. } => Some(transaction),
            _ => None,
        }
    }

    pub fn key(&self) -> Option<NoteKey> {
        match self {
            Note::Transactional { key, .. } => Some(NoteKey::new(key.as_u64())),
            _ => None,
        }
    }

    pub fn size(&self) -> usize {
        match self {
            Note::Owned { size, .. } => *size,
            Note::Transactional { size, .. } => *size,
        }
    }

    pub fn as_ptr(&self) -> *mut bindings::ndb_note {
        match self {
            Note::Owned { ptr, .. } => *ptr,
            Note::Transactional { ptr, .. } => *ptr,
        }
    }

    pub fn json_with_bufsize(&self, bufsize: usize) -> Result<String, Error> {
        let mut buf = Vec::with_capacity(bufsize);
        unsafe {
            let size = bindings::ndb_note_json(
                self.as_ptr(),
                buf.as_mut_ptr() as *mut ::std::os::raw::c_char,
                bufsize as ::std::os::raw::c_int,
            ) as usize;

            // Step 4: Check the return value for success
            if size == 0 {
                return Err(Error::BufferOverflow); // Handle the error appropriately
            }

            buf.set_len(size);

            Ok(std::str::from_utf8_unchecked(&buf[..size - 1]).to_string())
        }
    }

    pub fn json(&self) -> Result<String, Error> {
        // 1mb buffer
        self.json_with_bufsize(1024usize * 1024usize)
    }

    fn content_size(&self) -> usize {
        unsafe { bindings::ndb_note_content_length(self.as_ptr()) as usize }
    }

    pub fn created_at(&self) -> u64 {
        unsafe { bindings::ndb_note_created_at(self.as_ptr()).into() }
    }

    pub fn content_ptr(&self) -> *const ::std::os::raw::c_char {
        unsafe { bindings::ndb_note_content(self.as_ptr()) }
    }

    /// Get the [`Note`] contents.
    pub fn content(&self) -> &'a str {
        unsafe {
            let content = self.content_ptr();
            let byte_slice = std::slice::from_raw_parts(content as *const u8, self.content_size());
            std::str::from_utf8_unchecked(byte_slice)
        }
    }

    /// Get the note pubkey
    pub fn pubkey(&self) -> &'a [u8; 32] {
        unsafe {
            let ptr = bindings::ndb_note_pubkey(self.as_ptr());
            &*(ptr as *const [u8; 32])
        }
    }

    pub fn id(&self) -> &'a [u8; 32] {
        unsafe {
            let ptr = bindings::ndb_note_id(self.as_ptr());
            &*(ptr as *const [u8; 32])
        }
    }

    pub fn kind(&self) -> u32 {
        unsafe { bindings::ndb_note_kind(self.as_ptr()) }
    }

    pub fn tags(&self) -> Tags<'a> {
        let tags = unsafe { bindings::ndb_note_tags(self.as_ptr()) };
        Tags::new(tags, self.clone())
    }

    pub fn sig(&self) -> &'a [u8; 64] {
        unsafe {
            let ptr = bindings::ndb_note_sig(self.as_ptr());
            &*(ptr as *const [u8; 64])
        }
    }
}

impl<'a> Drop for Note<'a> {
    fn drop(&mut self) {
        if let Note::Owned { ptr, .. } = self {
            unsafe { libc::free((*ptr) as *mut libc::c_void) }
        }
    }
}

impl bindings::ndb_builder {
    fn as_mut_ptr(&mut self) -> *mut bindings::ndb_builder {
        self as *mut bindings::ndb_builder
    }
}

impl bindings::ndb_keypair {
    fn as_mut_ptr(&mut self) -> *mut bindings::ndb_keypair {
        self as *mut bindings::ndb_keypair
    }
}

impl Default for bindings::ndb_keypair {
    fn default() -> Self {
        bindings::ndb_keypair {
            pubkey: [0; 32],
            secret: [0; 32],
            pair: [0; 96],
        }
    }
}

impl Default for bindings::ndb_builder {
    fn default() -> Self {
        bindings::ndb_builder {
            mem: bindings::cursor::default(),
            note_cur: bindings::cursor::default(),
            strings: bindings::cursor::default(),
            str_indices: bindings::cursor::default(),
            note: std::ptr::null_mut(),
            current_tag: std::ptr::null_mut(),
        }
    }
}

impl Default for bindings::cursor {
    fn default() -> Self {
        Self {
            start: std::ptr::null_mut(),
            p: std::ptr::null_mut(),
            end: std::ptr::null_mut(),
        }
    }
}

pub struct NoteBuilder<'a> {
    buffer: *mut ::std::os::raw::c_uchar,
    builder: bindings::ndb_builder,
    options: NoteBuildOptions<'a>,
}

impl<'a> Default for NoteBuilder<'a> {
    fn default() -> Self {
        NoteBuilder::new()
    }
}

impl<'a> NoteBuilder<'a> {
    pub fn with_bufsize(size: usize) -> Option<Self> {
        let buffer: *mut c_uchar = unsafe { libc::malloc(size as libc::size_t) as *mut c_uchar };
        if buffer.is_null() {
            return None;
        }

        let mut builder = NoteBuilder {
            buffer,
            options: NoteBuildOptions::default(),
            builder: bindings::ndb_builder::default(),
        };

        let ok = unsafe {
            bindings::ndb_builder_init(builder.builder.as_mut_ptr(), builder.buffer, size) != 0
        };

        if !ok {
            // this really shouldn't happen
            return None;
        }

        Some(builder)
    }

    /// Create a note builder with a 1mb buffer, if you need bigger notes
    /// then use with_bufsize with a custom buffer size
    pub fn new() -> Self {
        let default_bufsize = 1024usize * 1024usize;
        Self::with_bufsize(default_bufsize).expect("OOM when creating NoteBuilder")
    }

    pub fn as_mut_ptr(&mut self) -> *mut bindings::ndb_builder {
        &mut self.builder as *mut bindings::ndb_builder
    }

    pub fn sig(mut self, signature: &[u8; 64]) -> Self {
        self.options.sign_key = None;
        unsafe {
            bindings::ndb_builder_set_sig(self.as_mut_ptr(), signature.as_ptr() as *mut c_uchar)
        };
        self
    }

    pub fn id(mut self, id: &[u8; 32]) -> Self {
        unsafe { bindings::ndb_builder_set_id(self.as_mut_ptr(), id.as_ptr() as *mut c_uchar) };
        self
    }

    pub fn content(mut self, content: &str) -> Self {
        unsafe {
            // Call the external C function with the appropriate arguments
            bindings::ndb_builder_set_content(
                self.as_mut_ptr(),
                content.as_ptr() as *const ::std::os::raw::c_char,
                content.len() as ::std::os::raw::c_int,
            );
        }
        self
    }

    pub fn created_at(mut self, created_at: u64) -> Self {
        self.options.set_created_at = false;
        self.set_created_at(created_at);
        self
    }

    pub fn kind(mut self, kind: u32) -> Self {
        unsafe {
            bindings::ndb_builder_set_kind(self.as_mut_ptr(), kind);
        };
        self
    }

    pub fn pubkey(mut self, pubkey: &[u8; 32]) -> Self {
        self.set_pubkey(pubkey);
        self
    }

    fn set_pubkey(&mut self, pubkey: &[u8; 32]) {
        unsafe {
            bindings::ndb_builder_set_pubkey(self.as_mut_ptr(), pubkey.as_ptr() as *mut c_uchar)
        };
    }

    fn set_created_at(&mut self, created_at: u64) {
        unsafe {
            bindings::ndb_builder_set_created_at(self.as_mut_ptr(), created_at);
        };
    }

    pub fn start_tag(mut self) -> Self {
        unsafe {
            bindings::ndb_builder_new_tag(self.as_mut_ptr());
        };
        self
    }

    pub fn tag_str(mut self, str: &str) -> Self {
        unsafe {
            // Call the external C function with the appropriate arguments
            bindings::ndb_builder_push_tag_str(
                self.as_mut_ptr(),
                str.as_ptr() as *const ::std::os::raw::c_char,
                str.len() as ::std::os::raw::c_int,
            );
        }
        self
    }

    pub fn options(mut self, options: NoteBuildOptions<'a>) -> NoteBuilder<'a> {
        self.options = options;
        self
    }

    pub fn sign(mut self, seckey: &'a [u8; 32]) -> NoteBuilder<'a> {
        self.options = self.options.sign(seckey);
        self
    }

    pub fn build(&mut self) -> Option<Note<'static>> {
        let mut note_ptr: *mut bindings::ndb_note = std::ptr::null_mut();
        let mut keypair = bindings::ndb_keypair::default();

        if self.options.set_created_at {
            let start = std::time::SystemTime::now();
            if let Ok(since_the_epoch) = start.duration_since(std::time::UNIX_EPOCH) {
                let timestamp = since_the_epoch.as_secs();
                self.set_created_at(timestamp);
            } else {
                return None;
            }
        }

        let keypair_ptr = if let Some(sec) = self.options.sign_key {
            keypair.secret.copy_from_slice(sec);
            let ok = unsafe { bindings::ndb_create_keypair(keypair.as_mut_ptr()) != 0 };
            if ok {
                // if we're signing, we should set the pubkey as well
                self.set_pubkey(&keypair.pubkey);
                keypair.as_mut_ptr()
            } else {
                std::ptr::null_mut()
            }
        } else {
            std::ptr::null_mut()
        };

        let size = unsafe {
            bindings::ndb_builder_finalize(
                self.as_mut_ptr(),
                &mut note_ptr as *mut *mut bindings::ndb_note,
                keypair_ptr,
            ) as usize
        };

        if size == 0 {
            return None;
        }

        note_ptr = unsafe {
            libc::realloc(note_ptr as *mut libc::c_void, size) as *mut bindings::ndb_note
        };

        if note_ptr.is_null() {
            return None;
        }

        Some(Note::new_owned(note_ptr, size))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn note_query_works() {
        use crate::config::Config;
        use crate::error::Error;
        use crate::ndb::Ndb;
        use crate::test_util;

        let db = "target/testdbs/note_query_works";

        // Initialize ndb
        {
            let cfg = Config::new();
            let ndb = Ndb::new(&db, &cfg).expect("db open");
            let mut txn = Transaction::new(&ndb).expect("new txn");

            let err = ndb
                .get_note_by_id(&mut txn, &[0; 32])
                .expect_err("not found");
            assert!(err == Error::NotFound);
        }

        test_util::cleanup_db(db);
    }

    #[test]
    fn note_builder_works() {
        let pubkey: [u8; 32] = [
            0x6c, 0x54, 0x0e, 0xd0, 0x60, 0xbf, 0xc2, 0xb0, 0xc5, 0xb6, 0xf0, 0x9c, 0xd3, 0xeb,
            0xed, 0xf9, 0x80, 0xef, 0x7b, 0xc8, 0x36, 0xd6, 0x95, 0x82, 0x36, 0x1d, 0x20, 0xf2,
            0xad, 0x12, 0x4f, 0x23,
        ];

        let seckey: [u8; 32] = [
            0xd8, 0x62, 0x2e, 0x92, 0x47, 0xab, 0x39, 0x30, 0x11, 0x7e, 0x66, 0x45, 0xd5, 0xf7,
            0x8b, 0x66, 0xbd, 0xd3, 0xaf, 0xe2, 0x46, 0x4f, 0x90, 0xbc, 0xd9, 0xe0, 0x38, 0x75,
            0x8d, 0x2d, 0x55, 0x34,
        ];

        let id: [u8; 32] = [
            0xfb, 0x16, 0x5b, 0xe2, 0x2c, 0x7b, 0x25, 0x18, 0xb7, 0x49, 0xaa, 0xbb, 0x71, 0x40,
            0xc7, 0x3f, 0x08, 0x87, 0xfe, 0x84, 0x47, 0x5c, 0x82, 0x78, 0x57, 0x00, 0x66, 0x3b,
            0xe8, 0x5b, 0xa8, 0x59,
        ];

        let note = NoteBuilder::new()
            .kind(1)
            .content("this is the content")
            .created_at(42)
            .start_tag()
            .tag_str("comment")
            .tag_str("this is a comment")
            .start_tag()
            .tag_str("blah")
            .tag_str("something")
            .sign(&seckey)
            .build()
            .expect("expected build to work");

        assert_eq!(note.created_at(), 42);
        assert_eq!(note.content(), "this is the content");
        assert_eq!(note.kind(), 1);
        assert_eq!(note.pubkey(), &pubkey);
        assert!(note.sig() != &[0; 64]);
        assert_eq!(note.id(), &id);

        for tag in note.tags() {
            assert_eq!(tag.get_unchecked(0).variant().str().unwrap(), "comment");
            assert_eq!(
                tag.get_unchecked(1).variant().str().unwrap(),
                "this is a comment"
            );
            break;
        }

        for tag in note.tags().iter().skip(1) {
            assert_eq!(tag.get_unchecked(0).variant().str().unwrap(), "blah");
            assert_eq!(tag.get_unchecked(1).variant().str().unwrap(), "something");
            break;
        }

        let json = note.json().expect("note json");
        // the signature changes so 267 is everything up until the signature
        assert_eq!(&json[..267], "{\"id\":\"fb165be22c7b2518b749aabb7140c73f0887fe84475c82785700663be85ba859\",\"pubkey\":\"6c540ed060bfc2b0c5b6f09cd3ebedf980ef7bc836d69582361d20f2ad124f23\",\"created_at\":42,\"kind\":1,\"tags\":[[\"comment\",\"this is a comment\"],[\"blah\",\"something\"]],\"content\":\"this is the content\"");
    }
}
