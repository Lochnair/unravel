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
use evidence::{Assessment, Evidence, Qualifications, WINDOW};
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
/// What a protocol measures, declared best-first so `min` finds the preferred class.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Class {
    /// One-way delay as well as RTT.
    Directional,
    /// RTT only; used when too few directional peers qualify.
    Echo,
}

impl Class {
    fn of(target: &MeasurementTarget) -> Self {
        match target {
            MeasurementTarget::IcmpEcho(_) => Self::Echo,
            MeasurementTarget::IcmpTimestamps(_) | MeasurementTarget::Irtt { .. } => {
                Self::Directional
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PromotionKind {
    /// Fills an empty or failed slot, judged over the newest STREAK outcomes.
    Recovery,
    /// Replaces a healthy active reflector, judged over the full WINDOW.
    Improvement,
}

impl PromotionKind {
    fn rtt(self, quality: Qualifications) -> Option<Duration> {
        match self {
            Self::Recovery => quality.recovery,
            Self::Improvement => quality.improvement,
        }
    }

    fn reason(self) -> &'static str {
        match self {
            Self::Recovery => "health",
            Self::Improvement => "quality",
        }
    }
}

struct Promotion {
    slot: usize,
    candidate: usize,
    kind: PromotionKind,
}

/// A target under measurement. Evidence follows it across role changes.
struct Live {
    inventory: usize,
    evidence: Evidence,
    entered_at: Instant,
    unhealthy_since: Option<Instant>,
}

/// One live target as this pass's selection sees it.
struct Seat<'a> {
    entry: &'a InventoryTarget,
    failed: bool,
    quality: Qualifications,
}

impl Seat<'_> {
    fn class(&self) -> Class {
        Class::of(&self.entry.target)
    }

    fn ip(&self) -> IpAddr {
        self.entry.target.peer()
    }
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

    fn assess<'a>(
        &mut self,
        entry: &'a InventoryTarget,
        period: Duration,
        applied: &AppliedState,
        now: Instant,
    ) -> Seat<'a> {
        let status = applied
            .targets
            .iter()
            .find(|s| s.assignment.target == entry.target);
        let Assessment { unhealthy, quality } =
            self.evidence.assess(now, self.entered_at, period, status);
        self.unhealthy_since = unhealthy.then(|| self.unhealthy_since.unwrap_or(now));
        Seat {
            entry,
            failed: self.failed(now),
            quality,
        }
    }
}

/// The class first, then the fastest RTT within it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct ClassRtt {
    class: Class,
    rtt: Duration,
}

/// Where a reflector ranked for a slot comes from.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Source {
    Active,
    Candidate(usize),
}

struct RankEntry {
    source: Source,
    baseline: ClassRtt,
    preference: usize,
}

/// The slot's active reflector as the improvement pass sees it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ActiveRtt {
    Measured(Duration),
    Missing,
}

/// Which healthy slot the periodic pass improves: the greatest wins.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ImprovementPriority {
    SameClass(ActiveRtt),
    DirectionalUpgrade(ActiveRtt),
}

/// Choose this pass's promotions from the assessed seats.
fn select_promotions(
    active: &[Option<Seat>],
    candidates: &[Option<Seat>],
    periodic: bool,
    rng: &mut fastrand::Rng,
) -> Vec<Promotion> {
    // Each slot's effective IP. A winner reserves its IP at once, which also
    // keeps it out of every later contest.
    let mut active_ips = active
        .iter()
        .map(|seat| seat.as_ref().map(Seat::ip))
        .collect::<Vec<_>>();
    let mut promotions = Vec::new();

    // Fill empty or failed slots.
    for (slot, seat) in active.iter().enumerate() {
        if seat.as_ref().is_some_and(|s| !s.failed) {
            continue;
        }
        let kind = PromotionKind::Recovery;
        if let Some(candidate) =
            select_replacement(slot, seat.as_ref(), kind, candidates, &active_ips, rng)
        {
            active_ips[slot] = candidates[candidate].as_ref().map(Seat::ip);
            promotions.push(Promotion {
                slot,
                candidate,
                kind,
            });
        }
    }

    // Periodically improve one slot that was healthy at the start of the pass.
    if periodic {
        let best = active
            .iter()
            .enumerate()
            .filter_map(|(slot, seat)| {
                let current = seat.as_ref().filter(|s| !s.failed)?;
                let kind = PromotionKind::Improvement;
                let candidate =
                    select_replacement(slot, Some(current), kind, candidates, &active_ips, rng)?;
                let rtt = current
                    .quality
                    .improvement
                    .map_or(ActiveRtt::Missing, ActiveRtt::Measured);
                let upgrade = current.class() == Class::Echo
                    && candidates[candidate].as_ref()?.class() == Class::Directional;
                let priority = if upgrade {
                    ImprovementPriority::DirectionalUpgrade(rtt)
                } else {
                    ImprovementPriority::SameClass(rtt)
                };
                Some((
                    priority,
                    Promotion {
                        slot,
                        candidate,
                        kind,
                    },
                ))
            })
            .max_by_key(|(priority, _)| *priority)
            .map(|(_, promotion)| promotion);
        promotions.extend(best);
    }
    promotions
}

/// The candidate that should replace the slot's active reflector, if any outranks it.
fn select_replacement(
    slot: usize,
    active: Option<&Seat>,
    kind: PromotionKind,
    candidates: &[Option<Seat>],
    active_ips: &[Option<IpAddr>],
    rng: &mut fastrand::Rng,
) -> Option<usize> {
    // A healthy directional active reflector never gives way to echo.
    let worst_allowed = active
        .filter(|s| !s.failed)
        .map_or(Class::Echo, Seat::class);
    let held_elsewhere = |ip| {
        active_ips
            .iter()
            .enumerate()
            .any(|(other, held)| other != slot && *held == Some(ip))
    };
    let rank_entry = |source, seat: &Seat| {
        Some(RankEntry {
            source,
            baseline: ClassRtt {
                class: seat.class(),
                rtt: kind.rtt(seat.quality)?,
            },
            preference: seat.entry.preference,
        })
    };

    let mut pool = Vec::new();
    for (index, seat) in candidates.iter().enumerate() {
        let Some(seat) = seat else { continue };
        if seat.class() > worst_allowed || held_elsewhere(seat.ip()) {
            continue;
        }
        pool.extend(rank_entry(Source::Candidate(index), seat));
    }
    // A qualified active reflector competes on equal terms.
    pool.extend(active.and_then(|seat| rank_entry(Source::Active, seat)));

    match rank(pool, rng)? {
        Source::Active => None,
        Source::Candidate(index) => Some(index),
    }
}

/// The best class's fastest RTT sets the shortlist ceiling, and protocol
/// preference narrows it. The active reflector keeps its slot if it is a
/// finalist; otherwise a random finalist spreads load.
fn rank(mut pool: Vec<RankEntry>, rng: &mut fastrand::Rng) -> Option<Source> {
    let best = pool.iter().map(|e| e.baseline).min()?;
    let ceiling = best
        .rtt
        .saturating_add(Duration::from_millis(5).max(best.rtt / 5));
    pool.retain(|e| e.baseline.class == best.class && e.baseline.rtt <= ceiling);
    let preference = pool.iter().map(|e| e.preference).min()?;
    pool.retain(|e| e.preference == preference);
    if pool.iter().any(|e| e.source == Source::Active) {
        return Some(Source::Active);
    }
    Some(pool[rng.usize(..pool.len())].source)
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

        for Promotion {
            slot,
            candidate,
            kind,
        } in select_promotions(&active, &candidates, periodic, &mut self.rng)
        {
            let next = self.candidates[candidate].take().unwrap().enter(now);
            let entry = &self.inventory[next.inventory].target;
            let peer = entry.peer();
            info!(
                "Promoting {peer} ({}) into active slot {slot}: {}",
                entry.protocol(),
                kind.reason()
            );
            self.metrics.send(Metric::event(
                "reflector_promoted",
                kind.reason(),
                Some(peer),
            ));
            // Improvement replaces a healthy active reflector, which stays
            // under measurement; recovery replaces an empty or failed one.
            let old = self.active[slot].replace(next);
            let keep = kind == PromotionKind::Improvement;
            self.candidates[candidate] = old.filter(|_| keep).map(|old| old.enter(now));
            self.unpublished = true;
        }
        // Candidates: fill empty and failed slots, periodically rotate the
        // oldest resident. A slot only changes if exploration has a target.
        // Computed after promotions, so a just-demoted active reflector is not rotated.
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
