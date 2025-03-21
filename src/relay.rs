use crate::{bindings, NoteKey, Transaction};
use tracing::error;

#[derive(Debug)]
pub struct NoteRelaysIter<'a> {
    _txn: &'a Transaction,
    iter: bindings::ndb_note_relay_iterator,
}

#[derive(Debug)]
pub enum NoteRelays<'a> {
    Empty,
    Active(NoteRelaysIter<'a>),
}

impl<'a> NoteRelays<'a> {
    pub fn empty() -> Self {
        Self::Empty
    }

    pub fn new(txn: &'a Transaction, note_key: NoteKey) -> Self {
        Self::Active(NoteRelaysIter::new(txn, note_key))
    }
}

impl<'a> NoteRelaysIter<'a> {
    pub fn as_mut_ptr(&mut self) -> *mut bindings::ndb_note_relay_iterator {
        &mut self.iter
    }

    pub fn new(txn: &'a Transaction, note_key: NoteKey) -> Self {
        let note_key = note_key.as_u64();
        let mut val = Self {
            _txn: txn,
            iter: empty_iterator(),
        };

        let ok = unsafe {
            bindings::ndb_note_relay_iterate_start(txn.as_mut_ptr(), val.as_mut_ptr(), note_key)
        };

        if ok == 0 {
            // NOTE (jb55): this should never happen, no need to burden the api. let's log just in case?
            error!("error starting note relay iterator? {}", note_key);
        }

        val
    }
}

fn empty_iterator() -> bindings::ndb_note_relay_iterator {
    bindings::ndb_note_relay_iterator {
        txn: std::ptr::null_mut(),
        note_key: 0,
        cursor_op: 0,
        mdb_cur: std::ptr::null_mut(),
    }
}

impl<'a> Iterator for NoteRelays<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<Self::Item> {
        let iter = match self {
            Self::Empty => {
                return None;
            }
            Self::Active(iter) => iter,
        };

        let relay = unsafe { bindings::ndb_note_relay_iterate_next(iter.as_mut_ptr()) };
        if relay.is_null() {
            return None;
        }

        let relay = unsafe {
            let byte_slice = std::slice::from_raw_parts(relay as *const u8, libc::strlen(relay));
            std::str::from_utf8_unchecked(byte_slice)
        };

        Some(relay)
    }
}

impl Drop for NoteRelays<'_> {
    fn drop(&mut self) {
        let iter = match self {
            Self::Empty => {
                return;
            }
            Self::Active(iter) => iter,
        };

        unsafe {
            bindings::ndb_note_relay_iterate_close(iter.as_mut_ptr());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::Config, test_util, IngestMetadata, Ndb};
    use tokio::time::{self, sleep, Duration};

    #[test]
    fn process_event_relays_works() {
        let db = "target/testdbs/relays_work";
        test_util::cleanup_db(&db);

        {
            let ndb = Ndb::new(db, &Config::new()).expect("ndb");
            let eva = r#"["EVENT","s",{"id": "702555e52e82cc24ad517ba78c21879f6e47a7c0692b9b20df147916ae8731a3","pubkey": "32bf915904bfde2d136ba45dde32c88f4aca863783999faea2e847a8fafd2f15","created_at": 1702675561,"kind": 1,"tags": [],"content": "hello, world","sig": "2275c5f5417abfd644b7bc74f0388d70feb5d08b6f90fa18655dda5c95d013bfbc5258ea77c05b7e40e0ee51d8a2efa931dc7a0ec1db4c0a94519762c6625675"}]"#;
            ndb.process_event_with(eva, IngestMetadata::new().client(false).relay("a"))
                .expect("process ok");
            let evb = r#"["EVENT","s",{"id": "702555e52e82cc24ad517ba78c21879f6e47a7c0692b9b20df147916ae8731a3","pubkey": "32bf915904bfde2d136ba45dde32c88f4aca863783999faea2e847a8fafd2f15","created_at": 1702675561,"kind": 1,"tags": [],"content": "hello, world","sig": "2275c5f5417abfd644b7bc74f0388d70feb5d08b6f90fa18655dda5c95d013bfbc5258ea77c05b7e40e0ee51d8a2efa931dc7a0ec1db4c0a94519762c6625675"}]"#;
            ndb.process_event_with(evb, IngestMetadata::new().client(false).relay("b"))
                .expect("process ok");
            let evc = r#"["EVENT","s",{"id": "702555e52e82cc24ad517ba78c21879f6e47a7c0692b9b20df147916ae8731a3","pubkey": "32bf915904bfde2d136ba45dde32c88f4aca863783999faea2e847a8fafd2f15","created_at": 1702675561,"kind": 1,"tags": [],"content": "hello, world","sig": "2275c5f5417abfd644b7bc74f0388d70feb5d08b6f90fa18655dda5c95d013bfbc5258ea77c05b7e40e0ee51d8a2efa931dc7a0ec1db4c0a94519762c6625675"}]"#;
            ndb.process_event_with(evc, IngestMetadata::new().client(false).relay("c"))
                .expect("process ok");
        }

        {
            let ndb = Ndb::new(db, &Config::new()).expect("ndb");
            let id =
                hex::decode("702555e52e82cc24ad517ba78c21879f6e47a7c0692b9b20df147916ae8731a3")
                    .expect("hex id");
            let mut txn = Transaction::new(&ndb).expect("txn");
            let id_bytes: [u8; 32] = id.try_into().expect("id bytes");
            let note = ndb.get_note_by_id(&txn, &id_bytes).expect("note");

            let relays: Vec<&str> = note.relays(&txn).collect();
            assert_eq!(relays, vec!["a", "b", "c"]);

            assert_eq!(note.kind(), 1);
        }
    }
}
