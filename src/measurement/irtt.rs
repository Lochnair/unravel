// SPDX-FileCopyrightText: 2026-Present Nils Andreas Svee mailto:contact@lochnair.net (github @Lochnair)
//
// SPDX-License-Identifier: MPL-2.0

use super::{
    model::{
        MeasurementOutcome, MeasurementStreamId, OneWayClock, OneWayLatency, ProbeCompletion,
        SignedDuration,
    },
    plan::{ExecutionPhase, MeasurementAssignment, StreamExecution},
};
use crate::settings::NetworkSettings;
use irtt_client::{
    AsyncClient, AsyncOpenState, ClientConfig, ClientEvent, NegotiationPolicy, OpenOutcome,
    OpenPolicy, SendProbeError,
};
use log::{debug, warn};
use std::{
    collections::BTreeMap,
    future::Future,
    pin::Pin,
    task::{Context, Poll},
    time::{Duration, Instant},
};

const RETRY_DELAY: Duration = Duration::from_secs(5);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(1);
type Connecting =
    Pin<Box<dyn Future<Output = Result<AsyncClient, irtt_client::ClientError>> + Send>>;

enum Session {
    Connecting(Connecting),
    Opening {
        client: Box<AsyncClient>,
        operation: Box<AsyncOpenState>,
    },
    Ready(Box<AsyncClient>),
    Cooling(Instant),
}

struct Target {
    assignment: MeasurementAssignment,
    interval: Duration,
    session: Session,
    failures: u64,
    generation: u64,
}

struct Closing {
    client: Box<AsyncClient>,
    status: StreamExecution,
    deadline: Instant,
}

pub struct IrttDriver {
    settings: NetworkSettings,
    targets: BTreeMap<MeasurementStreamId, Target>,
    closing: Vec<Closing>,
}

/// A send can be accepted even when post-send protocol bookkeeping reports an
/// operational failure. Preserve that receipt for scheduler accounting.
pub struct SendProgress {
    pub accepted_at: Option<Instant>,
    pub failed: bool,
}

impl IrttDriver {
    pub fn new(settings: &NetworkSettings) -> Self {
        Self {
            settings: settings.clone(),
            targets: BTreeMap::new(),
            closing: Vec::new(),
        }
    }

    fn connect(
        settings: &NetworkSettings,
        assignment: &MeasurementAssignment,
        interval: Duration,
    ) -> Connecting {
        let mut config = client_config(interval, settings);
        if let Some(key) = assignment
            .irtt_hmac
            .as_ref()
            .filter(|k| !k.trim().is_empty())
        {
            config.auth = irtt_client::Authentication::Hmac(irtt_client::HmacKey::new(
                key.as_bytes().to_vec().into_boxed_slice(),
            ));
        }
        let endpoint =
            std::net::SocketAddr::new(assignment.stream.peer, assignment.irtt_port).to_string();
        Box::pin(async move { AsyncClient::connect(endpoint, config).await })
    }

    pub fn reconcile(&mut self, assignments: &[MeasurementAssignment], interval: Duration) {
        let removed = self
            .targets
            .keys()
            .copied()
            .filter(|id| !assignments.iter().any(|a| a.stream.id == *id))
            .collect::<Vec<_>>();
        for id in removed {
            let target = self.targets.remove(&id).unwrap();
            self.retire(target);
        }
        for assignment in assignments {
            if let Some(target) = self.targets.get_mut(&assignment.stream.id) {
                if target.interval == interval
                    && target.assignment.stream == assignment.stream
                    && target.assignment.irtt_port == assignment.irtt_port
                    && target.assignment.irtt_hmac == assignment.irtt_hmac
                {
                    target.assignment = assignment.clone();
                    continue;
                }
                let target = self.targets.remove(&assignment.stream.id).unwrap();
                self.retire(target);
            }
            self.targets.insert(
                assignment.stream.id,
                Target {
                    assignment: assignment.clone(),
                    interval,
                    session: Session::Connecting(Self::connect(
                        &self.settings,
                        assignment,
                        interval,
                    )),
                    failures: 0,
                    generation: 0,
                },
            );
        }
    }

    fn retire(&mut self, target: Target) {
        let status = StreamExecution {
            assignment: target.assignment,
            phase: ExecutionPhase::Retiring,
            failures: target.failures,
            generation: target.generation,
        };
        match target.session {
            Session::Opening {
                mut client,
                mut operation,
            } => {
                let _ = client.cancel_open(&mut operation);
            }
            Session::Ready(mut client) => {
                client.discard_prepared_probe();
                self.closing.push(Closing {
                    client,
                    status,
                    deadline: Instant::now() + CLOSE_TIMEOUT,
                });
            }
            _ => {}
        }
    }

    fn fail(target: &mut Target, closing: &mut Vec<Closing>, error: impl std::fmt::Display) {
        warn!(
            "IRTT {} execution failure: {error}",
            target.assignment.stream.peer
        );
        target.failures = target.failures.saturating_add(1);
        let previous = std::mem::replace(
            &mut target.session,
            Session::Cooling(Instant::now() + RETRY_DELAY),
        );
        match previous {
            Session::Ready(mut client) => {
                client.discard_prepared_probe();
                closing.push(Closing {
                    client,
                    status: StreamExecution {
                        assignment: target.assignment.clone(),
                        phase: ExecutionPhase::Retiring,
                        failures: target.failures,
                        generation: target.generation,
                    },
                    deadline: Instant::now() + CLOSE_TIMEOUT,
                });
            }
            Session::Opening {
                mut client,
                mut operation,
            } => {
                let _ = client.cancel_open(&mut operation);
            }
            _ => {}
        }
    }

    /// Each target gets bounded opening, receive, timeout and cleanup work,
    /// independently of whether any scheduler opportunity exists.
    pub fn poll_maintenance(
        &mut self,
        cx: &mut Context<'_>,
        now: Instant,
        completions: &mut Vec<ProbeCompletion>,
    ) {
        for target in self.targets.values_mut() {
            let mut failure = None;
            match &mut target.session {
                Session::Connecting(future) => match future.as_mut().poll(cx) {
                    Poll::Ready(Ok(mut client)) => match client.begin_open() {
                        Ok(operation) => {
                            target.session = Session::Opening {
                                client: Box::new(client),
                                operation: Box::new(operation),
                            };
                            cx.waker().wake_by_ref();
                        }
                        Err(error) => failure = Some(error.to_string()),
                    },
                    Poll::Ready(Err(error)) => failure = Some(error.to_string()),
                    Poll::Pending => {}
                },
                Session::Opening { client, operation } => match client.poll_open(operation, cx) {
                    Poll::Ready(Ok(OpenOutcome::Started(session))) => {
                        debug!(
                            "IRTT session started for {}: token {}",
                            session.remote, session.token
                        );
                        target.generation += 1;
                        let previous =
                            std::mem::replace(&mut target.session, Session::Cooling(now));
                        let Session::Opening { client, .. } = previous else {
                            unreachable!()
                        };
                        target.session = Session::Ready(client);
                        cx.waker().wake_by_ref();
                    }
                    Poll::Ready(Ok(OpenOutcome::NoTestCompleted(_))) => {
                        failure = Some("server did not establish a probe session".into())
                    }
                    Poll::Ready(Err(error)) => failure = Some(error.to_string()),
                    Poll::Pending => {}
                },
                Session::Ready(client) => {
                    if let Poll::Ready(result) = client.poll_recv(cx) {
                        match result {
                            Ok(events) => {
                                map_events(&target.assignment, events, completions);
                                cx.waker().wake_by_ref();
                            }
                            Err(error) => failure = Some(error.to_string()),
                        }
                    }
                    if failure.is_none() {
                        match client.poll_timeouts_bounded_at(now, 32) {
                            Ok(batch) => {
                                map_events(&target.assignment, batch.events, completions);
                                if batch.more_due {
                                    cx.waker().wake_by_ref();
                                }
                            }
                            Err(error) => failure = Some(error.to_string()),
                        }
                        if client.is_peer_closed() {
                            failure = Some("remote closed session".into());
                        }
                    }
                }
                Session::Cooling(until) if now >= *until => {
                    target.session = Session::Connecting(Self::connect(
                        &self.settings,
                        &target.assignment,
                        target.interval,
                    ));
                    cx.waker().wake_by_ref();
                }
                Session::Cooling(_) => {}
            }
            if let Some(error) = failure {
                Self::fail(target, &mut self.closing, error);
            }
        }
        self.closing.retain_mut(|closing| {
            now < closing.deadline && closing.client.poll_close(cx).is_pending()
        });
    }

    pub fn poll_send(
        &mut self,
        id: MeasurementStreamId,
        cx: &mut Context<'_>,
    ) -> Poll<SendProgress> {
        let Some(target) = self.targets.get_mut(&id) else {
            return Poll::Ready(SendProgress {
                accepted_at: None,
                failed: true,
            });
        };
        let Session::Ready(client) = &mut target.session else {
            return Poll::Pending;
        };
        match client.poll_send_probe(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(receipt)) => Poll::Ready(SendProgress {
                accepted_at: Some(receipt.sent_at.mono),
                failed: false,
            }),
            Poll::Ready(Err(error)) => {
                let accepted_at = match &error {
                    SendProbeError::AfterCommit { receipt, .. } => Some(receipt.sent_at.mono),
                    SendProbeError::NotCommitted(_) => None,
                };
                Self::fail(target, &mut self.closing, error);
                Poll::Ready(SendProgress {
                    accepted_at,
                    failed: true,
                })
            }
        }
    }

    pub fn discard(&mut self, id: MeasurementStreamId) {
        if let Some(Target {
            session: Session::Ready(client),
            ..
        }) = self.targets.get_mut(&id)
        {
            client.discard_prepared_probe();
        }
    }

    pub fn next_deadline(&self) -> Option<Instant> {
        self.targets
            .values()
            .filter_map(|t| match &t.session {
                Session::Ready(c) => c.next_probe_timeout_deadline(),
                Session::Cooling(until) => Some(*until),
                _ => None,
            })
            .chain(self.closing.iter().map(|c| c.deadline))
            .min()
    }

    pub fn statuses(&self) -> Vec<StreamExecution> {
        self.targets
            .values()
            .map(|t| StreamExecution {
                assignment: t.assignment.clone(),
                failures: t.failures,
                generation: t.generation,
                phase: match t.session {
                    Session::Ready(_) => ExecutionPhase::Ready,
                    Session::Cooling(_) => ExecutionPhase::Failed,
                    _ => ExecutionPhase::Establishing,
                },
            })
            .chain(
                self.closing
                    .iter()
                    .filter(|c| !self.targets.contains_key(&c.status.assignment.stream.id))
                    .map(|c| c.status.clone()),
            )
            .collect()
    }

    pub fn shutdown(&mut self) {
        let targets = std::mem::take(&mut self.targets);
        for (_, target) in targets {
            self.retire(target);
        }
    }
    pub fn is_closed(&self) -> bool {
        self.targets.is_empty() && self.closing.is_empty()
    }
}

fn map_events(
    assignment: &MeasurementAssignment,
    events: Vec<ClientEvent>,
    completions: &mut Vec<ProbeCompletion>,
) {
    for event in events {
        let (started_at, completed_at, outcome) = match event {
            ClientEvent::EchoReply {
                sent_at,
                received_at,
                one_way,
                rtt,
                ..
            } => (
                sent_at.mono,
                received_at.mono,
                MeasurementOutcome::Reply {
                    round_trip: i64::try_from(rtt.effective.as_nanos())
                        .ok()
                        .map(SignedDuration::from_nanos),
                    one_way: one_way.and_then(map_one_way),
                },
            ),
            ClientEvent::EchoLoss {
                sent_at,
                timeout_at,
                ..
            } => (sent_at.mono, timeout_at, MeasurementOutcome::Loss),
            _ => continue,
        };
        completions.push(ProbeCompletion {
            stream: assignment.stream,
            started_at,
            completed_at,
            outcome,
        });
    }
}

fn client_config(interval: Duration, #[allow(unused)] settings: &NetworkSettings) -> ClientConfig {
    ClientConfig {
        socket: irtt_client::SocketConfig {
            #[cfg(any(target_os = "android", target_os = "fuchsia", target_os = "linux"))]
            bind_to_device: settings.measurement_bind_device.clone(),
            #[cfg(any(target_os = "android", target_os = "fuchsia", target_os = "linux"))]
            mark: settings.measurement_mark,
            #[cfg(target_os = "freebsd")]
            fib: settings.measurement_fib,
            ..Default::default()
        },
        request: irtt_client::SessionRequest {
            duration: None,
            interval,
            ..Default::default()
        },
        open: OpenPolicy {
            negotiation: NegotiationPolicy::Strict,
            ..Default::default()
        },
        ..Default::default()
    }
}

fn map_one_way(sample: irtt_client::OneWayDelaySample) -> Option<OneWayLatency> {
    Some(OneWayLatency {
        uplink: SignedDuration::from_nanos(
            i64::try_from(sample.client_to_server?.as_nanos()).ok()?,
        ),
        downlink: SignedDuration::from_nanos(
            i64::try_from(sample.server_to_client?.as_nanos()).ok()?,
        ),
        clock: OneWayClock::Uncalibrated,
    })
}
