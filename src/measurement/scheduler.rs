// SPDX-FileCopyrightText: 2026-Present Nils Andreas Svee mailto:contact@lochnair.net (github @Lochnair)
//
// SPDX-License-Identifier: MPL-2.0

use super::model::MeasurementTarget;
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProbeSchedule {
    pub period: Duration,
    pub offset: Duration,
}

impl ProbeSchedule {
    pub fn validate(self) -> anyhow::Result<()> {
        anyhow::ensure!(!self.period.is_zero(), "probe period must be nonzero");
        anyhow::ensure!(
            self.offset < self.period,
            "probe offset must be less than period"
        );
        Ok(())
    }

    pub(crate) fn next_probe_at(self, origin: Instant, now: Instant) -> Instant {
        let first = origin + self.offset;
        if now < first {
            return first;
        }
        let remainder = now.duration_since(first).as_nanos() % self.period.as_nanos();
        now + duration_from_nanos(self.period.as_nanos() - remainder)
    }
}

pub fn evenly_spaced_offset(period: Duration, index: usize, count: usize) -> Duration {
    duration_from_nanos(period.as_nanos() * index as u128 / count as u128)
}

fn duration_from_nanos(nanos: u128) -> Duration {
    Duration::new(
        (nanos / 1_000_000_000) as u64,
        (nanos % 1_000_000_000) as u32,
    )
}

struct ScheduledTarget {
    schedule: ProbeSchedule,
    next: Instant,
    pending: Option<Instant>,
    last_committed: Option<Instant>,
}

/// Protocol-independent owner of every probe opportunity. A pending opportunity
/// is a lease on one grid slot, not a send. Only socket acceptance commits it.
pub struct ProbeScheduler {
    epoch: Instant,
    targets: BTreeMap<MeasurementTarget, ScheduledTarget>,
    cursor: Option<MeasurementTarget>,
}

impl ProbeScheduler {
    pub fn new(epoch: Instant) -> Self {
        Self {
            epoch,
            targets: BTreeMap::new(),
            cursor: None,
        }
    }

    /// Returns targets whose uncommitted protocol preparation must be discarded.
    /// The engine validates the complete plan before calling this method.
    pub fn reconcile(
        &mut self,
        assignments: &[(MeasurementTarget, ProbeSchedule)],
        now: Instant,
    ) -> Vec<MeasurementTarget> {
        let mut withdrawn = Vec::new();
        self.targets.retain(|target, state| {
            let keep = assignments.iter().any(|(wanted, _)| wanted == target);
            if !keep && state.pending.is_some() {
                withdrawn.push(target.clone());
            }
            keep
        });
        for (target, schedule) in assignments {
            let schedule = *schedule;
            match self.targets.get_mut(target) {
                Some(state) if state.schedule == schedule => {}
                Some(state) => {
                    if state.pending.take().is_some() {
                        withdrawn.push(target.clone());
                    }
                    let mut next = schedule.next_probe_at(self.epoch, now);
                    if let Some(last) = state.last_committed {
                        let earliest = last + schedule.period;
                        if next < earliest {
                            // Inclusive lower bound, without leaving the common grid.
                            next = schedule
                                .next_probe_at(self.epoch, earliest - Duration::from_nanos(1));
                        }
                    }
                    state.schedule = schedule;
                    state.next = next;
                }
                None => {
                    self.targets.insert(
                        target.clone(),
                        ScheduledTarget {
                            schedule,
                            next: schedule.next_probe_at(self.epoch, now),
                            pending: None,
                            last_committed: None,
                        },
                    );
                }
            }
        }
        withdrawn
    }

    /// Skips missed grid slots; at most one opportunity survives per target.
    /// Coincident deadlines are served in rotating target order.
    pub fn opportunities(
        &mut self,
        now: Instant,
    ) -> (Vec<MeasurementTarget>, Vec<MeasurementTarget>) {
        let mut due = Vec::new();
        let mut withdrawn = Vec::new();
        for (target, state) in &mut self.targets {
            if now >= state.next {
                if state.pending.take().is_some() {
                    withdrawn.push(target.clone());
                }
                let next = state.schedule.next_probe_at(self.epoch, now);
                state.pending = Some(next - state.schedule.period);
                state.next = next;
            }
            if state.pending.is_some() {
                due.push(target.clone());
            }
        }
        if let Some(cursor) = &self.cursor {
            let split = due.partition_point(|target| target <= cursor);
            due.rotate_left(split);
        }
        if let Some(first) = due.first() {
            self.cursor = Some(first.clone());
        }
        (due, withdrawn)
    }

    pub fn committed(&mut self, target: &MeasurementTarget, accepted_at: Instant) {
        if let Some(state) = self.targets.get_mut(target) {
            state.pending = None;
            state.last_committed = Some(accepted_at);
            state.next = state.schedule.next_probe_at(self.epoch, accepted_at);
            // Normal reactor jitter stays on the grid. If a blocked submission
            // consumed most of its slot, skip the following deadline instead
            // of sending a catch-up pair nearly back-to-back. This bound does
            // not shift the epoch or accumulate delay in subsequent periods.
            let earliest = accepted_at + state.schedule.period / 2;
            if state.next < earliest {
                state.next = state
                    .schedule
                    .next_probe_at(self.epoch, earliest - Duration::from_nanos(1));
            }
        }
    }

    pub fn withdraw(&mut self, target: &MeasurementTarget) {
        if let Some(state) = self.targets.get_mut(target) {
            state.pending = None;
        }
    }

    pub fn next_deadline(&self) -> Option<Instant> {
        self.targets.values().map(|s| s.next).min()
    }
}
