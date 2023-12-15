use std::ffi::CString;
use std::ptr;

use crate::bindings;
use crate::config::Config;
use crate::error::Error;
use crate::note::Note;
use crate::result::Result;
use crate::transaction::Transaction;

pub struct Ndb {
    ndb: *mut bindings::ndb,
}

impl Ndb {
    // Constructor
    pub fn new(db_dir: &str, config: &Config) -> Result<Self> {
        let db_dir_cstr = match CString::new(db_dir) {
            Ok(cstr) => cstr,
            Err(_) => return Err(Error::DbOpenFailed),
        };
        let mut ndb: *mut bindings::ndb = ptr::null_mut();
        let result = unsafe { bindings::ndb_init(&mut ndb, db_dir_cstr.as_ptr(), config.as_ptr()) };

        if result == 0 {
            return Err(Error::DbOpenFailed);
        }

        Ok(Ndb { ndb })
    }

    pub fn get_note_by_id<'a>(
        &self,
        transaction: &'a mut Transaction,
        id: &[u8; 32],
    ) -> Result<Note<'a>> {
        let mut len: usize = 0;
        let mut primkey: u64 = 0;

        let note_ptr = unsafe {
            bindings::ndb_get_note_by_id(
                transaction.as_mut_ptr(),
                id.as_ptr(),
                &mut len,
                &mut primkey,
            )
        };

        if note_ptr.is_null() {
            // Handle null pointer (e.g., note not found or error occurred)
            return Err(Error::NotFound);
        }

        // Convert the raw pointer to a Note instance
        Ok(Note::new_transactional(note_ptr, len, primkey, transaction))
    }

    pub fn as_ptr(&self) -> *mut bindings::ndb {
        return self.ndb;
    }
}

impl Drop for Ndb {
    fn drop(&mut self) {
        unsafe {
            bindings::ndb_destroy(self.ndb);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::test_util;
    use std::fs;

    #[test]
    fn ndb_init_works() {
        // Initialize ndb
        {
            let cfg = Config::new();
            let _ = Ndb::new(".", &cfg).expect("ok");
        }

        test_util::cleanup_db();
    }
}
