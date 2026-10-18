//! The `created_at` windows a windowed reconcile bisects over, written once for
//! both [`pull_reconcile_windowed`](super::pull_reconcile_windowed) and the
//! bidirectional over-cap reconcile.
//!
//! A window is inclusive on both ends, NIP-01's meaning, which is how the relay
//! answers its wire filter. The local side must match the same events, so its set
//! is built through [`ndb_filter_from_wire`](crate::ndb_filter_from_wire), which
//! corrects for nostrdb's exclusive `until`. Splitting `[since, until]` into
//! `[since, mid]` and `[mid + 1, until]` then partitions it: every second is in
//! exactly one half, so no event on a boundary is skipped or fetched twice.

use super::progress::{Coverage, Progress, SyncEvent};
use serde_json::{Value, json};

/// An inclusive `created_at` range `[since, until]`, in unix seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct CreatedAtWindow {
    /// First second in the window.
    pub since: u64,
    /// Last second in the window, inclusive.
    pub until: u64,
}

/// The two disjoint windows a bisection makes of one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Halves {
    /// `[since, mid]`.
    pub lower: CreatedAtWindow,
    /// `[mid + 1, until]`.
    pub upper: CreatedAtWindow,
}

impl CreatedAtWindow {
    /// The wire filter for this window: `base` with its `since`/`until`
    /// overwritten by the window's. Object-key assignment replaces any inherited
    /// bound, so no field is duplicated.
    pub fn wire_json(&self, base: &Value) -> String {
        let mut obj = base.clone();
        obj["since"] = json!(self.since);
        obj["until"] = json!(self.until);
        obj.to_string()
    }

    /// Split into two disjoint halves that together cover the window, or `None`
    /// for a window of one second (or an empty one), which can't be split.
    pub fn split(&self) -> Option<Halves> {
        if self.until <= self.since {
            return None;
        }
        let mid = self.since + (self.until - self.since) / 2;
        Some(Halves {
            lower: CreatedAtWindow {
                since: self.since,
                until: mid,
            },
            upper: CreatedAtWindow {
                since: mid + 1,
                until: self.until,
            },
        })
    }
}

/// The windows still to reconcile, as a LIFO stack, and the coverage of the ones
/// finished, which each finish reports as a [`SyncEvent::Window`].
pub(super) struct WindowStack {
    windows: Vec<CreatedAtWindow>,
    coverage: Coverage,
}

impl WindowStack {
    /// A stack holding the whole `range`, its coverage clamped to now.
    pub fn new(range: CreatedAtWindow) -> Self {
        WindowStack {
            windows: vec![range],
            coverage: Coverage::until_now(range.since, range.until),
        }
    }

    /// The next window to reconcile, or `None` when the range is done.
    pub fn pop(&mut self) -> Option<CreatedAtWindow> {
        self.windows.pop()
    }

    /// Replace `window`, which the relay refused as over its cap, with its two
    /// halves, and report a [`SyncEvent::Split`]. Returns `false`, pushing
    /// nothing, for a one-second window that can't be split; the caller handles
    /// it some other way and then [`finish`](Self::finish)es it.
    pub fn bisect<P: Progress + ?Sized>(&mut self, window: CreatedAtWindow, progress: &P) -> bool {
        let Some(halves) = window.split() else {
            return false;
        };
        progress.event(SyncEvent::Split);
        // Lower first, so the upper half pops next: the range syncs newest
        // first, and recent history lands before old history.
        self.windows.push(halves.lower);
        self.windows.push(halves.upper);
        true
    }

    /// Mark `window` reconciled and report the new coverage.
    pub fn finish<P: Progress + ?Sized>(&mut self, window: CreatedAtWindow, progress: &P) {
        self.coverage.finish(window.since, window.until, progress);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(since: u64, until: u64) -> CreatedAtWindow {
        CreatedAtWindow { since, until }
    }

    /// Halves meet without a gap or an overlap, down to one-second windows,
    /// which don't split.
    #[test]
    fn split_partitions_the_window() {
        assert_eq!(
            w(0, 9).split(),
            Some(Halves {
                lower: w(0, 4),
                upper: w(5, 9)
            })
        );
        assert_eq!(
            w(7, 8).split(),
            Some(Halves {
                lower: w(7, 7),
                upper: w(8, 8)
            })
        );
        assert_eq!(w(7, 7).split(), None);
        let top = u32::MAX as u64;
        let halves = w(0, top).split().expect("splits");
        assert_eq!(halves.lower.until + 1, halves.upper.since);
        assert_eq!(halves.upper.until, top);
    }

    /// Bisecting down to single seconds visits every second exactly once, newest
    /// first.
    #[test]
    fn bisecting_to_seconds_covers_every_second_once() {
        let mut stack = WindowStack::new(w(100, 163));
        let mut seen = Vec::new();
        while let Some(window) = stack.pop() {
            if !stack.bisect(window, &()) {
                seen.push(window.since);
                stack.finish(window, &());
            }
        }
        assert_eq!(seen, (100..=163).rev().collect::<Vec<_>>());
    }

    /// The wire JSON carries the window's bounds over any in `base`.
    #[test]
    fn wire_json_overrides_base_bounds() {
        let base = json!({"kinds": [1], "since": 1, "until": 2});
        let got: Value = serde_json::from_str(&w(10, 20).wire_json(&base)).expect("json");
        assert_eq!(got, json!({"kinds": [1], "since": 10, "until": 20}));
    }
}
