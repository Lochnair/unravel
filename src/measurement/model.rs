// SPDX-FileCopyrightText: 2026-Present Nils Andreas Svee mailto:contact@lochnair.net (github @Lochnair)
//
// SPDX-License-Identifier: MPL-2.0

use serde::{Deserialize, Deserializer, de::Error};
use std::{
    fmt,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::Arc,
    time::{Duration, Instant},
};

/// Declared, and numbered, in protocol preference order: within a class's RTT
/// shortlist the lowest discriminant wins. `Irtt` declares `endpoint` before
/// `authentication`, so definitions of one endpoint sort next to each other.
#[derive(Clone, Deserialize, schemars::JsonSchema, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(tag = "protocol", rename_all = "kebab-case", deny_unknown_fields)]
#[repr(u8)]
pub enum MeasurementTarget {
    Irtt {
        endpoint: SocketAddr,
        #[serde(default, deserialize_with = "nonblank")]
        authentication: Option<Arc<str>>,
    } = 0,
    IcmpTimestamps {
        endpoint: Ipv4Addr,
    } = 1,
    IcmpEcho {
        endpoint: Ipv4Addr,
    } = 2,
}

impl MeasurementTarget {
    pub fn peer(&self) -> IpAddr {
        match self {
            Self::IcmpEcho { endpoint } | Self::IcmpTimestamps { endpoint } => (*endpoint).into(),
            Self::Irtt { endpoint, .. } => endpoint.ip(),
        }
    }

    /// The IP, or `ip:port` for IRTT, where one IP can serve several
    /// endpoints. With the protocol it identifies the target.
    #[cfg_attr(
        not(any(feature = "lua", all(feature = "ubus", unix))),
        allow(dead_code)
    )]
    pub fn name(&self) -> String {
        match self {
            Self::Irtt { endpoint, .. } => endpoint.to_string(),
            target => target.peer().to_string(),
        }
    }

    /// The external protocol label, as configuration spells it.
    pub fn protocol_name(&self) -> &'static str {
        match self {
            Self::Irtt { .. } => "irtt",
            Self::IcmpTimestamps { .. } => "icmp-timestamps",
            Self::IcmpEcho { .. } => "icmp-echo",
        }
    }
}

/// A blank key means an unauthenticated session; any other is kept byte for byte.
/// Serde's own type errors quote the value, so they are replaced: an unquoted
/// numeric key would otherwise reach the startup log.
fn nonblank<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<Arc<str>>, D::Error> {
    let key = Option::<Arc<str>>::deserialize(deserializer)
        .map_err(|_| D::Error::custom("authentication must be a string"))?;
    Ok(key.filter(|key| !key.trim().is_empty()))
}

// Never derived: authentication keys stay out of logs and errors.
impl fmt::Debug for MeasurementTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Irtt { endpoint, .. } => write!(f, "irtt({endpoint})"),
            _ => write!(f, "{}({})", self.protocol_name(), self.peer()),
        }
    }
}

/// Engine assessment of directional continuity, not absolute clock calibration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OneWayStatus {
    Usable,
    /// Start new directional history before consuming this observation.
    ReferenceChanged,
    /// Raw values remain available for diagnostics; RTT is assessed independently.
    Unusable(OneWayRejection),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OneWayRejection {
    InvalidRtt,
    InconsistentTimestamps,
    Discontinuity,
    LocalClockStep,
    OutOfOrder,
}

impl OneWayRejection {
    pub fn reason(self) -> &'static str {
        match self {
            Self::InvalidRtt => "invalid_rtt",
            Self::InconsistentTimestamps => "inconsistent_timestamps",
            Self::Discontinuity => "directional_discontinuity",
            Self::LocalClockStep => "local_clock_step",
            Self::OutOfOrder => "out_of_order",
        }
    }
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
    /// Finalized by MeasurementEngine before publication.
    pub status: OneWayStatus,
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
    /// Usable RTT. A protocol RTT is authoritative and invalid when negative;
    /// otherwise the local elapsed time. Losses return None.
    pub fn rtt(&self) -> Option<Duration> {
        let MeasurementOutcome::Reply { round_trip, .. } = self.outcome else {
            return None;
        };
        match round_trip {
            Some(rtt) => u64::try_from(rtt.as_nanos()).ok().map(Duration::from_nanos),
            None => self.completed_at.checked_duration_since(self.started_at),
        }
    }

    /// Usable RTT in milliseconds. Raw protocol values remain in `outcome`.
    /// Invalid RTTs and losses return None; directional rejection does not affect RTT.
    pub fn rtt_ms(&self) -> Option<f64> {
        match self.outcome {
            MeasurementOutcome::Reply { round_trip, .. } => Some(
                round_trip
                    .map(SignedDuration::as_secs_f64)
                    .unwrap_or_else(|| {
                        self.completed_at
                            .checked_duration_since(self.started_at)
                            .map_or(f64::NAN, |d| d.as_secs_f64())
                    })
                    * 1000.0,
            )
            .filter(|rtt| rtt.is_finite() && *rtt >= 0.0),
            MeasurementOutcome::Loss => None,
        }
    }
}
