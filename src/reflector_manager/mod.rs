// SPDX-FileCopyrightText: 2026-Present Nils Andreas Svee mailto:contact@lochnair.net (github @Lochnair)
//
// SPDX-License-Identifier: MPL-2.0

mod evidence;
use crate::{
    error::ChannelClosed,
    measurement::{
        model::{MeasurementEvent, MeasurementRole, MeasurementTarget},
        plan::{AppliedState, AppliedTarget, MeasurementAssignment, MeasurementPlan},
        scheduler::{ProbeSchedule, evenly_spaced_offset},
    },
    metrics::{Metric, MetricsSender},
    settings::{ReflectorProtocol, Settings},
};
use evidence::{Evidence, Quality, WINDOW};
use log::{debug, info, warn};
use std::{
    collections::HashMap,
    net::{SocketAddr, ToSocketAddrs},
    time::{Duration, Instant},
};
use tokio::sync::{broadcast, watch};

const POLICY_INTERVAL: Duration = Duration::from_secs(30);

struct InventoryTarget {
    target: MeasurementTarget,
    bootstrap: bool,
    preference: usize,
}

struct InventoryGroup {
    targets: Vec<usize>,
    cursor: usize,
}

struct Live {
    inventory: usize,
    target: MeasurementTarget,
    evidence: Evidence,
    entered_at: Instant,
    strikes: u8,
}

impl Live {
    fn status<'a>(&self, applied: &'a AppliedState) -> Option<&'a AppliedTarget> {
        applied
            .targets
            .iter()
            .find(|s| s.assignment.target == self.target)
    }

    fn needs_recovery(&self) -> bool {
        self.strikes >= 3
    }

    fn check_health(&mut self, applied: &AppliedState, now: Instant) {
        let Some(status) = self.status(applied) else {
            return;
        };
        let deadline = status.assignment.schedule.period.saturating_mul(3);
        let last_reply = self
            .evidence
            .last_reply
            .unwrap_or(self.entered_at)
            .max(self.entered_at);
        let stale = now.saturating_duration_since(last_reply) > deadline;
        let unavailable =
            !status.ready && now.saturating_duration_since(self.entered_at) > deadline;
        self.strikes = if stale || unavailable {
            (self.strikes + 1).min(3)
        } else {
            0
        };
    }

    fn quality(&self, applied: &AppliedState, now: Instant, max_age: Duration) -> Option<Quality> {
        self.evidence.quality(self.status(applied)?, now, max_age)
    }
}

/// Selection owns desired slots and current evidence; the engine owns execution.
pub struct ReflectorManager {
    inventory: Vec<InventoryTarget>,
    groups: Vec<InventoryGroup>,
    next_group: usize,
    active: Vec<Option<Live>>,
    candidates: Vec<Option<Live>>,
    num_candidates: usize,
    active_period: Duration,
    candidate_period: Duration,
    next_policy_at: Instant,
    next_health_at: Instant,
    rng: fastrand::Rng,
    desired: watch::Sender<MeasurementPlan>,
    applied: watch::Receiver<AppliedState>,
    measurements: broadcast::Receiver<MeasurementEvent>,
    metrics: MetricsSender,
}

impl ReflectorManager {
    pub fn new(
        settings: &Settings,
        desired: watch::Sender<MeasurementPlan>,
        applied: watch::Receiver<AppliedState>,
        measurements: broadcast::Receiver<MeasurementEvent>,
        metrics: MetricsSender,
    ) -> anyhow::Result<Self> {
        let inventory = discover(settings)?;
        anyhow::ensure!(
            !inventory.is_empty(),
            "no usable reflectors for selected protocol preferences"
        );
        let now = Instant::now();
        let mut rng = fastrand::Rng::new();
        let groups = (0..settings.protocol_preferences().len())
            .map(|preference| {
                let mut targets = (0..inventory.len())
                    .filter(|&i| inventory[i].preference == preference)
                    .collect::<Vec<_>>();
                rng.shuffle(&mut targets);
                InventoryGroup { targets, cursor: 0 }
            })
            .collect();
        let mut startup = (0..inventory.len()).collect::<Vec<_>>();
        rng.shuffle(&mut startup);
        startup.sort_by_key(|&i| (inventory[i].preference, !inventory[i].bootstrap));
        let mut manager = Self {
            inventory,
            groups,
            next_group: 0,
            active: (0..settings.reflectors.num_reflectors)
                .map(|_| None)
                .collect(),
            candidates: (0..settings.reflectors.num_candidates)
                .map(|_| None)
                .collect(),
            num_candidates: settings.reflectors.num_candidates,
            active_period: Duration::from_secs_f64(settings.advanced_settings.tick_interval),
            candidate_period: Duration::from_secs_f64(settings.reflectors.candidate_period),
            next_policy_at: now + POLICY_INTERVAL,
            next_health_at: now,
            rng,
            desired,
            applied,
            measurements,
            metrics,
        };
        for slot in 0..manager.active.len() {
            let Some(&target) = startup.iter().find(|&&i| {
                !manager
                    .active
                    .iter()
                    .flatten()
                    .any(|a| a.target.peer() == manager.inventory[i].target.peer())
            }) else {
                break;
            };
            manager.active[slot] = Some(manager.assign(target, now));
        }
        if manager.active.iter().any(Option::is_none) {
            warn!("Inventory cannot fill all active slots with distinct peers");
        }
        manager.fill_candidates(now, &[]);
        manager.publish()?;
        Ok(manager)
    }

    fn evaluation_period(&self) -> Duration {
        POLICY_INTERVAL.max(self.candidate_period.saturating_mul(WINDOW as u32))
    }

    fn max_age(&self) -> Duration {
        self.evaluation_period()
            .max(self.active_period.saturating_mul(WINDOW as u32))
    }

    fn assign(&mut self, target: usize, now: Instant) -> Live {
        Live {
            inventory: target,
            target: self.inventory[target].target.clone(),
            evidence: Evidence::new(now),
            entered_at: now,
            strikes: 0,
        }
    }

    fn next_target(&mut self, excluded: &[usize]) -> Option<usize> {
        for _ in 0..self.groups.len() {
            let group_index = self.next_group;
            self.next_group = (self.next_group + 1) % self.groups.len();
            let group = &mut self.groups[group_index];
            if group.cursor >= group.targets.len() {
                self.rng.shuffle(&mut group.targets);
                group.cursor = 0;
            }
            for _ in 0..group.targets.len() {
                // Finish this scan before reshuffling, so every target is
                // visited even when the scan crosses the end of the deck.
                let target = group.targets[group.cursor % group.targets.len()];
                group.cursor += 1;
                if !excluded.contains(&target)
                    && !self
                        .active
                        .iter()
                        .chain(&self.candidates)
                        .flatten()
                        .any(|a| a.inventory == target)
                {
                    return Some(target);
                }
            }
        }
        None
    }

    fn fill_candidates(&mut self, now: Instant, excluded: &[usize]) {
        for slot in 0..self.candidates.len() {
            if self.candidates[slot].is_none()
                && let Some(target) = self.next_target(excluded)
            {
                self.candidates[slot] = Some(self.assign(target, now));
                debug!(
                    "Evaluating {} ({}) in candidate slot {slot}",
                    self.inventory[target].target.peer(),
                    self.inventory[target].target.protocol()
                );
            }
        }
    }

    fn synchronize(&mut self) -> anyhow::Result<AppliedState> {
        let applied = self.applied.borrow_and_update().clone();
        let desired = self.desired.borrow();
        anyhow::ensure!(
            applied.rejected_revision != Some(desired.revision),
            "engine rejected reflector plan {}",
            desired.revision
        );
        Ok(applied)
    }

    fn observe(&mut self, event: MeasurementEvent) {
        if let Some(live) = self
            .active
            .iter_mut()
            .chain(&mut self.candidates)
            .flatten()
            .find(|a| a.target == event.target)
        {
            live.evidence.record(event);
        }
    }

    // The incumbent participates in exactly the same shortlist as the standbys.
    // None means either keep it, or no qualified replacement exists.
    fn choose(
        &mut self,
        slot: usize,
        recovery: bool,
        applied: &AppliedState,
        now: Instant,
    ) -> Option<usize> {
        let mut choices = self
            .candidates
            .iter()
            .enumerate()
            .filter_map(|(candidate_slot, live)| {
                let live = live.as_ref()?;
                if self.active.iter().enumerate().any(|(other, a)| {
                    other != slot
                        && a.as_ref()
                            .is_some_and(|a| a.target.peer() == live.target.peer())
                }) {
                    return None;
                }
                Some((
                    Some(candidate_slot),
                    live.inventory,
                    live.quality(applied, now, self.max_age())?,
                ))
            })
            .collect::<Vec<_>>();
        if !recovery {
            let incumbent = self.active[slot].as_ref()?;
            choices.push((
                None,
                incumbent.inventory,
                incumbent.quality(applied, now, self.max_age())?,
            ));
        }
        let best = choices
            .iter()
            .map(|(_, _, q)| q.rtt)
            .min_by(f64::total_cmp)?;
        choices.retain(|(_, _, q)| q.rtt <= best + 5.0_f64.max(best * 0.20));
        if choices.iter().any(|(_, _, q)| q.directional) {
            choices.retain(|(_, _, q)| q.directional);
        }
        let preference = choices
            .iter()
            .map(|(_, i, _)| self.inventory[*i].preference)
            .min()?;
        choices.retain(|(_, i, _)| self.inventory[*i].preference == preference);
        if choices.iter().any(|(slot, _, _)| slot.is_none()) {
            return None;
        }
        let (slot, target, quality) = &choices[self.rng.usize(..choices.len())];
        debug!(
            "Shortlisted {}: {:?}, preference={preference}",
            self.inventory[*target].target.peer(),
            quality
        );
        *slot
    }

    fn promote(
        &mut self,
        slot: usize,
        candidate: usize,
        recovery: bool,
        now: Instant,
        retired: &mut Vec<usize>,
    ) {
        let mut next = self.candidates[candidate].take().unwrap();
        next.entered_at = now;
        next.strikes = 0;
        let peer = next.target.peer();
        let old = self.active[slot].replace(next);
        if recovery {
            if let Some(old) = old {
                retired.push(old.inventory);
            }
        } else {
            self.candidates[candidate] = old.map(|mut old| {
                old.entered_at = now;
                old.strikes = 0;
                old
            });
        }
        let reason = if recovery { "health" } else { "quality" };
        info!("Promoting {peer} into active slot {slot}: {reason}");
        self.metrics.send(Metric::Event {
            name: "reflector_promoted",
            reason,
            reflector: Some(peer),
            tags: &[],
        });
    }

    fn recovering(&self) -> bool {
        self.active
            .iter()
            .any(|a| a.as_ref().is_none_or(|a| a.needs_recovery()))
    }

    fn reconcile(&mut self, applied: &AppliedState, now: Instant) -> anyhow::Result<()> {
        // Evidence can arrive while a plan is pending; assignment decisions wait.
        if applied.revision != self.desired.borrow().revision {
            return Ok(());
        }
        if now >= self.next_health_at {
            let period = self.active_period.max(Duration::from_secs(1));
            self.next_health_at = now + period;
            for live in self.active.iter_mut().flatten() {
                live.check_health(applied, now);
            }
        }
        let periodic = now >= self.next_policy_at;
        let mut retired = Vec::new();
        let mut promoted = false;
        for slot in 0..self.active.len() {
            if self.active[slot]
                .as_ref()
                .is_none_or(|a| a.needs_recovery())
                && let Some(candidate) = self.choose(slot, true, applied, now)
            {
                self.promote(slot, candidate, true, now, &mut retired);
                promoted = true;
            }
        }
        let recovering = self.recovering();
        let open_temporary = self.num_candidates == 0 && recovering && self.candidates.is_empty();
        if open_temporary {
            self.candidates.push(None);
        }
        if self.num_candidates == 0 && !recovering {
            self.candidates.clear();
        }
        if periodic {
            self.next_policy_at = now + POLICY_INTERVAL;
            if !recovering && !promoted {
                let mut slots = self
                    .active
                    .iter()
                    .enumerate()
                    .filter_map(|(slot, a)| {
                        Some((slot, a.as_ref()?.quality(applied, now, self.max_age())?.rtt))
                    })
                    .collect::<Vec<_>>();
                slots.sort_by(|a, b| b.1.total_cmp(&a.1));
                for (slot, _) in slots {
                    if let Some(candidate) = self.choose(slot, false, applied, now) {
                        self.promote(slot, candidate, false, now, &mut retired);
                        break;
                    }
                }
            }
        }
        if periodic || promoted || open_temporary {
            self.fill_candidates(now, &retired);
        }
        if periodic {
            let oldest = self
                .candidates
                .iter()
                .enumerate()
                .filter_map(|(slot, a)| {
                    let a = a.as_ref()?;
                    (now.saturating_duration_since(a.entered_at) >= self.evaluation_period())
                        .then_some((slot, a.entered_at))
                })
                .min_by_key(|(_, at)| *at);
            if let Some((slot, _)) = oldest
                && let Some(target) = self.next_target(&retired)
            {
                let next = self.assign(target, now);
                let old = self.candidates[slot].replace(next).unwrap();
                info!(
                    "Exploring {} ({}) in candidate slot {slot}, replacing {}",
                    self.inventory[target].target.peer(),
                    self.inventory[target].target.protocol(),
                    old.target.peer()
                );
                self.metrics.send(Metric::Event {
                    name: "reflector_explored",
                    reason: "inventory",
                    reflector: Some(self.inventory[target].target.peer()),
                    tags: &[],
                });
            }
        }
        self.publish()
    }

    fn publish(&mut self) -> anyhow::Result<()> {
        let mut assignments = Vec::new();
        for (slots, role, period) in [
            (&self.active, MeasurementRole::Active, self.active_period),
            (
                &self.candidates,
                MeasurementRole::Candidate,
                self.candidate_period,
            ),
        ] {
            for (slot, live) in slots.iter().enumerate() {
                let Some(live) = live else { continue };
                assignments.push(MeasurementAssignment {
                    target: live.target.clone(),
                    role,
                    schedule: ProbeSchedule {
                        period,
                        offset: evenly_spaced_offset(period, slot, slots.len()),
                    },
                });
            }
        }
        assignments.sort_by(|a, b| a.target.cmp(&b.target));
        if self.desired.borrow().assignments == assignments {
            return Ok(());
        }
        let plan = MeasurementPlan {
            revision: self.desired.borrow().revision + 1,
            active_interval: self.active_period,
            assignments,
        };
        plan.validate()?;
        debug!(
            "Publishing measurement plan {}: {} assignments",
            plan.revision,
            plan.assignments.len()
        );
        self.desired.send(plan).map_err(|_| ChannelClosed)?;
        Ok(())
    }

    pub async fn run(mut self) -> anyhow::Result<()> {
        let mut maintenance = tokio::time::interval(Duration::from_secs(1));
        maintenance.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            let event = tokio::select! {
                event = self.measurements.recv() => Some(event),
                changed = self.applied.changed() => { changed.map_err(|_| ChannelClosed)?; None },
                _ = maintenance.tick() => None,
            };
            let now = Instant::now();
            let applied = self.synchronize()?;
            match event {
                Some(Ok(event)) => self.observe(event),
                Some(Err(broadcast::error::RecvError::Lagged(count))) => {
                    warn!("Reflector manager missed {count} measurements");
                    self.metrics.send(Metric::Event {
                        name: "measurement_lag",
                        reason: "reflector_manager",
                        reflector: None,
                        tags: &[],
                    });
                }
                Some(Err(broadcast::error::RecvError::Closed)) => return Err(ChannelClosed.into()),
                None => {}
            }
            self.reconcile(&applied, now)?;
        }
    }
}

fn discover(settings: &Settings) -> anyhow::Result<Vec<InventoryTarget>> {
    let preferences = settings.protocol_preferences();
    // Country is needed only to validate duplicate definitions during discovery.
    let mut inventory: HashMap<(SocketAddr, ReflectorProtocol), (InventoryTarget, Option<String>)> =
        HashMap::new();
    for (bootstrap, definitions) in [
        (true, &settings.reflectors.bootstrap),
        (false, &settings.reflectors.entries),
    ] {
        for definition in definitions {
            if !bootstrap
                && !settings.reflectors.countries.is_empty()
                && definition.country.as_ref().is_some_and(|country| {
                    !settings
                        .reflectors
                        .countries
                        .iter()
                        .any(|allowed| allowed.eq_ignore_ascii_case(country))
                })
            {
                continue;
            }
            for (preference, &protocol) in preferences.iter().enumerate() {
                if !definition.protocols.contains(&protocol) {
                    continue;
                }
                let port = if protocol == ReflectorProtocol::Irtt {
                    definition.irtt_port.unwrap_or(2112)
                } else {
                    0
                };
                let endpoints = match (definition.host.as_str(), port).to_socket_addrs() {
                    Ok(endpoints) => endpoints,
                    Err(error) => {
                        warn!("Failed to resolve reflector {}: {error}", definition.host);
                        continue;
                    }
                };
                for endpoint in endpoints {
                    let target = match (protocol, endpoint.ip()) {
                        (ReflectorProtocol::IcmpEcho, std::net::IpAddr::V4(peer)) => {
                            MeasurementTarget::IcmpEcho(peer)
                        }
                        (ReflectorProtocol::IcmpTimestamps, std::net::IpAddr::V4(peer)) => {
                            MeasurementTarget::IcmpTimestamps(peer)
                        }
                        (ReflectorProtocol::Irtt, _) => MeasurementTarget::Irtt {
                            endpoint,
                            authentication: definition
                                .irtt_hmac
                                .as_deref()
                                .filter(|key| !key.trim().is_empty())
                                .map(Into::into),
                        },
                        _ => continue,
                    };
                    if let Some((existing, country)) = inventory.get_mut(&(endpoint, protocol)) {
                        let same_country = match (&*country, &definition.country) {
                            (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
                            _ => true,
                        };
                        anyhow::ensure!(
                            same_country && existing.target == target,
                            "conflicting reflector definitions for {endpoint} ({protocol})"
                        );
                        if country.is_none() {
                            country.clone_from(&definition.country);
                        }
                        existing.bootstrap |= bootstrap;
                    } else {
                        inventory.insert(
                            (endpoint, protocol),
                            (
                                InventoryTarget {
                                    target,
                                    bootstrap,
                                    preference,
                                },
                                definition.country.clone(),
                            ),
                        );
                    }
                }
            }
        }
    }
    let mut inventory = inventory
        .into_values()
        .map(|(target, _)| target)
        .collect::<Vec<_>>();
    inventory.sort_by(|a, b| {
        (a.preference, !a.bootstrap, &a.target).cmp(&(b.preference, !b.bootstrap, &b.target))
    });
    Ok(inventory)
}
