// SPDX-FileCopyrightText: 2026-Present Nils Andreas Svee mailto:contact@lochnair.net (github @Lochnair)
//
// SPDX-License-Identifier: MPL-2.0

use super::model::{
    MeasurementEvent, MeasurementOutcome, MeasurementTarget, OneWayClock, OneWayLatency,
    SignedDuration,
};
use crate::settings::NetworkSettings;
use irtt_client::{
    AsyncClient, AsyncOpenState, ClientConfig, ClientEvent, NegotiationPolicy, OpenOutcome,
    OpenPolicy, SendProbeError,
};
use log::{debug, trace, warn};
use std::{
    collections::BTreeMap,
    future::Future,
    pin::Pin,
    task::{Context, Poll},
    time::{Duration, Instant},
};

const RETRY_DELAY: Duration = Duration::from_secs(5);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(1);
// A remotely expired token can be silently discarded. Repeated protocol
// timeouts trigger session recovery, independently of target selection policy.
const SESSION_TIMEOUT_LIMIT: u32 = 2;

#[derive(Default)]
struct SessionLiveness {
    last_reply_started: Option<Instant>,
    consecutive_timeouts: u32,
}
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
    interval: Duration,
    session: Session,
    liveness: SessionLiveness,
}

struct Closing {
    client: Box<AsyncClient>,
    deadline: Instant,
}

pub struct IrttDriver {
    settings: NetworkSettings,
    targets: BTreeMap<MeasurementTarget, Target>,
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
        target: &MeasurementTarget,
        interval: Duration,
    ) -> Connecting {
        let MeasurementTarget::Irtt {
            endpoint,
            authentication,
        } = target
        else {
            unreachable!("IRTT driver requires an IRTT target");
        };
        let mut config = client_config(interval, settings);
        if let Some(key) = authentication {
            config.auth = irtt_client::Authentication::Hmac(irtt_client::HmacKey::new(
                key.as_bytes().to_vec().into_boxed_slice(),
            ));
        }
        let endpoint = endpoint.to_string();
        Box::pin(async move { AsyncClient::connect(endpoint, config).await })
    }

    pub fn reconcile(&mut self, targets: &[MeasurementTarget], interval: Duration) {
        let removed = self
            .targets
            .keys()
            .filter(|target| !targets.contains(target))
            .cloned()
            .collect::<Vec<_>>();
        for target in removed {
            let execution = self.targets.remove(&target).unwrap();
            self.retire(execution);
        }
        for target in targets {
            if self
                .targets
                .get(target)
                .is_some_and(|t| t.interval == interval)
            {
                continue;
            }
            if let Some(previous) = self.targets.remove(target) {
                self.retire(previous);
            }
            self.targets.insert(
                target.clone(),
                Target {
                    interval,
                    session: Session::Connecting(Self::connect(&self.settings, target, interval)),
                    liveness: SessionLiveness::default(),
                },
            );
        }
    }

    fn retire(&mut self, target: Target) {
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
                    deadline: Instant::now() + CLOSE_TIMEOUT,
                });
            }
            _ => {}
        }
    }

    fn fail(
        identity: &MeasurementTarget,
        target: &mut Target,
        closing: &mut Vec<Closing>,
        error: impl std::fmt::Display,
    ) {
        warn!("IRTT {identity:?} execution failure: {error}");
        let previous = std::mem::replace(
            &mut target.session,
            Session::Cooling(Instant::now() + RETRY_DELAY),
        );
        match previous {
            Session::Ready(mut client) => {
                client.discard_prepared_probe();
                closing.push(Closing {
                    client,
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
        completions: &mut Vec<MeasurementEvent>,
    ) {
        for (identity, target) in &mut self.targets {
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
                        debug!("IRTT session started for {}", session.remote);
                        target.liveness = SessionLiveness::default();
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
                                map_events(identity, &mut target.liveness, events, completions);
                                cx.waker().wake_by_ref();
                            }
                            Err(error) => failure = Some(error.to_string()),
                        }
                    }
                    if failure.is_none() {
                        match client.poll_timeouts_bounded_at(now, 32) {
                            Ok(batch) => {
                                map_events(
                                    identity,
                                    &mut target.liveness,
                                    batch.events,
                                    completions,
                                );
                                if batch.more_due {
                                    cx.waker().wake_by_ref();
                                }
                            }
                            Err(error) => failure = Some(error.to_string()),
                        }
                        if client.is_peer_closed() {
                            failure = Some("remote closed session".into());
                        } else if target.liveness.consecutive_timeouts >= SESSION_TIMEOUT_LIMIT {
                            failure = Some("session stopped replying to probes".into());
                        }
                    }
                }
                Session::Cooling(until) if now >= *until => {
                    target.session = Session::Connecting(Self::connect(
                        &self.settings,
                        identity,
                        target.interval,
                    ));
                    cx.waker().wake_by_ref();
                }
                Session::Cooling(_) => {}
            }
            if let Some(error) = failure {
                Self::fail(identity, target, &mut self.closing, error);
            }
        }
        self.closing.retain_mut(|closing| {
            if now >= closing.deadline {
                warn!("IRTT retired session close deadline expired");
                return false;
            }
            match closing.client.poll_close(cx) {
                Poll::Pending => true,
                Poll::Ready(Ok(_)) => false,
                Poll::Ready(Err(error)) => {
                    debug!("IRTT retired session close finished with error: {error}");
                    false
                }
            }
        });
    }

    pub fn poll_send(
        &mut self,
        identity: &MeasurementTarget,
        cx: &mut Context<'_>,
    ) -> Poll<SendProgress> {
        let Some(target) = self.targets.get_mut(identity) else {
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
            Poll::Ready(Ok(receipt)) => {
                trace!(
                    "IRTT {} next protocol timeout: {:?}, configured timeout: {:?}",
                    identity.peer(),
                    client.next_probe_timeout_deadline(),
                    client.probe_timeout()
                );
                Poll::Ready(SendProgress {
                    accepted_at: Some(receipt.sent_at.mono),
                    failed: false,
                })
            }
            Poll::Ready(Err(error)) => {
                let accepted_at = match &error {
                    SendProbeError::AfterCommit { receipt, .. } => Some(receipt.sent_at.mono),
                    SendProbeError::NotCommitted(_) => None,
                };
                Self::fail(identity, target, &mut self.closing, error);
                Poll::Ready(SendProgress {
                    accepted_at,
                    failed: true,
                })
            }
        }
    }

    pub fn discard(&mut self, identity: &MeasurementTarget) {
        if let Some(Target {
            session: Session::Ready(client),
            ..
        }) = self.targets.get_mut(identity)
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

    pub fn is_ready(&self, target: &MeasurementTarget) -> bool {
        self.targets
            .get(target)
            .is_some_and(|t| matches!(t.session, Session::Ready(_)))
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
    target: &MeasurementTarget,
    liveness: &mut SessionLiveness,
    events: Vec<ClientEvent>,
    completions: &mut Vec<MeasurementEvent>,
) {
    for event in events {
        match &event {
            ClientEvent::EchoReply { sent_at, .. }
                if liveness
                    .last_reply_started
                    .is_none_or(|started| sent_at.mono >= started) =>
            {
                liveness.last_reply_started = Some(sent_at.mono);
                liveness.consecutive_timeouts = 0;
            }
            ClientEvent::EchoLoss { sent_at, .. }
                if liveness
                    .last_reply_started
                    .is_none_or(|started| sent_at.mono > started) =>
            {
                liveness.consecutive_timeouts = liveness.consecutive_timeouts.saturating_add(1);
            }
            _ => {}
        }
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
            } => {
                debug!(
                    "IRTT {} protocol timeout at {:?}; consecutive timeouts: {}",
                    target.peer(),
                    timeout_at,
                    liveness.consecutive_timeouts
                );
                (sent_at.mono, timeout_at, MeasurementOutcome::Loss)
            }
            _ => continue,
        };
        completions.push(MeasurementEvent {
            target: target.clone(),
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
