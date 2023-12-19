use crate::bindings;
use crate::transaction::Transaction;

#[derive(Debug)]
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
        key: u64,
        transaction: &'a Transaction,
    },
}

impl<'a> Note<'a> {
    /// Constructs an owned `Note`. This note is a pointer to a note in
    /// memory and will be free'd when [Drop]ped. You normally wouldn't
    /// use this method directly, public consumer would use from_json instead.
    ///
    /// [Drop]: std::ops::Drop
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
        key: u64,
        transaction: &'a Transaction,
    ) -> Note<'a> {
        Note::Transactional {
            ptr,
            size,
            key,
            transaction,
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

    fn content_size(&self) -> usize {
        unsafe { bindings::ndb_note_content_length(self.as_ptr()) as usize }
    }

    /// Get the [`Note`] contents.
    pub fn content(&self) -> &'a str {
        unsafe {
            let content = bindings::ndb_note_content(self.as_ptr());
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

    pub fn kind(&self) -> u32 {
        unsafe { bindings::ndb_note_kind(self.as_ptr()) }
    }
}

impl<'a> Drop for Note<'a> {
    fn drop(&mut self) {
        match self {
            Note::Owned { ptr, .. } => unsafe { libc::free((*ptr) as *mut libc::c_void) },
            _ => (),
        }
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
}
