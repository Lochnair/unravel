// SPDX-FileCopyrightText: 2026-Present Nils Andreas Svee mailto:contact@lochnair.net (github @Lochnair)
//
// SPDX-License-Identifier: MPL-2.0

mod context;
mod sqma;

pub(crate) use context::LawContext;

use std::time::Duration;

use crate::{diagnostics::DiagnosticsSender, settings::Settings};

const AVAILABLE: &[&str] = &["sqma"];

/// How often a law wants reflectors probed and traffic counters sampled.
#[derive(Clone, Copy, Debug)]
pub struct Cadence {
    pub probe: Duration,
    pub traffic: Duration,
}

#[async_trait::async_trait]
pub trait Law: Send {
    fn cadence(&self) -> Cadence;

    async fn run(self: Box<Self>, ctx: LawContext) -> anyhow::Result<()>;
}

pub(crate) fn build(
    settings: &Settings,
    diagnostics: Option<DiagnosticsSender>,
) -> anyhow::Result<Box<dyn Law>> {
    match settings.law.name.as_str() {
        "sqma" => Ok(Box::new(sqma::SqmaLaw::new(
            settings.law.config.clone(),
            diagnostics,
        )?)),
        other => anyhow::bail!("unknown law {other:?}; available: {}", AVAILABLE.join(", ")),
    }
}
