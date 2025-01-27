use std::ffi::CString;
use std::ptr;

use crate::bindings::ndb_search;
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

    /// Ingest a client-sent event in the form `["EVENT", {"id:"...}]`
    /// This function returns immediately and doesn't provide any information on
    /// if ingestion was successful or not.
    pub fn process_client_event(&self, json: &str) -> Result<()> {
        // Convert the Rust string to a C-style string
        let c_json = CString::new(json).expect("CString::new failed");
        let c_json_ptr = c_json.as_ptr();

        // Get the length of the string
        let len = json.len() as libc::c_int;

        let res = unsafe { bindings::ndb_process_client_event(self.as_ptr(), c_json_ptr, len) };

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

        debug!(
            "unsubscribed from {}, sub count {}",
            sub.id(),
            self.subscription_count()
        );

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

    pub fn search_profile<'a>(
        &self,
        transaction: &'a Transaction,
        search: &str,
        limit: u32,
    ) -> Result<Vec<&'a [u8; 32]>> {
        let mut results = Vec::new();

        let mut ndb_search = ndb_search {
            key: std::ptr::null_mut(),
            profile_key: 0,
            cursor: std::ptr::null_mut(),
        };

        let c_query = CString::new(search).map_err(|_| Error::DecodeError)?;

        let success = unsafe {
            bindings::ndb_search_profile(
                transaction.as_mut_ptr(),
                &mut ndb_search as *mut ndb_search,
                c_query.as_c_str().as_ptr(),
            )
        };

        if success == 0 {
            return Ok(results);
        }

        // Add the first result
        if let Some(key) = unsafe { ndb_search.key.as_ref() } {
            results.push(&key.id);
        }

        // Iterate through additional results up to the limit
        let mut remaining = limit;
        while remaining > 0 {
            let next_success =
                unsafe { bindings::ndb_search_profile_next(&mut ndb_search as *mut ndb_search) };

            if next_success == 0 {
                break;
            }

            if let Some(key) = unsafe { ndb_search.key.as_ref() } {
                results.push(&key.id);
            }

            remaining -= 1;
        }

        unsafe {
            bindings::ndb_search_profile_end(&mut ndb_search as *mut ndb_search);
        }

        Ok(results)
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
    use tokio::time::{self, sleep, Duration};

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
    async fn search_profile_works() {
        let db = "target/testdbs/search_profile";
        test_util::cleanup_db(&db);

        {
            let ndb = Ndb::new(db, &Config::new()).expect("ndb");

            let filter = Filter::new().kinds(vec![0]).build();
            let filters = vec![filter];

            let sub_id = ndb.subscribe(&filters).expect("sub_id");
            let mut sub = sub_id.stream(&ndb).notes_per_await(1);
            ndb.process_event(r#"["EVENT","b",{  "id": "0b9f0e14727733e430dcb00c69b12a76a1e100f419ce369df837f7eb33e4523c",  "pubkey": "3f770d65d3a764a9c5cb503ae123e62ec7598ad035d836e2a810f3877a745b24",  "created_at": 1736785355,  "kind": 0,  "tags": [    [      "alt",      "User profile for Derek Ross"    ],    [      "i",      "twitter:derekmross",      "1634343988407726081"    ],    [      "i",      "github:derekross",      "3edaf845975fa4500496a15039323fa3I"    ]  ],  "content": "{\"about\":\"Building NostrPlebs.com and NostrNests.com. The purple pill helps the orange pill go down. Nostr is the social glue that binds all of your apps together.\",\"banner\":\"https://i.nostr.build/O2JE.jpg\",\"display_name\":\"Derek Ross\",\"lud16\":\"derekross@strike.me\",\"name\":\"Derek Ross\",\"nip05\":\"derekross@nostrplebs.com\",\"picture\":\"https://i.nostr.build/MVIJ6OOFSUzzjVEc.jpg\",\"website\":\"https://nostrplebs.com\",\"created_at\":1707238393}",  "sig": "51e1225ccaf9b6739861dc218ac29045b09d5cf3a51b0ac6ea64bd36827d2d4394244e5f58a4e4a324c84eeda060e1a27e267e0d536e5a0e45b0b6bdc2c43bbc"}]"#).unwrap();
            ndb.process_event(r#"["EVENT","b",{  "id": "232a02ec7e1b2febf85370b52ed49bf34e2701c385c3d563511508dcf0767bcf",  "pubkey": "4a0510f26880d40e432f4865cb5714d9d3c200ca6ebb16b418ae6c555f574967",  "created_at": 1736017863,  "kind": 0,  "tags": [    [      "client",      "Damus Notedeck"    ]  ],  "content": "{\"display_name\":\"KernelKind\",\"name\":\"KernelKind\",\"about\":\"hello from notedeck!\",\"lud16\":\"kernelkind@getalby.com\"}",  "sig": "18c7dea0da3c30677d6822a31a6dfd9ebc02a18a31d69f0f2ac9ba88409e437d3db0ac433639111df1e4948a6d18451d1582173ee4fcd018d0ec92939f2c1506"}]"#).unwrap();
            ndb.process_event(r#"["EVENT","b",{  "id": "3e9e3b63a7831f09bf2963616a2440e6f30c6e95adbc7841d59376ec100ae9dc",  "pubkey": "32e1827635450ebb3c5a7d12c1f8e7b2b514439ac10a67eef3d9fd9c5c68e245",  "created_at": 1737466417,  "kind": 0,  "tags": [],  "content": "{\"banner\":\"https://nostr.build/i/3d6f22d45d95ecc2c19b1acdec57aa15f2dba9c423b536e26fc62707c125f557.jpg\",\"website\":\"https://damus.io\",\"nip05\":\"_@jb55.com\",\"display_name\":\"\",\"about\":\"I made damus, zaps, and npubs. Bitcoin core, lightning, and nostr dev. \",\"picture\":\"https://cdn.jb55.com/img/red-me.jpg\",\"name\":\"jb55\",\"lud16\":\"jb55@sendsats.lol\"}",  "sig": "9cf1c89a4dbb2888e0f5fc300e56f93eb788bd84d3d0f8b52e4ac4abdd92256b0fb694bfd82d917c3923f01e8eac7886bb75c8043dcd9d4e070e4eaa5ab3bd0a"}]"#).unwrap();
            for _ in 0..3 {
                let _ = sub.next().await;
            }
            let txn = Transaction::new(&ndb).expect("txn");

            let res = ndb.search_profile(&txn, "jb55", 1);
            assert!(res.is_ok());
            let res = res.unwrap();
            assert!(res.len() >= 1);
            let will_bytes: [u8; 32] = [
                0x32, 0xe1, 0x82, 0x76, 0x35, 0x45, 0x0e, 0xbb, 0x3c, 0x5a, 0x7d, 0x12, 0xc1, 0xf8,
                0xe7, 0xb2, 0xb5, 0x14, 0x43, 0x9a, 0xc1, 0x0a, 0x67, 0xee, 0xf3, 0xd9, 0xfd, 0x9c,
                0x5c, 0x68, 0xe2, 0x45,
            ];
            assert_eq!(will_bytes, **res.first().unwrap());

            let res = ndb.search_profile(&txn, "kernel", 1);
            assert!(res.is_ok());
            let res = res.unwrap();
            assert!(res.len() >= 1);
            let kernelkind_bytes: [u8; 32] = [
                0x4a, 0x05, 0x10, 0xf2, 0x68, 0x80, 0xd4, 0x0e, 0x43, 0x2f, 0x48, 0x65, 0xcb, 0x57,
                0x14, 0xd9, 0xd3, 0xc2, 0x00, 0xca, 0x6e, 0xbb, 0x16, 0xb4, 0x18, 0xae, 0x6c, 0x55,
                0x5f, 0x57, 0x49, 0x67,
            ];
            assert_eq!(kernelkind_bytes, **res.first().unwrap());

            let res = ndb.search_profile(&txn, "Derek", 1);
            assert!(res.is_ok());
            let res = res.unwrap();
            assert!(res.len() >= 1);
            let derek_bytes: [u8; 32] = [
                0x3f, 0x77, 0x0d, 0x65, 0xd3, 0xa7, 0x64, 0xa9, 0xc5, 0xcb, 0x50, 0x3a, 0xe1, 0x23,
                0xe6, 0x2e, 0xc7, 0x59, 0x8a, 0xd0, 0x35, 0xd8, 0x36, 0xe2, 0xa8, 0x10, 0xf3, 0x87,
                0x7a, 0x74, 0x5b, 0x24,
            ];
            assert_eq!(derek_bytes, **res.first().unwrap());
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

    #[tokio::test]
    async fn multiple_events_work() {
        let db = "target/testdbs/multiple_events";
        test_util::cleanup_db(&db);

        {
            let ndb = Ndb::new(db, &Config::new()).expect("ndb");

            let filter = Filter::new().kinds(vec![1]).build();

            let sub_id = ndb.subscribe(&[filter]).expect("sub_id");
            let mut sub = sub_id.stream(&ndb).notes_per_await(1);

            ndb.process_event(r#"["EVENT","b",{"id": "702555e52e82cc24ad517ba78c21879f6e47a7c0692b9b20df147916ae8731a3","pubkey": "32bf915904bfde2d136ba45dde32c88f4aca863783999faea2e847a8fafd2f15","created_at": 1702675561,"kind": 1,"tags": [],"content": "hello, world","sig": "2275c5f5417abfd644b7bc74f0388d70feb5d08b6f90fa18655dda5c95d013bfbc5258ea77c05b7e40e0ee51d8a2efa931dc7a0ec1db4c0a94519762c6625675"}]"#).expect("process ok");
            ndb.process_event(r#"["EVENT","b",{"id":"d379f55b520a9b2442556917e2cc7b7c16bfe3f4f08856dcc5735eadb2706267","pubkey":"850605096dbfb50b929e38a6c26c3d56c425325c85e05de29b759bc0e5d6cebc","created_at":1720482500,"kind":1,"tags":[["p","5e7ae588d7d11eac4c25906e6da807e68c6498f49a38e4692be5a089616ceb18"]],"content":"@npub1teawtzxh6y02cnp9jphxm2q8u6xxfx85nguwg6ftuksgjctvavvqnsgq5u Verifying My Public Key: \"ksedgwic\"\n","sig":"3e8683490d951e0f5b3b59835063684d3d159322394d2aad3ee027890dcf8d9ff337027f07ec9c5f9799195466723bc459c67fbf3c902ad40a6b51bcb45d3feb"}]"#).expect("process ok");
            ndb.process_event(r#"["EVENT","b",{"id":"8600bdc1f35ec4662b32609e93cc51a42e5ea9f6b8d656ca9d6b541310052885","pubkey":"dcdc0e77fe223f3f62a476578350133ca97767927df676ca7ca7b92a413a7703","created_at":1734636009,"kind":1,"tags":[],"content":"testing blocked pubkey","sig":"e8949493d81474085cd084d3b81e48b1673fcb2c738a9e7c130915fc85944e787885577b71be6a0822df10f7e823229417774d1e6a66e5cfac9d151f460a5291"}]"#).expect("process ok");

            // this pause causes problems
            sleep(Duration::from_millis(100)).await;

            ndb.process_event(r#"["EVENT","b",{"id":"e3ba832d4399528beb1c677a50d139c94e67220600dd424eb3ad3fa673a45dd5","pubkey":"850605096dbfb50b929e38a6c26c3d56c425325c85e05de29b759bc0e5d6cebc","created_at":1735920949,"kind":1,"tags":[["e","83e37c70a84df8a9b1fe85df15fb892a3852f3a9acc8f9af34449772b1cb07f3","","root"],["e","a3ed05a377b1c1f460fa4e9c2dd393e9563dd2da6955d48287847278d1039277","","reply"],["p","37f2654c028c224b36507facf80c62d53b6c2eebb8d5590aa238d71d3c48723a"],["p","d4bad8c24d4bee499afb08830e71dd103e61e007556d20ba2ef3867fb57136de"],["r","https://meshtastic.org/docs/hardware/devices/"]],"content":"I think anything on this list that runs stock meshtastic should work. You do need a USB connection for the early proof of concept \nhttps://meshtastic.org/docs/hardware/devices/\n\nOthers might have better advice about which are the best though","sig":"85318ea5b83c3316063be82a6e45180767e9ea6b114d0a181dde7d4dc040f2c7f86f8750cc106b66bf666a4ac2debfd8b07c986b7814a715e3ea1cb42626cc68"}]"#).expect("process ok");
            ndb.process_event(r#"["EVENT","b",{"id":"d7ba624865319e95f49c30f5d9644525ab2daaba4e503ecb125798ff038fef13","pubkey":"850605096dbfb50b929e38a6c26c3d56c425325c85e05de29b759bc0e5d6cebc","created_at":1732839586,"kind":1,"tags":[["e","57f1ec61f29d01e2171089aaa86a43694e05ac68507ba7b540e1b968d14f45c2","","root"],["e","77e8e33005b7139901b7e3100eff1043ea4f1faa491c678e8ba9aa3b324011d1"],["e","6eb98593d806ba5fe0ab9aa0e50591af9bbbc7874401183daf59ce788a4bf79f","","reply"],["p","1fccce68f977187c91a7091ece205e214d436eeb8049bc72e266cf4f976d8f77"],["p","32e1827635450ebb3c5a7d12c1f8e7b2b514439ac10a67eef3d9fd9c5c68e245"]],"content":"Works great on Fedora too","sig":"559ac1e852ddedd489fbfc600e4a69f1d182c57fb7dc89e0b3c385cb40ef6e4aff137a34da55b2504798171e957dd39bef57bd3bf946ee70e2eb4023bb446c8b"}]"#).expect("process ok");
            ndb.process_event(r#"["EVENT","b",{"id":"242ae4cf1c719e2c4b656a3aac47c860b1a3ee7bf85c2317e660e27904438b08","pubkey":"850605096dbfb50b929e38a6c26c3d56c425325c85e05de29b759bc0e5d6cebc","created_at":1729652152,"kind":1,"tags":[["e","760f76e66e1046066f134367e2da93f1ac4c8d9d6b7b5e0b990c6725fe8d1442","","root"],["e","85575dbb1aeca2c7875e242351394d9c21ca0bc41946de069b267aeb9e672774","","reply"],["p","7c765d407d3a9d5ea117cb8b8699628560787fc084a0c76afaa449bfbd121d84"],["p","9a0e2043afaa056a12b8bbe77ac4c3185c0e2bc46b12aac158689144323c0e3c"]],"content":"","sig":"3ab9c19640a2efb55510f9ac2e12117582bc94ef985fac33f6f4c6d8fecc3a4e83647a347772aad3cfb12a8ee91649b36feee7b66bc8b61d5232aca29afc4186"}]"#).expect("process ok");

            let timeout_duration = Duration::from_secs(2);
            let result = time::timeout(timeout_duration, async {
                let mut count = 0;
                while count < 6 {
                    let res = sub.next();
                    let _ = res.await.expect("await ok");
                    count += 1;
                    println!("saw an event, count = {}", count);
                }
            })
            .await;

            match result {
                Ok(_) => println!("Test completed successfully"),
                Err(_) => panic!("Test timed out"),
            }
        }
    }

    #[tokio::test]
    async fn multiple_events_with_final_pause_work() {
        let db = "target/testdbs/multiple_events_with_final_pause";
        test_util::cleanup_db(&db);

        {
            let ndb = Ndb::new(db, &Config::new()).expect("ndb");

            let filter = Filter::new().kinds(vec![1]).build();

            let sub_id = ndb.subscribe(&[filter]).expect("sub_id");
            let mut sub = sub_id.stream(&ndb).notes_per_await(1);

            ndb.process_event(r#"["EVENT","b",{"id": "702555e52e82cc24ad517ba78c21879f6e47a7c0692b9b20df147916ae8731a3","pubkey": "32bf915904bfde2d136ba45dde32c88f4aca863783999faea2e847a8fafd2f15","created_at": 1702675561,"kind": 1,"tags": [],"content": "hello, world","sig": "2275c5f5417abfd644b7bc74f0388d70feb5d08b6f90fa18655dda5c95d013bfbc5258ea77c05b7e40e0ee51d8a2efa931dc7a0ec1db4c0a94519762c6625675"}]"#).expect("process ok");
            ndb.process_event(r#"["EVENT","b",{"id":"d379f55b520a9b2442556917e2cc7b7c16bfe3f4f08856dcc5735eadb2706267","pubkey":"850605096dbfb50b929e38a6c26c3d56c425325c85e05de29b759bc0e5d6cebc","created_at":1720482500,"kind":1,"tags":[["p","5e7ae588d7d11eac4c25906e6da807e68c6498f49a38e4692be5a089616ceb18"]],"content":"@npub1teawtzxh6y02cnp9jphxm2q8u6xxfx85nguwg6ftuksgjctvavvqnsgq5u Verifying My Public Key: \"ksedgwic\"\n","sig":"3e8683490d951e0f5b3b59835063684d3d159322394d2aad3ee027890dcf8d9ff337027f07ec9c5f9799195466723bc459c67fbf3c902ad40a6b51bcb45d3feb"}]"#).expect("process ok");
            ndb.process_event(r#"["EVENT","b",{"id":"8600bdc1f35ec4662b32609e93cc51a42e5ea9f6b8d656ca9d6b541310052885","pubkey":"dcdc0e77fe223f3f62a476578350133ca97767927df676ca7ca7b92a413a7703","created_at":1734636009,"kind":1,"tags":[],"content":"testing blocked pubkey","sig":"e8949493d81474085cd084d3b81e48b1673fcb2c738a9e7c130915fc85944e787885577b71be6a0822df10f7e823229417774d1e6a66e5cfac9d151f460a5291"}]"#).expect("process ok");

            sleep(Duration::from_millis(100)).await;

            ndb.process_event(r#"["EVENT","b",{"id":"e3ba832d4399528beb1c677a50d139c94e67220600dd424eb3ad3fa673a45dd5","pubkey":"850605096dbfb50b929e38a6c26c3d56c425325c85e05de29b759bc0e5d6cebc","created_at":1735920949,"kind":1,"tags":[["e","83e37c70a84df8a9b1fe85df15fb892a3852f3a9acc8f9af34449772b1cb07f3","","root"],["e","a3ed05a377b1c1f460fa4e9c2dd393e9563dd2da6955d48287847278d1039277","","reply"],["p","37f2654c028c224b36507facf80c62d53b6c2eebb8d5590aa238d71d3c48723a"],["p","d4bad8c24d4bee499afb08830e71dd103e61e007556d20ba2ef3867fb57136de"],["r","https://meshtastic.org/docs/hardware/devices/"]],"content":"I think anything on this list that runs stock meshtastic should work. You do need a USB connection for the early proof of concept \nhttps://meshtastic.org/docs/hardware/devices/\n\nOthers might have better advice about which are the best though","sig":"85318ea5b83c3316063be82a6e45180767e9ea6b114d0a181dde7d4dc040f2c7f86f8750cc106b66bf666a4ac2debfd8b07c986b7814a715e3ea1cb42626cc68"}]"#).expect("process ok");
            ndb.process_event(r#"["EVENT","b",{"id":"d7ba624865319e95f49c30f5d9644525ab2daaba4e503ecb125798ff038fef13","pubkey":"850605096dbfb50b929e38a6c26c3d56c425325c85e05de29b759bc0e5d6cebc","created_at":1732839586,"kind":1,"tags":[["e","57f1ec61f29d01e2171089aaa86a43694e05ac68507ba7b540e1b968d14f45c2","","root"],["e","77e8e33005b7139901b7e3100eff1043ea4f1faa491c678e8ba9aa3b324011d1"],["e","6eb98593d806ba5fe0ab9aa0e50591af9bbbc7874401183daf59ce788a4bf79f","","reply"],["p","1fccce68f977187c91a7091ece205e214d436eeb8049bc72e266cf4f976d8f77"],["p","32e1827635450ebb3c5a7d12c1f8e7b2b514439ac10a67eef3d9fd9c5c68e245"]],"content":"Works great on Fedora too","sig":"559ac1e852ddedd489fbfc600e4a69f1d182c57fb7dc89e0b3c385cb40ef6e4aff137a34da55b2504798171e957dd39bef57bd3bf946ee70e2eb4023bb446c8b"}]"#).expect("process ok");
            ndb.process_event(r#"["EVENT","b",{"id":"242ae4cf1c719e2c4b656a3aac47c860b1a3ee7bf85c2317e660e27904438b08","pubkey":"850605096dbfb50b929e38a6c26c3d56c425325c85e05de29b759bc0e5d6cebc","created_at":1729652152,"kind":1,"tags":[["e","760f76e66e1046066f134367e2da93f1ac4c8d9d6b7b5e0b990c6725fe8d1442","","root"],["e","85575dbb1aeca2c7875e242351394d9c21ca0bc41946de069b267aeb9e672774","","reply"],["p","7c765d407d3a9d5ea117cb8b8699628560787fc084a0c76afaa449bfbd121d84"],["p","9a0e2043afaa056a12b8bbe77ac4c3185c0e2bc46b12aac158689144323c0e3c"]],"content":"","sig":"3ab9c19640a2efb55510f9ac2e12117582bc94ef985fac33f6f4c6d8fecc3a4e83647a347772aad3cfb12a8ee91649b36feee7b66bc8b61d5232aca29afc4186"}]"#).expect("process ok");

            // this final pause causes extra problems
            sleep(Duration::from_millis(100)).await;

            let timeout_duration = Duration::from_secs(2);
            let result = time::timeout(timeout_duration, async {
                let mut count = 0;
                while count < 6 {
                    let res = sub.next();
                    let _ = res.await.expect("await ok");
                    count += 1;
                    println!("saw an event, count = {}", count);
                }
            })
            .await;

            match result {
                Ok(_) => println!("Test completed successfully"),
                Err(_) => panic!("Test timed out"),
            }
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
    async fn test_unsub_on_drop() {
        let db = "target/testdbs/test_unsub_on_drop";
        test_util::cleanup_db(&db);

        {
            let ndb = Ndb::new(db, &Config::new()).expect("ndb");
            let sub_id = {
                let filter = Filter::new().kinds(vec![1]).build();
                let filters = vec![filter];

                let sub_id = ndb.subscribe(&filters).expect("sub_id");
                let mut sub = sub_id.stream(&ndb).notes_per_await(1);

                let res = sub.next();

                ndb.process_event(r#"["EVENT","b",{"id": "702555e52e82cc24ad517ba78c21879f6e47a7c0692b9b20df147916ae8731a3","pubkey": "32bf915904bfde2d136ba45dde32c88f4aca863783999faea2e847a8fafd2f15","created_at": 1702675561,"kind": 1,"tags": [],"content": "hello, world","sig": "2275c5f5417abfd644b7bc74f0388d70feb5d08b6f90fa18655dda5c95d013bfbc5258ea77c05b7e40e0ee51d8a2efa931dc7a0ec1db4c0a94519762c6625675"}]"#).expect("process ok");

                let res = res.await.expect("await ok");
                assert_eq!(res, vec![NoteKey::new(1)]);

                assert!(ndb.subs.lock().unwrap().contains_key(&sub_id));
                sub_id
            };

            // ensure subscription state is removed after stream is dropped
            assert!(!ndb.subs.lock().unwrap().contains_key(&sub_id));
            assert_eq!(ndb.subscription_count(), 0);
        }

        test_util::cleanup_db(&db);
    }

    #[tokio::test]
    async fn test_stream() {
        let db = "target/testdbs/test_stream";
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
