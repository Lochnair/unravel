// SPDX-FileCopyrightText: 2026-Present Nils Andreas Svee mailto:contact@lochnair.net (github @Lochnair)
//
// SPDX-License-Identifier: MPL-2.0

use crate::law::LawKind;
use crate::measurement::model::MeasurementTarget;
use config::{Config, ConfigError, Environment};
use log::Level;
use serde::{Deserialize, Deserializer, de};
use std::fmt::Display;
use std::str::FromStr;
use uci_derive::{UciConfig, UciSection};

#[cfg(all(feature = "uci", unix))]
use crate::settings::uci::UciSource;

#[cfg(all(feature = "uci", unix))]
pub mod uci;

#[cfg(all(feature = "uci", unix))]
pub(crate) mod uci_schema;
mod validation;

pub(crate) use validation::{
    ValidationReport, validate_greater_than_zero_f64, validate_zero_or_greater_f64,
};

pub(crate) fn valid_instance_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

struct FlexiBool(bool);

impl FromStr for FlexiBool {
    type Err = ConfigError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_lowercase().as_str() {
            "1" | "true" | "yes" | "y" | "on" | "enabled" => Ok(FlexiBool(true)),
            "0" | "false" | "no" | "n" | "off" | "disabled" => Ok(FlexiBool(false)),
            _ => Err(ConfigError::Message(format!(
                "invalid boolean string: {}",
                s
            ))),
        }
    }
}

pub(crate) fn deserialize_flex_bool<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: Deserializer<'de>,
{
    struct FlexiBoolVisitor;

    impl<'de> de::Visitor<'de> for FlexiBoolVisitor {
        type Value = bool;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a boolean, integer 0 or 1, or a UCI-compatible boolean string")
        }

        fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(value)
        }

        fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            match value {
                0 => Ok(false),
                1 => Ok(true),
                _ => Err(E::custom("boolean integer must be 0 or 1")),
            }
        }

        fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            match value {
                0 => Ok(false),
                1 => Ok(true),
                _ => Err(E::custom("boolean integer must be 0 or 1")),
            }
        }

        fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            value
                .parse::<FlexiBool>()
                .map(|value| value.0)
                .map_err(E::custom)
        }

        fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            self.visit_str(&value)
        }
    }

    deserializer.deserialize_any(FlexiBoolVisitor)
}

#[derive(Clone, Copy, Debug)]
pub enum ObservabilityProtocol {
    Udp,
    Tcp,
}

impl FromStr for ObservabilityProtocol {
    type Err = ConfigError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "udp" => Ok(ObservabilityProtocol::Udp),
            "tcp" => Ok(ObservabilityProtocol::Tcp),
            _ => Err(ConfigError::Message(format!(
                "invalid observability protocol: {s}",
            ))),
        }
    }
}

#[derive(Clone, Debug, Deserialize, UciConfig)]
#[uci(package = "unravel")]
pub(crate) struct Settings {
    #[serde(default, deserialize_with = "deserialize_flex_bool")]
    #[uci(instance_option)]
    pub(crate) dry_run: bool,

    #[serde(default)]
    #[uci(section)]
    pub(crate) law: LawSettings,

    #[uci(section = "instance", instance)]
    pub(crate) network: NetworkSettings,

    #[serde(default)]
    #[uci(section)]
    pub(crate) reflectors: ReflectorSettings,

    #[serde(default)]
    #[uci(section)]
    pub(crate) output: OutputSettings,

    #[serde(default)]
    #[uci(section)]
    pub(crate) observability: ObservabilitySettings,
}

// In UCI the law is chosen with `option type`, and the rest of the `law`
// section is that law's own configuration.
#[derive(Clone, Debug, Deserialize, UciSection)]
#[serde(default)]
#[uci(passthrough_key = "name")]
pub(crate) struct LawSettings {
    #[uci(rename = "type")]
    #[serde(deserialize_with = "deserialize_from_str")]
    pub(crate) name: LawKind,

    /// The selected law's own table, `law.<name>`, left for the law to read.
    #[serde(skip)]
    #[uci(skip)]
    pub(crate) config: Option<config::Value>,
}

impl Default for LawSettings {
    fn default() -> Self {
        Self {
            name: LawKind::Sqma,
            config: None,
        }
    }
}

impl Settings {
    /// Loads UCI, TOML and environment settings for the named instance.
    pub(crate) fn load(instance: &str) -> Result<Self, ConfigError> {
        Self::load_from(
            instance,
            std::path::Path::new("/etc/config"),
            std::path::Path::new("/etc/unravel"),
            Environment::with_prefix("UNRAVEL"),
        )
    }

    fn load_from(
        _instance: &str,
        _uci_dir: &std::path::Path,
        _toml_dir: &std::path::Path,
        environment: Environment,
    ) -> Result<Self, ConfigError> {
        let builder = Config::builder();

        #[cfg(all(feature = "uci", unix))]
        let builder = builder.add_source(UciSource::from_schema::<Self>(_instance, _uci_dir));

        #[cfg(feature = "toml")]
        let builder = builder.add_source(
            config::File::from(_toml_dir.join(format!("{_instance}.toml"))).required(false),
        );

        let builder = builder.add_source(
            environment
                .separator("__")
                .try_parsing(true)
                .list_separator(",")
                .with_list_parse_key("reflectors.files"),
        );
        Self::from_config(builder.build()?)
    }

    fn from_config(config: Config) -> Result<Self, ConfigError> {
        let mut settings: Self = config.clone().try_deserialize()?;

        settings.law.config = match config.get(&format!("law.{}", settings.law.name.as_str())) {
            Ok(value) => Some(value),
            Err(ConfigError::NotFound(_)) => None,
            Err(error) => return Err(error),
        };
        settings.reflectors.load_files()?;

        Ok(settings)
    }
}

// `enabled` is read by the OpenWrt init script, which starts only the
// instances that are not disabled.
#[derive(Clone, Debug, Deserialize, UciSection)]
#[uci(external = "enabled")]
pub(crate) struct NetworkSettings {
    pub(crate) download_interface: String,
    pub(crate) upload_interface: String,
    #[serde(default)]
    pub(crate) download_base_kbits: f64,
    #[serde(default = "default_min_percent")]
    pub(crate) download_min_percent: f64,
    #[serde(default)]
    pub(crate) upload_base_kbits: f64,
    #[serde(default = "default_min_percent")]
    pub(crate) upload_min_percent: f64,
    /// The highest rate a law may set, as a percentage of base. Unset means
    /// no maximum.
    pub(crate) download_max_percent: Option<f64>,
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

#[derive(Clone, Debug, Deserialize, UciSection)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct ReflectorSettings {
    /// JSON pools, each an array of measurement targets.
    #[serde(deserialize_with = "deserialize_reflector_files")]
    pub(crate) files: Vec<String>,

    /// Inline targets, joined by the pool files' targets once loaded.
    #[uci(sections = "reflector")]
    pub(crate) entries: Vec<MeasurementTarget>,

    pub(crate) num_reflectors: u8,
    pub(crate) num_candidates: usize,
    pub(crate) candidate_period: f64,
}

impl ReflectorSettings {
    fn load_files(&mut self) -> Result<(), ConfigError> {
        for path in &self.files {
            let pool = std::fs::read(path).map_err(|error| {
                ConfigError::Message(format!("reading reflector pool {path}: {error}"))
            })?;
            let targets: Vec<MeasurementTarget> =
                serde_json::from_slice(&pool).map_err(|error| {
                    ConfigError::Message(format!("parsing reflector pool {path}: {error}"))
                })?;
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

// Each `config reflector` section is one target; serde enforces which
// options its protocol takes.
#[cfg(all(feature = "uci", unix))]
impl uci_schema::UciSectionSchema for MeasurementTarget {
    const UCI_OPTIONS: &'static [uci_schema::UciOptionMapping] = &[
        uci_schema::UciOptionMapping {
            uci_option: "protocol",
            config_field: "protocol",
        },
        uci_schema::UciOptionMapping {
            uci_option: "endpoint",
            config_field: "endpoint",
        },
        uci_schema::UciOptionMapping {
            uci_option: "authentication",
            config_field: "authentication",
        },
    ];
    const UCI_EXTERNAL_OPTIONS: &'static [&'static str] = &[];
    const UCI_PASSTHROUGH_KEY: Option<&'static str> = None;
    const UCI_REPEATED: &'static [uci_schema::UciRepeatedSectionMapping] = &[];
}

// `use_syslog` is read by the OpenWrt init script, which uses it to decide
// whether procd sends stdout and stderr to syslog.
#[derive(Clone, Debug, Deserialize, UciSection)]
#[serde(default)]
#[uci(external = "use_syslog")]
pub(crate) struct OutputSettings {
    #[serde(deserialize_with = "deserialize_from_str")]
    pub(crate) log_level: Level,
    /// Holds a folder per instance, with the CSV statistics and each law's
    /// saved state.
    pub(crate) folder: String,
    #[serde(deserialize_with = "deserialize_flex_bool")]
    pub(crate) suppress_statistics: bool,
}

impl Default for OutputSettings {
    fn default() -> Self {
        Self {
            log_level: Level::Info,
            folder: "/tmp/unravel".to_owned(),
            suppress_statistics: false,
        }
    }
}

#[derive(Clone, Debug, Deserialize, UciSection)]
#[serde(default)]
pub(crate) struct ObservabilitySettings {
    #[serde(deserialize_with = "deserialize_flex_bool")]
    pub(crate) enabled: bool,
    #[serde(deserialize_with = "deserialize_from_str")]
    pub(crate) protocol: ObservabilityProtocol,
    pub(crate) host: Option<String>,
    pub(crate) port: u16,
    pub(crate) batch_size: usize,
    pub(crate) batch_timeout_ms: u64,
    #[serde(deserialize_with = "deserialize_flex_bool")]
    pub(crate) export_ping_metrics: bool,
    #[serde(deserialize_with = "deserialize_flex_bool")]
    pub(crate) export_rate_metrics: bool,
    #[serde(deserialize_with = "deserialize_flex_bool")]
    pub(crate) export_law_metrics: bool,
    #[serde(deserialize_with = "deserialize_flex_bool")]
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

fn deserialize_from_str<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: FromStr,
    T::Err: Display,
{
    let value = String::deserialize(deserializer)?;
    value.parse().map_err(de::Error::custom)
}

// UCI drops empty string options; `option files 'none'` disables file loading.
fn deserialize_reflector_files<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Files {
        List(Vec<String>),
        String(String),
    }

    match Files::deserialize(deserializer)? {
        Files::List(files) => Ok(files),
        Files::String(value) if value.trim().is_empty() || value.trim() == "none" => Ok(Vec::new()),
        Files::String(_) => Err(de::Error::custom(
            "reflector files must be a list, an empty string, or none",
        )),
    }
}
