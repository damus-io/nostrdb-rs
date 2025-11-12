//! Subscription identifiers returned by nostrdb (see mdBook *CLI Guide → query*).

use crate::{Ndb, SubscriptionStream};

#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
pub struct Subscription(u64);

impl Subscription {
    /// Wrap a raw subscription id (usually from `Ndb::subscribe`).
    pub fn new(id: u64) -> Self {
        Self(id)
    }
    /// Raw subscription id as returned by the C API.
    pub fn id(self) -> u64 {
        self.0
    }

    /// Construct an async `SubscriptionStream` for this subscription.
    pub fn stream(&self, ndb: &Ndb) -> SubscriptionStream {
        SubscriptionStream::new(ndb.clone(), *self)
    }
}
