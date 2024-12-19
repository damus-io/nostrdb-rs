use crate::ndb_profile::{
    root_as_ndb_profile_record, root_as_ndb_profile_record_unchecked, NdbProfileRecord,
};
use crate::{Error, Result, Transaction};

pub struct TransactionalProfileRecord<'a> {
    pub record: NdbProfileRecord<'a>,
    pub primary_key: ProfileKey,
    pub transaction: &'a Transaction,
}

pub enum ProfileRecord<'a> {
    Transactional(TransactionalProfileRecord<'a>),
    Owned(NdbProfileRecord<'a>),
}

#[derive(Debug, Clone, Copy, Eq, Ord, PartialEq, PartialOrd, Hash)]
pub struct ProfileKey(u64);

impl ProfileKey {
    pub fn as_u64(&self) -> u64 {
        self.0
    }

    pub fn new(key: u64) -> Self {
        ProfileKey(key)
    }
}

impl<'a> ProfileRecord<'a> {
    pub fn record(&self) -> NdbProfileRecord<'a> {
        match self {
            ProfileRecord::Transactional(tr) => tr.record,
            ProfileRecord::Owned(r) => *r,
        }
    }

    pub fn key(&self) -> Option<ProfileKey> {
        match self {
            ProfileRecord::Transactional(tr) => Some(tr.primary_key),
            ProfileRecord::Owned(_) => None,
        }
    }

    pub fn new_owned(root: &'a [u8]) -> Result<ProfileRecord<'a>> {
        let record = root_as_ndb_profile_record(root).map_err(|_| Error::DecodeError)?;
        Ok(ProfileRecord::Owned(record))
    }

    pub(crate) fn new_transactional(
        ptr: *mut ::std::os::raw::c_void,
        len: usize,
        primary_key: ProfileKey,
        transaction: &'a Transaction,
    ) -> ProfileRecord<'a> {
        let record = unsafe {
            let bytes = std::slice::from_raw_parts(ptr as *const u8, len);
            root_as_ndb_profile_record_unchecked(bytes)
        };
        ProfileRecord::Transactional(TransactionalProfileRecord {
            record,
            transaction,
            primary_key,
        })
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

            let profile = pr.record().profile().unwrap();
            assert_eq!(Some("jb55"), profile.name());
        }

        test_util::cleanup_db(db);
    }
}
