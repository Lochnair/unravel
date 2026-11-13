// SPDX-FileCopyrightText: 2026-Present Nils Andreas Svee mailto:contact@lochnair.net (github @Lochnair)
//
// SPDX-License-Identifier: MPL-2.0

use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Arc, RwLock},
    time::Duration,
};

use thiserror::Error;
use tokio::sync::{mpsc::UnboundedReceiver, watch};

use crate::{
    error::ChannelClosed,
    measurement::{
        model::{MeasurementRole, MeasurementSource, MeasurementStream, MeasurementStreamId},
        plan::{MeasurementAssignment, MeasurementPlan},
        scheduler::{ProbeSchedule, evenly_spaced_offset},
    },
    settings::ReflectorDefinition,
};

#[derive(Debug, Error)]
pub enum ControllerError {
    #[error("ICMP channel error")]
    IcmpChannelError(#[source] ChannelClosed),
}

pub struct Controller {
    reflector_rx: UnboundedReceiver<Vec<SocketAddr>>,
    plan_tx: watch::Sender<MeasurementPlan>,
    reflectors: HashMap<SocketAddr, ReflectorDefinition>,
    active_streams: Arc<RwLock<Vec<MeasurementStream>>>,
    endpoint_streams: Arc<RwLock<HashMap<SocketAddr, MeasurementStream>>>,
    source: MeasurementSource,
    period: Duration,
    next_stream_id: u64,
    stream_ids: HashMap<(SocketAddr, MeasurementSource), MeasurementStreamId>,
}

impl Controller {
    #[allow(
        clippy::too_many_arguments,
        reason = "Constructor wires the controller channels and shared stream state"
    )]
    pub fn new(
        reflector_rx: UnboundedReceiver<Vec<SocketAddr>>,
        plan_tx: watch::Sender<MeasurementPlan>,
        reflectors: HashMap<SocketAddr, ReflectorDefinition>,
        active_streams: Arc<RwLock<Vec<MeasurementStream>>>,
        endpoint_streams: Arc<RwLock<HashMap<SocketAddr, MeasurementStream>>>,
        source: MeasurementSource,
        period: Duration,
    ) -> Self {
        Self {
            reflector_rx,
            plan_tx,
            reflectors,
            active_streams,
            endpoint_streams,
            source,
            period,
            next_stream_id: 0,
            stream_ids: HashMap::new(),
        }
    }

    pub async fn run(mut self, initial_reflectors: Vec<SocketAddr>) -> anyhow::Result<()> {
        self.update_reflectors(initial_reflectors).await?;
        while let Some(reflectors) = self.reflector_rx.recv().await {
            self.update_reflectors(reflectors).await?;
        }
        Ok(())
    }

    fn stream_for(&mut self, endpoint: SocketAddr) -> MeasurementStream {
        let key = (endpoint, self.source);
        let id = *self.stream_ids.entry(key).or_insert_with(|| {
            let id = MeasurementStreamId(self.next_stream_id);
            self.next_stream_id += 1;
            id
        });
        MeasurementStream {
            id,
            peer: endpoint.ip(),
            source: self.source,
        }
    }

    async fn update_reflectors(
        &mut self,
        endpoints: Vec<SocketAddr>,
    ) -> Result<(), ControllerError> {
        let count = endpoints.len();
        let mut assignments = Vec::new();
        let mut streams = Vec::with_capacity(count);
        let mut endpoint_streams = HashMap::new();
        for (index, endpoint) in endpoints.into_iter().enumerate() {
            let Some(_definition) = self.reflectors.get(&endpoint) else {
                continue;
            };
            let (port, hmac) = {
                (
                    _definition.irtt_port.unwrap_or(2112),
                    _definition.irtt_hmac.clone(),
                )
            };
            let stream = self.stream_for(endpoint);
            let schedule = ProbeSchedule {
                period: self.period,
                offset: evenly_spaced_offset(self.period, index, count),
            };
            assignments.push(MeasurementAssignment {
                stream,
                role: MeasurementRole::Active,
                schedule,
                irtt_port: port,
                irtt_hmac: hmac,
            });
            streams.push(stream);
            endpoint_streams.insert(endpoint, stream);
        }
        *self
            .active_streams
            .write()
            .expect("active stream lock poisoned") = streams;
        *self
            .endpoint_streams
            .write()
            .expect("endpoint stream lock poisoned") = endpoint_streams;
        let revision = self.plan_tx.borrow().revision + 1;
        self.plan_tx
            .send(MeasurementPlan {
                revision,
                active_interval: self.period,
                assignments,
            })
            .map_err(|_| ControllerError::IcmpChannelError(ChannelClosed))?;
        Ok(())
    }
}
