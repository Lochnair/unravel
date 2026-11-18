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
use std::{process::ExitCode, time::Duration};

const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Name of the instance started without `--instance`.
const DEFAULT_INSTANCE: &str = "default";

/// How long exit waits for blocking work (file writes, DNS lookups) still
/// running after the app has finished. Shapers are restored before this.
const RUNTIME_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);

fn main() -> anyhow::Result<ExitCode> {
    // Initialization errors return before any shaper can be modified. The
    // arguments are parsed first so every log line carries the instance; a
    // parse error is reported once the logger is up.
    let instance = parse_args(std::env::args().skip(1));
    log::init(instance.as_ref().ok().and_then(Option::as_deref))?;
    log::message(format_args!("Starting unRAVEL version {}", VERSION));

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(async {
        let instance = instance?;
        let settings = Settings::load(instance.as_deref())?;
        ::log::set_max_level(settings.output.log_level.to_level_filter());
        warn_platform_limitations();

        let name = instance.as_deref().unwrap_or(DEFAULT_INSTANCE);
        App::prepare(settings, name).await?.run().await
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
        anyhow::ensure!(
            name != DEFAULT_INSTANCE,
            "Instance name {name:?} is reserved for the unnamed instance; rename the network section"
        );
        instance = Some(name);
    }
    Ok(instance)
}
