use crate::{Error, Ndb, NoteKey, Result, Subscription};

use std::{
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

use futures::{Stream, StreamExt};
use tracing::error;

/// Used to track query futures
#[derive(Debug, Clone)]
pub(crate) struct SubscriptionState {
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

    /// Await `n` notes on this stream, summing each delivered batch, giving up
    /// after `timeout`.
    ///
    /// This borrows the stream rather than building one, so the subscription
    /// outlives the call and the same stream can be awaited again. That is the
    /// shape a test harness needs — subscribe once, then after each write await
    /// exactly the number of events that write produces — and it is the one
    /// thing the [`Ndb::wait_for_all_notes_within`] family cannot do: those
    /// build a stream per call, and this type's `Drop` unsubscribes, so a second
    /// call against the same subscription finds it closed.
    ///
    /// Bounded on purpose. A local ingest commits in microseconds, so a
    /// `timeout` of seconds is a backstop rather than a race: it trips only when
    /// the notes are never coming, and the resulting
    /// [`Error::WaitTimeout`] reports how many did arrive — 0 means the filter
    /// never matched, fewer-than-`n` means `n` was wrong.
    pub async fn wait_for_notes(&mut self, n: usize, timeout: Duration) -> Result<Vec<NoteKey>> {
        let mut notes: Vec<NoteKey> = Vec::new();

        let collect = async {
            while notes.len() < n {
                match self.next().await {
                    Some(batch) => notes.extend(batch),
                    None => return Err(Error::SubscriptionError),
                }
            }
            Ok(())
        };

        match tokio::time::timeout(timeout, collect).await {
            Ok(Ok(())) => Ok(notes),
            Ok(Err(err)) => Err(err),
            Err(_elapsed) => Err(Error::WaitTimeout {
                expected: n as u32,
                seen: notes.len(),
            }),
        }
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

        // Check if we've been unsubscribed while holding the lock,
        // but drop it before the FFI call to avoid deadlocking with
        // the C ingester thread's subscription callback.
        {
            let map = me.ndb.subs.lock().unwrap();
            if let Some(sub_state) = map.get(&me.sub_id) {
                if sub_state.done {
                    return Poll::Ready(None);
                }
            }
        }

        // Poll for notes without holding the Rust mutex. The C function
        // ndb_poll_for_notes may acquire nostrdb's internal subscription
        // mutex, and the ingester callback acquires our Rust subs mutex,
        // so holding both simultaneously would deadlock.
        let notes = me.ndb.poll_for_notes(me.sub_id, me.max_notes);
        if !notes.is_empty() {
            return Poll::Ready(Some(notes));
        }

        // Re-acquire the lock to store the waker
        {
            let mut map = me.ndb.subs.lock().unwrap();
            let sub_state = map.entry(me.sub_id).or_insert(SubscriptionState {
                done: false,
                waker: None,
            });

            if sub_state.done {
                return Poll::Ready(None);
            }

            sub_state.waker = Some(cx.waker().clone());
        }

        // Poll again after registering the waker to close the race window.
        // An event may have arrived between our first poll_for_notes and
        // storing the waker — the callback would have found no waker to
        // trigger. This second poll catches that case.
        let notes = me.ndb.poll_for_notes(me.sub_id, me.max_notes);
        if !notes.is_empty() {
            return Poll::Ready(Some(notes));
        }

        Poll::Pending
    }
}
