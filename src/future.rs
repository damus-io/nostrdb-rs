use crate::{Ndb, NoteKey, Subscription};

use std::{
    pin::Pin,
    task::{Context, Poll},
};

use futures::Stream;
use tracing::error;

/// Used to track query futures
#[derive(Debug, Clone)]
pub(crate) struct SubscriptionState {
    pub ready: bool,
    pub done: bool,
    pub waker: Option<std::task::Waker>,
}

/// A subscription that you can .await on. This can enables very clean
/// integration into Rust's async state machinery.
pub struct SubscriptionStream {
    // some handle or state
    // e.g., a reference to a non-blocking API or a shared atomic state
    ndb: Ndb,
    sub_id: Subscription,
    max_notes: u32,
    unsubscribe_on_drop: bool,
}

impl SubscriptionStream {
    pub fn new(ndb: Ndb, sub_id: Subscription) -> Self {
        // Most of the time we only want to fetch a few things. If expecting
        // lots of data, use `set_max_notes_per_await`
        let max_notes = 32;
        let unsubscribe_on_drop = true;
        SubscriptionStream {
            ndb,
            sub_id,
            unsubscribe_on_drop,
            max_notes,
        }
    }

    pub fn notes_per_await(mut self, max_notes: u32) -> Self {
        self.max_notes = max_notes;
        self
    }

    /// Unsubscribe the subscription when this stream goes out of scope. On
    /// by default. Recommended unless you want subscription leaks.
    pub fn unsubscribe_on_drop(mut self, yes: bool) -> Self {
        self.unsubscribe_on_drop = yes;
        self
    }

    pub fn sub_id(&self) -> Subscription {
        self.sub_id
    }
}

impl Drop for SubscriptionStream {
    fn drop(&mut self) {
        // Perform cleanup here, like removing the subscription from the global map
        {
            let mut map = self.ndb.subs.lock().unwrap();
            map.remove(&self.sub_id);
        }
        // unsubscribe
        if let Err(err) = self.ndb.unsubscribe(self.sub_id) {
            error!(
                "Error unsubscribing from {} in SubscriptionStream Drop: {err}",
                self.sub_id.id()
            );
        }
    }
}

impl Stream for SubscriptionStream {
    type Item = Vec<NoteKey>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let pinned = std::pin::pin!(self);
        let me = pinned.as_ref().get_ref();
        let mut map = me.ndb.subs.lock().unwrap();
        let sub_state = map.entry(me.sub_id).or_insert(SubscriptionState {
            ready: false,
            done: false,
            waker: None,
        });

        // we've unsubscribed
        if sub_state.done {
            return Poll::Ready(None);
        }

        if sub_state.ready {
            // Reset ready, fetch notes
            sub_state.ready = false;
            let notes = me.ndb.poll_for_notes(me.sub_id, me.max_notes);
            return Poll::Ready(Some(notes));
        }

        // Not ready yet, store waker
        sub_state.waker = Some(cx.waker().clone());
        std::task::Poll::Pending
    }
}
