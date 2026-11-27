// SPDX-FileCopyrightText: 2026-Present Nils Andreas Svee mailto:contact@lochnair.net (github @Lochnair)
//
// SPDX-License-Identifier: MPL-2.0

use crate::law::{LawKind, sqma::SqmaSettings};
use crate::measurement::model::MeasurementTarget;
use anyhow::Context;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::Path;
mod validation;

pub(crate) use validation::{rules, warning};

pub(crate) fn valid_instance_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ObservabilityProtocol {
    Udp,
    Tcp,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub(crate) enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl From<LogLevel> for log::Level {
    fn from(level: LogLevel) -> Self {
        match level {
            LogLevel::Error => log::Level::Error,
            LogLevel::Warn => log::Level::Warn,
            LogLevel::Info => log::Level::Info,
            LogLevel::Debug => log::Level::Debug,
            LogLevel::Trace => log::Level::Trace,
        }
    }
}

/// The configuration, read from one JSON or TOML file. Values must have their
/// own types: frontends such as the OpenWrt converter convert theirs first.
#[derive(Clone, Debug, Deserialize, garde::Validate, JsonSchema)]
#[serde(deny_unknown_fields)]
#[garde(allow_unvalidated)]
pub(crate) struct Settings {
    #[serde(default)]
    pub(crate) dry_run: bool,

    #[serde(default)]
    pub(crate) law: LawSettings,

    #[garde(dive)]
    pub(crate) network: NetworkSettings,

    #[serde(default)]
    #[garde(dive)]
    pub(crate) reflectors: ReflectorSettings,

    #[serde(default)]
    #[garde(dive)]
    pub(crate) output: OutputSettings,

    #[serde(default)]
    #[garde(dive)]
    pub(crate) observability: ObservabilitySettings,
}

/// `name` picks the law; each law reads its own table. SQMA's table is
/// validated by SQMA, and a script's by the options it declares.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct LawSettings {
    pub(crate) name: LawKind,
    pub(crate) sqma: SqmaSettings,
    /// `path` names the script; the other keys are the script's own options.
    pub(crate) script: serde_json::Map<String, serde_json::Value>,
}

impl Settings {
    pub(crate) fn load(path: &Path) -> anyhow::Result<Self> {
        let extension = path
            .extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_ascii_lowercase);
        let text = || {
            std::fs::read_to_string(path)
                .with_context(|| format!("Reading configuration {}", path.display()))
        };
        let invalid = |error: &dyn std::fmt::Display| {
            anyhow::anyhow!("Invalid configuration {}: {error}", path.display())
        };
        let mut settings: Self = match extension.as_deref() {
            Some("json") => serde_json::from_str(&text()?).map_err(|e| invalid(&e))?,
            #[cfg(feature = "toml")]
            Some("toml") => toml::from_str(&text()?).map_err(|e| invalid(&e))?,
            #[cfg(not(feature = "toml"))]
            Some("toml") => anyhow::bail!(
                "Cannot load {}: this build has no TOML support",
                path.display()
            ),
            _ => anyhow::bail!(
                "Unsupported configuration format for {}: expected .json or .toml",
                path.display()
            ),
        };
        settings.reflectors.load_files()?;
        Ok(settings)
    }

    /// The configuration's JSON Schema, with subschemas inlined. It describes
    /// types, defaults and simple bounds; validation remains authoritative.
    pub(crate) fn json_schema() -> schemars::Schema {
        schemars::generate::SchemaSettings::draft2020_12()
            .with(|settings| settings.inline_subschemas = true)
            .into_generator()
            .into_root_schema_for::<Self>()
    }
}

#[derive(Clone, Debug, Deserialize, garde::Validate, JsonSchema)]
#[serde(deny_unknown_fields)]
#[garde(allow_unvalidated)]
pub(crate) struct NetworkSettings {
    #[garde(custom(rules::nonblank))]
    pub(crate) download_interface: String,
    #[garde(custom(rules::nonblank))]
    pub(crate) upload_interface: String,
    #[serde(default)]
    #[garde(if(cond = self.download_interface().is_some(), range(min = 1.0)))]
    pub(crate) download_base_kbits: f64,
    #[serde(default = "default_min_percent")]
    #[garde(if(cond = self.download_interface().is_some(), range(min = 1.0, max = 100.0)))]
    pub(crate) download_min_percent: f64,
    #[serde(default)]
    #[garde(if(cond = self.upload_interface().is_some(), range(min = 1.0)))]
    pub(crate) upload_base_kbits: f64,
    #[serde(default = "default_min_percent")]
    #[garde(if(cond = self.upload_interface().is_some(), range(min = 1.0, max = 100.0)))]
    pub(crate) upload_min_percent: f64,
    /// The highest rate a law may set, as a percentage of base. Unset means
    /// no maximum.
    #[garde(if(cond = self.download_interface().is_some(), range(min = 100.0)))]
    pub(crate) download_max_percent: Option<f64>,
    #[garde(if(cond = self.upload_interface().is_some(), range(min = 100.0)))]
    pub(crate) upload_max_percent: Option<f64>,

    #[allow(dead_code)]
    pub(crate) measurement_bind_device: Option<String>,
    #[allow(dead_code)]
    pub(crate) measurement_mark: Option<u32>,
    #[allow(dead_code)]
    pub(crate) measurement_fib: Option<u32>,
}

impl NetworkSettings {
    // Keep interface strings in the configuration schema; reserve `none` in both
    // TOML and UCI, which does not reliably preserve empty string options.
    pub(crate) fn download_interface(&self) -> Option<&str> {
        (self.download_interface != "none").then_some(self.download_interface.as_str())
    }

    pub(crate) fn upload_interface(&self) -> Option<&str> {
        (self.upload_interface != "none").then_some(self.upload_interface.as_str())
    }

    pub(crate) fn traffic_interface(&self) -> Option<&str> {
        self.upload_interface()
            .or_else(|| self.download_interface())
    }

    pub(crate) fn download_min_kbits(&self) -> f64 {
        (self.download_base_kbits * self.download_min_percent / 100.0).floor()
    }

    pub(crate) fn upload_min_kbits(&self) -> f64 {
        (self.upload_base_kbits * self.upload_min_percent / 100.0).floor()
    }

    pub(crate) fn download_max_kbits(&self) -> Option<f64> {
        self.download_max_percent
            .map(|percent| (self.download_base_kbits * percent / 100.0).floor())
    }

    pub(crate) fn upload_max_kbits(&self) -> Option<f64> {
        self.upload_max_percent
            .map(|percent| (self.upload_base_kbits * percent / 100.0).floor())
    }
}

#[derive(Clone, Debug, Deserialize, garde::Validate, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[garde(allow_unvalidated)]
pub(crate) struct ReflectorSettings {
    /// JSON pools, each an array of measurement targets.
    pub(crate) files: Vec<String>,

    /// Inline targets, joined by the pool files' targets once loaded.
    pub(crate) entries: Vec<MeasurementTarget>,

    #[garde(range(min = 1))]
    pub(crate) num_reflectors: u8,
    pub(crate) num_candidates: usize,
    #[garde(custom(rules::timer_period))]
    pub(crate) candidate_period: f64,
}

impl ReflectorSettings {
    fn load_files(&mut self) -> anyhow::Result<()> {
        for path in &self.files {
            let pool =
                std::fs::read(path).with_context(|| format!("Reading reflector pool {path}"))?;
            let targets: Vec<MeasurementTarget> = serde_json::from_slice(&pool)
                .with_context(|| format!("Parsing reflector pool {path}"))?;
            self.entries.extend(targets);
        }
        Ok(())
    }
}

impl Default for ReflectorSettings {
    fn default() -> Self {
        Self {
            files: Vec::new(),
            entries: Vec::new(),
            num_reflectors: 5,
            num_candidates: 3,
            candidate_period: 3.0,
        }
    }
}

#[derive(Clone, Debug, Deserialize, garde::Validate, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[garde(allow_unvalidated)]
pub(crate) struct OutputSettings {
    pub(crate) log_level: LogLevel,
    /// Holds a folder per instance, with the CSV statistics and each law's
    /// saved state.
    #[garde(custom(rules::nonblank))]
    pub(crate) folder: String,
    pub(crate) suppress_statistics: bool,
}

impl Default for OutputSettings {
    fn default() -> Self {
        Self {
            log_level: LogLevel::Info,
            folder: "/tmp/unravel".to_owned(),
            suppress_statistics: false,
        }
    }
}

#[derive(Clone, Debug, Deserialize, garde::Validate, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[garde(allow_unvalidated)]
pub(crate) struct ObservabilitySettings {
    pub(crate) enabled: bool,
    pub(crate) protocol: ObservabilityProtocol,
    #[garde(if(cond = self.enabled, required, inner(custom(rules::nonblank))))]
    pub(crate) host: Option<String>,
    #[garde(if(cond = self.enabled, range(min = 1)))]
    pub(crate) port: u16,
    #[garde(if(cond = self.enabled, range(min = 1)))]
    pub(crate) batch_size: usize,
    #[garde(if(cond = self.enabled, range(min = 1)))]
    pub(crate) batch_timeout_ms: u64,
    pub(crate) export_ping_metrics: bool,
    pub(crate) export_rate_metrics: bool,
    pub(crate) export_law_metrics: bool,
    pub(crate) export_events: bool,
    pub(crate) host_tag: String,
}

impl Default for ObservabilitySettings {
    fn default() -> Self {
        Self {
            enabled: false,
            protocol: ObservabilityProtocol::Udp,
            host: None,
            port: 8089,
            batch_size: 25,
            batch_timeout_ms: 100,
            export_ping_metrics: false,
            export_rate_metrics: true,
            export_law_metrics: false,
            export_events: true,
            host_tag: default_host_tag(),
        }
    }
}

fn default_min_percent() -> f64 {
    20.0
}

fn default_host_tag() -> String {
    rustix::system::uname()
        .nodename()
        .to_string_lossy()
        .into_owned()
}
