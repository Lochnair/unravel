// SPDX-FileCopyrightText: 2026-Present Nils Andreas Svee mailto:contact@lochnair.net (github @Lochnair)
//
// SPDX-License-Identifier: MPL-2.0

use crate::{
    measurement::model::{
        MeasurementOutcome, MeasurementSource, MeasurementStream, MeasurementStreamId, OneWayClock,
        OneWayLatency, ProbeCompletion, SignedDuration,
    },
    settings::NetworkSettings,
    time::{ClockSample, Realtime},
};
use icmp_socket2::{
    IcmpSocket4, Icmpv4Message, Icmpv4Packet,
    packet::{WithEchoRequest, WithTimestampRequest},
    tokio::AsyncIcmpV4Socket,
};
use log::debug;
use std::{
    collections::{HashMap, HashSet},
    io,
    net::IpAddr,
    task::{Context, Poll},
    time::{Duration, Instant, SystemTime},
};
use thiserror::Error;

const PROBE_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Error)]
pub enum IcmpError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("invalid ICMP packet: {0}")]
    InvalidPacket(String),
    #[error("no matching probe: {0:?}")]
    NoMatchingProbe(InFlightProbeKey),
    #[error("wrong identifier: expected {expected}, found {found}")]
    WrongIdentifier { expected: u16, found: u16 },
}

#[derive(Debug, Clone, Copy)]
struct InFlightProbe {
    stream: MeasurementStream,
    sent_at: Instant,
    correlation: u64,
}
type InFlightProbeKey = (IpAddr, MeasurementSource, u16);
struct PreparedProbe {
    stream: MeasurementStream,
    sequence: u16,
    packet: Icmpv4Packet,
    correlation: u64,
}

pub struct IcmpDriver {
    socket: AsyncIcmpV4Socket,
    identifier: u16,
    next_sequence: u16,
    inflight: HashMap<InFlightProbeKey, InFlightProbe>,
    // Keep retired sequence leases until their original timeout too. Replacing
    // a stream must never attribute its late reply to a new stream.
    used: HashMap<InFlightProbeKey, Instant>,
    prepared: HashMap<MeasurementStreamId, PreparedProbe>,
}

impl IcmpDriver {
    pub fn new(#[allow(unused)] settings: &NetworkSettings) -> anyhow::Result<Self> {
        #[allow(unused_mut)]
        let mut socket = IcmpSocket4::new()?;

        #[cfg(any(target_os = "android", target_os = "fuchsia", target_os = "linux"))]
        if let Some(device) = &settings.measurement_bind_device {
            icmp_socket2::IcmpSocket::bind_device(&mut socket, device)?;
        }

        #[cfg(any(target_os = "android", target_os = "fuchsia", target_os = "linux"))]
        if let Some(mark) = settings.measurement_mark {
            icmp_socket2::IcmpSocket::set_mark(&mut socket, mark)?;
        }

        #[cfg(target_os = "freebsd")]
        if let Some(fib) = settings.measurement_fib {
            icmp_socket2::IcmpSocket::set_fib(&mut socket, fib)?;
        }

        socket.enable_receive_metadata()?;
        let identifier = (std::process::id() & 0xffff) as u16;
        Ok(Self {
            socket: socket.into_tokio()?,
            identifier,
            next_sequence: 0,
            inflight: HashMap::new(),
            used: HashMap::new(),
            prepared: HashMap::new(),
        })
    }

    pub fn retain(&mut self, ids: &HashSet<MeasurementStreamId>) {
        self.inflight
            .retain(|_, probe| ids.contains(&probe.stream.id));
        self.prepared.retain(|id, _| ids.contains(id));
    }

    pub fn discard(&mut self, id: MeasurementStreamId) {
        self.prepared.remove(&id);
    }

    pub fn poll_send(
        &mut self,
        stream: MeasurementStream,
        cx: &mut Context<'_>,
    ) -> Poll<anyhow::Result<Instant>> {
        if !self.prepared.contains_key(&stream.id) {
            let result = self.prepare(stream);
            if let Err(error) = result {
                return Poll::Ready(Err(error));
            }
        }
        let prepared = &self.prepared[&stream.id];
        let IpAddr::V4(peer) = stream.peer else {
            unreachable!("plan validated IPv4");
        };
        match self.socket.poll_send_to(cx, peer, &prepared.packet) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(error)) => {
                self.discard(stream.id);
                Poll::Ready(Err(error.into()))
            }
            Poll::Ready(Ok(_accepted_bytes)) => {
                let sent_at = Instant::now();
                let prepared = self.prepared.remove(&stream.id).unwrap();
                let key = (stream.peer, stream.source, prepared.sequence);
                self.used.insert(key, sent_at + PROBE_TIMEOUT);
                self.inflight.insert(
                    key,
                    InFlightProbe {
                        stream: prepared.stream,
                        sent_at,
                        correlation: prepared.correlation,
                    },
                );
                Poll::Ready(Ok(sent_at))
            }
        }
    }

    fn prepare(&mut self, stream: MeasurementStream) -> anyhow::Result<()> {
        self.used.retain(|_, until| *until > Instant::now());
        let sequence = (0..=u16::MAX)
            .find_map(|_| {
                let sequence = self.next_sequence;
                self.next_sequence = sequence.wrapping_add(1);
                (!self
                    .used
                    .contains_key(&(stream.peer, stream.source, sequence))
                    && !self.prepared.values().any(|p| {
                        p.stream.peer == stream.peer
                            && p.stream.source == stream.source
                            && p.sequence == sequence
                    }))
                .then_some(sequence)
            })
            .ok_or_else(|| anyhow::anyhow!("ICMP sequence space exhausted for {}", stream.peer))?;
        let correlation = match stream.source {
            MeasurementSource::IcmpTimestamp => {
                ClockSample::<Realtime>::now().as_time_since_midnight() as u64
            }
            _ => fastrand::u64(..),
        };
        let packet = match stream.source {
            MeasurementSource::IcmpEcho => Icmpv4Packet::with_echo_request(
                self.identifier,
                sequence,
                correlation.to_be_bytes().to_vec(),
            )?,
            MeasurementSource::IcmpTimestamp => {
                let originate = correlation as u32;
                Icmpv4Packet::with_timestamp_request(self.identifier, sequence, originate, 0, 0)?
            }
            _ => anyhow::bail!("invalid ICMP source"),
        };
        self.prepared.insert(
            stream.id,
            PreparedProbe {
                stream,
                sequence,
                packet,
                correlation,
            },
        );
        Ok(())
    }

    pub fn poll_receive(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<anyhow::Result<Option<ProbeCompletion>>> {
        match self.socket.poll_recv_from_with_meta(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(error)) if error.kind() == io::ErrorKind::InvalidData => {
                Poll::Ready(Ok(None))
            }
            Poll::Ready(Err(error)) => Poll::Ready(Err(error.into())),
            Poll::Ready(Ok(result)) => {
                let Some(peer) = result.peer.as_socket() else {
                    return Poll::Ready(Ok(None));
                };
                match self.handle_reply(
                    result.packet,
                    peer.ip(),
                    result.received_at,
                    result.kernel_rx_timestamp,
                ) {
                    Ok(event) => Poll::Ready(Ok(Some(event))),
                    Err(
                        IcmpError::WrongIdentifier { .. }
                        | IcmpError::NoMatchingProbe(..)
                        | IcmpError::InvalidPacket(_),
                    ) => Poll::Ready(Ok(None)),
                    Err(error) => Poll::Ready(Err(error.into())),
                }
            }
        }
    }

    pub fn next_deadline(&self) -> Option<Instant> {
        self.inflight
            .values()
            .map(|p| p.sent_at + PROBE_TIMEOUT)
            .min()
    }

    pub fn expire(&mut self, now: Instant) -> Vec<ProbeCompletion> {
        let mut events = Vec::new();
        self.inflight.retain(|_, probe| {
            let deadline = probe.sent_at + PROBE_TIMEOUT;
            if deadline > now {
                return true;
            }
            events.push(ProbeCompletion {
                stream: probe.stream,
                started_at: probe.sent_at,
                completed_at: deadline,
                outcome: MeasurementOutcome::Loss,
            });
            false
        });
        events
    }
    fn handle_reply(
        &mut self,
        packet: Icmpv4Packet,
        peer: IpAddr,
        observed_at: Instant,
        kernel_rx_timestamp: Option<SystemTime>,
    ) -> Result<ProbeCompletion, IcmpError> {
        match packet.message {
            Icmpv4Message::EchoReply {
                identifier,
                sequence,
                payload,
            } => {
                if identifier != self.identifier {
                    return Err(IcmpError::WrongIdentifier {
                        expected: self.identifier,
                        found: identifier,
                    });
                }

                let key = (peer, MeasurementSource::IcmpEcho, sequence);
                if self
                    .inflight
                    .get(&key)
                    .is_none_or(|p| payload != p.correlation.to_be_bytes())
                {
                    return Err(IcmpError::NoMatchingProbe(key));
                }
                let probe = self
                    .inflight
                    .remove(&key)
                    .ok_or(IcmpError::NoMatchingProbe(key))?;

                debug!(
                    "ICMP engine > Type: {:4}  | Reflector IP: {:>15}  | Seq: {:5}  | Current time: {:8}  | Originate: {:>8}  | Received time: {:>8}  | Transmit time: {:>8}  | RTT: {:8.3} ms  | UL time: {:>8}  | DL time: {:>8}",
                    "ICMP",
                    peer,
                    sequence,
                    ClockSample::<Realtime>::now().as_time_since_midnight(),
                    "N/A",
                    "N/A",
                    "N/A",
                    observed_at.duration_since(probe.sent_at).as_secs_f64() * 1_000.0,
                    "N/A",
                    "N/A",
                );

                Ok(ProbeCompletion {
                    stream: probe.stream,
                    started_at: probe.sent_at,
                    completed_at: observed_at,
                    outcome: MeasurementOutcome::Reply {
                        round_trip: None,
                        one_way: None,
                    },
                })
            }
            Icmpv4Message::TimestampReply {
                identifier,
                sequence,
                originate,
                receive,
                transmit,
            } => {
                if identifier != self.identifier {
                    return Err(IcmpError::WrongIdentifier {
                        expected: self.identifier,
                        found: identifier,
                    });
                }

                if originate >= MS_PER_DAY as u32
                    || receive >= MS_PER_DAY as u32
                    || transmit >= MS_PER_DAY as u32
                {
                    return Err(IcmpError::InvalidPacket(
                        "non-standard RFC 792 timestamp".into(),
                    ));
                }

                let key = (peer, MeasurementSource::IcmpTimestamp, sequence);
                if self
                    .inflight
                    .get(&key)
                    .is_none_or(|p| p.correlation != originate as u64)
                {
                    return Err(IcmpError::NoMatchingProbe(key));
                }
                let probe = self
                    .inflight
                    .remove(&key)
                    .ok_or(IcmpError::NoMatchingProbe(key))?;

                let now_ms = if let Some(timestamp) = kernel_rx_timestamp {
                    ClockSample::<Realtime>::from(timestamp).as_time_since_midnight()
                } else {
                    ClockSample::<Realtime>::now().as_time_since_midnight()
                };

                let uplink_ms = timestamp_delta(receive as i64, originate as i64);
                let downlink_ms = timestamp_delta(now_ms, transmit as i64);

                debug!(
                    "ICMP engine > Type: {:4}  | Reflector IP: {:>15}  | Seq: {:5}  | Current time: {:8}  | Originate: {:8}  | Received time: {:8}  | Transmit time: {:8}  | RTT: {:8.3} ms  | UL time: {:5} ms  | DL time: {:5} ms",
                    "ICMP",
                    peer,
                    sequence,
                    now_ms,
                    originate,
                    receive,
                    transmit,
                    observed_at.duration_since(probe.sent_at).as_secs_f64() * 1_000.0,
                    uplink_ms,
                    downlink_ms,
                );

                Ok(ProbeCompletion {
                    stream: probe.stream,
                    started_at: probe.sent_at,
                    completed_at: observed_at,
                    outcome: MeasurementOutcome::Reply {
                        round_trip: None,
                        one_way: Some(OneWayLatency {
                            uplink: SignedDuration::from_millis(uplink_ms),
                            downlink: SignedDuration::from_millis(downlink_ms),
                            clock: OneWayClock::Uncalibrated,
                        }),
                    },
                })
            }
            _ => Err(IcmpError::InvalidPacket(
                "unsupported ICMP message type".to_string(),
            )),
        }
    }
}
const MS_PER_DAY: i64 = 86_400_000;

fn timestamp_delta(later: i64, earlier: i64) -> i64 {
    let delta = later - earlier;
    if delta < -MS_PER_DAY / 2 {
        delta + MS_PER_DAY
    } else if delta > MS_PER_DAY / 2 {
        delta - MS_PER_DAY
    } else {
        delta
    }
}
