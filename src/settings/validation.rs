// SPDX-FileCopyrightText: 2026-Present Nils Andreas Svee mailto:contact@lochnair.net (github @Lochnair)
//
// SPDX-License-Identifier: MPL-2.0

use crate::settings::ReflectorSettings;

use super::{NetworkSettings, ObservabilitySettings, OutputSettings, Settings};
use std::error::Error;
use std::fmt::{self, Display, Formatter};

const WARN_DOWNLOAD_MIN_KBITS: f64 = 1_000.0;
const WARN_UPLOAD_MIN_KBITS: f64 = 500.0;
const WARN_HIGH_MIN_PERCENT: f64 = 80.0;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ValidationSeverity {
    Warning,
    Error,
}

#[derive(Debug)]
struct ValidationIssue {
    severity: ValidationSeverity,
    path: &'static str,
    message: String,
}

#[derive(Debug, Default)]
pub(crate) struct ValidationReport {
    issues: Vec<ValidationIssue>,
}

impl ValidationReport {
    pub(crate) fn warning(&mut self, path: &'static str, message: impl Into<String>) {
        self.issues.push(ValidationIssue {
            severity: ValidationSeverity::Warning,
            path,
            message: message.into(),
        });
    }

    pub(crate) fn error(&mut self, path: &'static str, message: impl Into<String>) {
        self.issues.push(ValidationIssue {
            severity: ValidationSeverity::Error,
            path,
            message: message.into(),
        });
    }

    pub(crate) fn finish(self) -> Result<(), Self> {
        let mut errors = Vec::new();

        for issue in self.issues {
            match issue.severity {
                ValidationSeverity::Warning => {
                    crate::log::message(format_args!("warning: {}: {}", issue.path, issue.message));
                }

                ValidationSeverity::Error => {
                    errors.push(issue);
                }
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(Self { issues: errors })
        }
    }
}

impl Display for ValidationReport {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "settings validation failed:")?;

        for issue in &self.issues {
            write!(f, "\n  {}: {}", issue.path, issue.message)?;
        }

        Ok(())
    }
}

impl Error for ValidationReport {}

impl Settings {
    pub(crate) fn validate(&self) -> Result<(), ValidationReport> {
        let mut report = ValidationReport::default();

        self.network.validate(&mut report);
        self.reflectors.validate(&mut report);
        self.output.validate(&mut report);
        self.observability.validate(&mut report);

        report.finish()
    }
}

impl NetworkSettings {
    fn validate(&self, report: &mut ValidationReport) {
        if self.download_interface.trim().is_empty() {
            report.error("network.download_interface", "must not be empty");
        }

        if self.upload_interface.trim().is_empty() {
            report.error("network.upload_interface", "must not be empty");
        }

        if self.traffic_interface().is_none() {
            report.error(
                "network",
                "at least one shaping interface must be configured (both are none)",
            );
        }

        let download_base_valid = self.download_interface().is_some()
            && validate_greater_than_zero_f64(
                report,
                "network.download_base_kbits",
                self.download_base_kbits,
            );

        let upload_base_valid = self.upload_interface().is_some()
            && validate_greater_than_zero_f64(
                report,
                "network.upload_base_kbits",
                self.upload_base_kbits,
            );

        let download_percent_valid = self.download_interface().is_some()
            && validate_percent(
                report,
                "network.download_min_percent",
                self.download_min_percent,
            );

        let upload_percent_valid = self.upload_interface().is_some()
            && validate_percent(
                report,
                "network.upload_min_percent",
                self.upload_min_percent,
            );

        if download_percent_valid && self.download_min_percent > WARN_HIGH_MIN_PERCENT {
            report.warning(
                "network.download_min_percent",
                format!(
                    "{}% leaves little room for the shaper to reduce the download rate",
                    self.download_min_percent,
                ),
            );
        }

        if upload_percent_valid && self.upload_min_percent > WARN_HIGH_MIN_PERCENT {
            report.warning(
                "network.upload_min_percent",
                format!(
                    "{}% leaves little room for the shaper to reduce the upload rate",
                    self.upload_min_percent,
                ),
            );
        }

        if download_base_valid && download_percent_valid {
            let minimum = self.download_min_kbits();

            if minimum < 1.0 {
                report.error(
                    "network.download_min_percent",
                    "minimum download rate floors to zero",
                );
            } else if minimum < WARN_DOWNLOAD_MIN_KBITS {
                report.warning(
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

            if minimum < 1.0 {
                report.error(
                    "network.upload_min_percent",
                    "minimum upload rate floors to zero",
                );
            } else if minimum < WARN_UPLOAD_MIN_KBITS {
                report.warning(
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

impl ReflectorSettings {
    fn validate(&self, report: &mut ValidationReport) {
        if self.num_reflectors == 0 {
            report.error("reflectors.num_reflectors", "must be greater than 0");
        }
        // Switching needs two candidates qualified at once (see the manager's
        // MIN_SWITCH_CANDIDATES); one slot can replace failed reflectors of the
        // current class but never change class.
        if self.num_candidates == 1 && self.num_reflectors >= 2 {
            report.warning(
                "reflectors.num_candidates",
                "a single candidate cannot switch between directional and RTT-only \
                 measurement; use 0 for a static set or at least 2",
            );
        }

        if !std::time::Duration::try_from_secs_f64(self.candidate_period).is_ok_and(|period| {
            !period.is_zero() && std::time::Instant::now().checked_add(period).is_some()
        }) {
            report.error(
                "reflectors.candidate_period",
                "must be a nonzero representable duration",
            );
        }
        if self.files.is_empty() && self.entries.is_empty() && self.bootstrap.is_empty() {
            report.error(
                "reflectors",
                "at least one reflector file, inline entry, or bootstrap entry is required",
            );
        }

        for (index, reflector) in self.entries.iter().enumerate() {
            if reflector.host.trim().is_empty() {
                report.error(
                    "reflectors.entries",
                    format!("entry {index} must have a nonempty host"),
                );
            }
            if reflector.protocols.is_empty() {
                report.error(
                    "reflectors.entries",
                    format!("entry {index} must have at least one measurement protocol"),
                );
            }
        }

        for (index, file) in self.files.iter().enumerate() {
            if file.trim().is_empty() {
                report.error(
                    "reflectors.files",
                    format!("entry {index} must not be empty"),
                );
            }
        }

        for (index, country) in self.countries.iter().enumerate() {
            if country.len() != 2 || !country.bytes().all(|c| c.is_ascii_alphabetic()) {
                report.error(
                    "reflectors.countries",
                    format!(
                        "entry {index} ({country:?}) must be an ISO 3166-1 alpha-2 country code"
                    ),
                );
            }
        }
    }
}

impl OutputSettings {
    fn validate(&self, report: &mut ValidationReport) {
        if self.suppress_statistics {
            return;
        }

        let speed_hist_file = self.speed_hist_file.trim();
        let stats_file = self.stats_file.trim();

        if speed_hist_file.is_empty() {
            report.error(
                "output.speed_hist_file",
                "must not be empty when statistics are enabled",
            );
        }

        if stats_file.is_empty() {
            report.error(
                "output.stats_file",
                "must not be empty when statistics are enabled",
            );
        }

        if !speed_hist_file.is_empty() && !stats_file.is_empty() && speed_hist_file == stats_file {
            report.error(
                "output.stats_file",
                "must not refer to the same file as output.speed_hist_file",
            );
        }
    }
}

impl ObservabilitySettings {
    fn validate(&self, report: &mut ValidationReport) {
        if !self.enabled {
            return;
        }

        if self
            .host
            .as_deref()
            .is_none_or(|host| host.trim().is_empty())
        {
            report.error(
                "observability.host",
                "must be set when observability is enabled",
            );
        }

        if self.port == 0 {
            report.error("observability.port", "must be greater than 0");
        }

        if self.batch_size == 0 {
            report.error("observability.batch_size", "must be greater than 0");
        }

        if self.batch_timeout_ms == 0 {
            report.error("observability.batch_timeout_ms", "must be greater than 0");
        }

        if !self.export_ping_metrics
            && !self.export_rate_metrics
            && !self.export_baseline_metrics
            && !self.export_events
        {
            report.warning(
                "observability.enabled",
                "observability is enabled but all metric and event exports are disabled",
            );
        }
    }
}

pub(crate) fn validate_greater_than_zero_f64(
    report: &mut ValidationReport,
    path: &'static str,
    value: f64,
) -> bool {
    if !value.is_finite() {
        report.error(path, format!("must be finite, got {value}"));
        false
    } else if value <= 0.0 {
        report.error(path, format!("must be greater than 0, got {value}"));
        false
    } else {
        true
    }
}

pub(crate) fn validate_zero_or_greater_f64(
    report: &mut ValidationReport,
    path: &'static str,
    value: f64,
) -> bool {
    if !value.is_finite() {
        report.error(path, format!("must be finite, got {value}"));
        false
    } else if value < 0.0 {
        report.error(path, format!("must be 0 or greater, got {value}"));
        false
    } else {
        true
    }
}

fn validate_percent(report: &mut ValidationReport, path: &'static str, value: f64) -> bool {
    if !value.is_finite() {
        report.error(path, format!("must be finite, got {value}"));
        false
    } else if !(1.0..=100.0).contains(&value) {
        report.error(path, format!("must be between 1 and 100, got {value}"));
        false
    } else {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn network(download_base_kbits: f64, upload_base_kbits: f64) -> NetworkSettings {
        NetworkSettings {
            download_interface: "ifb0".into(),
            upload_interface: "eth0".into(),
            download_base_kbits,
            download_min_percent: 1.0,
            upload_base_kbits,
            upload_min_percent: 1.0,
            measurement_bind_device: None,
            measurement_mark: None,
            measurement_fib: None,
        }
    }

    #[test]
    fn minimum_rates_that_floor_to_zero_are_errors_for_both_directions() {
        let mut report = ValidationReport::default();
        network(50.0, 50.0).validate(&mut report);
        let error = report.finish().unwrap_err().to_string();

        assert!(error.contains("network.download_min_percent"));
        assert!(error.contains("network.upload_min_percent"));
        assert!(error.contains("floors to zero"));
    }

    #[test]
    fn representable_low_minimum_rates_remain_warnings() {
        let mut report = ValidationReport::default();
        network(10_000.0, 10_000.0).validate(&mut report);

        assert!(report.finish().is_ok());
    }
}
