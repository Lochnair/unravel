// SPDX-FileCopyrightText: 2026-Present Nils Andreas Svee mailto:contact@lochnair.net (github @Lochnair)
//
// SPDX-License-Identifier: MPL-2.0

use super::same_protocol;
use crate::measurement::model::MeasurementTarget;
use log::warn;

/// The targets this build can measure, sorted and so grouped by protocol in
/// preference order. Runs before anything else starts, and never changes after.
pub fn build(mut targets: Vec<MeasurementTarget>) -> anyhow::Result<Vec<MeasurementTarget>> {
    anyhow::ensure!(
        !targets.is_empty(),
        "no reflectors are configured: add reflectors.entries or pool files"
    );
    targets.sort_unstable();
    targets.dedup();
    // Sorting places the definitions of one IRTT endpoint next to each other,
    // and after dedup any two of them differ in authentication.
    if let Some(pair) = targets.windows(2).find(|pair| {
        matches!(
            pair,
            [
                MeasurementTarget::Irtt { endpoint: a, .. },
                MeasurementTarget::Irtt { endpoint: b, .. },
            ] if a == b
        )
    }) {
        anyhow::bail!("conflicting authentication for reflector {:?}", pair[0]);
    }
    if !cfg!(feature = "irtt") {
        let configured = targets.len();
        targets.retain(|target| !matches!(target, MeasurementTarget::Irtt { .. }));
        let skipped = configured - targets.len();
        if skipped > 0 {
            warn!("Skipping {skipped} IRTT reflectors: this build has no IRTT support");
        }
    }
    anyhow::ensure!(
        !targets.is_empty(),
        "no configured reflectors use a protocol this build supports"
    );
    Ok(targets)
}

/// One protocol's targets as a shuffled deck of inventory indices.
pub(super) struct InventoryGroup {
    targets: Vec<usize>,
    cursor: usize,
}

impl InventoryGroup {
    /// The next target in the deck that `eligible` accepts. A scan finishes
    /// before the deck is reshuffled, so every target is visited even when the
    /// scan crosses the end of the deck.
    pub fn deal(
        &mut self,
        rng: &mut fastrand::Rng,
        eligible: impl Fn(usize) -> bool,
    ) -> Option<usize> {
        if self.cursor >= self.targets.len() {
            rng.shuffle(&mut self.targets);
            self.cursor = 0;
        }
        for _ in 0..self.targets.len() {
            let target = self.targets[self.cursor % self.targets.len()];
            self.cursor += 1;
            if eligible(target) {
                return Some(target);
            }
        }
        None
    }
}

/// Groups in the inventory's protocol preference order.
pub(super) fn groups(
    inventory: &[MeasurementTarget],
    rng: &mut fastrand::Rng,
) -> Vec<InventoryGroup> {
    let mut start = 0;
    inventory
        .chunk_by(same_protocol)
        .map(|chunk| {
            let mut targets = (start..start + chunk.len()).collect::<Vec<_>>();
            start += chunk.len();
            rng.shuffle(&mut targets);
            InventoryGroup { targets, cursor: 0 }
        })
        .collect()
}
