// SPDX-FileCopyrightText: 2026-Present Nils Andreas Svee mailto:contact@lochnair.net (github @Lochnair)
//
// SPDX-License-Identifier: MPL-2.0

use std::{
    collections::{HashMap, VecDeque},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::Context;
use log::{debug, info, warn};
use serde::Deserialize;

use crate::{
    diagnostics::{DiagnosticRecord, DiagnosticsSender, DirectionStatistics, SpeedHistory},
    error::ChannelClosed,
    law::{Cadence, Law, LawContext, LawStore},
    measurement::model::{
        MeasurementEvent, MeasurementOutcome, MeasurementTarget, OneWayLatency, OneWayRejection,
        OneWayStatus,
    },
    metrics::Metric,
    runtime::{Direction, Shaper},
    settings::{
        ValidationReport, deserialize_flex_bool, validate_greater_than_zero_f64,
        validate_zero_or_greater_f64,
    },
};

const BASELINE_HALF_LIFE_SECS: f64 = 135.0;
const RECENT_HALF_LIFE_SECS: f64 = 0.4;
const STALE_AFTER: Duration = Duration::from_secs(30);
// Let queues drain before rebuilding all baselines after a local clock step.
const MEASUREMENT_SETTLE_TIME: Duration = Duration::from_secs(5);

// With save_history set, the recorded safe rates are saved in the law's store
// so a service restart does not throw them away.
const HISTORY_FILE: &str = "history.txt";
const HISTORY_SAVE_EVERY: Duration = Duration::from_secs(30);
const HISTORY_MAX_AGE_SECS: u64 = 6 * 3600;
const HISTORY_HEADER: &str = "unravel-history 1";

/// SQMA's own configuration, read from `law.sqma`.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct SqmaSettings {
    pub(crate) download_delay_ms: f64,
    pub(crate) high_load_level: f64,
    pub(crate) min_change_interval: f64,
    pub(crate) speed_hist_size: u32,
    /// Probe and traffic sampling period, in seconds.
    pub(crate) tick_interval: f64,
    pub(crate) upload_delay_ms: f64,
    /// At or below low_load_level, each cut can take the rate no lower than
    /// this fraction of the current rate (0 = off, the upstream behaviour).
    pub(crate) low_load_cut_floor: f64,
    /// When the delay is under the threshold and load is at or below
    /// low_load_level, step the rate towards the median of the recorded
    /// safe rates by this percentage of the base rate per evaluation
    /// (0 = off, the upstream behaviour).
    pub(crate) idle_recovery_percent: f64,
    /// Load at or below which low_load_cut_floor and idle_recovery_percent
    /// act. Above it, the decision is unchanged from upstream.
    pub(crate) low_load_level: f64,
    /// Keep the recorded safe rates across a service restart. They are read
    /// at startup if under 6 h old and saved with the same speed_hist_size
    /// and base rate. Off by default, the upstream behaviour.
    #[serde(deserialize_with = "deserialize_flex_bool")]
    pub(crate) save_history: bool,
}

impl Default for SqmaSettings {
    fn default() -> Self {
        Self {
            download_delay_ms: 15.0,
            high_load_level: 0.8,
            min_change_interval: 0.5,
            speed_hist_size: 100,
            tick_interval: 0.5,
            upload_delay_ms: 15.0,
            low_load_cut_floor: 0.0,
            idle_recovery_percent: 0.0,
            low_load_level: 0.3,
            save_history: false,
        }
    }
}

impl SqmaSettings {
    /// Reads the `law.sqma` table; an absent table means all defaults.
    fn from_config(config: Option<config::Value>) -> anyhow::Result<Self> {
        let settings = match config {
            Some(value) => value
                .try_deserialize::<Self>()
                .context("Reading law.sqma settings")?,
            None => Self::default(),
        };
        let mut report = ValidationReport::default();
        settings.validate(&mut report);
        report.finish()?;
        Ok(settings)
    }

    fn validate(&self, report: &mut ValidationReport) {
        validate_zero_or_greater_f64(report, "law.sqma.download_delay_ms", self.download_delay_ms);

        validate_zero_or_greater_f64(report, "law.sqma.upload_delay_ms", self.upload_delay_ms);

        let high_load_level_valid =
            validate_zero_or_greater_f64(report, "law.sqma.high_load_level", self.high_load_level);

        if validate_greater_than_zero_f64(
            report,
            "law.sqma.min_change_interval",
            self.min_change_interval,
        ) && !Duration::try_from_secs_f64(self.min_change_interval)
            .is_ok_and(|interval| !interval.is_zero())
        {
            report.error(
                "law.sqma.min_change_interval",
                "must convert to a nonzero representable duration",
            );
        }

        if !Duration::try_from_secs_f64(self.tick_interval)
            .is_ok_and(|period| !period.is_zero() && Instant::now().checked_add(period).is_some())
        {
            report.error(
                "law.sqma.tick_interval",
                "must be a nonzero representable duration",
            );
        }

        if !(0.0..1.0).contains(&self.low_load_cut_floor) {
            report.error(
                "law.sqma.low_load_cut_floor",
                "must be at least 0 and below 1",
            );
        }

        if !(0.0..=100.0).contains(&self.idle_recovery_percent) {
            report.error(
                "law.sqma.idle_recovery_percent",
                "must be between 0 and 100",
            );
        }

        if !(0.0..=1.0).contains(&self.low_load_level) {
            report.error("law.sqma.low_load_level", "must be between 0 and 1");
        }

        if self.speed_hist_size == 0 {
            report.error("law.sqma.speed_hist_size", "must be greater than 0");
        }

        if high_load_level_valid {
            if self.high_load_level == 0.0 {
                report.warning(
                    "law.sqma.high_load_level",
                    "0 causes any positive load to be treated as high load",
                );
            } else if self.high_load_level > 1.0 {
                report.warning(
                    "law.sqma.high_load_level",
                    format!(
                        "{} requires utilisation to exceed the current shaper rate \
                         before the high-load threshold is reached",
                        self.high_load_level,
                    ),
                );
            }
        }
    }
}

struct RateState {
    current_kbps: f64,
    safe_rates: VecDeque<f64>,
    /// How many safe rates are recorded rather than startup placeholders.
    /// Recorded rates sit at the back.
    observed: usize,
    /// Base the history was learned against.
    base_kbps: f64,
}

/// The saved history, without the header and time lines.
/// One line per direction: `<name> <base> <count> <rate>...`, recorded rates only.
fn history_body(size: usize, dirs: &[(&str, f64, &RateState)]) -> String {
    let mut body = format!("size {size}\n");
    for (name, base, state) in dirs {
        let skip = state.safe_rates.len() - state.observed;
        let recorded: Vec<String> = state
            .safe_rates
            .iter()
            .skip(skip)
            .map(|r| r.to_string())
            .collect();
        body.push_str(&format!(
            "{name} {base} {} {}\n",
            recorded.len(),
            recorded.join(" ")
        ));
    }
    body
}

/// Parse a saved history. None if it is malformed, from a
/// different history size, or older than HISTORY_MAX_AGE_SECS.
fn parse_history(text: &str, now_unix: u64, size: usize) -> Option<Vec<(String, f64, Vec<f64>)>> {
    let mut lines = text.lines();
    if lines.next()? != HISTORY_HEADER {
        return None;
    }
    let saved: u64 = lines.next()?.strip_prefix("saved ")?.parse().ok()?;
    if saved > now_unix + 60 || now_unix.saturating_sub(saved) > HISTORY_MAX_AGE_SECS {
        return None;
    }
    let saved_size: usize = lines.next()?.strip_prefix("size ")?.parse().ok()?;
    if saved_size != size {
        return None;
    }
    let mut dirs = vec![];
    for line in lines {
        let mut words = line.split_whitespace();
        let name = words.next()?.to_string();
        let base: f64 = words.next()?.parse().ok()?;
        let count: usize = words.next()?.parse().ok()?;
        let rates: Vec<f64> = words
            .map(|w| w.parse::<f64>())
            .collect::<Result<_, _>>()
            .ok()?;
        if !(base.is_finite() && base > 0.0)
            || rates.len() != count
            || count > size
            || rates
                .iter()
                .any(|r| !r.is_finite() || *r <= 0.0 || *r > 10.0 * base)
        {
            return None;
        }
        dirs.push((name, base, rates));
    }
    Some(dirs)
}

/// Put saved recorded rates at the back of the history, oldest first.
fn restore_samples(state: &mut RateState, rates: &[f64]) {
    let n = rates.len().min(state.safe_rates.len());
    for r in &rates[rates.len() - n..] {
        state.safe_rates.pop_front();
        state.safe_rates.push_back(*r);
    }
    state.observed = n;
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

impl RateState {
    fn new(base_speed: f64, size: u32, rng: &mut fastrand::Rng) -> Self {
        let mut rates = VecDeque::new();

        for _ in 0..size {
            rates.push_back((rng.f64() * 0.2 + 0.75) * base_speed);
        }

        Self {
            current_kbps: base_speed,
            safe_rates: rates,
            observed: 0,
            base_kbps: base_speed,
        }
    }

    fn calculate_rate(
        &mut self,
        shaper: &Shaper,
        traffic_kbps: f64,
        deltas: &[f64],
        delay_threshold_ms: f64,
        settings: &SqmaSettings,
        rng: &mut fastrand::Rng,
    ) -> f64 {
        let mut next_kbps = self.current_kbps;

        if !deltas.is_empty() {
            let delta = deltas[deltas.len() / 2];
            let load = traffic_kbps / self.current_kbps;

            if delta > 0.0 {
                if delta < delay_threshold_ms && load > settings.high_load_level {
                    self.safe_rates.pop_front();
                    self.safe_rates.push_back(traffic_kbps.floor());
                    self.observed = (self.observed + 1).min(self.safe_rates.len());

                    let max_rate = self
                        .safe_rates
                        .iter()
                        .copied()
                        .max_by(f64::total_cmp)
                        .unwrap();
                    next_kbps = self.current_kbps
                        * (1.0 + 0.1 * (1.0_f64 - self.current_kbps / max_rate).max(0.0))
                        + (shaper.base_kbps * 0.03);
                }

                if delta > delay_threshold_ms {
                    let rnd_rate = self.safe_rates[rng.usize(..self.safe_rates.len())];
                    next_kbps = rnd_rate.min(0.9 * traffic_kbps);

                    // At low load, each cut can take the rate no lower than
                    // low_load_cut_floor times the current rate.
                    if settings.low_load_cut_floor > 0.0 && load <= settings.low_load_level {
                        next_kbps = next_kbps.max(self.current_kbps * settings.low_load_cut_floor);
                    }
                }
            }

            // With the delay at or under the threshold and the link not
            // busy, step towards the median of the safe-rate history. This
            // sits outside the delta > 0 check, as the median delta on an
            // idle link can be exactly 0. Only recorded rates count: the
            // startup placeholders would pull an idle link towards about 85%
            // of the base rate after every restart.
            if settings.idle_recovery_percent > 0.0
                && self.observed > 0
                && delta <= delay_threshold_ms
                && load <= settings.low_load_level
            {
                let skip = self.safe_rates.len() - self.observed;
                let mut rates: Vec<f64> = self.safe_rates.iter().skip(skip).copied().collect();
                rates.sort_by(f64::total_cmp);
                let target = rates[rates.len() / 2].max(shaper.min_kbps);
                let step = shaper.base_kbps * settings.idle_recovery_percent / 100.0;
                if self.current_kbps < target {
                    next_kbps = (self.current_kbps + step).min(target);
                } else if self.current_kbps > target {
                    next_kbps = (self.current_kbps - step).max(target);
                }
            }
        }

        next_kbps.max(shaper.min_kbps).floor()
    }
}

pub(crate) struct SqmaLaw {
    settings: SqmaSettings,

    reflectors: HashMap<MeasurementTarget, ReflectorState>,
    /// Also retained after settling to exclude late replies to older probes.
    measurements_after: Option<Instant>,

    download: Option<RateState>,
    upload: Option<RateState>,

    fast_factor: f64,
    slow_factor: f64,

    rng: fastrand::Rng,
    diagnostics: Option<DiagnosticsSender>,

    // Restart-proof safe-rate history, when save_history is on.
    history_saved_at: Option<Instant>,
    history_saved_body: String,

    // Missing reflector data is checked every tick; warn once per episode.
    warned_few_deltas: bool,
    warned_no_deltas: bool,
}

#[async_trait::async_trait]
impl Law for SqmaLaw {
    fn cadence(&self) -> Cadence {
        let tick = Duration::from_secs_f64(self.settings.tick_interval);
        Cadence {
            probe: tick,
            traffic: tick,
        }
    }

    async fn run(mut self: Box<Self>, mut ctx: LawContext) -> anyhow::Result<()> {
        let interval = Duration::from_secs_f64(self.settings.min_change_interval);

        let mut ticker = tokio::time::interval_at(
            tokio::time::Instant::now() + Duration::from_secs(10),
            interval,
        );

        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        let mut initialized = false;

        loop {
            tokio::select! {
                biased;
                changed = ctx.clock_steps.changed() => {
                    changed.context("Clock-step channel closed")?;
                    let step = *ctx.clock_steps.borrow_and_update();
                    if let Some(step) = step {
                        warn!("SQMA resetting after local clock step of {:.3}s detected {:?} ago",
                            step.delta_secs, step.detected_at.elapsed());
                        self.recover_after_clock_step(&ctx)?;
                    }
                }

                _ = ticker.tick() => {
                    if self.measurements_after.is_some_and(|until| Instant::now() < until) {
                        continue;
                    }
                    if !initialized {
                        self.initialize(&ctx).await?;
                        initialized = true;
                    } else {
                        self.evaluate(&mut ctx).await?;
                    }

                    self.reflectors.retain(|_, state| {
                        Instant::now().saturating_duration_since(state.last_usable_at) <= STALE_AFTER
                    });
                }

                event = ctx.measurements.recv() => {
                    match event {
                        Ok(event) => self.observe(event, &ctx)?,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(count)) => {
                            warn!("Control law missed {count} measurement events");
                            ctx.metrics.send(Metric::event("measurement_lag", "control_law", None));
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            return Err(ChannelClosed).context("Measurement channel closed");
                        }
                    }
                }
            }
        }
    }
}

impl SqmaLaw {
    /// `config` is the `law.sqma` table, if the configuration has one.
    pub fn new(
        config: Option<config::Value>,
        diagnostics: Option<DiagnosticsSender>,
    ) -> anyhow::Result<Self> {
        let settings = SqmaSettings::from_config(config)?;
        let tick = settings.tick_interval;

        Ok(Self {
            settings,
            reflectors: HashMap::new(),
            measurements_after: None,
            download: None,
            upload: None,
            fast_factor: (0.5_f64.ln() / (RECENT_HALF_LIFE_SECS / tick)).exp(),
            slow_factor: (0.5_f64.ln() / (BASELINE_HALF_LIFE_SECS / tick)).exp(),
            rng: fastrand::Rng::new(),
            diagnostics,
            history_saved_at: None,
            history_saved_body: String::new(),
            warned_few_deltas: false,
            warned_no_deltas: false,
        })
    }

    /// Put back the recorded rates saved before a restart.
    async fn restore_history(
        &self,
        store: &LawStore,
        name: &str,
        base_kbps: f64,
        state: &mut RateState,
    ) {
        if !self.settings.save_history {
            return;
        }
        let text = match store.read(HISTORY_FILE).await {
            Ok(Some(text)) => text,
            Ok(None) => return,
            Err(error) => {
                warn!("Not restoring saved history: {error:#}");
                return;
            }
        };
        let size = state.safe_rates.len();
        let Some(dirs) = parse_history(&text, unix_now(), size) else {
            warn!("Ignoring saved history: unreadable, stale or a different history size");
            return;
        };
        let Some((_, saved_base, rates)) = dirs.iter().find(|(n, _, _)| n == name) else {
            return;
        };
        if *saved_base != base_kbps {
            info!("Not restoring {name} history: base was {saved_base}, now {base_kbps}");
            return;
        }
        restore_samples(state, rates);
        info!("Restored {} recorded {name} safe rates", rates.len());
    }

    /// Write the recorded rates out, at most every 30 s and only if they changed.
    async fn save_history(&mut self, store: &LawStore, now: Instant) {
        if !self.settings.save_history {
            return;
        }
        if self
            .history_saved_at
            .is_some_and(|t| now.saturating_duration_since(t) < HISTORY_SAVE_EVERY)
        {
            return;
        }
        self.history_saved_at = Some(now);
        let mut dirs = vec![];
        for (name, state) in [
            ("download", self.download.as_ref()),
            ("upload", self.upload.as_ref()),
        ] {
            if let Some(state) = state {
                dirs.push((name, state.base_kbps, state));
            }
        }
        let size = dirs.first().map_or(0, |(_, _, s)| s.safe_rates.len());
        let body = history_body(size, &dirs);
        if body == self.history_saved_body {
            return;
        }
        let text = format!("{HISTORY_HEADER}\nsaved {}\n{body}", unix_now());
        match store.write(HISTORY_FILE, &text).await {
            Ok(()) => self.history_saved_body = body,
            Err(error) => warn!("Could not save history: {error:#}"),
        }
    }

    fn recover_after_clock_step(&mut self, ctx: &LawContext) -> anyhow::Result<()> {
        self.reflectors.clear();
        for (direction, state) in [
            (Direction::Download, self.download.as_mut()),
            (Direction::Upload, self.upload.as_mut()),
        ] {
            if let Some(shaper) = ctx.shaper(direction) {
                ctx.set_rate(direction, shaper.min_kbps)?;
                if let Some(state) = state {
                    state.current_kbps = shaper.min_kbps;
                }
            }
        }
        // Start the hold after lowering both rates, not when Runtime detected the step.
        self.measurements_after = Some(Instant::now() + MEASUREMENT_SETTLE_TIME);
        ctx.metrics.send(Metric::event(
            "measurement_recovery",
            "local_clock_step",
            None,
        ));
        Ok(())
    }

    async fn initialize(&mut self, ctx: &super::LawContext) -> anyhow::Result<()> {
        let size = self.settings.speed_hist_size;

        if let Some(shaper) = ctx.shaper(Direction::Download) {
            let initial = if self.measurements_after.is_some() {
                shaper.min_kbps
            } else {
                (shaper.base_kbps * 0.6).max(shaper.min_kbps)
            };

            ctx.set_rate(Direction::Download, initial)?;

            let mut state = RateState::new(shaper.base_kbps, size, &mut self.rng);
            state.current_kbps = initial;
            self.restore_history(ctx.store(), "download", shaper.base_kbps, &mut state)
                .await;

            self.download = Some(state);
        }

        if let Some(shaper) = ctx.shaper(Direction::Upload) {
            let initial = if self.measurements_after.is_some() {
                shaper.min_kbps
            } else {
                (shaper.base_kbps * 0.6).max(shaper.min_kbps)
            };

            ctx.set_rate(Direction::Upload, initial)?;

            let mut state = RateState::new(shaper.base_kbps, size, &mut self.rng);
            state.current_kbps = initial;
            self.restore_history(ctx.store(), "upload", shaper.base_kbps, &mut state)
                .await;

            self.upload = Some(state);
        }

        Ok(())
    }

    fn deltas(&self, targets: &[MeasurementTarget], now: Instant) -> (Vec<f64>, Vec<f64>) {
        let mut down = Vec::with_capacity(targets.len());
        let mut up = Vec::with_capacity(targets.len());

        let max_age = Duration::from_secs_f64(self.settings.tick_interval * 2.0);

        for target in targets {
            let Some(state) = self.reflectors.get(target) else {
                continue;
            };

            if !state.valid || now.saturating_duration_since(state.last_usable_at) >= max_age {
                continue;
            }

            let down_delta = state.recent.down - state.baseline.down;
            let up_delta = state.recent.up - state.baseline.up;

            down.push(down_delta);
            up.push(up_delta);

            debug!(
                "Reflector: {} ({:?}) down_delay: {} up_delay: {}",
                target.peer(),
                target.protocol(),
                down_delta,
                up_delta
            );
        }

        down.sort_by(f64::total_cmp);
        up.sort_by(f64::total_cmp);

        (down, up)
    }

    async fn evaluate(&mut self, ctx: &mut super::LawContext) -> anyhow::Result<()> {
        let now = Instant::now();

        let (down_deltas, up_deltas, target_count) = {
            let targets = ctx.applied.borrow().active_targets();
            let (down, up) = self.deltas(targets.as_slice(), now);
            (down, up, targets.len())
        };

        let minimum = ctx.num_reflectors().min(target_count);
        if target_count == 0 || down_deltas.len() < minimum || up_deltas.len() < minimum {
            if !self.warned_few_deltas {
                warn!(
                    "Not enough active delta values (D: {}, U: {}, need {})",
                    down_deltas.len(),
                    up_deltas.len(),
                    minimum
                );
                self.warned_few_deltas = true;
            }
        } else if self.warned_few_deltas {
            info!("Enough active delta values again");
            self.warned_few_deltas = false;
        }

        if down_deltas.is_empty() || up_deltas.is_empty() {
            if !self.warned_no_deltas {
                warn!("No reflector data available, dropping to minimum rates");
                self.warned_no_deltas = true;
            }
            ctx.metrics
                .send(Metric::event("reflector_unavailable", "", None));

            for (direction, state) in [
                (Direction::Download, self.download.as_mut()),
                (Direction::Upload, self.upload.as_mut()),
            ] {
                let (Some(state), Some(shaper)) = (state, ctx.shaper(direction)) else {
                    continue;
                };

                ctx.set_rate(direction, shaper.min_kbps)?;
                state.current_kbps = shaper.min_kbps;
            }

            return Ok(());
        }
        if self.warned_no_deltas {
            info!("Reflector data available again");
            self.warned_no_deltas = false;
        }

        let Some(traffic) = *ctx.traffic.borrow() else {
            // No traffic sample available. Retain current rates.
            ctx.metrics
                .send(Metric::event("traffic_sample_unavailable", "", None));
            return Ok(());
        };

        // Load uses the rate in effect during the traffic sample, before this evaluation.
        let (down_load, up_load) = if self.diagnostics.is_some() {
            (
                self.download
                    .as_ref()
                    .map(|s| traffic.rx_kbps / s.current_kbps),
                self.upload
                    .as_ref()
                    .map(|s| traffic.tx_kbps / s.current_kbps),
            )
        } else {
            (None, None)
        };

        for (direction, state, deltas, traffic_kbps, delay_ms) in [
            (
                Direction::Download,
                self.download.as_mut(),
                down_deltas.as_slice(),
                traffic.rx_kbps,
                self.settings.download_delay_ms,
            ),
            (
                Direction::Upload,
                self.upload.as_mut(),
                up_deltas.as_slice(),
                traffic.tx_kbps,
                self.settings.upload_delay_ms,
            ),
        ] {
            let (Some(state), Some(shaper)) = (state, ctx.shaper(direction)) else {
                continue;
            };

            let load = traffic_kbps / state.current_kbps;
            let next = state.calculate_rate(
                shaper,
                traffic_kbps,
                deltas,
                delay_ms,
                &self.settings,
                &mut self.rng,
            );

            if next != state.current_kbps {
                ctx.set_rate(direction, next)?;
                state.current_kbps = next;
            }

            ctx.metrics.send(Metric::Rate {
                direction,
                rate_kbps: state.current_kbps,
                load,
                delta_delay: deltas[deltas.len() / 2],
            });
        }

        self.save_history(ctx.store(), now).await;

        if let Some(diagnostics) = &mut self.diagnostics {
            diagnostics.publish(now, |history_due| DiagnosticRecord {
                time: SystemTime::now(),
                download: self.download.as_ref().map(|s| DirectionStatistics {
                    load: down_load.unwrap(),
                    delta_ms: down_deltas[down_deltas.len() / 2],
                    rate_kbps: s.current_kbps,
                }),
                upload: self.upload.as_ref().map(|s| DirectionStatistics {
                    load: up_load.unwrap(),
                    delta_ms: up_deltas[up_deltas.len() / 2],
                    rate_kbps: s.current_kbps,
                }),
                history: history_due.then(|| SpeedHistory {
                    download: self
                        .download
                        .as_ref()
                        .map(|s| s.safe_rates.iter().copied().collect()),
                    upload: self
                        .upload
                        .as_ref()
                        .map(|s| s.safe_rates.iter().copied().collect()),
                }),
            });
        }

        Ok(())
    }

    fn observe(&mut self, event: MeasurementEvent, ctx: &super::LawContext) -> anyhow::Result<()> {
        let MeasurementOutcome::Reply { one_way, .. } = event.outcome else {
            return Ok(());
        };
        if !ctx.applied.borrow().is_active(&event.target) {
            // Demoted targets can still have retained history. Consume their
            // reference invalidations without changing the active shaper rates.
            if let Some(state) = self.reflectors.get(&event.target)
                && event.started_at > state.last_started_at
                && (one_way.is_some_and(|o| o.status == OneWayStatus::ReferenceChanged)
                    || one_way.is_some() != (state.mode == MeasurementMode::Directional))
            {
                self.reflectors.remove(&event.target);
            }
            return Ok(());
        }
        // Diagnostics retain raw signed values, including rejected OWDs and
        // observations arriving during the recovery hold.
        if let Some(rtt_ms) = event.rtt_ms() {
            ctx.metrics.send(Metric::Ping {
                reflector: event.target.peer(),
                protocol: event.target.protocol(),
                rtt: rtt_ms,
                up_time: one_way
                    .map(|o| o.uplink.as_secs_f64() * 1000.0)
                    .unwrap_or(rtt_ms / 2.0),
                down_time: one_way
                    .map(|o| o.downlink.as_secs_f64() * 1000.0)
                    .unwrap_or(rtt_ms / 2.0),
            });
        }
        if let Some(OneWayLatency {
            status: OneWayStatus::Unusable(reason),
            ..
        }) = one_way
        {
            // A delayed observation must not invalidate more recent history.
            if reason != OneWayRejection::OutOfOrder
                && let Some(state) = self.reflectors.get_mut(&event.target)
                && event.started_at > state.last_started_at
            {
                state.valid = false;
                state.last_started_at = event.started_at;
            }
            ctx.metrics.send(Metric::event(
                "directional_rejected",
                reason.reason(),
                Some(event.target.peer()),
            ));
            return Ok(());
        }
        let Some(rtt_ms) = event.rtt_ms() else {
            return Ok(());
        };
        if self
            .measurements_after
            .is_some_and(|after| event.started_at < after)
        {
            return Ok(());
        }
        let mode = if one_way.is_some() {
            MeasurementMode::Directional
        } else {
            MeasurementMode::Rtt
        };
        if let Some(state) = self.reflectors.get(&event.target) {
            if event.started_at <= state.last_started_at {
                return Ok(());
            }
            let changed = one_way.is_some_and(|o| o.status == OneWayStatus::ReferenceChanged);
            if changed || state.mode != mode {
                // Rebuild only this target from its accepted new reference,
                // as for a newly active target. Other reflectors remain usable.
                self.reflectors.remove(&event.target);
                let reason = if changed {
                    "directional_reference_changed"
                } else {
                    "measurement_mode_changed"
                };
                ctx.metrics.send(Metric::event(
                    "measurement_recovery",
                    reason,
                    Some(event.target.peer()),
                ));
            }
        }
        let sample = EwmaStats::from_reply(rtt_ms, one_way);
        let state = self
            .reflectors
            .entry(event.target.clone())
            .or_insert_with(|| {
                ReflectorState::new(sample, event.started_at, event.completed_at, mode)
            });
        state.update(
            sample,
            event.started_at,
            event.completed_at,
            self.slow_factor,
            self.fast_factor,
        );
        ctx.metrics.send(Metric::Baseline {
            reflector: event.target.peer(),
            baseline_up_ewma: state.baseline.up,
            baseline_down_ewma: state.baseline.down,
            recent_up_ewma: state.recent.up,
            recent_down_ewma: state.recent.down,
        });
        Ok(())
    }
}

#[derive(Clone, Copy, Debug)]
pub struct EwmaStats {
    pub down: f64,
    pub up: f64,
}

impl EwmaStats {
    fn from_reply(rtt_ms: f64, one_way: Option<OneWayLatency>) -> Self {
        Self {
            down: one_way
                .map(|o| o.downlink.as_nanos() as f64 / 1000000.0)
                .unwrap_or(rtt_ms / 2.0),
            up: one_way
                .map(|o| o.uplink.as_nanos() as f64 / 1000000.0)
                .unwrap_or(rtt_ms / 2.0),
        }
    }

    fn update(&mut self, sample: Self, factor: f64) {
        self.down = self.down * factor + sample.down * (1.0 - factor);
        self.up = self.up * factor + sample.up * (1.0 - factor);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MeasurementMode {
    Directional,
    Rtt,
}

#[derive(Clone, Copy, Debug)]
pub struct ReflectorState {
    pub baseline: EwmaStats,
    pub recent: EwmaStats,
    /// Freshness of usable data, independent of raw reply liveness.
    pub last_usable_at: Instant,
    last_started_at: Instant,
    mode: MeasurementMode,
    pub valid: bool,
}

impl ReflectorState {
    fn new(
        sample: EwmaStats,
        started_at: Instant,
        received_at: Instant,
        mode: MeasurementMode,
    ) -> Self {
        Self {
            baseline: sample,
            recent: sample,
            last_usable_at: received_at,
            last_started_at: started_at,
            mode,
            valid: true,
        }
    }

    fn update(
        &mut self,
        sample: EwmaStats,
        started_at: Instant,
        received_at: Instant,
        slow_factor: f64,
        fast_factor: f64,
    ) {
        if received_at.saturating_duration_since(self.last_usable_at) > STALE_AFTER {
            *self = Self::new(sample, started_at, received_at, self.mode);
        }
        self.last_usable_at = received_at;
        self.last_started_at = started_at;
        self.baseline.update(sample, slow_factor);
        self.recent.update(sample, fast_factor);
        self.baseline.down = self.baseline.down.min(self.recent.down);
        self.baseline.up = self.baseline.up.min(self.recent.up);
        self.valid = true;
    }
}
