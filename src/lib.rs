#[allow(non_upper_case_globals)]
#[allow(non_camel_case_types)]
#[allow(non_snake_case)]
#[allow(unused)]
mod bindings;

#[allow(unused)]
#[allow(non_snake_case)]
mod ndb_profile;

pub type Profile<'a> = ndb_profile::NdbProfile<'a>;

pub mod config;
pub mod error;
pub mod ndb;
pub mod note;
pub mod result;
pub mod transaction;

pub use config::Config;
pub use error::Error;
pub use ndb::Ndb;
pub use note::Note;
pub use result::Result;
pub use transaction::Transaction;

mod test_util;
