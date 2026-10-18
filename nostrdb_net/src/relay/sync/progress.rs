//! A progress observer for the one-shot reconcile/pull paths, so a caller can show
//! what a sync is doing instead of hanging silently while it runs.
//!
//! The paths call [`Progress::event`] at each observable step — a negentropy round
//! trip, a reconcile's need count, a fetch chunk landing, a push, a windowed pull's
//! coverage. `()` is the no-op observer the plain entry points pass, so observing
//! is opt-in and costs nothing when unused.
//!
//! The `_with_progress` entry points take `progress: &P` with `P: Progress +
//! ?Sized` rather than `&dyn Progress`, so the returned future's `Send`-ness
//! follows the caller's type: a `Sync` observer (atomics, a channel sender) keeps
//! [`pull_reconcile_windowed_with_progress`](super::pull_reconcile_windowed_with_progress)
//! `Send` for a multi-thread runtime, while a CLI on a current-thread runtime can
//! pass a `Cell`/`RefCell` adapter.

/// One observable step of a sync.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncEvent {
    /// A `NEG-MSG` round trip finished.
    Round,
    /// A reconcile finished: this many ids to fetch in this (window's) pass.
    Need { count: usize },
    /// A fetch landed (queryable): this many more events.
    Fetched { count: usize },
    /// This many local events were pushed to the relay.
    Pushed { count: usize },
    /// A windowed pull finished a window. `covered_secs`/`span_secs` are the
    /// cumulative `created_at` seconds reconciled vs the whole range, with the
    /// range's upper bound clamped to now — callers pass `u32::MAX` as the upper
    /// bound, so the unclamped span would be meaningless as a fraction.
    Window { covered_secs: u64, span_secs: u64 },
    /// A window was over the relay's per-sync cap and got bisected.
    Split,
}

/// An observer of a sync's [`SyncEvent`]s.
///
/// Takes `&self` so one observer can be shared by reference down the call chain;
/// an implementation that records needs interior mutability (a `Cell`, a
/// `RefCell`, atomics).
pub trait Progress {
    /// Called at each observable step of a sync.
    fn event(&self, ev: SyncEvent);
}

/// The no-op observer, passed by the plain (non-`_with_progress`) entry points.
impl Progress for () {
    fn event(&self, _: SyncEvent) {}
}

/// Running coverage of a windowed reconcile's `created_at` range, so each
/// finished window can report a [`SyncEvent::Window`] fraction.
///
/// The span is `[since, min(until, now)]` inclusive: windowed callers search up
/// to `u32::MAX` (year 2106) so events stamped ahead of this clock aren't missed,
/// but measured against that bound every real window would read as ~0%. Each
/// finished window contributes only its overlap with the clamped span, and the
/// windows partition the full search range, so `covered_secs` reaches `span_secs`
/// exactly when the last window finishes.
pub(super) struct Coverage {
    /// Lower bound of the clamped span (inclusive).
    since: u64,
    /// Upper bound of the clamped span (inclusive).
    until: u64,
    /// Seconds of the clamped span finished so far.
    covered: u64,
}

impl Coverage {
    /// Coverage of the search range `[since, until]`, clamped to `now` (unix
    /// seconds).
    pub(super) fn new(since: u64, until: u64, now: u64) -> Self {
        Coverage {
            since,
            until: until.min(now),
            covered: 0,
        }
    }

    /// Coverage of `[since, until]` clamped to the current wall-clock time.
    pub(super) fn until_now(since: u64, until: u64) -> Self {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(u64::MAX);
        Self::new(since, until, now)
    }

    /// Seconds in the clamped span (0 when it is empty).
    fn span(&self) -> u64 {
        if self.until < self.since {
            return 0;
        }
        self.until - self.since + 1
    }

    /// Mark the window `[since, until]` (inclusive) finished and report the new
    /// coverage to `progress`.
    pub(super) fn finish<P: Progress + ?Sized>(&mut self, since: u64, until: u64, progress: &P) {
        let lo = since.max(self.since);
        let hi = until.min(self.until);
        if hi >= lo {
            self.covered += hi - lo + 1;
        }
        progress.event(SyncEvent::Window {
            covered_secs: self.covered,
            span_secs: self.span(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// Records every event's coverage, for checking [`Coverage`] directly.
    #[derive(Default)]
    struct Windows(RefCell<Vec<SyncEvent>>);

    impl Progress for Windows {
        fn event(&self, ev: SyncEvent) {
            self.0.borrow_mut().push(ev);
        }
    }

    /// Windows that run past `now` (the `u32::MAX` upper bound callers pass)
    /// count only their part inside the clamped span, and the partition of the
    /// whole range ends at exactly 100%.
    #[test]
    fn coverage_clamps_to_now_and_completes() {
        let rec = Windows::default();
        let mut cov = Coverage::new(0, u32::MAX as u64, 99);
        // A bisection of [0, u32::MAX]: the upper half is all future.
        let mid = (u32::MAX as u64) / 2;
        cov.finish(mid + 1, u32::MAX as u64, &rec);
        cov.finish(50, mid, &rec);
        cov.finish(0, 49, &rec);
        assert_eq!(
            *rec.0.borrow(),
            vec![
                SyncEvent::Window {
                    covered_secs: 0,
                    span_secs: 100
                },
                SyncEvent::Window {
                    covered_secs: 50,
                    span_secs: 100
                },
                SyncEvent::Window {
                    covered_secs: 100,
                    span_secs: 100
                },
            ]
        );
    }
}
