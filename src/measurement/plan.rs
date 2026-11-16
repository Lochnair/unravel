// SPDX-FileCopyrightText: 2026-Present Nils Andreas Svee mailto:contact@lochnair.net (github @Lochnair)
//
// SPDX-License-Identifier: MPL-2.0

use super::{
    model::{MeasurementRole, MeasurementTarget},
    scheduler::ProbeSchedule,
};
use std::{
    collections::HashSet,
    time::{Duration, Instant},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeasurementAssignment {
    pub target: MeasurementTarget,
    pub role: MeasurementRole,
    pub schedule: ProbeSchedule,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeasurementPlan {
    pub revision: u64,
    /// Both active and candidate IRTT sessions negotiate this interval.
    pub active_interval: Duration,
    pub assignments: Vec<MeasurementAssignment>,
}

impl MeasurementPlan {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.active_interval.is_zero(),
            "active interval must be nonzero"
        );
        anyhow::ensure!(
            Instant::now().checked_add(self.active_interval).is_some(),
            "active interval overflows monotonic clock"
        );
        let mut endpoints = HashSet::new();
        for assignment in &self.assignments {
            assignment.schedule.validate()?;
            anyhow::ensure!(
                assignment.role != MeasurementRole::Active
                    || assignment.schedule.period == self.active_interval,
                "active assignment must use the active interval"
            );
            anyhow::ensure!(
                Instant::now()
                    .checked_add(assignment.schedule.period)
                    .is_some(),
                "probe period overflows monotonic clock"
            );
            let endpoint = match &assignment.target {
                MeasurementTarget::Irtt { endpoint, .. } => {
                    anyhow::ensure!(cfg!(feature = "irtt"), "IRTT feature is disabled");
                    anyhow::ensure!(endpoint.port() != 0, "IRTT port must be nonzero");
                    *endpoint
                }
                _ => std::net::SocketAddr::new(assignment.target.peer(), 0),
            };
            anyhow::ensure!(
                endpoints.insert((endpoint, assignment.target.protocol())),
                "duplicate measurement target"
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedTarget {
    pub assignment: MeasurementAssignment,
    pub ready: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AppliedState {
    pub revision: u64,
    pub targets: Vec<AppliedTarget>,
    pub rejected_revision: Option<u64>,
}

impl AppliedState {
    pub fn is_active(&self, target: &MeasurementTarget) -> bool {
        self.targets.iter().any(|s| {
            &s.assignment.target == target
                && s.assignment.role == MeasurementRole::Active
                && s.ready
        })
    }
    pub fn active_targets(&self) -> Vec<MeasurementTarget> {
        self.targets
            .iter()
            .filter(|s| s.assignment.role == MeasurementRole::Active && s.ready)
            .map(|s| s.assignment.target.clone())
            .collect()
    }
}
