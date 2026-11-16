// SPDX-FileCopyrightText: 2026-Present Nils Andreas Svee mailto:contact@lochnair.net (github @Lochnair)
//
// SPDX-License-Identifier: MPL-2.0

use std::sync::Arc;

use crate::{
    measurement::{model::MeasurementEvent, plan::AppliedState},
    metrics::MetricsSender,
    runtime::{Direction, Runtime, Shaper, TrafficRates},
};
use tokio::sync::{broadcast, watch};

pub struct LawContext {
    pub measurements: broadcast::Receiver<MeasurementEvent>,
    pub metrics: MetricsSender,
    runtime: Arc<Runtime>,
    pub applied: watch::Receiver<AppliedState>,
    pub traffic: watch::Receiver<Option<TrafficRates>>,
}

impl LawContext {
    pub fn new(
        measurements: broadcast::Receiver<MeasurementEvent>,
        metrics: MetricsSender,
        runtime: Arc<Runtime>,
        applied: watch::Receiver<AppliedState>,
        traffic: watch::Receiver<Option<TrafficRates>>,
    ) -> Self {
        Self {
            measurements,
            metrics,
            runtime,
            applied,
            traffic,
        }
    }

    pub fn set_rate(&self, direction: Direction, rate_kbps: f64) -> anyhow::Result<()> {
        self.runtime.set_rate(direction, rate_kbps)
    }

    pub fn shaper(&self, direction: Direction) -> Option<&Shaper> {
        self.runtime.shaper(direction)
    }
}
