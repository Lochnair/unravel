// SPDX-FileCopyrightText: 2026-Present Nils Andreas Svee mailto:contact@lochnair.net (github @Lochnair)
//
// SPDX-License-Identifier: MPL-2.0

#[cfg(feature = "irtt")]
use crate::measurement::model::MeasurementTarget;
use std::fmt::Display;
#[cfg(feature = "irtt")]
use std::net::IpAddr;

#[cfg(feature = "irtt")]
use super::ReflectorSettings;
use super::{NetworkSettings, ObservabilitySettings, Settings};

pub(crate) mod rules;

const WARN_DOWNLOAD_MIN_KBITS: f64 = 1_000.0;
const WARN_UPLOAD_MIN_KBITS: f64 = 500.0;
const WARN_HIGH_MIN_PERCENT: f64 = 80.0;

use garde::{Path, Report, Validate};

pub(crate) fn warning(path: &str, message: impl Display) {
    crate::log::message(format_args!("warning: {path}: {message}"));
}

impl Settings {
    pub(crate) fn validate(&self) -> anyhow::Result<()> {
        let mut report = Report::new();
        self.validate_into(&(), &mut Path::empty, &mut report);
        self.network.validate_rates(&mut report);
        #[cfg(feature = "irtt")]
        self.reflectors.validate_endpoints(&mut report);
        self.observability.warn_if_exports_disabled();
        if report.is_empty() {
            Ok(())
        } else {
            Err(anyhow::Error::new(report).context("settings validation failed"))
        }
    }
}

impl NetworkSettings {
    fn validate_rates(&self, report: &mut Report) {
        if self.traffic_interface().is_none() {
            report.append(
                Path::new("network"),
                garde::Error::new("at least one shaping interface must be configured"),
            );
        }

        let download_base_valid = self.download_interface().is_some()
            && garde::select!(report, network.download_base_kbits)
                .next()
                .is_none();
        let download_percent_valid = self.download_interface().is_some()
            && garde::select!(report, network.download_min_percent)
                .next()
                .is_none();
        let upload_base_valid = self.upload_interface().is_some()
            && garde::select!(report, network.upload_base_kbits)
                .next()
                .is_none();
        let upload_percent_valid = self.upload_interface().is_some()
            && garde::select!(report, network.upload_min_percent)
                .next()
                .is_none();

        if download_percent_valid && self.download_min_percent > WARN_HIGH_MIN_PERCENT {
            warning(
                "network.download_min_percent",
                format!(
                    "{}% leaves little room for the shaper to reduce the download rate",
                    self.download_min_percent,
                ),
            );
        }

        if upload_percent_valid && self.upload_min_percent > WARN_HIGH_MIN_PERCENT {
            warning(
                "network.upload_min_percent",
                format!(
                    "{}% leaves little room for the shaper to reduce the upload rate",
                    self.upload_min_percent,
                ),
            );
        }

        if download_base_valid && download_percent_valid {
            let minimum = self.download_min_kbits();

            if !(1.0..=f64::MAX).contains(&minimum) {
                report.append(
                    Path::new("network").join("download_min_percent"),
                    garde::Error::new(
                        "derived download minimum must be at least 1 kbit/s and finite",
                    ),
                );
            } else if minimum < WARN_DOWNLOAD_MIN_KBITS {
                warning(
                    "network.download_min_percent",
                    format!(
                        "results in a minimum download rate of only {minimum:.0} kbit/s \
                         ({:.1}% of {:.0} kbit/s); connectivity may become severely constrained",
                        self.download_min_percent, self.download_base_kbits,
                    ),
                );
            }
        }

        if upload_base_valid && upload_percent_valid {
            let minimum = self.upload_min_kbits();

            if !(1.0..=f64::MAX).contains(&minimum) {
                report.append(
                    Path::new("network").join("upload_min_percent"),
                    garde::Error::new(
                        "derived upload minimum must be at least 1 kbit/s and finite",
                    ),
                );
            } else if minimum < WARN_UPLOAD_MIN_KBITS {
                warning(
                    "network.upload_min_percent",
                    format!(
                        "results in a minimum upload rate of only {minimum:.0} kbit/s \
                         ({:.1}% of {:.0} kbit/s); connectivity may become severely constrained",
                        self.upload_min_percent, self.upload_base_kbits,
                    ),
                );
            }
        }
    }
}

#[cfg(feature = "irtt")]
impl ReflectorSettings {
    fn validate_endpoints(&self, report: &mut Report) {
        // The inventory owns duplicates and conflicts; these are per target.
        // Builds without IRTT skip its targets, so they go unchecked there.
        for (index, target) in self.entries.iter().enumerate() {
            let MeasurementTarget::Irtt { endpoint, .. } = target else {
                continue;
            };
            if endpoint.port() == 0 {
                report.append(
                    Path::new("reflectors").join("entries").join(index),
                    garde::Error::new(format!("{target:?} must have a nonzero port")),
                );
            }
            if let IpAddr::V6(ip) = endpoint.ip()
                && ip.to_ipv4_mapped().is_some()
            {
                report.append(
                    Path::new("reflectors").join("entries").join(index),
                    garde::Error::new(format!("{target:?} must use its plain IPv4 address")),
                );
            }
        }
    }
}

impl ObservabilitySettings {
    fn warn_if_exports_disabled(&self) {
        if self.enabled
            && !self.export_ping_metrics
            && !self.export_rate_metrics
            && !self.export_law_metrics
            && !self.export_events
        {
            warning(
                "observability.enabled",
                "observability is enabled but all metric and event exports are disabled",
            );
        }
    }
}