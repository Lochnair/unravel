// SPDX-FileCopyrightText: 2026-Present Nils Andreas Svee mailto:contact@lochnair.net (github @Lochnair)
//
// SPDX-License-Identifier: MPL-2.0

mod context;
#[cfg(feature = "lua")]
mod script;
mod sqma;
mod store;

pub(crate) use context::LawContext;
pub(crate) use store::LawStore;

use std::{str::FromStr, time::Duration};

use crate::{diagnostics::DiagnosticsSender, settings::Settings};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LawKind {
    Script,
    Sqma,
}

impl LawKind {
    /// The name in the config, which also names the law's own table, its
    /// store folder and its metrics tag.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            LawKind::Script => "script",
            LawKind::Sqma => "sqma",
        }
    }
}

impl FromStr for LawKind {
    type Err = config::ConfigError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "script" => Ok(LawKind::Script),
            "sqma" => Ok(LawKind::Sqma),
            _ => Err(config::ConfigError::Message(format!(
                "unknown law {s:?}; available: script, sqma"
            ))),
        }
    }
}

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
    match settings.law.name {
        #[cfg(feature = "lua")]
        LawKind::Script => Ok(Box::new(script::ScriptLaw::new(
            settings.law.config.clone(),
        )?)),
        #[cfg(not(feature = "lua"))]
        LawKind::Script => {
            anyhow::bail!("law \"script\" is not available: built without a script backend")
        }
        LawKind::Sqma => Ok(Box::new(sqma::SqmaLaw::new(
            settings.law.config.clone(),
            diagnostics,
        )?)),
    }
}
