use libc;
use std::ffi::CString;
use std::ptr;

use crate::bindings;
use crate::{Blocks, Config, Error, Filter, Note, ProfileRecord, Result, Transaction};
use std::fs;
use std::os::raw::c_int;
use std::path::Path;
use std::sync::Arc;
use tokio::task; // Make sure to import the task module

#[derive(Debug)]
struct NdbRef {
    ndb: *mut bindings::ndb,
}

/// It's safe to have multi-threaded references to this because thread safety
/// is guaranteed by LMDB
unsafe impl Send for NdbRef {}
unsafe impl Sync for NdbRef {}

/// The database is automatically closed when [Ndb] is [Drop]ped.
impl Drop for NdbRef {
    fn drop(&mut self) {
        unsafe {
            bindings::ndb_destroy(self.ndb);
        }
    }
}

/// A nostrdb context. Construct one of these with [Ndb::new].
#[derive(Debug, Clone)]
pub struct Ndb {
    refs: Arc<NdbRef>,
}

impl Ndb {
    /// Construct a new nostrdb context. Takes a directory where the database
    /// is/will be located and a nostrdb config.
    pub fn new(db_dir: &str, config: &Config) -> Result<Self> {
        let db_dir_cstr = match CString::new(db_dir) {
            Ok(cstr) => cstr,
            Err(_) => return Err(Error::DbOpenFailed),
        };
        let mut ndb: *mut bindings::ndb = ptr::null_mut();

        let path = Path::new(db_dir);
        if !path.exists() {
            let _ = fs::create_dir_all(&path);
        }

        let result = unsafe { bindings::ndb_init(&mut ndb, db_dir_cstr.as_ptr(), config.as_ptr()) };

        if result == 0 {
            return Err(Error::DbOpenFailed);
        }

        let refs = Arc::new(NdbRef { ndb });
        Ok(Ndb { refs })
    }

    /// Ingest a relay-sent event in the form `["EVENT","subid", {"id:"...}]`
    /// This function returns immediately and doesn't provide any information on
    /// if ingestion was successful or not.
    pub fn process_event(&self, json: &str) -> Result<()> {
        // Convert the Rust string to a C-style string
        let c_json = CString::new(json).expect("CString::new failed");
        let c_json_ptr = c_json.as_ptr();

        // Get the length of the string
        let len = json.len() as libc::c_int;

        let res = unsafe { bindings::ndb_process_event(self.as_ptr(), c_json_ptr, len) };

        if res == 0 {
            return Err(Error::NoteProcessFailed);
        }

        Ok(())
    }

    pub fn subscribe(&self, filter: Filter) -> Result<u64> {
        unsafe {
            let res = bindings::ndb_subscribe(self.as_ptr(), filter.as_mut_ptr(), 1);
            if res == 0 {
                Err(Error::SubscriptionError)
            } else {
                Ok(res)
            }
        }
    }

    pub fn poll_for_notes(&self, sub_id: u64, max_notes: u32) -> Vec<u64> {
        let mut vec = vec![];
        vec.reserve_exact(max_notes as usize);

        let res = unsafe {
            let res = bindings::ndb_poll_for_notes(
                self.as_ptr(),
                sub_id,
                vec.as_mut_ptr(),
                max_notes as c_int,
            );
            vec.set_len(res as usize);
        };

        vec
    }

    pub async fn wait_for_notes(&self, sub_id: u64, max_notes: u32) -> Result<Vec<u64>> {
        let ndb = self.clone();
        let handle = task::spawn_blocking(move || {
            let mut vec = vec![];
            vec.reserve_exact(max_notes as usize);
            let res = unsafe {
                bindings::ndb_wait_for_notes(
                    ndb.as_ptr(),
                    sub_id,
                    vec.as_mut_ptr(),
                    max_notes as c_int,
                )
            };
            if res == 0 {
                Err(Error::SubscriptionError)
            } else {
                unsafe {
                    vec.set_len(res as usize);
                };
                Ok(vec)
            }
        });

        match handle.await {
            Ok(Ok(res)) => Ok(res),
            Ok(Err(err)) => Err(err),
            Err(_) => Err(Error::SubscriptionError),
        }
    }

    pub fn get_profile_by_pubkey<'a>(
        &self,
        transaction: &'a Transaction,
        id: &[u8; 32],
    ) -> Result<ProfileRecord<'a>> {
        let mut len: usize = 0;
        let mut primkey: u64 = 0;

        let profile_record_ptr = unsafe {
            bindings::ndb_get_profile_by_pubkey(
                transaction.as_mut_ptr(),
                id.as_ptr(),
                &mut len,
                &mut primkey,
            )
        };

        if profile_record_ptr.is_null() {
            // Handle null pointer (e.g., note not found or error occurred)
            return Err(Error::NotFound);
        }

        // Convert the raw pointer to a Note instance
        Ok(ProfileRecord::new(
            profile_record_ptr,
            len,
            primkey,
            transaction,
        ))
    }

    pub fn get_notekey_by_id(&self, txn: &Transaction, id: &[u8; 32]) -> Result<u64> {
        let res = unsafe {
            bindings::ndb_get_notekey_by_id(
                txn.as_mut_ptr(),
                id.as_ptr() as *const ::std::os::raw::c_uchar,
            )
        };

        if res == 0 {
            return Err(Error::NotFound);
        }

        Ok(res)
    }

    pub fn get_blocks_by_key<'a>(&self, txn: &'a Transaction, note_key: u64) -> Result<Blocks<'a>> {
        let blocks_ptr =
            unsafe { bindings::ndb_get_blocks_by_key(self.as_ptr(), txn.as_mut_ptr(), note_key) };

        if blocks_ptr.is_null() {
            return Err(Error::NotFound);
        }

        Ok(Blocks::new_transactional(blocks_ptr, txn))
    }

    /// Get a note from the database. Takes a [Transaction] and a 32-byte [Note] Id
    pub fn get_note_by_id<'a>(
        &self,
        transaction: &'a Transaction,
        id: &[u8; 32],
    ) -> Result<Note<'a>> {
        let mut len: usize = 0;
        let mut primkey: u64 = 0;

        let note_ptr = unsafe {
            bindings::ndb_get_note_by_id(
                transaction.as_ptr() as *mut bindings::ndb_txn,
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

    /// Get the underlying pointer to the context in C
    pub fn as_ptr(&self) -> *mut bindings::ndb {
        return self.refs.ndb;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::test_util;

    #[test]
    fn ndb_init_works() {
        let db = "target/testdbs/init_works";
        test_util::cleanup_db(db);

        {
            let cfg = Config::new();
            let _ = Ndb::new(db, &cfg).expect("ok");
        }
    }

    #[tokio::test]
    async fn subscribe_event_works() {
        let db = "target/testdbs/subscribe";
        test_util::cleanup_db(&db);

        {
            let ndb = Ndb::new(db, &Config::new()).expect("ndb");
            let filter = Filter::new().kinds(vec![1]);
            let sub_id = ndb.subscribe(filter).expect("sub_id");
            let waiter = ndb.wait_for_notes(sub_id, 1);
            ndb.process_event(r#"["EVENT","b",{"id": "702555e52e82cc24ad517ba78c21879f6e47a7c0692b9b20df147916ae8731a3","pubkey": "32bf915904bfde2d136ba45dde32c88f4aca863783999faea2e847a8fafd2f15","created_at": 1702675561,"kind": 1,"tags": [],"content": "hello, world","sig": "2275c5f5417abfd644b7bc74f0388d70feb5d08b6f90fa18655dda5c95d013bfbc5258ea77c05b7e40e0ee51d8a2efa931dc7a0ec1db4c0a94519762c6625675"}]"#).expect("process ok");
            let res = waiter.await.expect("await ok");
            assert_eq!(res, vec![1]);
        }
    }

    #[test]
    fn poll_note_works() {
        let db = "target/testdbs/poll";
        test_util::cleanup_db(&db);

        {
            let ndb = Ndb::new(db, &Config::new()).expect("ndb");
            let filter = Filter::new().kinds(vec![1]);
            let sub_id = ndb.subscribe(filter).expect("sub_id");
            ndb.process_event(r#"["EVENT","b",{"id": "702555e52e82cc24ad517ba78c21879f6e47a7c0692b9b20df147916ae8731a3","pubkey": "32bf915904bfde2d136ba45dde32c88f4aca863783999faea2e847a8fafd2f15","created_at": 1702675561,"kind": 1,"tags": [],"content": "hello, world","sig": "2275c5f5417abfd644b7bc74f0388d70feb5d08b6f90fa18655dda5c95d013bfbc5258ea77c05b7e40e0ee51d8a2efa931dc7a0ec1db4c0a94519762c6625675"}]"#).expect("process ok");
            // this is too fast, we should have nothing
            let res = ndb.poll_for_notes(sub_id, 1);
            assert_eq!(res, vec![]);

            std::thread::sleep(std::time::Duration::from_millis(100));
            // now we should have something
            let res = ndb.poll_for_notes(sub_id, 1);
            assert_eq!(res, vec![1]);
        }
    }

    #[test]
    fn process_event_works() {
        let db = "target/testdbs/event_works";
        test_util::cleanup_db(&db);

        {
            let ndb = Ndb::new(db, &Config::new()).expect("ndb");
            ndb.process_event(r#"["EVENT","s",{"id": "702555e52e82cc24ad517ba78c21879f6e47a7c0692b9b20df147916ae8731a3","pubkey": "32bf915904bfde2d136ba45dde32c88f4aca863783999faea2e847a8fafd2f15","created_at": 1702675561,"kind": 1,"tags": [],"content": "hello, world","sig": "2275c5f5417abfd644b7bc74f0388d70feb5d08b6f90fa18655dda5c95d013bfbc5258ea77c05b7e40e0ee51d8a2efa931dc7a0ec1db4c0a94519762c6625675"}]"#).expect("process ok");
        }

        {
            let ndb = Ndb::new(db, &Config::new()).expect("ndb");
            let id =
                hex::decode("702555e52e82cc24ad517ba78c21879f6e47a7c0692b9b20df147916ae8731a3")
                    .expect("hex id");
            let mut txn = Transaction::new(&ndb).expect("txn");
            let id_bytes: [u8; 32] = id.try_into().expect("id bytes");
            let note = ndb.get_note_by_id(&mut txn, &id_bytes).expect("note");
            assert_eq!(note.kind(), 1);
        }
    }
}
