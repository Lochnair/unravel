// SPDX-FileCopyrightText: 2026-Present Nils Andreas Svee mailto:contact@lochnair.net (github @Lochnair)
//
// SPDX-License-Identifier: MPL-2.0

use super::Class;
use crate::measurement::{
    model::{MeasurementEvent, MeasurementOutcome, OneWayRejection, OneWayStatus},
    plan::AppliedTarget,
};
use std::time::{Duration, Instant};

pub(super) const WINDOW: usize = 9;
pub(super) const STREAK: usize = 3;
// Each median is an observed sample.
const _: () = assert!(STREAK % 2 == 1 && WINDOW % 2 == 1 && STREAK <= WINDOW);

/// Median RTTs, each qualified independently.
#[derive(Clone, Copy, Default)]
pub(super) struct Qualifications {
    /// Over the newest STREAK outcomes: enough to fill an empty or failed slot.
    pub recovery: Option<Duration>,
    /// Over the full WINDOW: enough to replace a healthy active reflector.
    pub improvement: Option<Duration>,
}

/// What the evidence says about a target in one pass.
pub(super) struct Assessment {
    /// Before Live applies the failure debounce.
    pub unhealthy: bool,
    pub quality: Qualifications,
}

pub(super) struct Evidence {
    since: Instant,
    last_reply: Option<Instant>,
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
        if event.rtt().is_some() {
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
    fn timing_unhealthy(&self, now: Instant, max_age: Duration) -> bool {
        let (mut changes, mut recent, mut rejected) = (0, 0, 0);
        for e in self
            .outcomes
            .iter()
            .rev()
            .filter(|e| now.saturating_duration_since(e.completed_at) <= max_age)
        {
            let MeasurementOutcome::Reply { one_way, .. } = e.outcome else {
                recent += 1;
                continue;
            };
            if one_way.is_some_and(|owd| owd.status == OneWayStatus::ReferenceChanged)
                && Some(e.started_at) != self.first_reference
            {
                changes += 1;
            }
            let bad_timing = match one_way {
                Some(owd) => matches!(
                    owd.status,
                    OneWayStatus::Unusable(
                        OneWayRejection::InconsistentTimestamps | OneWayRejection::Discontinuity
                    )
                ),
                None => Class::of(&e.target) == Class::Directional,
            };
            // Counts the newest STREAK outcomes only.
            if recent < STREAK && bad_timing && e.rtt().is_some() {
                rejected += 1;
            }
            recent += 1;
        }
        changes >= 2 || rejected == STREAK
    }

    /// Health and qualification for one pass. Health counts silence from role
    /// entry; qualification needs an actual recent reply.
    pub fn assess(
        &self,
        now: Instant,
        entered_at: Instant,
        period: Duration,
        status: Option<&AppliedTarget>,
    ) -> Assessment {
        // The manager publishes the role's period and selects only once the
        // engine has applied that plan.
        debug_assert!(status.is_none_or(|s| s.assignment.schedule.period == period));
        let grace = period.saturating_mul(3);
        let fresh = |at: Instant| now.saturating_duration_since(at) <= grace;
        let ready = status.filter(|s| s.ready);
        let timing_unhealthy = ready.is_some()
            && self.timing_unhealthy(now, period.saturating_mul((WINDOW + 1) as u32));

        let reply_or_entry = self.last_reply.map_or(entered_at, |at| at.max(entered_at));
        let unhealthy = now.saturating_duration_since(entered_at) >= grace
            && (ready.is_none() || !fresh(reply_or_entry) || timing_unhealthy);

        let quality = match ready {
            Some(status) if !timing_unhealthy && self.last_reply.is_some_and(fresh) => {
                self.qualify(now, period, Class::of(&status.assignment.target))
            }
            _ => Qualifications::default(),
        };
        Assessment { unhealthy, quality }
    }

    /// Medians over the newest STREAK and WINDOW outcomes, where each of them
    /// is usable and the oldest was sent within that window's horizon.
    fn qualify(&self, now: Instant, period: Duration, class: Class) -> Qualifications {
        // The newest contiguous run of usable samples.
        let mut rtts = [Duration::ZERO; WINDOW];
        let mut usable = 0;
        for (rtt, event) in rtts.iter_mut().zip(self.outcomes.iter().rev()) {
            let Some(sample) = event
                .rtt()
                .filter(|_| class == Class::Echo || self.one_way_usable(event))
            else {
                break;
            };
            *rtt = sample;
            usable += 1;
        }
        let mut median = |samples: usize| {
            if usable < samples {
                return None;
            }
            let oldest = &self.outcomes[self.outcomes.len() - samples];
            let horizon = period.saturating_mul((samples + 1) as u32);
            if now.saturating_duration_since(oldest.started_at) > horizon {
                return None;
            }
            Some(*rtts[..samples].select_nth_unstable(samples / 2).1)
        };
        let recovery = median(STREAK);
        // Selecting within the shorter prefix keeps the full window's samples.
        let improvement = median(WINDOW);
        Qualifications {
            recovery,
            improvement,
        }
    }

    /// Directional samples need usable one-way timing from the current reference.
    fn one_way_usable(&self, event: &MeasurementEvent) -> bool {
        let usable = matches!(
            event.outcome,
            MeasurementOutcome::Reply { one_way: Some(owd), .. }
                if !matches!(owd.status, OneWayStatus::Unusable(_))
        );
        usable
            && self
                .reference_since
                .is_none_or(|since| event.started_at >= since)
    }
}
