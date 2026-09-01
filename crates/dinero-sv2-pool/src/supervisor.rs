//! Watchdog for the template worker.
//!
//! The failure this exists for: on 2026-08-19 the template worker task
//! panicked, the task died, and the main process stayed alive. systemd
//! saw a healthy unit, never restarted it, and the pool sat there
//! accepting connections while serving nobody — a zombie.
//!
//! The specific panic is fixed (`e9bc489`), but "a spawned task can die
//! without taking the process with it" is structural, not specific to
//! that bug. Two independent signals cover it:
//!
//!   1. **Task death** — the supervisor holds the `JoinHandle`. If the
//!      worker returns or panics, the process exits non-zero.
//!   2. **Wedge** — the worker stamps a monotonic heartbeat as it moves
//!      between phases. If that goes stale the process exits non-zero.
//!
//! Exit code MUST be non-zero: the unit is `Restart=on-failure`.
//!
//! Timing note: the RPC client has a 15s per-request timeout, so a slow
//! daemon makes iterations slow, not infinite. The default threshold is
//! deliberately far above that — killing a healthy pool mid-template
//! drops live miners, which is worse than a late restart.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// Where the worker was when it last checked in. Lets an operator tell
/// "wedged in an RPC call" from "loop never turned at all".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Starting = 0,
    PollingTip = 1,
    FetchingTemplate = 2,
    Publishing = 3,
    Sleeping = 4,
}

impl Phase {
    pub fn from_u64(v: u64) -> Phase {
        match v {
            1 => Phase::PollingTip,
            2 => Phase::FetchingTemplate,
            3 => Phase::Publishing,
            4 => Phase::Sleeping,
            _ => Phase::Starting,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Starting => "starting",
            Phase::PollingTip => "polling_tip",
            Phase::FetchingTemplate => "fetching_template",
            Phase::Publishing => "publishing",
            Phase::Sleeping => "sleeping",
        }
    }
}

/// Default staleness threshold. Generous on purpose — see module docs.
pub const DEFAULT_STALL_SECS: u64 = 600;

/// Monotonic liveness stamp. Uses `Instant`, never the wall clock, so
/// an NTP step or a manual clock change can neither trip the watchdog
/// nor mask a real wedge.
#[derive(Debug)]
pub struct Heartbeat {
    origin: Instant,
    last_beat_secs: AtomicU64,
    phase: AtomicU64,
}

impl Heartbeat {
    pub fn new() -> Self {
        Self {
            origin: Instant::now(),
            last_beat_secs: AtomicU64::new(0),
            phase: AtomicU64::new(Phase::Starting as u64),
        }
    }

    /// Called by the worker as it enters each phase.
    pub fn beat(&self, phase: Phase) {
        self.last_beat_secs
            .store(self.origin.elapsed().as_secs(), Ordering::Relaxed);
        self.phase.store(phase as u64, Ordering::Relaxed);
    }

    pub fn phase(&self) -> Phase {
        Phase::from_u64(self.phase.load(Ordering::Relaxed))
    }

    /// Seconds since the last beat.
    pub fn age_secs(&self) -> u64 {
        age_secs(
            self.last_beat_secs.load(Ordering::Relaxed),
            self.origin.elapsed().as_secs(),
        )
    }

    pub fn is_stalled(&self, threshold_secs: u64) -> bool {
        heartbeat_expired(
            self.last_beat_secs.load(Ordering::Relaxed),
            self.origin.elapsed().as_secs(),
            threshold_secs,
        )
    }
}

impl Default for Heartbeat {
    fn default() -> Self {
        Self::new()
    }
}

/// Age of a beat. Saturating: a `now` behind `last_beat` yields 0
/// rather than wrapping to ~u64::MAX and killing a healthy pool.
pub fn age_secs(last_beat_secs: u64, now_secs: u64) -> u64 {
    now_secs.saturating_sub(last_beat_secs)
}

/// Has the worker gone silent? Strictly greater-than, so a beat exactly
/// at the threshold is still considered alive.
pub fn heartbeat_expired(last_beat_secs: u64, now_secs: u64, threshold_secs: u64) -> bool {
    age_secs(last_beat_secs, now_secs) > threshold_secs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_heartbeat_is_not_expired() {
        assert!(!heartbeat_expired(100, 105, 600));
    }

    #[test]
    fn silent_worker_past_threshold_is_expired() {
        assert!(heartbeat_expired(100, 701, 600));
    }

    #[test]
    fn exactly_at_threshold_is_still_alive() {
        // Boundary matters: a pool that beats every N seconds with a
        // threshold of N must never be killed for being punctual.
        assert!(!heartbeat_expired(100, 700, 600));
        assert!(heartbeat_expired(100, 701, 600));
    }

    #[test]
    fn clock_going_backwards_never_trips_the_watchdog() {
        // now < last_beat must saturate to 0, not wrap to u64::MAX.
        assert_eq!(age_secs(500, 100), 0);
        assert!(!heartbeat_expired(500, 100, 600));
    }

    #[test]
    fn a_slow_but_working_iteration_is_not_a_wedge() {
        // Worst realistic iteration: several 15s RPC timeouts back to
        // back. Must stay far below the threshold.
        assert!(!heartbeat_expired(0, 90, DEFAULT_STALL_SECS));
    }

    #[test]
    fn heartbeat_records_phase_and_resets_age() {
        let hb = Heartbeat::new();
        assert_eq!(hb.phase(), Phase::Starting);
        hb.beat(Phase::FetchingTemplate);
        assert_eq!(hb.phase(), Phase::FetchingTemplate);
        assert_eq!(hb.phase().as_str(), "fetching_template");
        assert!(!hb.is_stalled(DEFAULT_STALL_SECS));
        assert!(hb.age_secs() < 5);
    }

    #[test]
    fn phase_round_trips_through_the_atomic() {
        for p in [
            Phase::Starting,
            Phase::PollingTip,
            Phase::FetchingTemplate,
            Phase::Publishing,
            Phase::Sleeping,
        ] {
            assert_eq!(Phase::from_u64(p as u64), p);
        }
    }
}
