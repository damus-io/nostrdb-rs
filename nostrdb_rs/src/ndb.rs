use std::ffi::CString;
use std::ptr;

use crate::{
    bindings, Blocks, Config, Error, Filter, Note, NoteKey, ProfileKey, ProfileRecord, QueryResult,
    Result, Subscription, SubscriptionState, SubscriptionStream, Transaction,
};
use futures::StreamExt;
use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::fs;
use std::os::raw::c_int;
use std::path::Path;
use std::sync::{Arc, Mutex};
use tracing::debug;

#[derive(Debug)]
struct NdbRef {
    ndb: *mut bindings::ndb,
    rust_cb_ctx: *mut ::std::os::raw::c_void,
}

/// SAFETY: thread safety is ensured by nostrdb
unsafe impl Send for NdbRef {}

/// SAFETY: thread safety is ensured by nostrdb
unsafe impl Sync for NdbRef {}

/// The database is automatically closed when [Ndb] is [Drop]ped.
impl Drop for NdbRef {
    fn drop(&mut self) {
        unsafe {
            bindings::ndb_destroy(self.ndb);

            if !self.rust_cb_ctx.is_null() {
                // Rebuild the Box from the raw pointer and drop it.
                let _ = Box::from_raw(self.rust_cb_ctx as *mut Box<dyn FnMut()>);
            }
        }
    }
}

type SubMap = HashMap<Subscription, SubscriptionState>;

/// A nostrdb context. Construct one of these with [Ndb::new].
#[derive(Debug, Clone)]
pub struct Ndb {
    refs: Arc<NdbRef>,

    /// Track query future states
    pub(crate) subs: Arc<Mutex<SubMap>>,
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
            let _ = fs::create_dir_all(path);
        }

        let min_mapsize = 1024 * 1024 * 512;
        let mut mapsize = config.config.mapsize;
        let config = *config;

        let prev_callback = config.config.sub_cb;
        let prev_callback_ctx = config.config.sub_cb_ctx;
        let subs = Arc::new(Mutex::new(SubMap::default()));
        let subs_clone = subs.clone();

        // We need to register our own callback so that we can wake
        // query futures
        let mut config = config.set_sub_callback(move |sub_id: u64| {
            let mut map = subs_clone.lock().unwrap();
            if let Some(s) = map.get_mut(&Subscription::new(sub_id)) {
                s.ready = true;
                if let Some(w) = s.waker.take() {
                    w.wake();
                }
            }

            if let Some(pcb) = prev_callback {
                unsafe {
                    pcb(prev_callback_ctx, sub_id);
                };
            }
        });

        let result = loop {
            let result =
                unsafe { bindings::ndb_init(&mut ndb, db_dir_cstr.as_ptr(), config.as_ptr()) };

            if result == 0 {
                mapsize /= 2;
                config = config.set_mapsize(mapsize);
                debug!("ndb init failed, reducing mapsize to {}", mapsize);

                if mapsize > min_mapsize {
                    continue;
                } else {
                    break 0;
                }
            } else {
                break result;
            }
        };

        if result == 0 {
            return Err(Error::DbOpenFailed);
        }

        let rust_cb_ctx = config.config.sub_cb_ctx;
        let refs = Arc::new(NdbRef { ndb, rust_cb_ctx });

        Ok(Ndb { refs, subs })
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

    pub fn query<'a>(
        &self,
        txn: &'a Transaction,
        filters: &[Filter],
        max_results: i32,
    ) -> Result<Vec<QueryResult<'a>>> {
        let mut ndb_filters: Vec<bindings::ndb_filter> = filters.iter().map(|a| a.data).collect();
        let mut out: Vec<bindings::ndb_query_result> = vec![];
        let mut returned: i32 = 0;
        out.reserve_exact(max_results as usize);
        let res = unsafe {
            bindings::ndb_query(
                txn.as_mut_ptr(),
                ndb_filters.as_mut_ptr(),
                ndb_filters.len() as i32,
                out.as_mut_ptr(),
                max_results,
                &mut returned as *mut i32,
            )
        };
        if res == 1 {
            unsafe {
                out.set_len(returned as usize);
            };
            Ok(out.iter().map(|r| QueryResult::new(r, txn)).collect())
        } else {
            Err(Error::QueryError)
        }
    }

    pub fn subscription_count(&self) -> u32 {
        unsafe { bindings::ndb_num_subscriptions(self.as_ptr()) as u32 }
    }

    pub fn unsubscribe(&mut self, sub: Subscription) -> Result<()> {
        let r = unsafe { bindings::ndb_unsubscribe(self.as_ptr(), sub.id()) };

        // mark the subscription as done if it exists in our stream map
        {
            let mut map = self.subs.lock().unwrap();
            if let Entry::Occupied(mut entry) = map.entry(sub) {
                entry.get_mut().done = true;
            }
        }

        if r == 0 {
            Err(Error::SubscriptionError)
        } else {
            Ok(())
        }
    }

    pub fn subscribe(&self, filters: &[Filter]) -> Result<Subscription> {
        unsafe {
            let mut ndb_filters: Vec<bindings::ndb_filter> =
                filters.iter().map(|a| a.data).collect();
            let id = bindings::ndb_subscribe(
                self.as_ptr(),
                ndb_filters.as_mut_ptr(),
                filters.len() as i32,
            );
            if id == 0 {
                Err(Error::SubscriptionError)
            } else {
                Ok(Subscription::new(id))
            }
        }
    }

    pub fn poll_for_notes(&self, sub: Subscription, max_notes: u32) -> Vec<NoteKey> {
        let mut vec = vec![];
        vec.reserve_exact(max_notes as usize);

        unsafe {
            let res = bindings::ndb_poll_for_notes(
                self.as_ptr(),
                sub.id(),
                vec.as_mut_ptr(),
                max_notes as c_int,
            );
            vec.set_len(res as usize);
        };

        vec.into_iter().map(NoteKey::new).collect()
    }

    pub async fn wait_for_notes(
        &self,
        sub_id: Subscription,
        max_notes: u32,
    ) -> Result<Vec<NoteKey>> {
        let mut stream = SubscriptionStream::new(self.clone(), sub_id).notes_per_await(max_notes);

        match stream.next().await {
            Some(res) => Ok(res),
            None => Err(Error::SubscriptionError),
        }
    }

    pub fn get_profile_by_key<'a>(
        &self,
        transaction: &'a Transaction,
        key: ProfileKey,
    ) -> Result<ProfileRecord<'a>> {
        let mut len: usize = 0;

        let profile_record_ptr = unsafe {
            bindings::ndb_get_profile_by_key(transaction.as_mut_ptr(), key.as_u64(), &mut len)
        };

        if profile_record_ptr.is_null() {
            // Handle null pointer (e.g., note not found or error occurred)
            return Err(Error::NotFound);
        }

        // Convert the raw pointer to a Note instance
        Ok(ProfileRecord::new_transactional(
            profile_record_ptr,
            len,
            key,
            transaction,
        ))
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
        Ok(ProfileRecord::new_transactional(
            profile_record_ptr,
            len,
            ProfileKey::new(primkey),
            transaction,
        ))
    }

    pub fn get_notekey_by_id(&self, txn: &Transaction, id: &[u8; 32]) -> Result<NoteKey> {
        let res = unsafe {
            bindings::ndb_get_notekey_by_id(
                txn.as_mut_ptr(),
                id.as_ptr() as *const ::std::os::raw::c_uchar,
            )
        };

        if res == 0 {
            return Err(Error::NotFound);
        }

        Ok(NoteKey::new(res))
    }

    pub fn get_profilekey_by_pubkey(
        &self,
        txn: &Transaction,
        pubkey: &[u8; 32],
    ) -> Result<ProfileKey> {
        let res = unsafe {
            bindings::ndb_get_profilekey_by_pubkey(
                txn.as_mut_ptr(),
                pubkey.as_ptr() as *const ::std::os::raw::c_uchar,
            )
        };

        if res == 0 {
            return Err(Error::NotFound);
        }

        Ok(ProfileKey::new(res))
    }

    pub fn get_blocks_by_key<'a>(
        &self,
        txn: &'a Transaction,
        note_key: NoteKey,
    ) -> Result<Blocks<'a>> {
        let blocks_ptr = unsafe {
            bindings::ndb_get_blocks_by_key(self.as_ptr(), txn.as_mut_ptr(), note_key.as_u64())
        };

        if blocks_ptr.is_null() {
            return Err(Error::NotFound);
        }

        Ok(Blocks::new_transactional(blocks_ptr, txn))
    }

    pub fn get_note_by_key<'a>(
        &self,
        transaction: &'a Transaction,
        note_key: NoteKey,
    ) -> Result<Note<'a>> {
        let mut len: usize = 0;

        let note_ptr = unsafe {
            bindings::ndb_get_note_by_key(transaction.as_mut_ptr(), note_key.as_u64(), &mut len)
        };

        if note_ptr.is_null() {
            // Handle null pointer (e.g., note not found or error occurred)
            return Err(Error::NotFound);
        }

        // Convert the raw pointer to a Note instance
        Ok(Note::new_transactional(
            note_ptr,
            len,
            note_key,
            transaction,
        ))
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
        Ok(Note::new_transactional(
            note_ptr,
            len,
            NoteKey::new(primkey),
            transaction,
        ))
    }

    /// Get the underlying pointer to the context in C
    pub fn as_ptr(&self) -> *mut bindings::ndb {
        self.refs.ndb
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
    async fn query_works() {
        let db = "target/testdbs/query";
        test_util::cleanup_db(&db);

        {
            let ndb = Ndb::new(db, &Config::new()).expect("ndb");

            let filter = Filter::new().kinds(vec![1]).build();
            let filters = vec![filter];

            let sub = ndb.subscribe(&filters).expect("sub_id");
            let waiter = ndb.wait_for_notes(sub, 1);
            ndb.process_event(r#"["EVENT","b",{"id": "702555e52e82cc24ad517ba78c21879f6e47a7c0692b9b20df147916ae8731a3","pubkey": "32bf915904bfde2d136ba45dde32c88f4aca863783999faea2e847a8fafd2f15","created_at": 1702675561,"kind": 1,"tags": [],"content": "hello, world","sig": "2275c5f5417abfd644b7bc74f0388d70feb5d08b6f90fa18655dda5c95d013bfbc5258ea77c05b7e40e0ee51d8a2efa931dc7a0ec1db4c0a94519762c6625675"}]"#).expect("process ok");
            let res = waiter.await.expect("await ok");
            assert_eq!(res, vec![NoteKey::new(1)]);
            let txn = Transaction::new(&ndb).expect("txn");
            let res = ndb.query(&txn, &filters, 1).expect("query ok");
            assert_eq!(res.len(), 1);
            assert_eq!(
                hex::encode(res[0].note.id()),
                "702555e52e82cc24ad517ba78c21879f6e47a7c0692b9b20df147916ae8731a3"
            );
        }
    }

    #[tokio::test]
    async fn subscribe_event_works() {
        let db = "target/testdbs/subscribe";
        test_util::cleanup_db(&db);

        {
            let ndb = Ndb::new(db, &Config::new()).expect("ndb");

            let filter = Filter::new().kinds(vec![1]).build();

            let sub = ndb.subscribe(&[filter]).expect("sub_id");
            let waiter = ndb.wait_for_notes(sub, 1);
            ndb.process_event(r#"["EVENT","b",{"id": "702555e52e82cc24ad517ba78c21879f6e47a7c0692b9b20df147916ae8731a3","pubkey": "32bf915904bfde2d136ba45dde32c88f4aca863783999faea2e847a8fafd2f15","created_at": 1702675561,"kind": 1,"tags": [],"content": "hello, world","sig": "2275c5f5417abfd644b7bc74f0388d70feb5d08b6f90fa18655dda5c95d013bfbc5258ea77c05b7e40e0ee51d8a2efa931dc7a0ec1db4c0a94519762c6625675"}]"#).expect("process ok");
            let res = waiter.await.expect("await ok");
            assert_eq!(res, vec![NoteKey::new(1)]);
        }
    }

    #[test]
    fn poll_note_works() {
        let db = "target/testdbs/poll";
        test_util::cleanup_db(&db);

        {
            let ndb = Ndb::new(db, &Config::new()).expect("ndb");

            let filter = Filter::new().kinds(vec![1]).build();

            let sub = ndb.subscribe(&[filter]).expect("sub_id");
            ndb.process_event(r#"["EVENT","b",{"id": "702555e52e82cc24ad517ba78c21879f6e47a7c0692b9b20df147916ae8731a3","pubkey": "32bf915904bfde2d136ba45dde32c88f4aca863783999faea2e847a8fafd2f15","created_at": 1702675561,"kind": 1,"tags": [],"content": "hello, world","sig": "2275c5f5417abfd644b7bc74f0388d70feb5d08b6f90fa18655dda5c95d013bfbc5258ea77c05b7e40e0ee51d8a2efa931dc7a0ec1db4c0a94519762c6625675"}]"#).expect("process ok");
            // this is too fast, we should have nothing
            let res = ndb.poll_for_notes(sub, 1);
            assert_eq!(res, vec![]);

            std::thread::sleep(std::time::Duration::from_millis(150));
            // now we should have something
            let res = ndb.poll_for_notes(sub, 1);
            assert_eq!(res, vec![NoteKey::new(1)]);
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

    #[test]
    #[cfg(target_os = "windows")]
    fn test_windows_large_mapsize() {
        use std::{fs, path::Path};

        let db = "target/testdbs/windows_large_mapsize";
        test_util::cleanup_db(&db);

        {
            // 32 TiB should be way too big for CI
            let config =
                Config::new().set_mapsize(1024usize * 1024usize * 1024usize * 1024usize * 32usize);

            // in this case, nostrdb should try to keep resizing to
            // smaller mapsizes until success

            let ndb = Ndb::new(db, &config);

            assert!(ndb.is_ok());
        }

        let file_len = fs::metadata(Path::new(db).join("data.mdb"))
            .expect("metadata")
            .len();

        assert!(file_len > 0);

        if cfg!(target_os = "windows") {
            // on windows the default mapsize will be 1MB when we fail
            // to open it
            assert_ne!(file_len, 1048576);
        } else {
            assert!(file_len < 1024u64 * 1024u64);
        }

        // we should definitely clean this up... especially on windows
        test_util::cleanup_db(&db);
    }

    #[tokio::test]
    async fn test_stream() {
        let db = "target/testdbs/test_callback";
        test_util::cleanup_db(&db);

        {
            let mut ndb = Ndb::new(db, &Config::new()).expect("ndb");
            let sub_id = {
                let filter = Filter::new().kinds(vec![1]).build();
                let filters = vec![filter];

                let sub_id = ndb.subscribe(&filters).expect("sub_id");
                let mut sub = sub_id.stream(&ndb).notes_per_await(1);

                let res = sub.next();

                ndb.process_event(r#"["EVENT","b",{"id": "702555e52e82cc24ad517ba78c21879f6e47a7c0692b9b20df147916ae8731a3","pubkey": "32bf915904bfde2d136ba45dde32c88f4aca863783999faea2e847a8fafd2f15","created_at": 1702675561,"kind": 1,"tags": [],"content": "hello, world","sig": "2275c5f5417abfd644b7bc74f0388d70feb5d08b6f90fa18655dda5c95d013bfbc5258ea77c05b7e40e0ee51d8a2efa931dc7a0ec1db4c0a94519762c6625675"}]"#).expect("process ok");

                let res = res.await.expect("await ok");
                assert_eq!(res, vec![NoteKey::new(1)]);

                // ensure that unsubscribing kills the stream
                assert!(ndb.unsubscribe(sub_id).is_ok());
                assert!(sub.next().await.is_none());

                assert!(ndb.subs.lock().unwrap().contains_key(&sub_id));
                sub_id
            };

            // ensure subscription state is removed after stream is dropped
            assert!(!ndb.subs.lock().unwrap().contains_key(&sub_id));
        }

        test_util::cleanup_db(&db);
    }
}
