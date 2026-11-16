// SPDX-FileCopyrightText: 2026-Present Nils Andreas Svee mailto:contact@lochnair.net (github @Lochnair)
//
// SPDX-License-Identifier: MPL-2.0

use super::PROTOCOLS;
use crate::{
    measurement::model::{MeasurementProtocol, MeasurementTarget},
    settings::ReflectorDefinition,
};
use log::warn;
use std::{collections::HashMap, net::SocketAddr};

pub(super) struct InventoryTarget {
    pub target: MeasurementTarget,
    pub preference: usize,
    country: Option<String>,
}

impl InventoryTarget {
    pub fn key(&self) -> (SocketAddr, MeasurementProtocol) {
        let endpoint = match &self.target {
            MeasurementTarget::Irtt { endpoint, .. } => *endpoint,
            target => SocketAddr::new(target.peer(), 0),
        };
        (endpoint, self.target.protocol())
    }

    pub fn merge(&mut self, other: Self) -> anyhow::Result<()> {
        let same_country = match (&self.country, &other.country) {
            (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
            _ => true,
        };
        anyhow::ensure!(
            same_country && self.target == other.target,
            "conflicting reflector definitions for {:?}",
            other.target
        );
        if self.country.is_none() {
            self.country = other.country;
        }
        Ok(())
    }
}

pub(super) struct InventoryGroup {
    pub targets: Vec<usize>,
    pub cursor: usize,
}

pub(super) fn groups(
    inventory: &[InventoryTarget],
    rng: &mut fastrand::Rng,
) -> Vec<InventoryGroup> {
    (0..PROTOCOLS.len())
        .map(|preference| {
            let mut targets = inventory
                .iter()
                .enumerate()
                .filter(|(_, entry)| entry.preference == preference)
                .map(|(i, _)| i)
                .collect::<Vec<_>>();
            rng.shuffle(&mut targets);
            InventoryGroup { targets, cursor: 0 }
        })
        .collect()
}

pub(super) async fn discover(
    definitions: &[ReflectorDefinition],
    countries: &[String],
) -> anyhow::Result<Vec<InventoryTarget>> {
    let mut inventory: Vec<InventoryTarget> = Vec::new();
    let mut indices: HashMap<(SocketAddr, MeasurementProtocol), usize> = HashMap::new();
    for definition in definitions {
        if !countries.is_empty()
            && definition.country.as_ref().is_some_and(|country| {
                !countries
                    .iter()
                    .any(|allowed| allowed.eq_ignore_ascii_case(country))
            })
        {
            continue;
        }
        for (preference, &protocol) in PROTOCOLS.iter().enumerate() {
            if !definition.protocols.contains(&protocol) {
                continue;
            }
            let port = if protocol == MeasurementProtocol::Irtt {
                definition.irtt_port.unwrap_or(2112)
            } else {
                0
            };
            let endpoints = match tokio::net::lookup_host((definition.host.as_str(), port)).await {
                Ok(endpoints) => endpoints,
                Err(error) => {
                    warn!("Failed to resolve reflector {}: {error}", definition.host);
                    continue;
                }
            };
            for endpoint in endpoints {
                let target = match (protocol, endpoint.ip()) {
                    (MeasurementProtocol::IcmpEcho, std::net::IpAddr::V4(peer)) => {
                        MeasurementTarget::IcmpEcho(peer)
                    }
                    (MeasurementProtocol::IcmpTimestamps, std::net::IpAddr::V4(peer)) => {
                        MeasurementTarget::IcmpTimestamps(peer)
                    }
                    (MeasurementProtocol::Irtt, _) => MeasurementTarget::Irtt {
                        endpoint,
                        authentication: definition
                            .irtt_hmac
                            .as_deref()
                            .filter(|key| !key.trim().is_empty())
                            .map(Into::into),
                    },
                    _ => continue,
                };
                let entry = InventoryTarget {
                    target,
                    preference,
                    country: definition.country.clone(),
                };
                if let Some(&index) = indices.get(&entry.key()) {
                    inventory[index].merge(entry)?;
                } else {
                    indices.insert(entry.key(), inventory.len());
                    inventory.push(entry);
                }
            }
        }
    }
    Ok(inventory)
}
