// SPDX-FileCopyrightText: 2026-Present Nils Andreas Svee mailto:contact@lochnair.net (github @Lochnair)
//
// SPDX-License-Identifier: MPL-2.0

//! The instance's state on ubus, for `ubus call unravel.wan status` and LuCI.

use std::{path::Path, sync::Arc, time::Instant};

use log::info;
use tokio::sync::watch;
use tokio_ubus::{DEFAULT_SOCKET, Object, Server, Table};

use crate::{
    VERSION,
    measurement::plan::AppliedState,
    runtime::{Direction, Runtime},
};

pub(crate) struct Status {
    server: Server,
}

struct State {
    runtime: Arc<Runtime>,
    applied: watch::Receiver<AppliedState>,
    instance: String,
    law: &'static str,
    dry_run: bool,
    started: Instant,
}

impl Status {
    /// The status object `unravel.<instance>`, or `None` without ubusd.
    pub(crate) fn new(
        runtime: Arc<Runtime>,
        applied: watch::Receiver<AppliedState>,
        instance: &str,
        law: &'static str,
        dry_run: bool,
    ) -> Option<Self> {
        if !Path::new(DEFAULT_SOCKET).exists() {
            info!("No ubus socket at {DEFAULT_SOCKET}; not publishing status");
            return None;
        }
        let state = Arc::new(State {
            runtime,
            applied,
            instance: instance.to_owned(),
            law,
            dry_run,
            started: Instant::now(),
        });
        let object = Object::new(format!("unravel.{instance}")).method("status", move |_| {
            let status = state.table();
            async move { Ok(Some(status)) }
        });
        Some(Self {
            server: Server::new().object(object),
        })
    }

    pub(crate) async fn run(self) -> anyhow::Result<()> {
        self.server.run().await
    }
}

impl State {
    fn table(&self) -> Table {
        let traffic = self.runtime.traffic();
        let reflectors = self.applied.borrow().active_targets().len();
        let mut table = Table::new()
            .with("version", VERSION)
            .with("instance", self.instance.as_str())
            .with("law", self.law)
            .with("dry_run", self.dry_run)
            .with(
                "uptime",
                i64::try_from(self.started.elapsed().as_secs()).unwrap_or(i64::MAX),
            )
            .with("reflectors", i32::try_from(reflectors).unwrap_or(i32::MAX));
        for direction in [Direction::Download, Direction::Upload] {
            let Some(shaper) = self.runtime.shaper(direction) else {
                continue;
            };
            let mut entry = Table::new()
                .with("interface", shaper.interface())
                .with("base_kbps", kbps(shaper.base_kbps))
                .with("min_kbps", kbps(shaper.min_kbps));
            if let Some(max) = shaper.max_kbps {
                entry.push("max_kbps", kbps(max));
            }
            if let Some(rate) = shaper.rate_kbps() {
                entry.push("rate_kbps", kbps(rate));
            }
            let load = traffic.map(|t| match direction {
                Direction::Download => t.rx_kbps,
                Direction::Upload => t.tx_kbps,
            });
            if let Some(load) = load {
                entry.push("traffic_kbps", kbps(load));
            }
            table.push(direction.as_str(), entry);
        }
        table
    }
}

/// Rates are whole kbit/s; `as` saturates and maps NaN to 0, so a bad value
/// cannot panic the handler.
fn kbps(rate: f64) -> i64 {
    rate.round() as i64
}
