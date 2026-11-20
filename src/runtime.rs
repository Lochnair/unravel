// SPDX-FileCopyrightText: 2026-Present Nils Andreas Svee mailto:contact@lochnair.net (github @Lochnair)
//
// SPDX-License-Identifier: MPL-2.0

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use log::{error, warn};
use tokio::{sync::watch, time};

use crate::{
    metrics::{Metric, MetricsSender},
    platform::{InterfaceStats, Platform},
    settings::Settings,
    time::{CLOCK_SAMPLE_INTERVAL, ClockStep, ClockTracker},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Direction {
    Download,
    Upload,
}

impl Direction {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Direction::Download => "download",
            Direction::Upload => "upload",
        }
    }
}

pub(crate) struct Shaper {
    interface: String,

    pub(crate) base_kbps: f64,
    pub(crate) min_kbps: f64,
    pub(crate) max_kbps: Option<f64>,
    /// The largest packet on the wire, read from the interface and its CAKE
    /// qdisc at startup. `None` where the platform can't tell.
    #[cfg_attr(not(feature = "lua"), allow(dead_code))]
    pub(crate) wire_packet_bytes: Option<u32>,
    /// The last rate set, simulated in dry run.
    rate: watch::Sender<Option<f64>>,
    /// The law's last delay signal, if it reports one.
    delay: watch::Sender<Option<DelaySignal>>,
}

/// What a law last saw as delay for one direction: milliseconds above its
/// baseline, and the threshold it reacts to, if it has a single one.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct DelaySignal {
    pub(crate) delay_ms: f64,
    pub(crate) threshold_ms: Option<f64>,
}

impl Shaper {
    fn new(
        platform: &Platform,
        interface: &str,
        base_kbps: f64,
        min_kbps: f64,
        max_kbps: Option<f64>,
    ) -> Self {
        let wire_packet_bytes = platform
            .wire_packet_bytes(interface)
            .unwrap_or_else(|error| {
                warn!("Couldn't read the wire packet size of {interface}: {error}");
                None
            });
        Self {
            interface: interface.to_owned(),
            base_kbps,
            min_kbps,
            max_kbps,
            wire_packet_bytes,
            rate: watch::channel(None).0,
            delay: watch::channel(None).0,
        }
    }

    #[cfg_attr(not(all(feature = "ubus", unix)), allow(dead_code))]
    pub(crate) fn interface(&self) -> &str {
        &self.interface
    }

    /// The rate last set, simulated in dry run, if any has been.
    #[cfg_attr(not(all(feature = "ubus", unix)), allow(dead_code))]
    pub(crate) fn rate_kbps(&self) -> Option<f64> {
        *self.rate.borrow()
    }

    /// The law's delay signal, as it reports it.
    #[cfg_attr(not(all(feature = "ubus", unix)), allow(dead_code))]
    pub(crate) fn delay(&self) -> watch::Receiver<Option<DelaySignal>> {
        self.delay.subscribe()
    }
}

#[derive(Clone, Copy, Debug)]
pub struct TrafficRates {
    #[cfg_attr(not(feature = "lua"), allow(dead_code))]
    pub sampled_at: Instant,
    pub _generation: u16,
    pub rx_kbps: f64,
    pub tx_kbps: f64,
}

pub struct Runtime {
    platform: Arc<Platform>,

    traffic_interface: String,
    sample_interval: Duration,
    traffic_tx: watch::Sender<Option<TrafficRates>>,
    clock_steps_tx: watch::Sender<Option<ClockStep>>,

    download: Option<Shaper>,
    upload: Option<Shaper>,
    dry_run: bool,
    metrics: MetricsSender,
}

impl Runtime {
    pub(super) fn new(
        platform: Arc<Platform>,
        settings: Settings,
        sample_interval: Duration,
        traffic_tx: watch::Sender<Option<TrafficRates>>,
        metrics: MetricsSender,
    ) -> anyhow::Result<Self> {
        let traffic_interface = settings
            .network
            .traffic_interface()
            .ok_or_else(|| anyhow::anyhow!("At least one shaping interface must be configured"))?
            .to_owned();
        Ok(Runtime {
            traffic_interface,
            sample_interval,
            traffic_tx,
            clock_steps_tx: watch::channel(None).0,
            download: settings.network.download_interface().map(|interface| {
                Shaper::new(
                    &platform,
                    interface,
                    settings.network.download_base_kbits,
                    settings.network.download_min_kbits(),
                    settings.network.download_max_kbits(),
                )
            }),
            upload: settings.network.upload_interface().map(|interface| {
                Shaper::new(
                    &platform,
                    interface,
                    settings.network.upload_base_kbits,
                    settings.network.upload_min_kbits(),
                    settings.network.upload_max_kbits(),
                )
            }),
            platform,
            metrics,
            dry_run: settings.dry_run,
        })
    }

    pub(super) async fn run(self: Arc<Self>) -> anyhow::Result<()> {
        let mut generation = 0u16;
        let mut clock = ClockTracker::default();
        clock.sample();
        let mut clock_ticker = time::interval(CLOCK_SAMPLE_INTERVAL);
        clock_ticker.set_missed_tick_behavior(time::MissedTickBehavior::Skip);

        let mut previous = self
            .platform
            .read_stats(&self.traffic_interface)
            .ok()
            .map(|stats| (Instant::now(), stats));

        let mut ticker = time::interval_at(
            time::Instant::now() + self.sample_interval,
            self.sample_interval,
        );

        ticker.set_missed_tick_behavior(time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                _ = clock_ticker.tick() => {
                    if let Some(step) = clock.sample() {
                        warn!("Local clock stepped by {:.3} seconds", step.delta_secs);
                        self.clock_steps_tx.send_replace(Some(step));
                    }
                }
                _ = ticker.tick() => self.sample_traffic(&mut previous, &mut generation),
            }
        }
    }

    pub(crate) fn clock_steps(&self) -> watch::Receiver<Option<ClockStep>> {
        let mut receiver = self.clock_steps_tx.subscribe();
        // A late subscriber must also handle an already-detected discontinuity.
        if receiver.borrow().is_some() {
            receiver.mark_changed();
        }
        receiver
    }

    pub fn restore_shaper(&self) -> anyhow::Result<()> {
        let mut failures = Vec::new();
        for direction in [Direction::Download, Direction::Upload] {
            let Some(shaper) = self.shaper(direction) else {
                continue;
            };

            if let Err(e) = self.set_rate(direction, shaper.base_kbps) {
                error!("Unable to restore shaper for {direction:?}: {e:?}");
                failures.push(format!("{direction:?}: {e:#}"));
            }
        }

        anyhow::ensure!(failures.is_empty(), "{}", failures.join("; "));
        Ok(())
    }

    fn sample_traffic(
        &self,
        previous: &mut Option<(Instant, InterfaceStats)>,
        generation: &mut u16,
    ) {
        let stats = match self.platform.read_stats(&self.traffic_interface) {
            Ok(stats) => stats,
            Err(err) => {
                log::warn!("Failed to read interface statistics: {err}");

                if previous.take().is_some() {
                    *generation = generation.wrapping_add(1);
                }

                self.traffic_tx.send_replace(None);
                return;
            }
        };

        let now = Instant::now();

        let Some((last_at, last_stats)) = previous.replace((now, stats)) else {
            self.traffic_tx.send_replace(None);
            return;
        };

        if stats.rx_bytes < last_stats.rx_bytes || stats.tx_bytes < last_stats.tx_bytes {
            *generation = generation.wrapping_add(1);
            self.traffic_tx.send_replace(None);
            return;
        }

        let elapsed = now.duration_since(last_at).as_secs_f64();

        if elapsed <= 0.0 {
            self.traffic_tx.send_replace(None);
            return;
        }

        const KBITS_PER_BYTE: f64 = 8.0 / 1_000.0;

        let rates = TrafficRates {
            sampled_at: now,
            _generation: *generation,
            rx_kbps: (stats.rx_bytes - last_stats.rx_bytes) as f64 * KBITS_PER_BYTE / elapsed,
            tx_kbps: (stats.tx_bytes - last_stats.tx_bytes) as f64 * KBITS_PER_BYTE / elapsed,
        };

        self.traffic_tx.send_replace(Some(rates));

        for (direction, traffic_kbps) in [
            (Direction::Download, rates.rx_kbps),
            (Direction::Upload, rates.tx_kbps),
        ] {
            self.metrics.send(Metric::traffic(direction, traffic_kbps));
        }
    }

    /// The latest traffic sample, if the last one succeeded.
    #[cfg_attr(not(all(feature = "ubus", unix)), allow(dead_code))]
    pub(crate) fn traffic(&self) -> Option<TrafficRates> {
        *self.traffic_tx.borrow()
    }

    /// Every traffic sample, `None` after a failed one.
    #[cfg_attr(not(all(feature = "ubus", unix)), allow(dead_code))]
    pub(crate) fn traffic_updates(&self) -> watch::Receiver<Option<TrafficRates>> {
        self.traffic_tx.subscribe()
    }

    pub(crate) fn shaper(&self, direction: Direction) -> Option<&Shaper> {
        match direction {
            Direction::Download => self.download.as_ref(),
            Direction::Upload => self.upload.as_ref(),
        }
    }

    /// Publishes the law's delay signal for `direction`. Only status readers
    /// see it; it changes nothing about shaping.
    pub(crate) fn report_delay(
        &self,
        direction: Direction,
        delay_ms: f64,
        threshold_ms: Option<f64>,
    ) -> anyhow::Result<()> {
        let shaper = self
            .shaper(direction)
            .ok_or_else(|| anyhow::anyhow!("No shaper configured for {direction:?}"))?;
        anyhow::ensure!(delay_ms.is_finite(), "Invalid delay: {delay_ms}");
        anyhow::ensure!(
            threshold_ms.is_none_or(f64::is_finite),
            "Invalid delay threshold: {threshold_ms:?}"
        );
        shaper.delay.send_replace(Some(DelaySignal {
            delay_ms,
            threshold_ms,
        }));
        Ok(())
    }

    pub(super) fn set_rate(&self, direction: Direction, rate_kbps: f64) -> anyhow::Result<()> {
        let shaper = self
            .shaper(direction)
            .ok_or_else(|| anyhow::anyhow!("No shaper configured for {direction:?}"))?;

        anyhow::ensure!(
            rate_kbps.is_finite() && rate_kbps >= 0.0,
            "Invalid requested shaper rate: {rate_kbps}"
        );

        let mut rate_kbps = rate_kbps;
        if let Some(max_kbps) = shaper.max_kbps
            && rate_kbps > max_kbps
        {
            warn!(
                "Law requested {direction:?} rate above configured maximum: \
                 {rate_kbps} > {max_kbps} kbps, using the maximum"
            );
            rate_kbps = max_kbps;
        }

        if rate_kbps < shaper.min_kbps {
            warn!(
                "Law requested {direction:?} rate below configured minimum: \
                 {rate_kbps} < {} kbps",
                shaper.min_kbps
            );
        }

        self.platform
            .set_rate(&shaper.interface, rate_kbps.round() as u64, self.dry_run)?;
        shaper.rate.send_replace(Some(rate_kbps));
        self.metrics.send(Metric::rate(direction, rate_kbps));

        Ok(())
    }
}
