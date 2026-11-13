// SPDX-FileCopyrightText: 2026-Present Nils Andreas Svee mailto:contact@lochnair.net (github @Lochnair)
//
// SPDX-License-Identifier: MPL-2.0

use crate::measurement::{
    model::{MeasurementEvent, MeasurementOutcome, MeasurementTarget},
    plan::AppliedTarget,
};
use std::time::{Duration, Instant};

pub(super) const WINDOW: usize = 9;
const STREAK: usize = 3;

#[derive(Debug)]
pub(super) struct Quality {
    pub rtt: f64,
    pub directional: bool,
}

pub(super) struct Evidence {
    since: Instant,
    pub last_reply: Option<Instant>,
    outcomes: Vec<MeasurementEvent>,
}

impl Evidence {
    pub fn new(now: Instant) -> Self {
        Self {
            since: now,
            last_reply: None,
            outcomes: Vec::with_capacity(WINDOW + 1),
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
    }

    pub fn quality(
        &self,
        status: &AppliedTarget,
        now: Instant,
        max_age: Duration,
    ) -> Option<Quality> {
        let freshness = status.assignment.schedule.period.saturating_mul(3);
        if !status.ready
            || self.outcomes.len() < STREAK
            || !self.outcomes.iter().rev().take(STREAK).all(|e| {
                valid_rtt(e).is_some() && now.saturating_duration_since(e.completed_at) <= freshness
            })
        {
            return None;
        }
        let mut rtts = Vec::with_capacity(WINDOW);
        let (mut directions, mut credible) = (0, 0);
        for event in &self.outcomes {
            if now.saturating_duration_since(event.completed_at) > max_age {
                continue;
            }
            let Some(rtt) = valid_rtt(event) else {
                continue;
            };
            rtts.push(rtt);
            if let MeasurementOutcome::Reply {
                one_way: Some(owd), ..
            } = event.outcome
            {
                directions += 1;
                let uncertainty = if matches!(event.target, MeasurementTarget::IcmpTimestamps(_)) {
                    4.0
                } else {
                    1.0
                };
                let tolerance = uncertainty + rtt * 0.10;
                if [owd.uplink, owd.downlink].iter().all(|d| {
                    let ms = d.as_secs_f64() * 1000.0;
                    ms >= -tolerance && ms <= rtt + tolerance
                }) {
                    credible += 1;
                }
            }
        }
        if rtts.len() < STREAK {
            return None;
        }
        rtts.sort_by(f64::total_cmp);
        let mid = rtts.len() / 2;
        let rtt = if rtts.len().is_multiple_of(2) {
            rtts[mid - 1] / 2.0 + rtts[mid] / 2.0
        } else {
            rtts[mid]
        };
        Some(Quality {
            rtt,
            directional: directions >= STREAK && credible * 3 >= directions * 2,
        })
    }
}

fn valid_rtt(event: &MeasurementEvent) -> Option<f64> {
    event.rtt_ms().filter(|rtt| rtt.is_finite() && *rtt >= 0.0)
}
