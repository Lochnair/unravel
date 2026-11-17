// SPDX-FileCopyrightText: 2026-Present Nils Andreas Svee mailto:contact@lochnair.net (github @Lochnair)
//
// SPDX-License-Identifier: MPL-2.0

mod context;
mod sqma;

pub(crate) use context::LawContext;

use crate::{diagnostics::DiagnosticsSender, settings::Settings};

const AVAILABLE: &[&str] = &["sqma"];

#[async_trait::async_trait]
pub trait Law: Send {
    async fn run(self: Box<Self>, ctx: LawContext) -> anyhow::Result<()>;
}

pub(crate) fn build(
    settings: &Settings,
    diagnostics: Option<DiagnosticsSender>,
) -> anyhow::Result<Box<dyn Law>> {
    match settings.law.name.as_str() {
        "sqma" => Ok(Box::new(sqma::SqmaLaw::new(settings.clone(), diagnostics))),
        other => anyhow::bail!("unknown law {other:?}; available: {}", AVAILABLE.join(", ")),
    }
}
