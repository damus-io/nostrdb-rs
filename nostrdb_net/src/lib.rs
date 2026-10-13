mod client;
mod error;
mod filter;
mod keypair;
mod note;
pub mod pns;
pub mod private_note;
mod profile;
mod pubkey;
pub mod relay;
mod replaceable;
pub mod sns;

pub use client::{ClientMessage, EventClientMessage};
pub use error::Error;
pub use filter::Filter;
pub use keypair::{FilledKeypair, FullKeypair, Keypair, KeypairUnowned, SerializableKeypair};
pub use nostr::SecretKey;
pub use note::{Note, NoteId};
pub use profile::ProfileState;
pub use pubkey::{Pubkey, PubkeyRef};
pub use relay::message::{RelayEvent, RelayMessage};
pub use relay::pool::{PoolEvent, PoolEventBuf, RelayPool};
pub use relay::ws::{self, WsEvent, WsMessage, WsReceiver, WsSender};
pub use relay::{Relay, RelayStatus};
pub use replaceable::{query_replaceable, query_replaceable_filtered};

pub type Result<T> = std::result::Result<T, error::Error>;
