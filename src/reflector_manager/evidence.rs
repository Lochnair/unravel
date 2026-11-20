// SPDX-FileCopyrightText: 2026-Present Nils Andreas Svee mailto:contact@lochnair.net (github @Lochnair)
//
// SPDX-License-Identifier: MPL-2.0

use crate::measurement::{
    model::{MeasurementEvent, MeasurementOutcome, OneWayRejection, OneWayStatus},
    plan::AppliedTarget,
};
use std::time::{Duration, Instant};

pub(super) const WINDOW: usize = 9;
pub(super) const STREAK: usize = 3;

#[derive(Debug)]
pub(super) struct Quality {
    pub rtt: f64,
    pub directional: bool,
}

pub(super) struct Evidence {
    since: Instant,
    pub last_reply: Option<Instant>,
    outcomes: Vec<MeasurementEvent>,
    first_reference: Option<Instant>,
    reference_since: Option<Instant>,
}

impl Evidence {
    pub fn new(now: Instant) -> Self {
        Self {
            since: now,
            last_reply: None,
            outcomes: Vec::with_capacity(WINDOW + 1),
            first_reference: None,
            reference_since: None,
        }
    }

    pub fn record(&mut self, event: MeasurementEvent) {
        // A revisit must qualify from probes sent during this evaluation.
        if event.started_at < self.since {
            return;
        }
        if valid_rtt(&event).is_some() {
            self.last_reply = Some(
                self.last_reply
                    .map_or(event.completed_at, |at| at.max(event.completed_at)),
            );
        }
        // Late timeouts belong before newer replies. Duplicate completions
        // cannot manufacture a qualifying streak; a real late reply may replace a loss.
        let started_at = event.started_at;
        let reference_changed = matches!(event.outcome,
            MeasurementOutcome::Reply { one_way: Some(owd), .. }
            if owd.status == OneWayStatus::ReferenceChanged);
        match self
            .outcomes
            .binary_search_by_key(&event.started_at, |e| e.started_at)
        {
            Ok(i)
                if matches!(self.outcomes[i].outcome, MeasurementOutcome::Loss)
                    && matches!(event.outcome, MeasurementOutcome::Reply { .. }) =>
            {
                self.outcomes[i] = event
            }
            Ok(_) => return,
            Err(i) => self.outcomes.insert(i, event),
        }
        if self.outcomes.len() > WINDOW {
            self.outcomes.remove(0);
        }
        if reference_changed && self.reference_since.is_none_or(|since| started_at > since) {
            self.first_reference.get_or_insert(started_at);
            self.reference_since = Some(started_at);
        }
    }

    /// RTT liveness and directional service health are independent. An isolated
    /// step can requalify; repeated transitions or a run of rejected timing
    /// should open replacement qualification even while RTT replies continue.
    pub fn timing_unhealthy(&self, now: Instant, max_age: Duration) -> bool {
        let (mut changes, mut recent, mut rejected) = (0, 0, 0);
        for e in self
            .outcomes
            .iter()
            .rev()
            .filter(|e| now.saturating_duration_since(e.completed_at) <= max_age)
        {
            let MeasurementOutcome::Reply {
                one_way: Some(owd), ..
            } = e.outcome
            else {
                recent += 1;
                continue;
            };
            if owd.status == OneWayStatus::ReferenceChanged
                && Some(e.started_at) != self.first_reference
            {
                changes += 1;
            }
            // Counts the newest STREAK outcomes only.
            if recent < STREAK
                && valid_rtt(e).is_some()
                && matches!(
                    owd.status,
                    OneWayStatus::Unusable(
                        OneWayRejection::InconsistentTimestamps | OneWayRejection::Discontinuity
                    )
                )
            {
                rejected += 1;
            }
            recent += 1;
        }
        changes >= 2 || rejected == STREAK
    }

    pub fn quality(
        &self,
        status: &AppliedTarget,
        now: Instant,
        min_samples: usize,
    ) -> Option<Quality> {
        let period = status.assignment.schedule.period;
        let freshness = period.saturating_mul(3);
        let horizon = period.saturating_mul((min_samples + 1) as u32);
        if !status.ready
            || self.timing_unhealthy(now, period.saturating_mul((WINDOW + 1) as u32))
            || self
                .last_reply
                .is_none_or(|at| now.saturating_duration_since(at) > freshness)
            || self.outcomes.len() < min_samples
        {
            return None;
        }
        // The window holds at most WINDOW outcomes, so the samples fit on the stack.
        let mut samples = [0.0; WINDOW];
        let mut count = 0;
        let mut directional = true;
        for event in self.outcomes.iter().rev().take(min_samples) {
            if now.saturating_duration_since(event.started_at) > horizon {
                return None;
            }
            samples[count] = valid_rtt(event)?;
            count += 1;
            match event.outcome {
                MeasurementOutcome::Reply {
                    one_way: Some(owd), ..
                } => {
                    if matches!(owd.status, OneWayStatus::Unusable(_))
                        || self
                            .reference_since
                            .is_some_and(|since| event.started_at < since)
                    {
                        return None;
                    }
                }
                _ => directional = false,
            }
        }
        let rtts = &mut samples[..count];
        rtts.sort_unstable_by(f64::total_cmp);
        let mid = rtts.len() / 2;
        let rtt = if rtts.len().is_multiple_of(2) {
            rtts[mid - 1] / 2.0 + rtts[mid] / 2.0
        } else {
            rtts[mid]
        };
        Some(Quality { rtt, directional })
    }
}

fn valid_rtt(event: &MeasurementEvent) -> Option<f64> {
    event.rtt_ms()
}
