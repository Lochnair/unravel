// SPDX-FileCopyrightText: 2026-Present Nils Andreas Svee mailto:contact@lochnair.net (github @Lochnair)
//
// SPDX-License-Identifier: MPL-2.0

//! Directional integrity at the engine's publication boundary. No shaping or
//! reflector-selection policy belongs here. RTT and raw OWDs are never rewritten.
use super::model::{
    MeasurementEvent, MeasurementOutcome, MeasurementTarget, OneWayLatency, OneWayRejection,
    OneWayStatus,
};
use std::{
    collections::{HashMap, HashSet},
    time::Instant,
};

const DISCONTINUITY_MS: f64 = 5_000.0;
const CONFIRMATIONS: u8 = 3;

#[derive(Clone, Copy)]
struct Sample {
    up: f64,
    down: f64,
}

impl From<OneWayLatency> for Sample {
    fn from(value: OneWayLatency) -> Self {
        Self {
            up: value.uplink.as_secs_f64() * 1000.0,
            down: value.downlink.as_secs_f64() * 1000.0,
        }
    }
}

impl Sample {
    fn compatible(self, other: Self, precision: f64) -> bool {
        -self.down <= other.up + precision && -other.down <= self.up + precision
    }

    fn discontinuous(self, other: Self, precision: f64) -> bool {
        // For raw up=u+c and down=d-c, nonnegative physical delays allow clock
        // offset c in [-down, up]. Overlapping intervals can be explained by
        // queue/path changes alone, even if an OWD has increased by seconds.
        !self.compatible(other, precision)
            && (self.up - other.up)
                .abs()
                .max((self.down - other.down).abs())
                > DISCONTINUITY_MS
    }
}

#[derive(Default)]
struct TargetHistory {
    last_started: Option<Instant>,
    reference: Option<Sample>,
    pending: Option<(Sample, u8)>,
    confirm_reference: bool,
}

#[derive(Default)]
pub(crate) struct MeasurementIntegrity {
    targets: HashMap<MeasurementTarget, TargetHistory>,
    local_cutoff: Option<Instant>,
}

impl MeasurementIntegrity {
    pub fn retain(&mut self, targets: &HashSet<MeasurementTarget>) {
        self.targets.retain(|target, _| targets.contains(target));
    }

    pub fn local_clock_step(&mut self, detected_at: Instant) {
        self.local_cutoff = Some(detected_at);
        for state in self.targets.values_mut() {
            state.reference = None;
            state.pending = None;
            state.confirm_reference = true;
        }
    }

    pub fn assess(&mut self, event: &mut MeasurementEvent) {
        let rtt = event.rtt_ms();
        let state = self.targets.entry(event.target.clone()).or_default();
        let ordered = state
            .last_started
            .is_none_or(|last| event.started_at > last);
        if ordered {
            state.last_started = Some(event.started_at);
        }
        let MeasurementOutcome::Reply { one_way, .. } = &mut event.outcome else {
            if ordered {
                state.pending = None;
            }
            return;
        };
        let Some(owd) = one_way else {
            if ordered {
                state.reference = None;
                state.pending = None;
            }
            return;
        };
        let rejection = if !ordered {
            Some(OneWayRejection::OutOfOrder)
        } else if rtt.is_none() {
            Some(OneWayRejection::InvalidRtt)
        } else if self
            .local_cutoff
            .is_some_and(|cutoff| event.started_at < cutoff)
        {
            Some(OneWayRejection::LocalClockStep)
        } else {
            None
        };
        if let Some(reason) = rejection {
            if ordered {
                state.pending = None;
            }
            owd.status = OneWayStatus::Unusable(reason);
            return;
        }
        let rtt = rtt.unwrap();
        let precision = if matches!(event.target, MeasurementTarget::IcmpTimestamps(_)) {
            4.0
        } else {
            1.0
        };
        let sample = Sample::from(*owd);
        let sum = sample.up + sample.down;
        // ICMP RTT includes remote processing; OWD sum excludes it. An exact
        // equality check would reject valid replies. Signed components are OK.
        let tolerance = precision + rtt * 0.10;
        if sum < -precision || sum > rtt + tolerance {
            state.pending = None;
            owd.status = OneWayStatus::Unusable(OneWayRejection::InconsistentTimestamps);
            return;
        }
        let discontinuous = state
            .reference
            .is_some_and(|reference| reference.discontinuous(sample, precision));
        if discontinuous || state.confirm_reference {
            let count = match state.pending {
                Some((first, count)) if first.compatible(sample, precision) => {
                    state.pending = Some((first, count + 1));
                    count + 1
                }
                _ => {
                    state.pending = Some((sample, 1));
                    1
                }
            };
            if count < CONFIRMATIONS {
                owd.status = OneWayStatus::Unusable(OneWayRejection::Discontinuity);
                return;
            }
            owd.status = OneWayStatus::ReferenceChanged;
        } else {
            // Also invalidate a consumer's retained history when a target is
            // removed/re-added or changes from RTT-only to directional timing.
            owd.status = if state.reference.is_none() {
                OneWayStatus::ReferenceChanged
            } else {
                OneWayStatus::Usable
            };
        }
        state.pending = None;
        state.confirm_reference = false;
        state.reference = Some(sample);
    }
}
