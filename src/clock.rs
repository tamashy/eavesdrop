//! Recording clock with start / pause / resume.
//!
//! The audio streams run the whole time (so the UI meters stay live); the writer
//! threads ask the clock where a captured chunk belongs in the recording. Chunks
//! captured before the start or while paused get no position and are dropped, and
//! paused time is cut out of the timeline, so the tracks stay in sync.

use std::{
    sync::Mutex,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Not started yet.
    Ready,
    Recording,
    Paused,
}

pub struct Clock {
    origin: Instant,
    inner: Mutex<Inner>,
}

struct Inner {
    started: bool,
    /// Non-recording intervals in order; the last one may be open (`None` end).
    /// Before the start, a single open interval from `origin`.
    gaps: Vec<(Instant, Option<Instant>)>,
}

impl Clock {
    /// A clock in the `Ready` state (nothing is recorded until `toggle`).
    pub fn new(origin: Instant) -> Self {
        Self {
            origin,
            inner: Mutex::new(Inner {
                started: false,
                gaps: vec![(origin, None)],
            }),
        }
    }

    /// When the clock was created (i.e. when the app was ready to record).
    pub fn origin(&self) -> Instant {
        self.origin
    }

    pub fn state(&self) -> State {
        let st = self.inner.lock().unwrap();
        match (st.started, st.gaps.last()) {
            (false, _) => State::Ready,
            (true, Some((_, None))) => State::Paused,
            (true, _) => State::Recording,
        }
    }

    /// When recording was first started.
    pub fn started_at(&self) -> Option<Instant> {
        let st = self.inner.lock().unwrap();
        if st.started {
            st.gaps.first().and_then(|g| g.1)
        } else {
            None
        }
    }

    /// Whether recording was ever started.
    pub fn started(&self) -> bool {
        self.inner.lock().unwrap().started
    }

    /// Ready/Paused → Recording, Recording → Paused. Returns the new state.
    pub fn toggle(&self) -> State {
        self.toggle_at(Instant::now())
    }

    fn toggle_at(&self, now: Instant) -> State {
        let mut st = self.inner.lock().unwrap();
        match st.gaps.last_mut() {
            Some((_, end @ None)) => {
                *end = Some(now);
                st.started = true;
                State::Recording
            }
            _ => {
                st.gaps.push((now, None));
                State::Paused
            }
        }
    }

    /// Start recording if not already recording.
    pub fn resume(&self) {
        if self.state() != State::Recording {
            self.toggle();
        }
    }

    /// Recorded time up to `at` (pauses excluded) and whether `at` falls in a pause.
    fn split(&self, at: Instant) -> (Duration, bool) {
        let st = self.inner.lock().unwrap();
        let mut t = at.saturating_duration_since(self.origin);
        let mut paused = false;
        for &(s, e) in &st.gaps {
            if at < s {
                break;
            }
            let end = match e {
                Some(e) if e <= at => e,
                _ => {
                    paused = true;
                    at
                }
            };
            t = t.saturating_sub(end - s);
        }
        (t, paused)
    }

    /// Position in the recording of audio captured at `at`, or `None` if it was
    /// captured before the start or during a pause.
    pub fn position(&self, at: Instant) -> Option<Duration> {
        match self.split(at) {
            (_, true) => None,
            (t, false) => Some(t),
        }
    }

    /// Total recorded time so far.
    pub fn recorded(&self) -> Duration {
        self.split(Instant::now()).0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[test]
    fn ready_records_nothing() {
        let o = Instant::now();
        let c = Clock::new(o);
        assert_eq!(c.state(), State::Ready);
        assert_eq!(c.position(o + s(5)), None);
        assert_eq!(c.split(o + s(5)).0, s(0));
    }

    #[test]
    fn start_pause_resume() {
        let o = Instant::now();
        let c = Clock::new(o);
        assert_eq!(c.toggle_at(o + s(10)), State::Recording); // start after 10 s ready
        assert_eq!(c.position(o + s(9)), None);
        assert_eq!(c.position(o + s(15)), Some(s(5)));
        assert_eq!(c.toggle_at(o + s(20)), State::Paused); // 10 s recorded
        assert_eq!(c.state(), State::Paused);
        assert_eq!(c.position(o + s(25)), None);
        assert_eq!(c.split(o + s(25)).0, s(10));
        assert_eq!(c.toggle_at(o + s(30)), State::Recording);
        // A chunk captured during the pause but processed after resume is still dropped.
        assert_eq!(c.position(o + s(25)), None);
        assert_eq!(c.position(o + s(18)), Some(s(8)));
        assert_eq!(c.position(o + s(35)), Some(s(15)));
        assert!(c.started());
    }
}
