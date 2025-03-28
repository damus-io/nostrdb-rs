#[allow(non_upper_case_globals)]
#[allow(non_camel_case_types)]
#[allow(non_snake_case)]
#[allow(unused)]
#[allow(clippy::upper_case_acronyms)]
mod bindings;

#[allow(unused)]
#[allow(non_snake_case)]
#[allow(clippy::needless_lifetimes)]
#[allow(clippy::missing_safety_doc)]
mod ndb_profile;

mod block;

mod future;

mod config;
mod error;
mod filter;
mod ingest;
mod ndb;
mod ndb_str;
mod note;
mod profile;
mod query;
mod relay;
mod result;
mod subscription;
mod tags;
mod transaction;
mod util;

pub use block::{Block, BlockType, Blocks, Mention};
pub use config::Config;
pub use error::{Error, FilterError};
pub use filter::{Filter, FilterBuilder, FilterElement, FilterField, MutFilterField};
pub(crate) use future::SubscriptionState;
pub use future::SubscriptionStream;
pub use ingest::IngestMetadata;
pub use ndb::Ndb;
pub use ndb_profile::{NdbProfile, NdbProfileRecord};
pub use ndb_str::{NdbStr, NdbStrVariant};
pub use note::{Note, NoteBuildOptions, NoteBuilder, NoteKey};
pub use profile::{ProfileKey, ProfileRecord};
pub use query::QueryResult;
pub use relay::NoteRelays;
pub use result::Result;
pub use subscription::Subscription;
pub use tags::{Tag, TagIter, Tags, TagsIter};
pub use transaction::Transaction;
pub use util::nip10::{Marker, NoteIdRef, NoteIdRefBuf, NoteReply, NoteReplyBuf};

mod test_util;
