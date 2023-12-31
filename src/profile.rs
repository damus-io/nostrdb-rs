use crate::ndb_profile::{root_as_ndb_profile_record_unchecked, NdbProfileRecord};
use crate::Transaction;

pub struct ProfileRecord<'a> {
    pub record: NdbProfileRecord<'a>,
    pub primary_key: u64,
    pub transaction: &'a Transaction,
}

impl<'a> ProfileRecord<'a> {
    pub(crate) fn new(
        ptr: *mut ::std::os::raw::c_void,
        len: usize,
        primary_key: u64,
        transaction: &'a Transaction,
    ) -> ProfileRecord<'a> {
        let record = unsafe {
            let bytes = std::slice::from_raw_parts(ptr as *const u8, len);
            root_as_ndb_profile_record_unchecked(bytes)
        };
        ProfileRecord {
            record,
            transaction,
            primary_key,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_record_words() {
        use crate::config::Config;
        use crate::ndb::Ndb;
        use crate::test_util;

        let db = "target/testdbs/profile_record_works";

        {
            let cfg = Config::new();
            let ndb = Ndb::new(&db, &cfg).unwrap();
            let _ = ndb.process_event(r#"["EVENT","nostril-query",{"content":"{\"nip05\":\"_@jb55.com\",\"website\":\"https://damus.io\",\"name\":\"jb55\",\"about\":\"I made damus, npubs and zaps. banned by apple & the ccp. my notes are not for sale.\",\"lud16\":\"jb55@sendsats.lol\",\"banner\":\"https://nostr.build/i/3d6f22d45d95ecc2c19b1acdec57aa15f2dba9c423b536e26fc62707c125f557.jpg\",\"display_name\":\"Will\",\"picture\":\"https://cdn.jb55.com/img/red-me.jpg\"}","created_at":1700855305,"id":"cad04d11f7fa9c36d57400baca198582dfeb94fa138366c4469e58da9ed60051","kind":0,"pubkey":"32e1827635450ebb3c5a7d12c1f8e7b2b514439ac10a67eef3d9fd9c5c68e245","sig":"7a15e379ff27318460172b4a1d55a13e064c5007d05d5a188e7f60e244a9ed08996cb7676058b88c7a91ae9488f8edc719bc966cb5bf1eb99be44cdb745f915f","tags":[]}]"#);
        }

        // Initialize ndb
        {
            let cfg = Config::new();
            let ndb = Ndb::new(&db, &cfg).expect("db open");
            let mut txn = Transaction::new(&ndb).expect("new txn");

            let pk =
                hex::decode("32e1827635450ebb3c5a7d12c1f8e7b2b514439ac10a67eef3d9fd9c5c68e245")
                    .expect("hex decode");
            let pr = ndb
                .get_profile_by_pubkey(&mut txn, &pk.try_into().expect("bytes"))
                .expect("profile record");

            let profile = pr.record.profile().unwrap();
            assert_eq!(Some("jb55"), profile.name());
        }

        test_util::cleanup_db(db);
    }
}
