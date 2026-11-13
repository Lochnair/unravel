// SPDX-FileCopyrightText: 2026-Present Nils Andreas Svee mailto:contact@lochnair.net (github @Lochnair)
//
// SPDX-License-Identifier: MPL-2.0

use super::model::MeasurementStreamId;
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

struct ScheduledStream {
    schedule: ProbeSchedule,
    next: Instant,
    pending: Option<Instant>,
    last_committed: Option<Instant>,
}

/// Protocol-independent owner of every probe opportunity. A pending opportunity
/// is a lease on one grid slot, not a send. Only socket acceptance commits it.
pub struct ProbeScheduler {
    epoch: Instant,
    streams: BTreeMap<MeasurementStreamId, ScheduledStream>,
    cursor: Option<MeasurementStreamId>,
}

impl ProbeScheduler {
    pub fn new(epoch: Instant) -> Self {
        Self {
            epoch,
            streams: BTreeMap::new(),
            cursor: None,
        }
    }

    /// Returns streams whose uncommitted protocol preparation must be discarded.
    /// The engine validates the complete plan before calling this method.
    pub fn reconcile(
        &mut self,
        assignments: &[(MeasurementStreamId, ProbeSchedule)],
        now: Instant,
    ) -> Vec<MeasurementStreamId> {
        let mut withdrawn = Vec::new();
        self.streams.retain(|id, state| {
            let keep = assignments.iter().any(|(wanted, _)| wanted == id);
            if !keep && state.pending.is_some() {
                withdrawn.push(*id);
            }
            keep
        });
        for &(id, schedule) in assignments {
            match self.streams.get_mut(&id) {
                Some(state) if state.schedule == schedule => {}
                Some(state) => {
                    if state.pending.take().is_some() {
                        withdrawn.push(id);
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
                    self.streams.insert(
                        id,
                        ScheduledStream {
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

    /// Skips missed grid slots; at most one opportunity survives per stream.
    /// Coincident deadlines are served in rotating stream order.
    pub fn opportunities(
        &mut self,
        now: Instant,
    ) -> (Vec<MeasurementStreamId>, Vec<MeasurementStreamId>) {
        let mut due = Vec::new();
        let mut withdrawn = Vec::new();
        for (&id, state) in &mut self.streams {
            if now >= state.next {
                if state.pending.take().is_some() {
                    withdrawn.push(id);
                }
                let next = state.schedule.next_probe_at(self.epoch, now);
                state.pending = Some(next - state.schedule.period);
                state.next = next;
            }
            if state.pending.is_some() {
                due.push(id);
            }
        }
        if let Some(cursor) = self.cursor {
            let split = due.partition_point(|id| *id <= cursor);
            due.rotate_left(split);
        }
        if let Some(&first) = due.first() {
            self.cursor = Some(first);
        }
        (due, withdrawn)
    }

    pub fn committed(&mut self, id: MeasurementStreamId, accepted_at: Instant) {
        if let Some(state) = self.streams.get_mut(&id) {
            state.pending = None;
            state.last_committed = Some(accepted_at);
            state.next = state.schedule.next_probe_at(self.epoch, accepted_at);
        }
    }

    pub fn withdraw(&mut self, id: MeasurementStreamId) {
        if let Some(state) = self.streams.get_mut(&id) {
            state.pending = None;
        }
    }

    pub fn next_deadline(&self) -> Option<Instant> {
        self.streams.values().map(|s| s.next).min()
    }
}
