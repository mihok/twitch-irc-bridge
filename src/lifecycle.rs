//! Live-only state machine: the bridge relays only while the stream is live
//! (plus a grace period after it ends), or while an op has started it by hand
//! (`!bridge start [30m|forever]`, for testing or off-stream moments).

use std::time::{Duration, Instant};

use crate::config::Trigger;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Offline,
    Live,
    /// Stream ended; still relaying until the deadline.
    Grace {
        until: Instant,
    },
    /// Started by an op, independent of the stream; stops at `until` (if any) or on
    /// `!bridge stop`. The stream going live turns it into `Live`.
    Manual {
        until: Option<Instant>,
    },
}

/// How long a manual start lasts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunFor {
    /// `lifecycle.manual_timeout` (none if that's 0).
    Default,
    For(Duration),
    /// Until `!bridge stop`.
    Forever,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    StreamOnline,
    StreamOffline,
    /// IRC op command; works in both trigger modes as an override.
    ManualStart(RunFor),
    ManualStop,
    Tick,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transition {
    WentLive,
    WentOffline,
}

#[derive(Debug)]
pub struct Lifecycle {
    trigger: Trigger,
    grace: Duration,
    /// Default length of a manual start; `None` = until stopped.
    manual_timeout: Option<Duration>,
    state: State,
}

impl Lifecycle {
    pub fn new(trigger: Trigger, grace: Duration, manual_timeout: Option<Duration>) -> Self {
        Self {
            trigger,
            grace,
            manual_timeout,
            state: State::Offline,
        }
    }

    pub fn trigger(&self) -> Trigger {
        self.trigger
    }

    pub fn state(&self) -> State {
        self.state
    }

    pub fn is_relaying(&self) -> bool {
        !matches!(self.state, State::Offline)
    }

    pub fn handle(&mut self, signal: Signal, now: Instant) -> Option<Transition> {
        let was_relaying = self.is_relaying();
        self.state = match (self.state, signal) {
            (_, Signal::StreamOnline) if self.trigger == Trigger::Auto => State::Live,
            (State::Live, Signal::StreamOffline) if self.trigger == Trigger::Auto => State::Grace {
                until: now + self.grace,
            },
            // Already relaying for a live stream: a manual start changes nothing.
            (State::Live, Signal::ManualStart(_)) => State::Live,
            (_, Signal::ManualStart(run)) => State::Manual {
                until: match run {
                    RunFor::Default => self.manual_timeout.map(|d| now + d),
                    RunFor::For(d) => Some(now + d),
                    RunFor::Forever => None,
                },
            },
            (_, Signal::ManualStop) => State::Offline,
            (State::Grace { until }, Signal::Tick) if now >= until => State::Offline,
            (State::Manual { until: Some(until) }, Signal::Tick) if now >= until => State::Offline,
            (s, _) => s,
        };
        match (was_relaying, self.is_relaying()) {
            (false, true) => Some(Transition::WentLive),
            (true, false) => Some(Transition::WentOffline),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GRACE: Duration = Duration::from_secs(600);
    const HOUR: Duration = Duration::from_secs(3600);

    #[test]
    fn auto_follows_stream_with_grace_period() {
        let t0 = Instant::now();
        let mut lc = Lifecycle::new(Trigger::Auto, GRACE, Some(HOUR));
        assert_eq!(
            lc.handle(Signal::StreamOnline, t0),
            Some(Transition::WentLive)
        );
        assert_eq!(lc.handle(Signal::StreamOffline, t0), None);
        assert!(lc.is_relaying());
        assert_eq!(lc.handle(Signal::Tick, t0 + GRACE / 2), None);
        assert_eq!(
            lc.handle(Signal::Tick, t0 + GRACE),
            Some(Transition::WentOffline)
        );
    }

    #[test]
    fn going_live_again_during_grace_cancels_it() {
        let t0 = Instant::now();
        let mut lc = Lifecycle::new(Trigger::Auto, GRACE, Some(HOUR));
        lc.handle(Signal::StreamOnline, t0);
        lc.handle(Signal::StreamOffline, t0);
        assert_eq!(lc.handle(Signal::StreamOnline, t0), None);
        assert_eq!(lc.state(), State::Live);
        assert_eq!(lc.handle(Signal::Tick, t0 + GRACE * 2), None);
    }

    #[test]
    fn manual_mode_ignores_stream_events() {
        let t0 = Instant::now();
        let mut lc = Lifecycle::new(Trigger::Manual, GRACE, Some(HOUR));
        assert_eq!(lc.handle(Signal::StreamOnline, t0), None);
        assert_eq!(
            lc.handle(Signal::ManualStart(RunFor::Default), t0),
            Some(Transition::WentLive)
        );
        assert_eq!(lc.handle(Signal::StreamOffline, t0), None);
        assert_eq!(
            lc.handle(Signal::ManualStop, t0),
            Some(Transition::WentOffline)
        );
    }

    #[test]
    fn manual_stop_overrides_auto() {
        let t0 = Instant::now();
        let mut lc = Lifecycle::new(Trigger::Auto, GRACE, Some(HOUR));
        lc.handle(Signal::StreamOnline, t0);
        assert_eq!(
            lc.handle(Signal::ManualStop, t0),
            Some(Transition::WentOffline)
        );
    }

    #[test]
    fn manual_start_times_out() {
        let t0 = Instant::now();
        let mut lc = Lifecycle::new(Trigger::Auto, GRACE, Some(HOUR));
        assert_eq!(
            lc.handle(Signal::ManualStart(RunFor::Default), t0),
            Some(Transition::WentLive)
        );
        assert_eq!(
            lc.state(),
            State::Manual {
                until: Some(t0 + HOUR)
            }
        );
        assert_eq!(lc.handle(Signal::Tick, t0 + HOUR / 2), None);
        assert_eq!(
            lc.handle(Signal::Tick, t0 + HOUR),
            Some(Transition::WentOffline)
        );
        // An explicit length, and "forever".
        let half = Duration::from_secs(1800);
        lc.handle(Signal::ManualStart(RunFor::For(half)), t0);
        assert_eq!(
            lc.state(),
            State::Manual {
                until: Some(t0 + half)
            }
        );
        lc.handle(Signal::ManualStart(RunFor::Forever), t0);
        assert_eq!(lc.handle(Signal::Tick, t0 + HOUR * 100), None);
        assert!(lc.is_relaying());
        // No default timeout configured: until stopped.
        let mut lc = Lifecycle::new(Trigger::Manual, GRACE, None);
        lc.handle(Signal::ManualStart(RunFor::Default), t0);
        assert_eq!(lc.state(), State::Manual { until: None });
    }

    #[test]
    fn manual_start_is_independent_of_the_stream_until_it_goes_live() {
        let t0 = Instant::now();
        let mut lc = Lifecycle::new(Trigger::Auto, GRACE, Some(HOUR));
        lc.handle(Signal::ManualStart(RunFor::Default), t0);
        // A stray stream.offline doesn't end a manual run...
        assert_eq!(lc.handle(Signal::StreamOffline, t0), None);
        assert!(matches!(lc.state(), State::Manual { .. }));
        // ...but going live takes over: no timer, and the normal grace period after.
        assert_eq!(lc.handle(Signal::StreamOnline, t0), None);
        assert_eq!(lc.state(), State::Live);
        assert_eq!(lc.handle(Signal::Tick, t0 + HOUR * 2), None);
        // A manual start while live keeps it live (and the stream's lifecycle).
        lc.handle(Signal::ManualStart(RunFor::For(GRACE)), t0);
        assert_eq!(lc.state(), State::Live);
    }
}
