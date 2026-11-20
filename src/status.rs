// SPDX-FileCopyrightText: 2026-Present Nils Andreas Svee mailto:contact@lochnair.net (github @Lochnair)
//
// SPDX-License-Identifier: MPL-2.0

//! The instance's state on ubus, for `ubus call unravel.wan status` and LuCI.
//! `status` is the current snapshot, `history` the last ten minutes of each
//! direction and `reflectors` the probe results of each reflector, so a
//! caller fetches only what it shows.

use std::{
    collections::{HashMap, VecDeque},
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::Context;
use log::info;
use tokio::{
    sync::{broadcast, broadcast::error::RecvError, watch},
    time,
};
use tokio_ubus::{DEFAULT_SOCKET, Object, Server, Table, Value};

use crate::{
    VERSION,
    error::ChannelClosed,
    measurement::{
        model::{
            MeasurementEvent, MeasurementOutcome, MeasurementRole, MeasurementTarget, OneWayStatus,
        },
        plan::AppliedState,
    },
    runtime::{DelayReports, Direction, Runtime, TrafficRates},
};

/// One history slot per this long...
const HISTORY_INTERVAL: Duration = Duration::from_secs(2);
/// ...for ten minutes.
const HISTORY_SLOTS: usize = 300;
/// Reflector figures cover this many of the latest probes.
const REFLECTOR_PROBES: usize = 20;

pub(crate) struct Status {
    server: Server,
    sampler: Sampler,
}

struct State {
    runtime: Arc<Runtime>,
    applied: watch::Receiver<AppliedState>,
    recorded: Arc<Mutex<Recorded>>,
    instance: String,
    law: &'static str,
    dry_run: bool,
    started: Instant,
}

impl Status {
    /// The status object `unravel.<instance>`, or `None` without ubusd.
    pub(crate) fn new(
        runtime: Arc<Runtime>,
        applied: watch::Receiver<AppliedState>,
        measurements: broadcast::Receiver<MeasurementEvent>,
        instance: &str,
        law: &'static str,
        dry_run: bool,
    ) -> Option<Self> {
        if !Path::new(DEFAULT_SOCKET).exists() {
            info!("No ubus socket at {DEFAULT_SOCKET}; not publishing status");
            return None;
        }
        let recorded = Arc::new(Mutex::new(Recorded::new(&runtime)));
        let sampler = Sampler {
            runtime: Arc::clone(&runtime),
            applied: applied.clone(),
            measurements,
            recorded: Arc::clone(&recorded),
        };
        let state = Arc::new(State {
            runtime,
            applied,
            recorded,
            instance: instance.to_owned(),
            law,
            dry_run,
            started: Instant::now(),
        });
        let object = Object::new(format!("unravel.{instance}"))
            .method("status", {
                let state = Arc::clone(&state);
                move |_| {
                    let reply = state.status();
                    async move { Ok(Some(reply)) }
                }
            })
            .method("history", {
                let state = Arc::clone(&state);
                move |_| {
                    let reply = state.history();
                    async move { Ok(Some(reply)) }
                }
            })
            .method("reflectors", move |_| {
                let reply = state.reflectors();
                async move { Ok(Some(reply)) }
            });
        Some(Self {
            server: Server::new().object(object),
            sampler,
        })
    }

    pub(crate) async fn run(self) -> anyhow::Result<()> {
        tokio::select! {
            result = self.server.run() => result,
            result = self.sampler.run() => result,
        }
    }
}

impl State {
    fn recorded(&self) -> std::sync::MutexGuard<'_, Recorded> {
        // The data stays consistent slot by slot, so a panic elsewhere
        // doesn't make it unreadable.
        self.recorded
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn status(&self) -> Table {
        let traffic = self.runtime.traffic();
        let reflectors = self.applied.borrow().active_targets().len();
        let mut table = Table::new()
            .with("version", VERSION)
            .with("instance", self.instance.as_str())
            .with("law", self.law)
            .with("dry_run", self.dry_run)
            .with(
                "uptime",
                i64::try_from(self.started.elapsed().as_secs()).unwrap_or(i64::MAX),
            )
            .with("reflectors", i32::try_from(reflectors).unwrap_or(i32::MAX));
        for direction in [Direction::Download, Direction::Upload] {
            let Some(shaper) = self.runtime.shaper(direction) else {
                continue;
            };
            let mut entry = Table::new()
                .with("interface", shaper.interface())
                .with("base_kbps", kbps(shaper.base_kbps))
                .with("min_kbps", kbps(shaper.min_kbps));
            if let Some(max) = shaper.max_kbps {
                entry.push("max_kbps", kbps(max));
            }
            if let Some(rate) = shaper.rate_kbps() {
                entry.push("rate_kbps", kbps(rate));
            }
            let load = traffic.map(|t| match direction {
                Direction::Download => t.rx_kbps,
                Direction::Upload => t.tx_kbps,
            });
            if let Some(load) = load {
                entry.push("traffic_kbps", kbps(load));
            }
            if let Some(delay) = shaper.delay().latest {
                entry.push("delay_us", us(delay.delay_ms));
                if let Some(threshold) = delay.threshold_ms {
                    entry.push("delay_threshold_us", us(threshold));
                }
            }
            table.push(direction.as_str(), entry);
        }
        table
    }

    fn history(&self) -> Table {
        let recorded = self.recorded();
        let interval = i32::try_from(HISTORY_INTERVAL.as_secs()).unwrap_or(i32::MAX);
        let window = i32::try_from(HISTORY_SLOTS).map_or(i32::MAX, |n| n.saturating_mul(interval));
        let mut table = Table::new()
            .with("interval", interval)
            .with("window", window);
        for (direction, history) in recorded.directions() {
            table.push(direction.as_str(), history.table());
        }
        table
    }

    fn reflectors(&self) -> Table {
        let applied = self.applied.borrow();
        let recorded = self.recorded();
        let now = Instant::now();
        // Active reflectors first, each group in the manager's order.
        let mut targets: Vec<_> = applied.targets.iter().collect();
        targets.sort_by_key(|t| t.assignment.role != MeasurementRole::Active);
        let list = targets
            .into_iter()
            .map(|target| {
                let assignment = &target.assignment;
                let role = match assignment.role {
                    MeasurementRole::Active => "active",
                    MeasurementRole::Candidate => "candidate",
                };
                let mut entry = Table::new()
                    .with("reflector", assignment.target.name())
                    .with("protocol", assignment.target.protocol().to_string())
                    .with("role", role)
                    .with("ready", target.ready);
                if let Some(probes) = recorded.reflectors.get(&assignment.target) {
                    probes.describe(&mut entry, now);
                }
                Value::Table(entry)
            })
            .collect::<Vec<_>>();
        Table::new().with("reflectors", list)
    }
}

/// What the sampler has recorded, read by the ubus handlers.
struct Recorded {
    download: Option<DirectionHistory>,
    upload: Option<DirectionHistory>,
    reflectors: HashMap<MeasurementTarget, Probes>,
}

impl Recorded {
    fn new(runtime: &Runtime) -> Self {
        let history = |direction| {
            runtime
                .shaper(direction)
                .map(|_| DirectionHistory::default())
        };
        Self {
            download: history(Direction::Download),
            upload: history(Direction::Upload),
            reflectors: HashMap::new(),
        }
    }

    fn directions(&self) -> impl Iterator<Item = (Direction, &DirectionHistory)> {
        [
            (Direction::Download, self.download.as_ref()),
            (Direction::Upload, self.upload.as_ref()),
        ]
        .into_iter()
        .filter_map(|(direction, history)| Some((direction, history?)))
    }

    fn direction(&mut self, direction: Direction) -> Option<&mut DirectionHistory> {
        match direction {
            Direction::Download => self.download.as_mut(),
            Direction::Upload => self.upload.as_mut(),
        }
    }
}

#[derive(Clone, Copy)]
struct Slot {
    /// The rate at the end of the slot.
    rate_kbps: Option<f64>,
    /// The means of the slot's samples and reports.
    traffic_kbps: Option<f64>,
    delay_ms: Option<f64>,
}

#[derive(Default)]
struct DirectionHistory {
    slots: VecDeque<Slot>,
    traffic: Mean,
    /// The law's delay reports when the last slot closed. Without any the
    /// history leaves `delay_us` out rather than sending nulls.
    delay: DelayReports,
}

impl DirectionHistory {
    fn close_slot(&mut self, rate_kbps: Option<f64>, delay: DelayReports) {
        if self.slots.len() == HISTORY_SLOTS {
            self.slots.pop_front();
        }
        // The mean of every report made during the slot.
        let reports = delay.count - self.delay.count;
        let delay_ms =
            (reports > 0).then(|| (delay.total_ms - self.delay.total_ms) / reports as f64);
        self.delay = delay;
        self.slots.push_back(Slot {
            rate_kbps,
            traffic_kbps: self.traffic.take(),
            delay_ms,
        });
    }

    /// Oldest first, `null` where a slot has no value.
    fn table(&self) -> Table {
        let series = |value: fn(&Slot) -> Value| self.slots.iter().map(value).collect::<Vec<_>>();
        let mut table = Table::new()
            .with("rate_kbps", series(|s| s.rate_kbps.map(kbps32).into()))
            .with(
                "traffic_kbps",
                series(|s| s.traffic_kbps.map(kbps32).into()),
            );
        if self.delay.count > 0 {
            table.push("delay_us", series(|s| s.delay_ms.map(us32).into()));
        }
        table
    }
}

#[derive(Default)]
struct Mean {
    sum: f64,
    count: u32,
}

impl Mean {
    fn add(&mut self, value: f64) {
        self.sum += value;
        self.count += 1;
    }

    fn take(&mut self) -> Option<f64> {
        let mean = (self.count > 0).then(|| self.sum / f64::from(self.count));
        *self = Self::default();
        mean
    }
}

/// The latest probes of one reflector.
#[derive(Default)]
struct Probes {
    /// RTT in milliseconds, `None` for a loss.
    results: VecDeque<Option<f64>>,
    last_reply: Option<Instant>,
    one_way: Option<&'static str>,
}

impl Probes {
    fn record(&mut self, event: &MeasurementEvent) {
        if self.results.len() == REFLECTOR_PROBES {
            self.results.pop_front();
        }
        let rtt = event.rtt_ms();
        self.results.push_back(rtt);
        if let MeasurementOutcome::Reply { one_way, .. } = event.outcome {
            self.last_reply = Some(event.completed_at);
            self.one_way = one_way.map(|o| match o.status {
                OneWayStatus::Usable => "usable",
                OneWayStatus::ReferenceChanged => "reference_changed",
                OneWayStatus::Unusable(rejection) => rejection.reason(),
            });
        }
    }

    fn describe(&self, entry: &mut Table, now: Instant) {
        let rtts: Vec<f64> = self.results.iter().flatten().copied().collect();
        let probes = self.results.len();
        entry.push("probes", i32::try_from(probes).unwrap_or(i32::MAX));
        if probes > 0 {
            let lost = probes - rtts.len();
            entry.push("loss_pct", (100 * lost / probes) as i32);
        }
        if let Some(Some(last)) = self.results.back() {
            entry.push("rtt_us", us(*last));
        }
        if !rtts.is_empty() {
            let mean = rtts.iter().sum::<f64>() / rtts.len() as f64;
            let min = rtts.iter().copied().fold(f64::INFINITY, f64::min);
            // Mean difference between consecutive replies, as RFC 3550 jitter
            // without the smoothing.
            let jitter = if rtts.len() > 1 {
                rtts.windows(2).map(|w| (w[1] - w[0]).abs()).sum::<f64>() / (rtts.len() - 1) as f64
            } else {
                0.0
            };
            entry.push("rtt_avg_us", us(mean));
            entry.push("rtt_min_us", us(min));
            entry.push("jitter_us", us(jitter));
        }
        if let Some(at) = self.last_reply {
            let age = now.saturating_duration_since(at).as_millis();
            entry.push("last_reply_ms", i64::try_from(age).unwrap_or(i64::MAX));
        }
        if let Some(one_way) = self.one_way {
            entry.push("one_way", one_way);
        }
    }
}

/// Records history and reflector results while the status object is up.
struct Sampler {
    runtime: Arc<Runtime>,
    applied: watch::Receiver<AppliedState>,
    measurements: broadcast::Receiver<MeasurementEvent>,
    recorded: Arc<Mutex<Recorded>>,
}

impl Sampler {
    async fn run(mut self) -> anyhow::Result<()> {
        let mut traffic = self.runtime.traffic_updates();
        let mut ticker =
            time::interval_at(time::Instant::now() + HISTORY_INTERVAL, HISTORY_INTERVAL);
        ticker.set_missed_tick_behavior(time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                _ = ticker.tick() => self.close_slots(),
                changed = traffic.changed() => {
                    changed.context("Traffic channel closed")?;
                    let sample: Option<TrafficRates> = *traffic.borrow_and_update();
                    if let Some(sample) = sample {
                        self.add_traffic(sample);
                    }
                }
                event = self.measurements.recv() => match event {
                    Ok(event) => self.lock().reflectors.entry(event.target.clone()).or_default().record(&event),
                    // Lost results only thin the reflector figures.
                    Err(RecvError::Lagged(_)) => {}
                    Err(RecvError::Closed) => {
                        return Err(ChannelClosed).context("Measurement channel closed");
                    }
                },
            }
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Recorded> {
        self.recorded
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn close_slots(&self) {
        let applied = self.applied.borrow();
        let mut recorded = self.lock();
        for direction in [Direction::Download, Direction::Upload] {
            let Some(shaper) = self.runtime.shaper(direction) else {
                continue;
            };
            if let Some(history) = recorded.direction(direction) {
                history.close_slot(shaper.rate_kbps(), shaper.delay());
            }
        }
        // Forget reflectors the manager has dropped.
        recorded.reflectors.retain(|target, _| {
            applied
                .targets
                .iter()
                .any(|t| &t.assignment.target == target)
        });
    }

    fn add_traffic(&self, sample: TrafficRates) {
        let mut recorded = self.lock();
        for (direction, kbps) in [
            (Direction::Download, sample.rx_kbps),
            (Direction::Upload, sample.tx_kbps),
        ] {
            if let Some(history) = recorded.direction(direction) {
                history.traffic.add(kbps);
            }
        }
    }
}

/// Rates are whole kbit/s; `as` saturates and maps NaN to 0, so a bad value
/// cannot panic the handler.
fn kbps(rate: f64) -> i64 {
    rate.round() as i64
}

/// History rates go out as `INT32` to keep the reply small; that still
/// holds 2 Tbit/s.
fn kbps32(rate: f64) -> i32 {
    rate.round() as i32
}

/// Delays go out as whole microseconds: ubus prints doubles at full
/// precision, and OpenWrt's own objects stick to integers.
fn us(ms: f64) -> i64 {
    (ms * 1000.0).round() as i64
}

fn us32(ms: f64) -> i32 {
    (ms * 1000.0).round() as i32
}
