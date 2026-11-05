//! Live-only state machine: the bridge relays only while the stream is live
//! (plus a grace period after it ends).

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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    StreamOnline,
    StreamOffline,
    /// IRC op command; works in both trigger modes as an override.
    ManualStart,
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
    state: State,
}

impl Lifecycle {
    pub fn new(trigger: Trigger, grace: Duration) -> Self {
        Self {
            trigger,
            grace,
            state: State::Offline,
        }
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
            (_, Signal::ManualStart) => State::Live,
            (_, Signal::ManualStop) => State::Offline,
            (State::Grace { until }, Signal::Tick) if now >= until => State::Offline,
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

    #[test]
    fn auto_follows_stream_with_grace_period() {
        let t0 = Instant::now();
        let mut lc = Lifecycle::new(Trigger::Auto, GRACE);
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
        let mut lc = Lifecycle::new(Trigger::Auto, GRACE);
        lc.handle(Signal::StreamOnline, t0);
        lc.handle(Signal::StreamOffline, t0);
        assert_eq!(lc.handle(Signal::StreamOnline, t0), None);
        assert_eq!(lc.state(), State::Live);
        assert_eq!(lc.handle(Signal::Tick, t0 + GRACE * 2), None);
    }

    #[test]
    fn manual_mode_ignores_stream_events() {
        let t0 = Instant::now();
        let mut lc = Lifecycle::new(Trigger::Manual, GRACE);
        assert_eq!(lc.handle(Signal::StreamOnline, t0), None);
        assert_eq!(
            lc.handle(Signal::ManualStart, t0),
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
        let mut lc = Lifecycle::new(Trigger::Auto, GRACE);
        lc.handle(Signal::StreamOnline, t0);
        assert_eq!(
            lc.handle(Signal::ManualStop, t0),
            Some(Transition::WentOffline)
        );
    }
}
