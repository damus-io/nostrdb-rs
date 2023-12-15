use crate::bindings;
use crate::transaction::Transaction;

#[derive(Debug)]
pub enum Note<'a> {
    Owned {
        ptr: *mut bindings::ndb_note,
        size: usize,
    },

    Transactional {
        ptr: *mut bindings::ndb_note,
        size: usize,
        key: u64,
        transaction: &'a Transaction,
    },
}

impl<'a> Note<'a> {
    pub fn new_owned(ptr: *mut bindings::ndb_note, size: usize) -> Note<'static> {
        Note::Owned { ptr, size }
    }

    // Create a new note tied to a transaction
    pub fn new_transactional(
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

        // Initialize ndb
        {
            let cfg = Config::new();
            let ndb = Ndb::new(".", &cfg).expect("db open");
            let mut txn = Transaction::new(&ndb).expect("new txn");

            let err = ndb
                .get_note_by_id(&mut txn, &[0; 32])
                .expect_err("not found");
            assert!(err == Error::NotFound);
        }

        test_util::cleanup_db();
    }
}
