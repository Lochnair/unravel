// SPDX-FileCopyrightText: 2026-Present Nils Andreas Svee mailto:contact@lochnair.net (github @Lochnair)
//
// SPDX-License-Identifier: MPL-2.0

//! A control law written as a script. This module is language-free: it owns
//! the loop, the events, `ctx` and config conversion. Each language is a
//! [`Backend`] in its own file behind its own cargo feature.

#[cfg(feature = "lua")]
mod lua;

use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::Context;
use log::warn;
use tokio::{sync::broadcast::error::RecvError, time};

use crate::{
    error::ChannelClosed,
    law::{Cadence, Law, LawContext},
    measurement::model::{MeasurementEvent, MeasurementOutcome, OneWayStatus},
    metrics::Metric,
    runtime::{Direction, TrafficRates},
};

pub(super) trait Backend: Send {
    /// Read once, right after loading.
    fn cadence(&self) -> anyhow::Result<ScriptCadence>;

    /// Calls the hook for `event`. An absent optional hook is not an error.
    fn call(&mut self, event: &Event, ctx: &ScriptCtx<'_>) -> anyhow::Result<()>;
}

pub(super) struct ScriptCadence {
    pub cadence: Cadence,
    /// `cadence.timer`; set exactly when the script has `on_timer`.
    pub timer: Option<Duration>,
}

/// What a hook is called with. Times are seconds since the law started.
pub(super) enum Event {
    Init(Value),
    Measurement(Measurement),
    Traffic(Traffic),
    Timer(f64),
    ClockStep,
}

impl Event {
    pub fn hook(&self) -> &'static str {
        match self {
            Event::Init(_) => "init",
            Event::Measurement(_) => "on_measurement",
            Event::Traffic(_) => "on_traffic",
            Event::Timer(_) => "on_timer",
            Event::ClockStep => "on_clock_step",
        }
    }
}

pub(super) const REQUIRED_HOOKS: &[&str] = &["on_measurement", "on_traffic"];
pub(super) const OPTIONAL_HOOKS: &[&str] = &["init", "on_timer", "on_clock_step"];

pub(super) struct Measurement {
    /// The IP, or `ip:port` for IRTT, where one IP can serve several
    /// endpoints. With `protocol` it identifies the target.
    pub reflector: String,
    pub protocol: String,
    /// `"active"` or `"candidate"`.
    pub role: &'static str,
    /// When the probe was sent; `t` is when it completed.
    pub sent: f64,
    pub t: f64,
    pub loss: bool,
    pub rtt_ms: Option<f64>,
    pub up_ms: Option<f64>,
    pub down_ms: Option<f64>,
    /// `"usable"`, `"reference_changed"` or the rejection reason; `None`
    /// without one-way times.
    pub one_way: Option<&'static str>,
}

pub(super) struct Traffic {
    pub t: f64,
    pub download_kbps: f64,
    pub upload_kbps: f64,
}

/// The script's config, backend-neutral.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Value {
    Nil,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<Value>),
    Map(Vec<(String, Value)>),
}

impl From<config::Value> for Value {
    fn from(value: config::Value) -> Self {
        use config::ValueKind;
        match value.kind {
            ValueKind::Nil => Value::Nil,
            ValueKind::Boolean(b) => Value::Bool(b),
            ValueKind::I64(n) => Value::Number(n as f64),
            ValueKind::I128(n) => Value::Number(n as f64),
            ValueKind::U64(n) => Value::Number(n as f64),
            ValueKind::U128(n) => Value::Number(n as f64),
            ValueKind::Float(n) => Value::Number(n),
            // UCI has only strings, so `'15'` has to arrive as 15.
            ValueKind::String(s) => match s.trim().parse::<f64>() {
                Ok(n) => Value::Number(n),
                Err(_) => Value::String(s),
            },
            ValueKind::Array(items) => Value::Array(items.into_iter().map(Value::from).collect()),
            ValueKind::Table(table) => {
                let mut entries: Vec<_> = table.into_iter().map(|(k, v)| (k, v.into())).collect();
                entries.sort_by(|a: &(String, Value), b| a.0.cmp(&b.0));
                Value::Map(entries)
            }
        }
    }
}

/// What a script can do, borrowed for one hook call.
pub(super) struct ScriptCtx<'a> {
    law: &'a LawContext,
}

impl ScriptCtx<'_> {
    pub fn set_rate(&self, direction: &str, rate_kbps: f64) -> anyhow::Result<()> {
        self.law.set_rate(parse_direction(direction)?, rate_kbps)
    }

    pub fn report_delay(
        &self,
        direction: &str,
        delay_ms: f64,
        threshold_ms: Option<f64>,
    ) -> anyhow::Result<()> {
        self.law
            .report_delay(parse_direction(direction)?, delay_ms, threshold_ms)
    }

    pub fn base(&self, direction: &str) -> anyhow::Result<Option<f64>> {
        let direction = parse_direction(direction)?;
        Ok(self.law.shaper(direction).map(|s| s.base_kbps))
    }

    pub fn min(&self, direction: &str) -> anyhow::Result<Option<f64>> {
        let direction = parse_direction(direction)?;
        Ok(self.law.shaper(direction).map(|s| s.min_kbps))
    }

    /// `None` without a shaper for `direction` or without a configured maximum.
    pub fn max(&self, direction: &str) -> anyhow::Result<Option<f64>> {
        let direction = parse_direction(direction)?;
        Ok(self.law.shaper(direction).and_then(|s| s.max_kbps))
    }

    /// The largest packet on the wire, MTU plus CAKE overhead and framing.
    /// `None` without a shaper for `direction` or where it can't be read.
    pub fn wire_packet_bytes(&self, direction: &str) -> anyhow::Result<Option<u32>> {
        let direction = parse_direction(direction)?;
        Ok(self.law.shaper(direction).and_then(|s| s.wire_packet_bytes))
    }

    pub fn num_reflectors(&self) -> usize {
        self.law.num_reflectors()
    }

    pub fn log(&self, level: &str, message: &str) -> anyhow::Result<()> {
        let level = match level {
            "error" => log::Level::Error,
            "warn" => log::Level::Warn,
            "info" => log::Level::Info,
            "debug" => log::Level::Debug,
            "trace" => log::Level::Trace,
            other => anyhow::bail!("unknown log level {other:?}"),
        };
        log::log!(level, "{message}");
        Ok(())
    }

    pub fn store_read(&self, name: &str) -> anyhow::Result<Option<String>> {
        self.law.store().read_blocking(name)
    }

    pub fn store_write(&self, name: &str, contents: &str) -> anyhow::Result<()> {
        self.law.store().write_blocking(name, contents)
    }

    pub fn event(&self, name: &str, reason: &str) {
        self.law.metrics.send(Metric::event(name, reason, None));
    }

    /// A `unravel_law` line for `series`.
    pub fn metric(&self, series: &str, fields: Vec<(String, f64)>, tags: Vec<(String, String)>) {
        let metric = tags
            .into_iter()
            .fold(Metric::law(series), |m, (k, v)| m.tag(k, v));
        let metric = fields.into_iter().fold(metric, |m, (k, v)| m.field(k, v));
        self.law.metrics.send(metric);
    }

    /// The reflectors currently probed as active, as `(reflector, protocol)`
    /// in the same form measurements carry.
    pub fn active_reflectors(&self) -> Vec<(String, String)> {
        self.law
            .applied
            .borrow()
            .active_targets()
            .iter()
            .map(|target| (target.name(), target.protocol().to_string()))
            .collect()
    }
}

fn parse_direction(direction: &str) -> anyhow::Result<Direction> {
    match direction {
        "download" => Ok(Direction::Download),
        "upload" => Ok(Direction::Upload),
        other => anyhow::bail!("unknown direction {other:?}; expected \"download\" or \"upload\""),
    }
}

pub(super) struct ScriptLaw {
    backend: Box<dyn Backend>,
    cadence: ScriptCadence,
    config: Value,
}

impl ScriptLaw {
    /// `config` is the `law.script` table: `path` plus the script's own config.
    pub fn new(config: Option<config::Value>) -> anyhow::Result<Self> {
        let Value::Map(mut entries) = config.map_or(Value::Map(vec![]), Value::from) else {
            anyhow::bail!("law.script must be a table");
        };
        let path = match entries.iter().position(|(k, _)| k == "path") {
            Some(i) => match entries.remove(i).1 {
                Value::String(path) => PathBuf::from(path),
                other => anyhow::bail!("law.script.path must be a string, not {other:?}"),
            },
            None => anyhow::bail!("law.script.path is required"),
        };

        let backend = load(&path).with_context(|| format!("Loading {}", path.display()))?;
        let cadence = backend
            .cadence()
            .with_context(|| format!("Reading the cadence of {}", path.display()))?;
        // The timer's first deadline is now + timer, which panics if it
        // doesn't fit in an Instant. App checks probe and traffic the same way.
        if let Some(timer) = cadence.timer {
            anyhow::ensure!(
                Instant::now().checked_add(timer).is_some(),
                "cadence.timer is too large: {timer:?}"
            );
        }

        Ok(Self {
            backend,
            cadence,
            config: Value::Map(entries),
        })
    }

    fn call(&mut self, ctx: &LawContext, event: Event) -> anyhow::Result<()> {
        let hook = event.hook();
        self.backend
            .call(&event, &ScriptCtx { law: ctx })
            .with_context(|| format!("Script hook {hook} failed"))
    }
}

/// A script language, picked by the file extension.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Language {
    Lua,
}

impl Language {
    fn from_path(path: &Path) -> anyhow::Result<Self> {
        match path.extension().and_then(|ext| ext.to_str()) {
            Some("lua") => Ok(Language::Lua),
            _ => anyhow::bail!("unknown script language; supported: .lua"),
        }
    }
}

fn load(path: &Path) -> anyhow::Result<Box<dyn Backend>> {
    match Language::from_path(path)? {
        Language::Lua => Ok(Box::new(lua::LuaBackend::load(path)?)),
    }
}

#[async_trait::async_trait]
impl Law for ScriptLaw {
    fn cadence(&self) -> Cadence {
        self.cadence.cadence
    }

    async fn run(mut self: Box<Self>, mut ctx: LawContext) -> anyhow::Result<()> {
        let start = Instant::now();
        let since_start = |at: Instant| match at.checked_duration_since(start) {
            Some(d) => d.as_secs_f64(),
            None => -start.duration_since(at).as_secs_f64(),
        };

        let config = std::mem::replace(&mut self.config, Value::Nil);
        self.call(&ctx, Event::Init(config))?;

        let mut timer = self.cadence.timer.map(|period| {
            let mut timer = time::interval_at(time::Instant::now() + period, period);
            timer.set_missed_tick_behavior(time::MissedTickBehavior::Delay);
            timer
        });

        loop {
            tokio::select! {
                // Rate-limited control inputs come before measurements, so a
                // measurement backlog can't starve them (as in SQMA).
                biased;
                changed = ctx.clock_steps.changed() => {
                    changed.context("Clock-step channel closed")?;
                    if ctx.clock_steps.borrow_and_update().is_some() {
                        self.call(&ctx, Event::ClockStep)?;
                    }
                }

                changed = ctx.traffic.changed() => {
                    changed.context("Traffic channel closed")?;
                    let sample: Option<TrafficRates> = *ctx.traffic.borrow_and_update();
                    if let Some(s) = sample {
                        self.call(&ctx, Event::Traffic(Traffic {
                            t: since_start(s.sampled_at),
                            download_kbps: s.rx_kbps,
                            upload_kbps: s.tx_kbps,
                        }))?;
                    }
                }

                _ = tick(&mut timer) => {
                    self.call(&ctx, Event::Timer(since_start(Instant::now())))?;
                }

                event = ctx.measurements.recv() => {
                    match event {
                        Ok(event) => {
                            let m = measurement(
                                &event,
                                &ctx,
                                since_start(event.started_at),
                                since_start(event.completed_at),
                            );
                            self.call(&ctx, Event::Measurement(m))?;
                        }
                        Err(RecvError::Lagged(count)) => {
                            warn!("Control law missed {count} measurement events");
                            ctx.metrics.send(Metric::event("measurement_lag", "control_law", None));
                        }
                        Err(RecvError::Closed) => {
                            return Err(ChannelClosed).context("Measurement channel closed");
                        }
                    }
                }
            }
        }
    }
}

/// Waits for the next timer tick, or forever without a timer.
async fn tick(timer: &mut Option<time::Interval>) {
    match timer {
        Some(timer) => {
            timer.tick().await;
        }
        None => std::future::pending().await,
    }
}

fn measurement(event: &MeasurementEvent, ctx: &LawContext, sent: f64, t: f64) -> Measurement {
    let role = if ctx.applied.borrow().is_active(&event.target) {
        "active"
    } else {
        "candidate"
    };
    let one_way = match event.outcome {
        MeasurementOutcome::Reply { one_way, .. } => one_way,
        MeasurementOutcome::Loss => None,
    };
    Measurement {
        reflector: event.target.name(),
        protocol: event.target.protocol().to_string(),
        role,
        sent,
        t,
        loss: matches!(event.outcome, MeasurementOutcome::Loss),
        rtt_ms: event.rtt_ms(),
        up_ms: one_way.map(|o| o.uplink.as_secs_f64() * 1000.0),
        down_ms: one_way.map(|o| o.downlink.as_secs_f64() * 1000.0),
        one_way: one_way.map(|o| match o.status {
            OneWayStatus::Usable => "usable",
            OneWayStatus::ReferenceChanged => "reference_changed",
            OneWayStatus::Unusable(rejection) => rejection.reason(),
        }),
    }
}
