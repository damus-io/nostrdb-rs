use crate::bindings;
use crate::error::Error;
use crate::ndb::Ndb;
use crate::result::Result;
use log::debug;

/// A `nostrdb` transaction. Only one is allowed to be active per thread.
#[derive(Debug)]
pub struct Transaction {
    txn: bindings::ndb_txn,
}

impl Transaction {
    /// Create a new `nostrdb` transaction. These are reference counted
    pub fn new(ndb: &Ndb) -> Result<Self> {
        // Initialize your transaction here
        let mut txn = bindings::ndb_txn::new();
        let res = unsafe { bindings::ndb_begin_query(ndb.as_ptr(), &mut txn) };

        if res == 0 {
            return Err(Error::TransactionFailed);
        }

        Ok(Transaction { txn })
    }

    pub fn as_ptr(&self) -> *const bindings::ndb_txn {
        &self.txn
    }

    pub fn as_mut_ptr(&mut self) -> *mut bindings::ndb_txn {
        &mut self.txn
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
        bindings::ndb_txn {
            lmdb: lmdb,
            mdb_txn: mdb_txn,
        }
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
        // Initialize ndb
        {
            let cfg = Config::new();
            let ndb = Ndb::new(".", &cfg).expect("ndb open failed");

            {
                let txn = Transaction::new(&ndb).expect("txn1 failed");
                let txn2 = Transaction::new(&ndb).expect_err("tx2");
                assert!(txn2 == Error::TransactionFailed);
            }

            {
                let txn = Transaction::new(&ndb).expect("txn1 failed");
            }
        }

        test_util::cleanup_db();
    }
}
