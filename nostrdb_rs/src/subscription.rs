use crate::{Ndb, SubscriptionStream};

#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
pub struct Subscription(u64);

impl Subscription {
    pub fn new(id: u64) -> Self {
        Self(id)
    }
    pub fn id(self) -> u64 {
        self.0
    }

    pub fn stream(&self, ndb: &Ndb) -> SubscriptionStream {
        SubscriptionStream::new(ndb.clone(), *self)
    }
}
