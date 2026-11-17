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
mod time;

use crate::app::App;
use crate::platform::warn_platform_limitations;
use crate::settings::Settings;
use std::process::ExitCode;

const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Name of the instance started without `--instance`.
const DEFAULT_INSTANCE: &str = "default";

#[tokio::main]
async fn main() -> anyhow::Result<ExitCode> {
    // Initialization errors return before any shaper can be modified.
    log::init()?;
    log::message(format_args!("Starting unRAVEL version {}", VERSION));

    let result = async {
        let instance = parse_args(std::env::args().skip(1))?;
        if let Some(name) = &instance {
            log::message(format_args!("Instance {name}"));
        }
        let settings = Settings::load(instance.as_deref())?;
        ::log::set_max_level(settings.output.log_level.to_level_filter());
        warn_platform_limitations();

        let name = instance.as_deref().unwrap_or(DEFAULT_INSTANCE);
        App::prepare(settings, name).await?.run().await
    }
    .await;

    // App::run has already completed shaper restoration and worker cleanup.
    log::finish(result.as_ref().err());
    Ok(if result.is_ok() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// The only argument is `--instance <name>` (or `--instance=<name>`).
fn parse_args(mut args: impl Iterator<Item = String>) -> anyhow::Result<Option<String>> {
    let mut instance = None;
    while let Some(arg) = args.next() {
        let value = match arg.strip_prefix("--instance") {
            Some("") => args.next(),
            Some(rest) => rest.strip_prefix('=').map(str::to_owned),
            None => None,
        };
        let Some(name) = value else {
            anyhow::bail!("Unexpected argument {arg:?}; usage: unravel [--instance <name>]");
        };
        anyhow::ensure!(instance.is_none(), "--instance given more than once");
        anyhow::ensure!(
            !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
            "Instance name {name:?} must be letters, digits and underscores"
        );
        instance = Some(name);
    }
    Ok(instance)
}
