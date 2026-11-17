// SPDX-FileCopyrightText: 2026-Present Nils Andreas Svee mailto:contact@lochnair.net (github @Lochnair)
//
// SPDX-License-Identifier: MPL-2.0

use std::sync::Arc;

use super::LawStore;
use crate::{
    measurement::{model::MeasurementEvent, plan::AppliedState},
    metrics::MetricsSender,
    runtime::{Direction, Runtime, Shaper, TrafficRates},
    time::ClockStep,
};
use tokio::sync::{broadcast, watch};

pub struct LawContext {
    pub measurements: broadcast::Receiver<MeasurementEvent>,
    pub metrics: MetricsSender,
    runtime: Arc<Runtime>,
    pub applied: watch::Receiver<AppliedState>,
    pub traffic: watch::Receiver<Option<TrafficRates>>,
    /// Local clock discontinuities. Await `changed()` or poll `has_changed()`,
    /// then acknowledge with `borrow_and_update()`. Updates may coalesce: this
    /// is an invalidation signal, not a log of corrections to sum together.
    pub clock_steps: watch::Receiver<Option<ClockStep>>,
    num_reflectors: usize,
    store: LawStore,
}

impl LawContext {
    pub fn new(
        measurements: broadcast::Receiver<MeasurementEvent>,
        metrics: MetricsSender,
        runtime: Arc<Runtime>,
        applied: watch::Receiver<AppliedState>,
        traffic: watch::Receiver<Option<TrafficRates>>,
        num_reflectors: usize,
        store: LawStore,
    ) -> Self {
        Self {
            clock_steps: runtime.clock_steps(),
            measurements,
            metrics,
            runtime,
            applied,
            traffic,
            num_reflectors,
            store,
        }
    }

    /// Configured number of active reflectors.
    pub fn num_reflectors(&self) -> usize {
        self.num_reflectors
    }

    /// The law's own files, kept across restarts.
    pub fn store(&self) -> &LawStore {
        &self.store
    }

    pub fn set_rate(&self, direction: Direction, rate_kbps: f64) -> anyhow::Result<()> {
        self.runtime.set_rate(direction, rate_kbps)
    }

    pub fn shaper(&self, direction: Direction) -> Option<&Shaper> {
        self.runtime.shaper(direction)
    }
}
