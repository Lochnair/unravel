// SPDX-FileCopyrightText: 2026-Present Nils Andreas Svee mailto:contact@lochnair.net (github @Lochnair)
//
// SPDX-License-Identifier: MPL-2.0

use std::fmt::{self, Display};
use std::{net::IpAddr, time::Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MeasurementStreamId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MeasurementSource {
    IcmpEcho,
    IcmpTimestamp,
    Irtt,
}

impl Display for MeasurementSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MeasurementSource::IcmpEcho => write!(f, "icmp-echo"),
            MeasurementSource::IcmpTimestamp => write!(f, "icmp-timestamps"),
            MeasurementSource::Irtt => write!(f, "irtt"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MeasurementStream {
    pub id: MeasurementStreamId,
    pub peer: IpAddr,
    pub source: MeasurementSource,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OneWayClock {
    Uncalibrated,
    Calibrated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct SignedDuration {
    nanos: i64,
}

#[allow(dead_code)]
impl SignedDuration {
    pub const fn from_nanos(nanos: i64) -> Self {
        Self { nanos }
    }

    pub const fn from_millis(millis: i64) -> Self {
        Self {
            nanos: millis * 1_000_000,
        }
    }

    pub const fn as_nanos(self) -> i64 {
        self.nanos
    }

    pub fn as_secs_f64(self) -> f64 {
        self.nanos as f64 / 1_000_000_000.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OneWayLatency {
    pub uplink: SignedDuration,
    pub downlink: SignedDuration,
    pub clock: OneWayClock,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MeasurementRole {
    Active,
    Candidate,
}

/// One completed measurement. Latencies belong only to successful replies.
#[derive(Debug, Clone, Copy)]
pub struct MeasurementEvent {
    pub stream: MeasurementStream,
    pub role: MeasurementRole,
    /// Local monotonic time at which the probe started.
    pub started_at: Instant,
    /// Local receive time for a reply; the producer-reported timeout time for
    /// a loss. IRTT preserves its protocol timeout deadline, not delivery time.
    pub completed_at: Instant,
    pub outcome: MeasurementOutcome,
}

#[derive(Debug, Clone, Copy)]
pub enum MeasurementOutcome {
    Reply {
        /// Signed protocol RTT, when it differs from local elapsed time.
        round_trip: Option<SignedDuration>,
        one_way: Option<OneWayLatency>,
    },
    Loss,
}

impl MeasurementEvent {
    /// RTT in milliseconds for a reply; timeout detection is never an RTT.
    pub fn rtt_ms(&self) -> Option<f64> {
        match self.outcome {
            MeasurementOutcome::Reply { round_trip, .. } => Some(
                round_trip
                    .map(SignedDuration::as_secs_f64)
                    .unwrap_or_else(|| {
                        self.completed_at
                            .duration_since(self.started_at)
                            .as_secs_f64()
                    })
                    * 1000.0,
            ),
            MeasurementOutcome::Loss => None,
        }
    }
}

/// Protocol completion before the engine attaches its currently applied role.
pub struct ProbeCompletion {
    pub stream: MeasurementStream,
    pub started_at: Instant,
    pub completed_at: Instant,
    pub outcome: MeasurementOutcome,
}
