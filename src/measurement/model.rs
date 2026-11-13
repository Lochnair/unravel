// SPDX-FileCopyrightText: 2026-Present Nils Andreas Svee mailto:contact@lochnair.net (github @Lochnair)
//
// SPDX-License-Identifier: MPL-2.0

use crate::settings::ReflectorProtocol;
use std::{
    fmt,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::Arc,
    time::Instant,
};

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MeasurementTarget {
    IcmpEcho(Ipv4Addr),
    IcmpTimestamps(Ipv4Addr),
    Irtt {
        endpoint: SocketAddr,
        authentication: Option<Arc<str>>,
    },
}

impl MeasurementTarget {
    pub fn peer(&self) -> IpAddr {
        match self {
            Self::IcmpEcho(peer) | Self::IcmpTimestamps(peer) => (*peer).into(),
            Self::Irtt { endpoint, .. } => endpoint.ip(),
        }
    }

    pub fn protocol(&self) -> ReflectorProtocol {
        match self {
            Self::IcmpEcho(_) => ReflectorProtocol::IcmpEcho,
            Self::IcmpTimestamps(_) => ReflectorProtocol::IcmpTimestamps,
            Self::Irtt { .. } => ReflectorProtocol::Irtt,
        }
    }
}

impl fmt::Debug for MeasurementTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Irtt { endpoint, .. } => write!(f, "irtt({endpoint})"),
            _ => write!(f, "{}({})", self.protocol(), self.peer()),
        }
    }
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
#[derive(Debug, Clone)]
pub struct MeasurementEvent {
    pub target: MeasurementTarget,
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
