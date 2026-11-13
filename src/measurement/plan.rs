// SPDX-FileCopyrightText: 2026-Present Nils Andreas Svee mailto:contact@lochnair.net (github @Lochnair)
//
// SPDX-License-Identifier: MPL-2.0

use super::{
    model::{MeasurementRole, MeasurementSource, MeasurementStream},
    scheduler::ProbeSchedule,
};
use std::{
    collections::HashSet,
    time::{Duration, Instant},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeasurementAssignment {
    pub stream: MeasurementStream,
    pub role: MeasurementRole,
    pub schedule: ProbeSchedule,
    pub irtt_port: u16,
    pub irtt_hmac: Option<String>,
}

#[derive(Debug, Clone)]
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
        let mut ids = HashSet::new();
        let mut endpoints = HashSet::new();
        for assignment in &self.assignments {
            assignment.schedule.validate()?;
            anyhow::ensure!(
                Instant::now()
                    .checked_add(assignment.schedule.period)
                    .is_some(),
                "probe period overflows monotonic clock"
            );
            anyhow::ensure!(
                ids.insert(assignment.stream.id),
                "duplicate measurement stream ID"
            );
            let port = if assignment.stream.source == MeasurementSource::Irtt {
                anyhow::ensure!(cfg!(feature = "irtt"), "IRTT feature is disabled");
                anyhow::ensure!(assignment.irtt_port != 0, "IRTT port must be nonzero");
                assignment.irtt_port
            } else {
                anyhow::ensure!(assignment.stream.peer.is_ipv4(), "ICMP requires IPv4");
                0
            };
            anyhow::ensure!(
                endpoints.insert((assignment.stream.peer, assignment.stream.source, port)),
                "duplicate measurement target"
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionPhase {
    Establishing,
    Ready,
    Failed,
    Retiring,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamExecution {
    pub assignment: MeasurementAssignment,
    pub phase: ExecutionPhase,
    pub failures: u64,
    pub generation: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AppliedState {
    pub revision: u64,
    pub streams: Vec<StreamExecution>,
    pub rejected_revision: Option<u64>,
}

impl AppliedState {
    pub fn active_streams(&self) -> Vec<MeasurementStream> {
        self.streams
            .iter()
            .filter(|s| {
                s.assignment.role == MeasurementRole::Active && s.phase == ExecutionPhase::Ready
            })
            .map(|s| s.assignment.stream)
            .collect()
    }
}
