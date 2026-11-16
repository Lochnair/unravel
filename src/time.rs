// SPDX-FileCopyrightText: 2022-Present Nils Andreas Svee mailto:contact@lochnair.net (github @Lochnair)
//
// SPDX-License-Identifier: MPL-2.0

use std::{
    marker::PhantomData,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use rustix::time::{ClockId, clock_gettime};

/// A discontinuity in local realtime relative to monotonic time.
/// This says nothing about remote clocks or whether a queue has drained.
#[derive(Clone, Copy, Debug)]
pub struct ClockStep {
    pub detected_at: Instant,
    /// Positive when realtime advanced relative to monotonic time.
    pub delta_secs: f64,
}

pub(crate) const CLOCK_SAMPLE_INTERVAL: Duration = Duration::from_millis(250);
const CLOCK_STEP_THRESHOLD_SECS: f64 = 0.1;
const MAX_CLOCK_SAMPLE_SPAN: Duration = Duration::from_millis(5);

/// Bracket realtime with monotonic reads so descheduling cannot look like a step.
struct ClockPair {
    at: Instant,
    realtime: SystemTime,
}

#[derive(Default)]
pub(crate) struct ClockTracker {
    previous: Option<ClockPair>,
}

impl ClockTracker {
    pub(crate) fn sample(&mut self) -> Option<ClockStep> {
        let before = Instant::now();
        let realtime = SystemTime::now();
        let after = Instant::now();
        self.observe(before, realtime, after)
    }

    fn observe(
        &mut self,
        before: Instant,
        realtime: SystemTime,
        after: Instant,
    ) -> Option<ClockStep> {
        let span = after.duration_since(before);
        if span > MAX_CLOCK_SAMPLE_SPAN {
            // Keep the last reliable pair so a real step isn't lost with this read.
            return None;
        }
        let current = ClockPair {
            at: before + span / 2,
            realtime,
        };
        let previous = self.previous.replace(current)?;
        let current = self.previous.as_ref().unwrap();
        let elapsed = match current.realtime.duration_since(previous.realtime) {
            Ok(elapsed) => elapsed.as_secs_f64(),
            Err(error) => -error.duration().as_secs_f64(),
        };
        let delta_secs = elapsed - current.at.duration_since(previous.at).as_secs_f64();
        // Compare adjacent reliable samples, rather than accumulating slow slew.
        (delta_secs.abs() >= CLOCK_STEP_THRESHOLD_SECS).then_some(ClockStep {
            detected_at: after,
            delta_secs,
        })
    }
}

pub trait Clock {
    const ID: ClockId;
}

pub enum Realtime {}

impl Clock for Realtime {
    const ID: ClockId = ClockId::Realtime;
}

pub struct ClockSample<C> {
    time_s: u64,
    time_ns: u64,
    _clock: PhantomData<C>,
}

impl<C: Clock> ClockSample<C> {
    pub fn now() -> Self {
        let time = clock_gettime(C::ID);

        Self {
            time_s: time.tv_sec as u64,
            time_ns: time.tv_nsec as u64,
            _clock: PhantomData,
        }
    }

    pub fn secs(&self) -> u64 {
        self.time_s
    }

    pub fn nsecs(&self) -> u64 {
        self.time_ns
    }

    pub fn as_nanos(&self) -> u64 {
        self.time_s * 1_000_000_000 + self.time_ns
    }

    pub fn as_secs_f64(&self) -> f64 {
        self.time_s as f64 + self.time_ns as f64 / 1_000_000_000.0
    }
}

impl ClockSample<Realtime> {
    pub fn as_time_since_midnight(&self) -> i64 {
        (self.time_s as i64 % 86_400 * 1000) + self.time_ns as i64 / 1_000_000
    }
}

impl From<SystemTime> for ClockSample<Realtime> {
    fn from(time: SystemTime) -> Self {
        let duration = time
            .duration_since(UNIX_EPOCH)
            .expect("realtime timestamp predates Unix epoch");

        Self {
            time_s: duration.as_secs(),
            time_ns: u64::from(duration.subsec_nanos()),
            _clock: PhantomData,
        }
    }
}
