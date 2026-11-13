// SPDX-FileCopyrightText: 2026-Present Nils Andreas Svee mailto:contact@lochnair.net (github @Lochnair)
//
// SPDX-License-Identifier: MPL-2.0

#[cfg(feature = "irtt")]
use super::irtt::IrttDriver;
use super::{
    icmp::IcmpDriver,
    model::{MeasurementEvent, MeasurementSource, MeasurementStreamId},
    plan::{AppliedState, ExecutionPhase, MeasurementPlan, StreamExecution},
    scheduler::ProbeScheduler,
};
use crate::{error::ChannelClosed, settings::NetworkSettings};
use log::warn;
use std::{
    collections::{BTreeMap, HashSet},
    future::{Future, poll_fn},
    pin::Pin,
    task::{Context, Poll},
    time::Instant,
};
use tokio::sync::{broadcast, watch};

/// The sole task that owns protocol execution and all probe scheduling.
pub struct MeasurementEngine {
    scheduler: ProbeScheduler,
    icmp: IcmpDriver,
    #[cfg(feature = "irtt")]
    irtt: IrttDriver,
    plan: MeasurementPlan,
    desired: watch::Receiver<MeasurementPlan>,
    applied: watch::Sender<AppliedState>,
    measurements: broadcast::Sender<MeasurementEvent>,
    stop: watch::Receiver<bool>,
    icmp_status: BTreeMap<MeasurementStreamId, StreamExecution>,
    timer: Pin<Box<tokio::time::Sleep>>,
    rejected_revision: Option<u64>,
}

impl MeasurementEngine {
    pub fn new(
        settings: &NetworkSettings,
        desired: watch::Receiver<MeasurementPlan>,
        applied: watch::Sender<AppliedState>,
        measurements: broadcast::Sender<MeasurementEvent>,
        stop: watch::Receiver<bool>,
    ) -> anyhow::Result<Self> {
        let initial = desired.borrow().clone();
        initial.validate()?;
        let mut engine = Self {
            scheduler: ProbeScheduler::new(Instant::now()),
            icmp: IcmpDriver::new(settings)?,
            #[cfg(feature = "irtt")]
            irtt: IrttDriver::new(settings),
            plan: MeasurementPlan {
                revision: 0,
                active_interval: initial.active_interval,
                assignments: Vec::new(),
            },
            desired,
            applied,
            measurements,
            stop,
            icmp_status: BTreeMap::new(),
            timer: Box::pin(tokio::time::sleep(std::time::Duration::ZERO)),
            rejected_revision: None,
        };
        engine.apply(initial)?;
        Ok(engine)
    }

    fn apply(&mut self, plan: MeasurementPlan) -> anyhow::Result<()> {
        plan.validate()?;
        for assignment in &plan.assignments {
            if let Some(old) = self
                .plan
                .assignments
                .iter()
                .find(|a| a.stream.id == assignment.stream.id)
            {
                anyhow::ensure!(
                    old.stream == assignment.stream
                        && old.irtt_port == assignment.irtt_port
                        && old.irtt_hmac == assignment.irtt_hmac,
                    "stream IDs cannot change protocol identity"
                );
            }
        }
        let schedules = plan
            .assignments
            .iter()
            .map(|a| (a.stream.id, a.schedule))
            .collect::<Vec<_>>();
        for id in self.scheduler.reconcile(&schedules, Instant::now()) {
            self.discard(id);
        }
        let icmp_ids = plan
            .assignments
            .iter()
            .filter(|a| a.stream.source != MeasurementSource::Irtt)
            .map(|a| a.stream.id)
            .collect::<HashSet<_>>();
        self.icmp.retain(&icmp_ids);
        self.icmp_status.retain(|id, _| icmp_ids.contains(id));
        for assignment in plan
            .assignments
            .iter()
            .filter(|a| icmp_ids.contains(&a.stream.id))
        {
            self.icmp_status
                .entry(assignment.stream.id)
                .and_modify(|s| s.assignment = assignment.clone())
                .or_insert(StreamExecution {
                    assignment: assignment.clone(),
                    phase: ExecutionPhase::Ready,
                    failures: 0,
                    generation: 1,
                });
        }
        #[cfg(feature = "irtt")]
        self.irtt.reconcile(
            &plan
                .assignments
                .iter()
                .filter(|a| a.stream.source == MeasurementSource::Irtt)
                .cloned()
                .collect::<Vec<_>>(),
            plan.active_interval,
        );
        self.plan = plan;
        self.rejected_revision = None;
        self.publish_status();
        Ok(())
    }

    fn discard(&mut self, id: MeasurementStreamId) {
        self.icmp.discard(id);
        #[cfg(feature = "irtt")]
        self.irtt.discard(id);
    }

    fn publish_status(&self) {
        let mut streams = self.icmp_status.values().cloned().collect::<Vec<_>>();
        #[cfg(feature = "irtt")]
        streams.extend(self.irtt.statuses());
        streams.sort_by_key(|s| s.assignment.stream.id);
        let state = AppliedState {
            revision: self.plan.revision,
            streams,
            rejected_revision: self.rejected_revision,
        };
        self.applied.send_if_modified(|current| {
            if *current == state {
                false
            } else {
                *current = state;
                true
            }
        });
    }

    fn poll_progress(&mut self, cx: &mut Context<'_>) -> Poll<anyhow::Result<()>> {
        let now = Instant::now();
        let mut completions = self.icmp.expire(now);
        // Bound ICMP receive work even on a continuously readable raw socket.
        for _ in 0..32 {
            match self.icmp.poll_receive(cx) {
                Poll::Ready(Ok(Some(event))) => completions.push(event),
                Poll::Ready(Ok(None)) => {}
                Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                Poll::Pending => break,
            }
            cx.waker().wake_by_ref();
        }
        #[cfg(feature = "irtt")]
        self.irtt.poll_maintenance(cx, now, &mut completions);
        let (due, withdrawn) = self.scheduler.opportunities(now);
        for id in withdrawn {
            self.discard(id);
        }
        for id in due {
            let assignment = self
                .plan
                .assignments
                .iter()
                .find(|a| a.stream.id == id)
                .unwrap();
            match assignment.stream.source {
                MeasurementSource::IcmpEcho | MeasurementSource::IcmpTimestamp => {
                    match self.icmp.poll_send(assignment.stream, cx) {
                        Poll::Ready(Ok(accepted_at)) => {
                            self.scheduler.committed(id, accepted_at);
                            self.icmp_status.get_mut(&id).unwrap().phase = ExecutionPhase::Ready;
                        }
                        Poll::Ready(Err(error)) => {
                            warn!("ICMP {} send failed: {error:#}", assignment.stream.peer);
                            self.scheduler.withdraw(id);
                            let status = self.icmp_status.get_mut(&id).unwrap();
                            status.phase = ExecutionPhase::Failed;
                            status.failures = status.failures.saturating_add(1);
                        }
                        Poll::Pending => {}
                    }
                }
                #[cfg(feature = "irtt")]
                MeasurementSource::Irtt => {
                    if let Poll::Ready(progress) = self.irtt.poll_send(id, cx) {
                        if let Some(accepted_at) = progress.accepted_at {
                            self.scheduler.committed(id, accepted_at);
                        }
                        if progress.failed {
                            self.scheduler.withdraw(id);
                            self.irtt.discard(id);
                        }
                    }
                }
                #[cfg(not(feature = "irtt"))]
                MeasurementSource::Irtt => unreachable!("validated plan"),
            }
        }
        for completion in completions {
            // Retired sessions cannot publish; surviving sessions use the role
            // applied now, including replies to probes sent before promotion.
            if let Some(assignment) = self
                .plan
                .assignments
                .iter()
                .find(|a| a.stream == completion.stream)
            {
                let _ = self.measurements.send(MeasurementEvent {
                    stream: completion.stream,
                    role: assignment.role,
                    started_at: completion.started_at,
                    completed_at: completion.completed_at,
                    outcome: completion.outcome,
                });
            }
        }
        self.publish_status();
        let deadline = self
            .scheduler
            .next_deadline()
            .into_iter()
            .chain(self.icmp.next_deadline())
            .chain({
                #[cfg(feature = "irtt")]
                {
                    self.irtt.next_deadline()
                }
                #[cfg(not(feature = "irtt"))]
                {
                    None
                }
            })
            .min();
        if let Some(deadline) = deadline {
            self.timer.as_mut().reset(deadline.into());
            if self.timer.as_mut().poll(cx).is_ready() {
                cx.waker().wake_by_ref();
            }
        }
        Poll::Pending
    }

    pub async fn run(mut self) -> anyhow::Result<()> {
        let mut desired = self.desired.clone();
        let mut stop = self.stop.clone();
        loop {
            tokio::select! {
                changed = desired.changed() => {
                    changed.map_err(|_| ChannelClosed)?;
                    let plan = desired.borrow_and_update().clone();
                    let revision = plan.revision;
                    if let Err(error) = self.apply(plan) {
                        warn!("Rejected measurement plan {revision}: {error:#}");
                        self.rejected_revision = Some(revision);
                        self.publish_status();
                    }
                },
                changed = stop.changed() => { changed.map_err(|_| ChannelClosed)?; if *stop.borrow() { break; } },
                result = poll_fn(|cx| self.poll_progress(cx)) => { result?; },
            }
        }
        self.plan.assignments.clear();
        self.icmp_status.clear();
        self.icmp.retain(&HashSet::new());
        #[cfg(feature = "irtt")]
        {
            self.irtt.shutdown();
            poll_fn(|cx| {
                self.irtt
                    .poll_maintenance(cx, Instant::now(), &mut Vec::new());
                self.publish_status();
                if self.irtt.is_closed() {
                    return Poll::Ready(());
                }
                if let Some(deadline) = self.irtt.next_deadline() {
                    self.timer.as_mut().reset(deadline.into());
                    if self.timer.as_mut().poll(cx).is_ready() {
                        cx.waker().wake_by_ref();
                    }
                }
                Poll::Pending
            })
            .await;
        }
        self.publish_status();
        Ok(())
    }
}
