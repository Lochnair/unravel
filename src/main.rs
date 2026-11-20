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

use crate::app::App;
use crate::platform::warn_platform_limitations;
use crate::settings::{Settings, valid_instance_name};
use std::{process::ExitCode, time::Duration};

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// How long exit waits for blocking work (file writes, DNS lookups) still
/// running after the app has finished. Shapers are restored before this.
const RUNTIME_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);

fn main() -> anyhow::Result<ExitCode> {
    let instance = parse_args(std::env::args().skip(1))?;
    log::init(&instance)?;
    log::message(format_args!("Starting unRAVEL version {}", VERSION));

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(async {
        let settings = Settings::load(&instance)?;
        ::log::set_max_level(settings.output.log_level.to_level_filter());
        warn_platform_limitations();

        App::prepare(settings, &instance).await?.run().await
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
fn parse_args(mut args: impl Iterator<Item = String>) -> anyhow::Result<String> {
    let mut instance = None;
    while let Some(arg) = args.next() {
        let value = match arg.strip_prefix("--instance") {
            Some("") => args.next(),
            Some(rest) => rest.strip_prefix('=').map(str::to_owned),
            None => None,
        };
        let Some(name) = value else {
            anyhow::bail!("Unexpected argument {arg:?}; usage: unravel --instance <name>");
        };
        anyhow::ensure!(instance.is_none(), "--instance given more than once");
        anyhow::ensure!(
            valid_instance_name(&name),
            "Instance name {name:?} must be letters, digits and underscores"
        );
        instance = Some(name);
    }
    instance
        .ok_or_else(|| anyhow::anyhow!("Missing instance name; usage: unravel --instance <name>"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requires_one_valid_instance_name() {
        let parse = |args: &[&str]| parse_args(args.iter().map(|arg| (*arg).to_owned()));
        assert_eq!(parse(&["--instance", "wan"]).unwrap(), "wan");
        assert_eq!(parse(&["--instance=default"]).unwrap(), "default");
        for args in [
            &[][..],
            &["--instance"][..],
            &["--instance="][..],
            &["--instance", "../wan"][..],
            &["--instance", "wan", "--instance=wan2"][..],
        ] {
            assert!(parse(args).is_err(), "{args:?}");
        }
    }
}
