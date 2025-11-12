//! Read-only query transactions. See mdBook *Architecture → Query path*.

use crate::bindings;
use crate::error::Error;
use crate::ndb::Ndb;
use crate::result::Result;

/// Read-only LMDB transaction (nostrdb enforces one per thread).
#[derive(Debug)]
pub struct Transaction {
    txn: bindings::ndb_txn,
}

impl Transaction {
    /// Begin a new `ndb_begin_query` session scoped to the current thread.
    pub fn new(ndb: &Ndb) -> Result<Self> {
        // Initialize your transaction here
        let mut txn = bindings::ndb_txn::new();
        let res = unsafe { bindings::ndb_begin_query(ndb.as_ptr(), &mut txn) };

        if res == 0 {
            return Err(Error::TransactionFailed);
        }

        Ok(Transaction { txn })
    }

    /// Raw pointer for FFI calls. Borrowed; do not free.
    pub fn as_ptr(&self) -> *const bindings::ndb_txn {
        &self.txn
    }

    /// Mutable raw pointer (e.g., `ndb_query` expects one).
    pub fn as_mut_ptr(&self) -> *mut bindings::ndb_txn {
        self.as_ptr() as *mut bindings::ndb_txn
    }
}

impl Drop for Transaction {
    fn drop(&mut self) {
        // End the transaction
        unsafe {
            // Replace with your actual function
            bindings::ndb_end_query(&mut self.txn);
        }
    }
}

impl bindings::ndb_txn {
    fn new() -> Self {
        // just create something uninitialized. ndb_begin_query will initialize it for us
        let lmdb: *mut bindings::ndb_lmdb = std::ptr::null_mut();
        let mdb_txn: *mut ::std::os::raw::c_void = std::ptr::null_mut();
        bindings::ndb_txn { lmdb, mdb_txn }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::ndb::Ndb;
    use crate::test_util;

    #[test]
    fn transaction_inheritence_fails() {
        let db = "target/testdbs/txn_inheritence_fails";
        // Initialize ndb
        {
            let cfg = Config::new();
            let ndb = Ndb::new(db, &cfg).expect("ndb open failed");

            {
                let _txn = Transaction::new(&ndb).expect("txn1 failed");
                let txn2 = Transaction::new(&ndb).expect_err("tx2");
                assert!(matches!(txn2, Error::TransactionFailed));
            }

            {
                let _txn = Transaction::new(&ndb).expect("txn1 failed");
            }
        }

        test_util::cleanup_db(db);
    }
}
