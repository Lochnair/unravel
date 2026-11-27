// SPDX-FileCopyrightText: 2022-Present Charles Corrigan mailto:chas-iot@runegate.org (github @chas-iot)
// SPDX-FileCopyrightText: 2022-Present Daniel Lakeland mailto:dlakelan@street-artists.org (github @dlakelan)
// SPDX-FileCopyrightText: 2022-Present Mark Baker mailto:mark@vpost.net (github @Fail-Safe)
// SPDX-FileCopyrightText: 2022-Present Nils Andreas Svee mailto:contact@lochnair.net (github @Lochnair)
//
// SPDX-License-Identifier: MPL-2.0

extern crate core;

mod app;
mod diagnostics;
mod error;
mod law;
mod log;
mod measurement;
mod metrics;
mod platform;
mod reflector_manager;
mod runtime;
mod settings;
#[cfg(all(feature = "ubus", unix))]
mod status;
mod time;

use crate::app::{App, ValidatedConfig};
use crate::platform::warn_platform_limitations;
use crate::settings::{Settings, valid_instance_name};
use std::{path::PathBuf, process::ExitCode, time::Duration};

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// How long exit waits for blocking work (file writes, DNS lookups) still
/// running after the app has finished. Shapers are restored before this.
const RUNTIME_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);

fn main() -> anyhow::Result<ExitCode> {
    let (instance, config) = match parse_args(std::env::args().skip(1))? {
        Mode::Run {
            instance,
            config,
            check,
        } => {
            if check {
                ValidatedConfig::new(Settings::load(&config)?)?;
                return Ok(ExitCode::SUCCESS);
            }
            (instance, config)
        }
        Mode::ConfigSchema(script) => {
            println!("{}", law::config_schema_json(script.as_deref())?);
            return Ok(ExitCode::SUCCESS);
        }
    };
    log::init(&instance)?;
    log::message(format_args!("Starting unRAVEL version {}", VERSION));

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(async {
        let settings = Settings::load(&config)?;
        ::log::set_max_level(::log::Level::from(settings.output.log_level).to_level_filter());
        warn_platform_limitations();

        App::prepare(ValidatedConfig::new(settings)?, &instance)
            .await?
            .run()
            .await
    });
    // Dropping the runtime would wait forever for blocking work; anything
    // still stuck after the timeout is left behind.
    runtime.shutdown_timeout(RUNTIME_SHUTDOWN_TIMEOUT);

    // App::run has already completed shaper restoration and worker cleanup.
    log::finish(result.as_ref().err());
    Ok(if result.is_ok() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

const USAGE: &str = "usage: unravel --instance <name> --config <path> [--check-config] | unravel --config-schema [--script <path>]";

enum Mode {
    /// Run the named instance.
    Run {
        instance: String,
        config: PathBuf,
        check: bool,
    },
    /// Print the configuration schema, with a script's options if given, and exit.
    ConfigSchema(Option<PathBuf>),
}

/// Option values can also follow an `=`.
fn parse_args(mut args: impl Iterator<Item = String>) -> anyhow::Result<Mode> {
    let mut instance = None;
    let mut script = None;
    let mut config = None;
    let mut check = false;
    let mut config_schema = false;
    while let Some(arg) = args.next() {
        if arg == "--config-schema" {
            anyhow::ensure!(!config_schema, "--config-schema given more than once");
            config_schema = true;
            continue;
        }
        if arg == "--check-config" {
            anyhow::ensure!(!check, "--check-config given more than once");
            check = true;
            continue;
        }
        let (flag, slot) = if arg.starts_with("--instance") {
            ("--instance", &mut instance)
        } else if arg.starts_with("--script") {
            ("--script", &mut script)
        } else if arg.starts_with("--config") {
            ("--config", &mut config)
        } else {
            anyhow::bail!("Unexpected argument {arg:?}; {USAGE}");
        };
        let value = match &arg[flag.len()..] {
            "" => args.next(),
            rest => rest.strip_prefix('=').map(str::to_owned),
        };
        let Some(value) = value.filter(|value| !value.is_empty()) else {
            anyhow::bail!("Unexpected argument {arg:?}; {USAGE}");
        };
        anyhow::ensure!(slot.is_none(), "{flag} given more than once");
        *slot = Some(value);
    }

    match (instance, script, config_schema, config, check) {
        (None, script, true, None, false) => Ok(Mode::ConfigSchema(script.map(PathBuf::from))),
        (Some(name), None, false, Some(config), check) => {
            anyhow::ensure!(
                valid_instance_name(&name),
                "Instance name {name:?} must be letters, digits and underscores"
            );
            Ok(Mode::Run {
                instance: name,
                config: PathBuf::from(config),
                check,
            })
        }
        (None, None, false, _, _) => anyhow::bail!("Missing required --instance <name>; {USAGE}"),
        (Some(_), None, false, None, _) => {
            anyhow::bail!("Missing required --config <path>; {USAGE}")
        }
        _ => anyhow::bail!("Invalid argument combination; {USAGE}"),
    }
}
