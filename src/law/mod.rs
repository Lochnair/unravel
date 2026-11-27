// SPDX-FileCopyrightText: 2026-Present Nils Andreas Svee mailto:contact@lochnair.net (github @Lochnair)
//
// SPDX-License-Identifier: MPL-2.0

mod context;
#[cfg(feature = "lua")]
mod script;
pub(crate) mod sqma;
mod store;

pub(crate) use context::LawContext;
pub(crate) use store::LawStore;

use std::{path::Path, time::Duration};

use anyhow::Context;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{diagnostics::DiagnosticsSender, settings::Settings};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub(crate) enum LawKind {
    Script,
    #[default]
    Sqma,
}

impl LawKind {
    /// The name in the config, which also names the law's own table.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            LawKind::Script => "script",
            LawKind::Sqma => "sqma",
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
    fn name(&self) -> &str;
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
            settings.law.script.clone(),
        )?)),
        #[cfg(not(feature = "lua"))]
        LawKind::Script => {
            anyhow::bail!("law \"script\" is not available: built without a script backend")
        }
        LawKind::Sqma => Ok(Box::new(sqma::SqmaLaw::new(
            settings.law.sqma.clone(),
            diagnostics,
        )?)),
    }
}

/// The configuration's JSON Schema. With a script, `law.script` lists the
/// options that script declares.
pub(crate) fn config_schema_json(script: Option<&Path>) -> anyhow::Result<String> {
    let mut schema = serde_json::to_value(Settings::json_schema())?;
    if let Some(path) = script {
        *schema
            .pointer_mut("/properties/law/properties/script")
            .context("the configuration schema has no law.script")? = script_schema(path)?;
    }
    Ok(serde_json::to_string_pretty(&schema)?)
}

#[cfg(feature = "lua")]
fn script_schema(path: &Path) -> anyhow::Result<serde_json::Value> {
    Ok(script::ScriptLaw::inspect(path)?.json_schema())
}

#[cfg(not(feature = "lua"))]
fn script_schema(path: &Path) -> anyhow::Result<serde_json::Value> {
    anyhow::bail!(
        "cannot read {}: built without a script backend",
        path.display()
    )
}
