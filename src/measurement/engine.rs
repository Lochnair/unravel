// SPDX-FileCopyrightText: 2026-Present Nils Andreas Svee mailto:contact@lochnair.net (github @Lochnair)
//
// SPDX-License-Identifier: MPL-2.0

#[cfg(feature = "irtt")]
use super::irtt::IrttDriver;
use super::{
    icmp::IcmpDriver,
    integrity::MeasurementIntegrity,
    model::{MeasurementEvent, MeasurementOutcome, MeasurementRole, MeasurementTarget},
    plan::{AppliedState, AppliedTarget, MeasurementPlan},
    scheduler::ProbeScheduler,
};
use crate::{
    error::ChannelClosed, metrics::Metric, metrics::MetricsSender, settings::NetworkSettings,
    time::ClockStep,
};
use log::{trace, warn};
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
    icmp_status: BTreeMap<MeasurementTarget, bool>,
    /// Scratch for each pass's due and withdrawn targets, kept for capacity.
    due: Vec<MeasurementTarget>,
    withdrawn: Vec<MeasurementTarget>,
    timer: Pin<Box<tokio::time::Sleep>>,
    rejected_revision: Option<u64>,
    integrity: MeasurementIntegrity,
    clock_steps: watch::Receiver<Option<ClockStep>>,
    metrics: MetricsSender,
}

impl MeasurementEngine {
    /// Raw one-way times are kept, including ones integrity rejected.
    fn publish_metric(&self, event: &MeasurementEvent, role: MeasurementRole) {
        let metric = match event.outcome {
            MeasurementOutcome::Loss => Metric::loss(&event.target, role),
            MeasurementOutcome::Reply { one_way, .. } => {
                let Some(rtt_ms) = event.rtt_ms() else {
                    return;
                };
                let one_way = one_way.map(|o| {
                    (
                        o.uplink.as_secs_f64() * 1000.0,
                        o.downlink.as_secs_f64() * 1000.0,
                    )
                });
                Metric::ping(&event.target, role, rtt_ms, one_way)
            }
        };
        self.metrics.send(metric);
    }

    pub fn new(
        settings: &NetworkSettings,
        desired: watch::Receiver<MeasurementPlan>,
        applied: watch::Sender<AppliedState>,
        measurements: broadcast::Sender<MeasurementEvent>,
        stop: watch::Receiver<bool>,
        clock_steps: watch::Receiver<Option<ClockStep>>,
        metrics: MetricsSender,
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
            due: Vec::new(),
            withdrawn: Vec::new(),
            timer: Box::pin(tokio::time::sleep(std::time::Duration::ZERO)),
            rejected_revision: None,
            integrity: MeasurementIntegrity::default(),
            clock_steps,
            metrics,
        };
        engine.apply(initial)?;
        Ok(engine)
    }

    fn apply(&mut self, plan: MeasurementPlan) -> anyhow::Result<()> {
        plan.validate()?;
        anyhow::ensure!(
            plan.revision >= self.plan.revision,
            "measurement plan revision went backwards"
        );
        anyhow::ensure!(
            plan.revision != self.plan.revision || plan == self.plan,
            "measurement plan changed without a new revision"
        );
        let schedules = plan
            .assignments
            .iter()
            .map(|a| (a.target.clone(), a.schedule))
            .collect::<Vec<_>>();
        for target in self.scheduler.reconcile(&schedules, Instant::now()) {
            self.discard(&target);
        }
        let icmp_targets = plan
            .assignments
            .iter()
            .filter(|a| !matches!(a.target, MeasurementTarget::Irtt { .. }))
            .map(|a| a.target.clone())
            .collect::<HashSet<_>>();
        self.icmp.retain(&icmp_targets);
        self.icmp_status
            .retain(|target, _| icmp_targets.contains(target));
        for target in icmp_targets {
            self.icmp_status.entry(target).or_insert(false);
        }
        #[cfg(feature = "irtt")]
        self.irtt.reconcile(
            &plan
                .assignments
                .iter()
                .filter(|a| matches!(a.target, MeasurementTarget::Irtt { .. }))
                .map(|a| a.target.clone())
                .collect::<Vec<_>>(),
            plan.active_interval,
        );
        self.integrity
            .retain(&plan.assignments.iter().map(|a| a.target.clone()).collect());
        self.plan = plan;
        self.rejected_revision = None;
        self.publish_status();
        Ok(())
    }

    fn discard(&mut self, target: &MeasurementTarget) {
        self.icmp.discard(target);
        #[cfg(feature = "irtt")]
        self.irtt.discard(target);
    }

    fn is_ready(&self, target: &MeasurementTarget) -> bool {
        match target {
            MeasurementTarget::IcmpEcho { .. } | MeasurementTarget::IcmpTimestamps { .. } => {
                self.icmp_status[target]
            }
            #[cfg(feature = "irtt")]
            MeasurementTarget::Irtt { .. } => self.irtt.is_ready(target),
            #[cfg(not(feature = "irtt"))]
            MeasurementTarget::Irtt { .. } => false,
        }
    }

    /// Whether the published state already says what a new one would.
    fn status_published(&self) -> bool {
        let current = self.applied.borrow();
        current.revision == self.plan.revision
            && current.rejected_revision == self.rejected_revision
            && current.targets.len() == self.plan.assignments.len()
            && current
                .targets
                .iter()
                .zip(&self.plan.assignments)
                .all(|(published, assignment)| {
                    published.assignment == *assignment
                        && published.ready == self.is_ready(&assignment.target)
                })
    }

    fn publish_status(&self) {
        // Runs on every engine wakeup; build a new state only when it differs.
        if self.status_published() {
            return;
        }
        let targets = self
            .plan
            .assignments
            .iter()
            .map(|assignment| AppliedTarget {
                assignment: assignment.clone(),
                ready: self.is_ready(&assignment.target),
            })
            .collect();
        let state = AppliedState {
            revision: self.plan.revision,
            targets,
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
        self.sync_clock()?;
        let now = Instant::now();
        let mut completions = self.icmp.expire(now);
        // Bound ICMP receive work even on a continuously readable raw socket.
        // Pending leaves the socket holding our waker; only a spent budget
        // needs another pass.
        let mut drained = false;
        for _ in 0..32 {
            match self.icmp.poll_receive(cx) {
                Poll::Ready(Ok(Some(event))) => completions.push(event),
                Poll::Ready(Ok(None)) => {}
                Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                Poll::Pending => {
                    drained = true;
                    break;
                }
            }
        }
        if !drained {
            cx.waker().wake_by_ref();
        }
        #[cfg(feature = "irtt")]
        self.irtt.poll_maintenance(cx, now, &mut completions);
        let mut due = std::mem::take(&mut self.due);
        self.scheduler
            .opportunities(now, &mut due, &mut self.withdrawn);
        while let Some(target) = self.withdrawn.pop() {
            self.discard(&target);
        }
        // A correlated reply proves ICMP receive readiness. A later send error
        // in this poll takes precedence over that reply.
        for completion in &completions {
            if matches!(completion.outcome, MeasurementOutcome::Reply { .. })
                && let Some(ready) = self.icmp_status.get_mut(&completion.target)
            {
                *ready = true;
            }
        }
        for target in due.drain(..) {
            match &target {
                MeasurementTarget::IcmpEcho { .. } | MeasurementTarget::IcmpTimestamps { .. } => {
                    match self.icmp.poll_send(&target, cx) {
                        Poll::Ready(Ok(accepted_at)) => {
                            self.scheduler.committed(&target, accepted_at);
                            trace!("Probe accepted: {target:?}");
                        }
                        Poll::Ready(Err(error)) => {
                            warn!("ICMP {target:?} send failed: {error:#}");
                            self.scheduler.withdraw(&target);
                            *self.icmp_status.get_mut(&target).unwrap() = false;
                        }
                        Poll::Pending => {}
                    }
                }
                #[cfg(feature = "irtt")]
                MeasurementTarget::Irtt { .. } => {
                    if let Poll::Ready(progress) = self.irtt.poll_send(&target, cx) {
                        if let Some(accepted_at) = progress.accepted_at {
                            self.scheduler.committed(&target, accepted_at);
                            trace!("Probe accepted: {target:?}");
                        }
                        if progress.failed {
                            self.scheduler.withdraw(&target);
                            self.irtt.discard(&target);
                        }
                    }
                }
                #[cfg(not(feature = "irtt"))]
                MeasurementTarget::Irtt { .. } => unreachable!("validated plan"),
            }
        }
        self.due = due;
        // Consumers must see readiness before the first usable measurement.
        self.publish_status();
        self.sync_clock()?;
        for mut completion in completions {
            // Retired targets must not recreate integrity state with late replies.
            let Some(role) = self
                .plan
                .assignments
                .iter()
                .find(|a| a.target == completion.target)
                .map(|a| a.role)
            else {
                continue;
            };
            self.integrity.assess(&mut completion);
            self.publish_metric(&completion, role);
            if let MeasurementOutcome::Reply {
                one_way: Some(owd), ..
            } = completion.outcome
                && let super::model::OneWayStatus::Unusable(reason) = owd.status
            {
                trace!(
                    "Rejected directional timing: {} {completion:?}",
                    reason.reason()
                );
            }
            let _ = self.measurements.send(completion);
        }
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
            self.arm_timer(deadline, cx);
        }
        Poll::Pending
    }

    /// Resetting re-registers with the timer driver, so skip it when the
    /// pending deadline is already the one we want.
    fn arm_timer(&mut self, deadline: Instant, cx: &mut Context<'_>) {
        let deadline = deadline.into();
        if self.timer.deadline() != deadline || self.timer.is_elapsed() {
            self.timer.as_mut().reset(deadline);
        }
        if self.timer.as_mut().poll(cx).is_ready() {
            cx.waker().wake_by_ref();
        }
    }

    pub async fn run(mut self) -> anyhow::Result<()> {
        let result = self.run_until_stop().await;
        self.shutdown().await;
        result
    }

    fn sync_clock(&mut self) -> anyhow::Result<()> {
        if self.clock_steps.has_changed().map_err(|_| ChannelClosed)? {
            let step = *self.clock_steps.borrow_and_update();
            if let Some(step) = step {
                self.integrity.local_clock_step(step.detected_at);
                // Prepared ICMP packets already contain a realtime originate
                // timestamp. Don't send one from before the detected change.
                for target in self
                    .plan
                    .assignments
                    .iter()
                    .map(|a| a.target.clone())
                    .collect::<Vec<_>>()
                {
                    self.discard(&target);
                }
            }
        }
        Ok(())
    }

    async fn run_until_stop(&mut self) -> anyhow::Result<()> {
        let mut desired = self.desired.clone();
        let mut stop = self.stop.clone();
        let mut clock_steps = self.clock_steps.clone();
        if *stop.borrow() {
            return Ok(());
        }
        loop {
            tokio::select! {
                biased;
                changed = clock_steps.changed() => {
                    changed.map_err(|_| ChannelClosed)?;
                    self.sync_clock()?;
                },
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
                changed = stop.changed() => { changed.map_err(|_| ChannelClosed)?; if *stop.borrow() { return Ok(()); } },
                result = poll_fn(|cx| self.poll_progress(cx)) => { result?; },
            }
        }
    }

    async fn shutdown(&mut self) {
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
                    self.arm_timer(deadline, cx);
                }
                Poll::Pending
            })
            .await;
        }
        self.publish_status();
    }
}
