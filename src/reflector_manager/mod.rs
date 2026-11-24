// SPDX-FileCopyrightText: 2026-Present Nils Andreas Svee mailto:contact@lochnair.net (github @Lochnair)
//
// SPDX-License-Identifier: MPL-2.0

mod evidence;
mod inventory;
use crate::{
    error::ChannelClosed,
    measurement::{
        model::{MeasurementEvent, MeasurementProtocol, MeasurementRole, MeasurementTarget},
        plan::{AppliedState, MeasurementAssignment, MeasurementPlan},
        scheduler::{ProbeSchedule, evenly_spaced_offset},
    },
    metrics::{Metric, MetricsSender},
    settings::Settings,
};
use evidence::{Evidence, STREAK, WINDOW};
use inventory::{InventoryGroup, InventoryTarget, discover};
use log::{debug, info, warn};
use std::{
    collections::HashSet,
    future::Future,
    net::IpAddr,
    pin::Pin,
    time::{Duration, Instant},
};
use tokio::sync::{broadcast, watch};

// Preference only breaks ties within a class's RTT shortlist.
const PROTOCOLS: &[MeasurementProtocol] = &[
    #[cfg(feature = "irtt")]
    MeasurementProtocol::Irtt,
    MeasurementProtocol::IcmpTimestamps,
    MeasurementProtocol::IcmpEcho,
];

const POLICY_INTERVAL: Duration = Duration::from_secs(30);
// A target fails once it has been continuously unhealthy for this long. Actives
// are probed faster than candidates, so without it a link-wide blip would fail
// every active while the candidates still looked qualified.
const DEBOUNCE: Duration = Duration::from_secs(2);
// Evidence tiers: index into `Seat::rtt`, and the reason a slot is open.
const SAMPLES: [usize; 2] = [STREAK, WINDOW];
const REASON: [&str; 2] = ["health", "quality"];
const RECOVERY: usize = 0;
const IMPROVEMENT: usize = 1;

/// A target under measurement. Evidence follows it across role changes.
struct Live {
    inventory: usize,
    evidence: Evidence,
    entered_at: Instant,
    unhealthy_since: Option<Instant>,
}

/// Everything the decision needs to know about one live target.
#[derive(Clone, Debug)]
struct Seat {
    ip: IpAddr,
    /// RTT only: the lower class, used when too few directional peers qualify.
    echo: bool,
    preference: usize,
    failed: bool,
    /// Median RTT when qualified over `SAMPLES[tier]` outcomes.
    rtt: [Option<f64>; 2],
}

impl Live {
    fn new(inventory: usize, now: Instant) -> Self {
        Self {
            inventory,
            evidence: Evidence::new(now),
            entered_at: now,
            unhealthy_since: None,
        }
    }

    fn enter(mut self, now: Instant) -> Self {
        self.entered_at = now;
        self.unhealthy_since = None;
        self
    }

    fn failed(&self, now: Instant) -> bool {
        self.unhealthy_since
            .is_some_and(|at| now.saturating_duration_since(at) >= DEBOUNCE)
    }

    fn assess(
        &mut self,
        entry: &InventoryTarget,
        period: Duration,
        applied: &AppliedState,
        now: Instant,
    ) -> Seat {
        let status = applied
            .targets
            .iter()
            .find(|s| s.assignment.target == entry.target);
        let grace = period.saturating_mul(3);
        let last_reply = self
            .evidence
            .last_reply
            .map_or(self.entered_at, |at| at.max(self.entered_at));
        let unhealthy = now.saturating_duration_since(self.entered_at) >= grace
            && (now.saturating_duration_since(last_reply) > grace
                || status.is_none_or(|s| !s.ready)
                || self
                    .evidence
                    .timing_unhealthy(now, period.saturating_mul((WINDOW + 1) as u32)));
        self.unhealthy_since = unhealthy.then(|| self.unhealthy_since.unwrap_or(now));
        let rtt = SAMPLES.map(|samples| self.evidence.quality(status?, now, samples));
        Seat {
            ip: entry.target.peer(),
            echo: matches!(entry.target, MeasurementTarget::IcmpEcho(_)),
            preference: entry.preference,
            failed: self.failed(now),
            rtt,
        }
    }
}

/// Choose promotions as (active slot, candidate slot, evidence tier).
fn select_promotions(
    active: &[Option<Seat>],
    candidates: &[Option<Seat>],
    periodic: bool,
    rng: &mut fastrand::Rng,
) -> Vec<(usize, usize, usize)> {
    let open = |slot: &usize| active[*slot].as_ref().is_none_or(|s| s.failed);
    let mut ips = active
        .iter()
        .map(|s| s.as_ref().map(|s| s.ip))
        .collect::<Vec<_>>();
    let mut promotions = Vec::new();

    // Fill empty or failed slots.
    for slot in (0..active.len()).filter(open) {
        let tier = RECOVERY;
        if let Some(candidate) = challenger(active, candidates, &ips, &promotions, slot, tier, rng)
        {
            ips[slot] = candidates[candidate].as_ref().map(|seat| seat.ip);
            promotions.push((slot, candidate, tier));
        }
    }

    // Periodically make one improvement: a directional upgrade of an echo slot
    // first, otherwise the slowest slot that has a better challenger.
    if periodic {
        let tier = IMPROVEMENT;
        let is_upgrade = |&(slot, candidate): &(usize, usize)| {
            active[slot].as_ref().is_some_and(|s| s.echo)
                && candidates[candidate].as_ref().is_some_and(|s| !s.echo)
        };
        let incumbent_rtt = |&(slot, _): &(usize, usize)| {
            active[slot]
                .as_ref()
                .and_then(|s| s.rtt[tier])
                .unwrap_or(f64::INFINITY)
        };
        let best = (0..active.len())
            .filter(|slot| !open(slot))
            .filter_map(|slot| {
                let candidate = challenger(active, candidates, &ips, &promotions, slot, tier, rng)?;
                Some((slot, candidate))
            })
            .max_by(|a, b| {
                is_upgrade(a)
                    .cmp(&is_upgrade(b))
                    .then_with(|| incumbent_rtt(a).total_cmp(&incumbent_rtt(b)))
            });
        promotions.extend(best.map(|(slot, candidate)| (slot, candidate, tier)));
    }
    promotions
}

/// The candidate that should take `slot` at `tier`, if any beats the incumbent.
fn challenger(
    active: &[Option<Seat>],
    candidates: &[Option<Seat>],
    ips: &[Option<IpAddr>],
    promotions: &[(usize, usize, usize)],
    slot: usize,
    tier: usize,
    rng: &mut fastrand::Rng,
) -> Option<usize> {
    let incumbent = active[slot].as_ref();
    // Echo never displaces a healthy directional incumbent.
    let protected = incumbent.is_some_and(|s| !s.failed && !s.echo);
    // An incumbent that qualifies competes on equal terms.
    let pool = candidates
        .iter()
        .enumerate()
        .filter_map(|(candidate, seat)| Some((Some(candidate), seat.as_ref()?)))
        .filter(|(candidate, seat)| {
            !(protected && seat.echo)
                && !promotions.iter().any(|p| Some(p.1) == *candidate)
                && !ips
                    .iter()
                    .enumerate()
                    .any(|(other, ip)| other != slot && *ip == Some(seat.ip))
        })
        .chain(incumbent.map(|seat| (None, seat)))
        .filter_map(|(source, seat)| Some((source, seat.rtt[tier]?, seat)))
        .collect::<Vec<_>>();
    rank(&pool, rng)
}

/// Directional before echo, then the RTT shortlist, then protocol preference.
/// The incumbent (`None`) keeps its slot if shortlisted; otherwise a random
/// pick spreads load.
fn rank(pool: &[(Option<usize>, f64, &Seat)], rng: &mut fastrand::Rng) -> Option<usize> {
    // Echo competes only when nothing directional qualifies.
    let echo = pool.iter().all(|c| c.2.echo);
    let pool = pool.iter().filter(|c| c.2.echo == echo);
    let best = pool.clone().map(|c| c.1).min_by(f64::total_cmp)?;
    let shortlist = pool.filter(|c| c.1 <= best + 5.0_f64.max(best * 0.20));
    let preference = shortlist.clone().map(|c| c.2.preference).min()?;
    let shortlist = shortlist
        .filter(|c| c.2.preference == preference)
        .collect::<Vec<_>>();
    if shortlist.iter().any(|c| c.0.is_none()) {
        return None;
    }
    shortlist[rng.usize(..shortlist.len())].0
}

type PoolDiscovery = Pin<Box<dyn Future<Output = anyhow::Result<Vec<InventoryTarget>>> + Send>>;

/// Selection owns desired slots and current evidence; the engine owns execution.
pub struct ReflectorManager {
    inventory: Vec<InventoryTarget>,
    groups: Vec<InventoryGroup>,
    next_group: usize,
    active: Vec<Option<Live>>,
    candidates: Vec<Option<Live>>,
    next_policy_at: Instant,
    pool_discovery: Option<PoolDiscovery>,
    active_period: Duration,
    candidate_period: Duration,
    rng: fastrand::Rng,
    desired: watch::Sender<MeasurementPlan>,
    applied: watch::Receiver<AppliedState>,
    /// The engine's last applied state, cloned only when it changes.
    applied_state: AppliedState,
    /// Slots changed since the plan was last published.
    unpublished: bool,
    measurements: broadcast::Receiver<MeasurementEvent>,
    metrics: MetricsSender,
}

impl ReflectorManager {
    pub async fn new(
        settings: &Settings,
        active_period: Duration,
        desired: watch::Sender<MeasurementPlan>,
        applied: watch::Receiver<AppliedState>,
        measurements: broadcast::Receiver<MeasurementEvent>,
        metrics: MetricsSender,
    ) -> anyhow::Result<Self> {
        let reflectors = &settings.reflectors;
        let mut inventory = discover(&reflectors.bootstrap, &[]).await?;
        let (entries, countries) = (reflectors.entries.clone(), reflectors.countries.clone());
        let pool = async move { discover(&entries, &countries).await };
        let pool_discovery = if inventory.is_empty() || reflectors.num_candidates == 0 {
            inventory::merge(&mut inventory, pool.await?)?;
            None
        } else {
            Some(Box::pin(pool) as PoolDiscovery)
        };
        anyhow::ensure!(
            !inventory.is_empty(),
            "no usable reflectors with protocols supported by this build"
        );
        let now = Instant::now();
        let mut rng = fastrand::Rng::new();
        let groups = inventory::groups(&inventory, &mut rng);
        let mut startup = (0..inventory.len()).collect::<Vec<_>>();
        // Keep bootstrap peers first within each protocol preference.
        startup.sort_by_key(|&i| inventory[i].preference);
        let mut peers = HashSet::new();
        startup.retain(|&i| peers.insert(inventory[i].target.peer()));
        let slots = reflectors.num_reflectors as usize;
        if startup.len() > slots {
            warn!("Configured startup set exceeds num_reflectors; selecting {slots} peers");
        } else if startup.len() < slots {
            warn!("Inventory cannot fill all active slots with distinct peers");
        }
        let mut manager = Self {
            inventory,
            groups,
            next_group: 0,
            active: (0..slots)
                .map(|slot| startup.get(slot).map(|&i| Live::new(i, now)))
                .collect(),
            candidates: (0..reflectors.num_candidates).map(|_| None).collect(),
            next_policy_at: now + POLICY_INTERVAL,
            pool_discovery,
            active_period,
            candidate_period: Duration::from_secs_f64(reflectors.candidate_period),
            rng,
            desired,
            applied,
            applied_state: AppliedState::default(),
            unpublished: true,
            measurements,
            metrics,
        };
        // With no evidence yet, a pass only fills the candidate slots.
        manager.select(&AppliedState::default(), now)?;
        Ok(manager)
    }

    /// One pass: assess peers, apply promotions, refresh candidates, publish.
    fn select(&mut self, applied: &AppliedState, now: Instant) -> anyhow::Result<()> {
        let periodic = now >= self.next_policy_at;
        if periodic {
            self.next_policy_at = now + POLICY_INTERVAL;
        }
        let inventory = &self.inventory;
        let assess = |slots: &mut [Option<Live>], period| {
            slots
                .iter_mut()
                .map(|live| {
                    let live = live.as_mut()?;
                    Some(live.assess(&inventory[live.inventory], period, applied, now))
                })
                .collect::<Vec<_>>()
        };
        let active = assess(&mut self.active, self.active_period);
        let candidates = assess(&mut self.candidates, self.candidate_period);
        // A target leaving the plan in this pass is not explored again in it.
        let seen = self
            .active
            .iter()
            .chain(&self.candidates)
            .flatten()
            .map(|live| live.inventory)
            .collect::<Vec<_>>();

        for (slot, candidate, tier) in
            select_promotions(&active, &candidates, periodic, &mut self.rng)
        {
            let next = self.candidates[candidate].take().unwrap().enter(now);
            let entry = &self.inventory[next.inventory].target;
            let peer = entry.peer();
            info!(
                "Promoting {peer} ({}) into active slot {slot}: {}",
                entry.protocol(),
                REASON[tier]
            );
            self.metrics.send(Metric::event(
                "reflector_promoted",
                REASON[tier],
                Some(peer),
            ));
            // A healthy incumbent stays under measurement.
            let keep = active[slot].as_ref().is_some_and(|s| !s.failed);
            let old = self.active[slot].replace(next);
            self.candidates[candidate] = old.filter(|_| keep).map(|old| old.enter(now));
            self.unpublished = true;
        }
        // Candidates: fill empty and failed slots, periodically rotate the
        // oldest resident. A slot only changes if exploration has a target.
        // Computed after promotions, so a just-demoted incumbent is not rotated.
        let evaluation = POLICY_INTERVAL.max(self.candidate_period.saturating_mul(WINDOW as u32));
        let resident = |live: &Live| now.saturating_duration_since(live.entered_at) >= evaluation;
        let oldest = (0..self.candidates.len())
            .filter(|&slot| periodic && self.candidates[slot].as_ref().is_some_and(resident))
            .min_by_key(|&slot| self.candidates[slot].as_ref().map(|live| live.entered_at));
        for slot in 0..self.candidates.len() {
            if (oldest == Some(slot)
                || self.candidates[slot]
                    .as_ref()
                    .is_none_or(|live| live.failed(now)))
                && let Some(target) = self.next_target(&seen)
            {
                let entry = &self.inventory[target].target;
                info!(
                    "Exploring {} ({}) in candidate slot {slot}",
                    entry.peer(),
                    entry.protocol()
                );
                self.metrics.send(Metric::event(
                    "reflector_explored",
                    "inventory",
                    Some(entry.peer()),
                ));
                self.candidates[slot] = Some(Live::new(target, now));
                self.unpublished = true;
            }
        }
        if self.unpublished {
            self.publish()?;
            self.unpublished = false;
        }
        Ok(())
    }

    /// Round-robin across protocol groups, each a shuffled deck.
    fn next_target(&mut self, excluded: &[usize]) -> Option<usize> {
        for _ in 0..self.groups.len() {
            let group_index = self.next_group;
            self.next_group = (self.next_group + 1) % self.groups.len();
            let group = &mut self.groups[group_index];
            if group.targets.is_empty() {
                continue;
            }
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

    fn accept_pool(&mut self, discovered: Vec<InventoryTarget>) -> anyhow::Result<()> {
        inventory::merge(&mut self.inventory, discovered)?;
        self.groups = inventory::groups(&self.inventory, &mut self.rng);
        Ok(())
    }

    fn observe(&mut self, event: MeasurementEvent) {
        let inventory = &self.inventory;
        if let Some(live) = self
            .active
            .iter_mut()
            .chain(&mut self.candidates)
            .flatten()
            .find(|live| inventory[live.inventory].target == event.target)
        {
            live.evidence.record(event);
        }
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
                    target: self.inventory[live.inventory].target.clone(),
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
            let mut refresh = false;
            tokio::select! {
                pool = async {
                    match &mut self.pool_discovery {
                        Some(discovery) => discovery.await,
                        None => std::future::pending().await,
                    }
                } => {
                    self.pool_discovery = None;
                    if let Err(error) = pool.and_then(|pool| self.accept_pool(pool)) {
                        warn!("Reflector pool discovery failed: {error}");
                    }
                },
                event = self.measurements.recv() => match event {
                    Ok(event) => self.observe(event),
                    Err(broadcast::error::RecvError::Lagged(count)) => {
                        warn!("Reflector manager missed {count} measurements");
                        self.metrics.send(Metric::event("measurement_lag", "reflector_manager", None));
                    }
                    Err(broadcast::error::RecvError::Closed) => return Err(ChannelClosed.into()),
                },
                changed = self.applied.changed() => {
                    changed.map_err(|_| ChannelClosed)?;
                    refresh = true;
                }
                _ = maintenance.tick() => {}
            }
            // A closed channel still holds its last value.
            if refresh || self.applied.has_changed().unwrap_or(true) {
                self.applied_state = self.applied.borrow_and_update().clone();
            }
            // Moved out for the pass, as select borrows the manager mutably.
            let applied = std::mem::take(&mut self.applied_state);
            let revision = self.desired.borrow().revision;
            anyhow::ensure!(
                applied.rejected_revision != Some(revision),
                "engine rejected reflector plan {revision}"
            );
            // Evidence can arrive while a plan is pending; decisions wait for it.
            if applied.revision == revision {
                self.select(&applied, Instant::now())?;
            }
            self.applied_state = applied;
        }
    }
}
