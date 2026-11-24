// SPDX-FileCopyrightText: 2026-Present Nils Andreas Svee mailto:contact@lochnair.net (github @Lochnair)
//
// SPDX-License-Identifier: MPL-2.0

use crate::measurement::model::{MeasurementProtocol, MeasurementRole};
use crate::runtime::Direction;
use crate::settings::Settings;
use crate::settings::{ObservabilityProtocol, ObservabilitySettings};
use crate::time::{ClockSample, Realtime};
use log::{info, warn};
use std::borrow::Cow;
use std::fmt::Write;
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::mpsc;

const MAX_RECONNECT_BACKOFF: u64 = 60;

enum Transport {
    Udp(UdpSocket),
    Tcp {
        stream: Option<TcpStream>,
        host: String,
        port: u16,
        reconnect_backoff: u64,
        next_reconnect_at: tokio::time::Instant,
    },
}

impl Transport {
    async fn new_udp(host: &str, port: u16) -> anyhow::Result<Self> {
        let socket = UdpSocket::bind("0.0.0.0:0").await?;
        socket.connect((host, port)).await?;
        Ok(Transport::Udp(socket))
    }

    fn new_tcp(host: &str, port: u16) -> Self {
        Transport::Tcp {
            stream: None,
            host: host.to_string(),
            port,
            reconnect_backoff: 1,
            next_reconnect_at: tokio::time::Instant::now(),
        }
    }

    async fn send(&mut self, data: &str) {
        match self {
            Transport::Udp(socket) => {
                match tokio::time::timeout(Duration::from_millis(100), socket.send(data.as_bytes()))
                    .await
                {
                    Ok(Ok(_)) => {}
                    Ok(Err(e)) => warn!("UDP send failed: {e}"),
                    Err(_) => warn!("UDP send timed out"),
                }
            }
            Transport::Tcp {
                stream,
                host,
                port,
                reconnect_backoff,
                next_reconnect_at,
            } => {
                if stream.is_none() {
                    // Drop this batch during backoff so the exporter can keep draining.
                    if tokio::time::Instant::now() < *next_reconnect_at {
                        return;
                    }
                    match TcpStream::connect((host.as_str(), *port)).await {
                        Ok(s) => {
                            info!("Connected to metrics collector at {}:{}", host, port);
                            *stream = Some(s);
                            *reconnect_backoff = 1;
                        }
                        Err(e) => {
                            warn!(
                                "Failed to connect to {}:{} - {}, backoff {}s",
                                host, port, e, reconnect_backoff
                            );
                            *next_reconnect_at = tokio::time::Instant::now()
                                + Duration::from_secs(*reconnect_backoff);
                            *reconnect_backoff =
                                (*reconnect_backoff * 2).min(MAX_RECONNECT_BACKOFF);
                            return;
                        }
                    }
                }

                let full_data = format!("{}\n", data);

                if let Some(s) = stream.as_mut() {
                    let result = tokio::time::timeout(
                        Duration::from_millis(500),
                        s.write_all(full_data.as_bytes()),
                    )
                    .await;

                    if !matches!(result, Ok(Ok(()))) {
                        warn!("TCP send failed: {result:?}");
                        *stream = None;
                        *next_reconnect_at =
                            tokio::time::Instant::now() + Duration::from_secs(*reconnect_backoff);
                        *reconnect_backoff = (*reconnect_backoff * 2).min(MAX_RECONNECT_BACKOFF);
                    }
                }
            }
        }
    }
}

/// Build metrics with the constructors below, not the variants.
pub enum Metric {
    /// One reply, sent by the measurement engine whatever law runs.
    Ping {
        reflector: IpAddr,
        protocol: MeasurementProtocol,
        role: MeasurementRole,
        rtt: f64,
        /// Raw one-way times in ms (up, down), when the protocol has them.
        one_way: Option<(f64, f64)>,
    },
    /// One lost probe, sent by the measurement engine.
    Loss {
        reflector: IpAddr,
        protocol: MeasurementProtocol,
        role: MeasurementRole,
    },
    /// A rate applied to the shaper, sent by the runtime when it is set.
    Rate {
        direction: Direction,
        rate_kbps: f64,
    },
    /// Measured traffic, sent by the runtime per traffic sample.
    Traffic {
        direction: Direction,
        traffic_kbps: f64,
    },
    Event {
        name: String,
        reason: String,
        reflector: Option<IpAddr>,
        tags: Vec<(String, String)>,
    },
    /// A law's own series. The writer adds the `law` tag.
    Law {
        series: String,
        tags: Vec<(String, String)>,
        fields: Vec<(String, f64)>,
    },
    Dropped {
        count: u32,
    },
}

impl Metric {
    pub fn ping(
        reflector: IpAddr,
        protocol: MeasurementProtocol,
        role: MeasurementRole,
        rtt: f64,
        one_way: Option<(f64, f64)>,
    ) -> Self {
        Metric::Ping {
            reflector,
            protocol,
            role,
            rtt,
            one_way,
        }
    }

    pub fn loss(reflector: IpAddr, protocol: MeasurementProtocol, role: MeasurementRole) -> Self {
        Metric::Loss {
            reflector,
            protocol,
            role,
        }
    }

    pub fn rate(direction: Direction, rate_kbps: f64) -> Self {
        Metric::Rate {
            direction,
            rate_kbps,
        }
    }

    pub fn traffic(direction: Direction, traffic_kbps: f64) -> Self {
        Metric::Traffic {
            direction,
            traffic_kbps,
        }
    }

    pub fn event(
        name: impl Into<String>,
        reason: impl Into<String>,
        reflector: Option<IpAddr>,
    ) -> Self {
        Metric::Event {
            name: name.into(),
            reason: reason.into(),
            reflector,
            tags: Vec::new(),
        }
    }

    pub fn law(series: impl Into<String>) -> Self {
        Metric::Law {
            series: series.into(),
            tags: Vec::new(),
            fields: Vec::new(),
        }
    }

    /// Adds a tag to an event or law metric; other metrics are unchanged.
    pub fn tag(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        if let Metric::Event { tags, .. } | Metric::Law { tags, .. } = &mut self {
            tags.push((key.into(), value.into()));
        }
        self
    }

    /// Adds a field to a law metric; other metrics are unchanged.
    pub fn field(mut self, key: impl Into<String>, value: f64) -> Self {
        if let Metric::Law { fields, .. } = &mut self {
            fields.push((key.into(), value));
        }
        self
    }
}

/// Escapes a tag key, tag value or field key for the line protocol.
fn escape(text: &str) -> Cow<'_, str> {
    if text.contains([',', '=', ' ', '\\']) {
        let mut out = String::with_capacity(text.len() + 4);
        for c in text.chars() {
            if matches!(c, ',' | '=' | ' ' | '\\') {
                out.push('\\');
            }
            out.push(c);
        }
        Cow::Owned(out)
    } else {
        Cow::Borrowed(text)
    }
}

#[derive(Clone)]
pub struct MetricsSender {
    tx: Option<mpsc::Sender<(Metric, u64)>>,
    dropped: Arc<AtomicU32>,
    export_ping_metrics: bool,
    export_law_metrics: bool,
    export_rate_metrics: bool,
    export_events: bool,
}

impl MetricsSender {
    pub fn new(
        tx: mpsc::Sender<(Metric, u64)>,
        dropped: Arc<AtomicU32>,
        settings: &ObservabilitySettings,
    ) -> Self {
        Self {
            tx: Some(tx),
            dropped,
            export_ping_metrics: settings.export_ping_metrics,
            export_law_metrics: settings.export_law_metrics,
            export_rate_metrics: settings.export_rate_metrics,
            export_events: settings.export_events,
        }
    }

    pub fn disabled() -> Self {
        Self {
            tx: None,
            dropped: Arc::new(AtomicU32::new(0)),
            export_ping_metrics: false,
            export_law_metrics: false,
            export_rate_metrics: false,
            export_events: false,
        }
    }

    pub fn send(&self, metric: Metric) {
        let enabled = match &metric {
            Metric::Ping { .. } | Metric::Loss { .. } => self.export_ping_metrics,
            Metric::Law { .. } => self.export_law_metrics,
            Metric::Rate { .. } | Metric::Traffic { .. } => self.export_rate_metrics,
            Metric::Event { .. } => self.export_events,
            Metric::Dropped { .. } => true,
        };
        if !enabled {
            return;
        }

        if let Some(ref tx) = self.tx {
            let ts = ClockSample::<Realtime>::now().as_nanos();
            if tx.try_send((metric, ts)).is_err() {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

/// Tags on every line, `host=…,instance=…`, and with `law=…` for law metrics.
#[derive(Clone)]
struct LineTags {
    common: String,
    law: String,
}

fn role_name(role: MeasurementRole) -> &'static str {
    match role {
        MeasurementRole::Active => "active",
        MeasurementRole::Candidate => "candidate",
    }
}

fn write_tags(out: &mut String, tags: &[(String, String)]) {
    // The line protocol has no empty tag keys or values.
    for (key, value) in tags.iter().filter(|(k, v)| !k.is_empty() && !v.is_empty()) {
        write!(out, ",{}={}", escape(key), escape(value)).unwrap_or(());
    }
}

pub struct Metrics {
    settings: Settings,
    tags: LineTags,
    metrics_dropped: Arc<AtomicU32>,
    metrics_rx: mpsc::Receiver<(Metric, u64)>,
    transport: Transport,
}

impl Metrics {
    pub async fn new(
        settings: Settings,
        instance: &str,
        law_name: &str,
        metrics_rx: mpsc::Receiver<(Metric, u64)>,
        metrics_dropped: Arc<AtomicU32>,
    ) -> anyhow::Result<Self> {
        let host = settings
            .observability
            .host
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("Observability host not configured"))?;
        let port = settings.observability.port;
        let transport = match settings.observability.protocol {
            ObservabilityProtocol::Udp => {
                info!("Metrics exporter configured for UDP to {}:{}", host, port);
                Transport::new_udp(host, port).await?
            }
            ObservabilityProtocol::Tcp => {
                info!("Metrics exporter configured for TCP to {}:{}", host, port);
                Transport::new_tcp(host, port)
            }
        };
        let common = format!(
            "host={},instance={}",
            escape(&settings.observability.host_tag),
            escape(instance)
        );
        let tags = LineTags {
            law: format!("{common},law={}", escape(law_name)),
            common,
        };
        Ok(Self {
            settings,
            tags,
            metrics_rx,
            metrics_dropped,
            transport,
        })
    }

    pub async fn run(mut self) -> anyhow::Result<()> {
        let batch_size = self.settings.observability.batch_size;
        let timeout = Duration::from_millis(self.settings.observability.batch_timeout_ms);
        let tags = self.tags.clone();
        let mut batch = Vec::with_capacity(batch_size);

        let deadline = tokio::time::sleep(timeout);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                // A steady stream must not postpone the current batch deadline.
                biased;
                _ = &mut deadline => {
                    self.collect_dropped(&mut batch);
                    if !batch.is_empty() {
                        Self::flush(&mut self.transport, &batch, &tags).await;
                        batch.clear();
                    }
                    deadline.as_mut().reset(tokio::time::Instant::now() + timeout);
                    continue;
                }
                metric = self.metrics_rx.recv() => {
                    match metric {
                        Some(metric) => batch.push(metric),
                        None => break,
                    }
                }
            }

            // Drain only up to the batch limit; do not starve the deadline.
            while batch.len() < batch_size {
                match self.metrics_rx.try_recv() {
                    Ok(metric) => batch.push(metric),
                    Err(_) => break,
                }
            }
            if batch.len() >= batch_size {
                Self::flush(&mut self.transport, &batch, &tags).await;
                batch.clear();
                deadline
                    .as_mut()
                    .reset(tokio::time::Instant::now() + timeout);
            }
        }

        self.collect_dropped(&mut batch);
        if !batch.is_empty() {
            Self::flush(&mut self.transport, &batch, &tags).await;
        }
        Ok(())
    }

    fn collect_dropped(&self, batch: &mut Vec<(Metric, u64)>) {
        let dropped = self.metrics_dropped.swap(0, Ordering::Relaxed);
        if dropped > 0 {
            let ts = ClockSample::<Realtime>::now().as_nanos();
            batch.push((Metric::Dropped { count: dropped }, ts));
        }
    }

    async fn flush(transport: &mut Transport, batch: &[(Metric, u64)], tags: &LineTags) {
        match transport {
            /*
             * the UDP receive buffer might be too small on the receiver to accept batched data,
             * and it'll get cutoff in the middle of a record. so send metrics directly on UDP
             */
            Transport::Udp(_) => {
                let mut data = String::with_capacity(300);
                for (metric, timestamp_ns) in batch {
                    data.clear();
                    Self::write_lines(metric, *timestamp_ns, tags, &mut data);
                    transport.send(&data).await;
                }
            }
            Transport::Tcp { .. } => {
                let mut data = String::with_capacity(batch.len() * 300);
                for (metric, timestamp_ns) in batch {
                    Self::write_lines(metric, *timestamp_ns, tags, &mut data);
                }
                transport.send(&data).await;
            }
        }
    }

    fn write_lines(metric: &Metric, timestamp_ns: u64, tags: &LineTags, out: &mut String) {
        let common = &tags.common;
        match metric {
            Metric::Ping {
                reflector,
                protocol,
                role,
                rtt,
                one_way,
            } => {
                write!(
                    out,
                    "unravel_ping,{common},reflector={reflector},type={protocol},role={} rtt={rtt:.3}",
                    role_name(*role)
                )
                .unwrap_or(());
                if let Some((up, down)) = one_way {
                    write!(out, ",up_time={up:.3},down_time={down:.3}").unwrap_or(());
                }
                writeln!(out, " {timestamp_ns}").unwrap_or(());
            }
            Metric::Loss {
                reflector,
                protocol,
                role,
            } => {
                writeln!(
                    out,
                    "unravel_loss,{common},reflector={reflector},type={protocol},role={} count=1i {timestamp_ns}",
                    role_name(*role)
                )
                .unwrap_or(());
            }
            Metric::Rate {
                direction,
                rate_kbps,
            } => {
                writeln!(
                    out,
                    "unravel_rate,{common},direction={} rate_kbps={rate_kbps:.0} {timestamp_ns}",
                    direction.as_str()
                )
                .unwrap_or(());
            }
            Metric::Traffic {
                direction,
                traffic_kbps,
            } => {
                writeln!(
                    out,
                    "unravel_traffic,{common},direction={} traffic_kbps={traffic_kbps:.0} {timestamp_ns}",
                    direction.as_str()
                )
                .unwrap_or(());
            }
            Metric::Event {
                name,
                reason,
                reflector,
                tags,
            } => {
                write!(out, "unravel_event,{common},type={}", escape(name)).unwrap_or(());
                if let Some(reflector) = reflector {
                    write!(out, ",reflector={reflector}").unwrap_or(());
                }
                if !reason.is_empty() {
                    write!(out, ",reason={}", escape(reason)).unwrap_or(());
                }
                write_tags(out, tags);
                writeln!(out, " count=1i {timestamp_ns}").unwrap_or(());
            }
            Metric::Law {
                series,
                tags: law_tags,
                fields,
            } => {
                let mut fields = fields.iter().filter(|(_, value)| value.is_finite());
                let Some((key, value)) = fields.next() else {
                    return;
                };
                write!(out, "unravel_law,{},series={}", tags.law, escape(series)).unwrap_or(());
                write_tags(out, law_tags);
                write!(out, " {}={value}", escape(key)).unwrap_or(());
                for (key, value) in fields {
                    write!(out, ",{}={value}", escape(key)).unwrap_or(());
                }
                writeln!(out, " {timestamp_ns}").unwrap_or(());
            }
            Metric::Dropped { count } => {
                writeln!(
                    out,
                    "unravel_metrics_dropped,{common} count={count}i {timestamp_ns}"
                )
                .unwrap_or(());
            }
        }
    }
}
