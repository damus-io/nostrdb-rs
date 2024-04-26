#[allow(non_upper_case_globals)]
#[allow(non_camel_case_types)]
#[allow(non_snake_case)]
#[allow(unused)]
mod bindings;

#[allow(unused)]
#[allow(non_snake_case)]
mod ndb_profile;

mod block;
mod config;
mod error;
mod filter;
mod ndb;
mod ndb_str;
mod note;
mod profile;
mod query;
mod result;
mod subscription;
mod tags;
mod transaction;
mod util;

pub use block::{Block, BlockType, Blocks, Mention};
pub use config::Config;
pub use error::Error;
pub use filter::Filter;
pub use ndb::Ndb;
pub use ndb_profile::{NdbProfile, NdbProfileRecord};
pub use ndb_str::{NdbStr, NdbStrVariant};
pub use note::{Note, NoteKey};
pub use profile::{ProfileKey, ProfileRecord};
pub use query::QueryResult;
pub use result::Result;
pub use subscription::Subscription;
pub use tags::{Tag, TagIter, Tags, TagsIter};
pub use transaction::Transaction;
pub use util::nip10::{Marker, NoteIdRef, NoteReply};

mod test_util;
